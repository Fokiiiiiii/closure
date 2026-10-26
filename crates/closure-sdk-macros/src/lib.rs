//! `#[derive(CommandArgs)]`: the options of a slash command as a struct.
//!
//! ```ignore
//! #[derive(CommandArgs)]
//! struct Warn {
//!     /// Member to warn
//!     user: UserId,
//!     /// Why (shown in the log)
//!     #[arg(max_length = 512)]
//!     reason: Option<String>,
//! }
//! ```
//!
//! Each field is one option: its name is the field's, its description the doc comment (or
//! `desc = "..."`), it is required unless the type is `Option<_>`. Attributes inside `#[arg(..)]`:
//! `name`, `desc`, `min`, `max` (numbers), `min_length`, `max_length` (strings), `choices`
//! (a list of strings or of integers) and `autocomplete` (a flag: the option's choices come from
//! the handler registered with `ModuleBuilder::autocomplete`).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, ExprLit, Fields, GenericArgument, Ident, Lit, LitStr, PathArguments,
    Type, parse_macro_input,
};

#[proc_macro_derive(CommandArgs, attributes(arg))]
pub fn derive_command_args(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// One option, as read from a field.
struct OptionField {
    ident: Ident,
    name: String,
    description: String,
    ty: Type,
    optional: bool,
    min: Option<f64>,
    max: Option<f64>,
    min_length: Option<u16>,
    max_length: Option<u16>,
    choices: Vec<TokenStream2>,
    autocomplete: bool,
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "CommandArgs can only be derived for a struct with named fields",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            input,
            "CommandArgs can only be derived for a struct with named fields",
        ));
    };
    let options = fields
        .named
        .iter()
        .map(option_of)
        .collect::<syn::Result<Vec<_>>>()?;

    let name = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let specs = options.iter().map(spec_of);
    let reads = options.iter().map(|option| {
        let (ident, ty, name) = (&option.ident, &option.ty, &option.name);
        if option.optional {
            quote! { #ident: ::closure_sdk::args::optional::<#ty>(invocation, #name)? }
        } else {
            quote! { #ident: ::closure_sdk::args::required::<#ty>(invocation, #name)? }
        }
    });
    Ok(quote! {
        impl #impl_generics ::closure_sdk::CommandArgs for #name #type_generics #where_clause {
            fn options() -> ::std::vec::Vec<::closure_sdk::OptionSpec> {
                ::std::vec![ #(#specs),* ]
            }

            fn parse(
                invocation: &::closure_sdk::CommandInvocation,
            ) -> ::std::result::Result<Self, ::closure_sdk::ArgError> {
                ::std::result::Result::Ok(Self { #(#reads),* })
            }
        }
    })
}

fn spec_of(option: &OptionField) -> TokenStream2 {
    let (name, description, ty) = (&option.name, &option.description, &option.ty);
    let required = (!option.optional).then(|| quote! { option.required = true; });
    let min = option
        .min
        .map(|v| quote! { option.min_value = ::std::option::Option::Some(#v); });
    let max = option
        .max
        .map(|v| quote! { option.max_value = ::std::option::Option::Some(#v); });
    let min_length = option
        .min_length
        .map(|v| quote! { option.min_length = ::std::option::Option::Some(#v); });
    let max_length = option
        .max_length
        .map(|v| quote! { option.max_length = ::std::option::Option::Some(#v); });
    let choices = (!option.choices.is_empty()).then(|| {
        let choices = &option.choices;
        quote! { option.choices = ::std::vec![ #(#choices),* ]; }
    });
    let autocomplete = option
        .autocomplete
        .then(|| quote! { option.autocomplete = true; });
    quote! {
        {
            let mut option = ::closure_sdk::OptionSpec::new(
                #name,
                #description,
                <#ty as ::closure_sdk::OptionType>::KIND,
            );
            #required #min #max #min_length #max_length #choices #autocomplete
            option
        }
    }
}

fn option_of(field: &syn::Field) -> syn::Result<OptionField> {
    let Some(ident) = field.ident.clone() else {
        return Err(syn::Error::new_spanned(field, "an option needs a name"));
    };
    let (optional, ty) = match option_inner(&field.ty) {
        Some(inner) => (true, inner.clone()),
        None => (false, field.ty.clone()),
    };
    let mut option = OptionField {
        name: ident.to_string(),
        description: doc_of(field),
        ident,
        ty,
        optional,
        min: None,
        max: None,
        min_length: None,
        max_length: None,
        choices: Vec::new(),
        autocomplete: false,
    };
    for attribute in field.attrs.iter().filter(|a| a.path().is_ident("arg")) {
        attribute.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "name" => option.name = meta.value()?.parse::<LitStr>()?.value(),
                "desc" => option.description = meta.value()?.parse::<LitStr>()?.value(),
                "min" => option.min = Some(number(&meta.value()?.parse()?)?),
                "max" => option.max = Some(number(&meta.value()?.parse()?)?),
                "min_length" => option.min_length = Some(length(&meta.value()?.parse()?)?),
                "max_length" => option.max_length = Some(length(&meta.value()?.parse()?)?),
                "choices" => option.choices = choices(&meta.value()?.parse()?)?,
                "autocomplete" => option.autocomplete = true,
                _ => return Err(meta.error("unknown option attribute")),
            }
            Ok(())
        })?;
    }
    if option.description.is_empty() {
        return Err(syn::Error::new_spanned(
            field,
            "an option needs a description: write a doc comment or `#[arg(desc = \"...\")]`",
        ));
    }
    Ok(option)
}

/// `T` of `Option<T>`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

/// The doc comment, as one line.
fn doc_of(field: &syn::Field) -> String {
    let lines: Vec<String> = field
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(value) => match &value.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(text),
                    ..
                }) => Some(text.value().trim().to_owned()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    lines.join(" ").trim().to_owned()
}

fn number(expr: &Expr) -> syn::Result<f64> {
    let (negative, literal) = match expr {
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => (true, &*unary.expr),
        other => (false, other),
    };
    let value = match literal {
        Expr::Lit(ExprLit {
            lit: Lit::Int(int), ..
        }) => int.base10_parse::<f64>()?,
        Expr::Lit(ExprLit {
            lit: Lit::Float(float),
            ..
        }) => float.base10_parse::<f64>()?,
        _ => return Err(syn::Error::new_spanned(expr, "expected a number")),
    };
    Ok(if negative { -value } else { value })
}

fn length(expr: &Expr) -> syn::Result<u16> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Int(int), ..
        }) => int.base10_parse::<u16>(),
        _ => Err(syn::Error::new_spanned(expr, "expected a whole number")),
    }
}

/// `["a", "b"]` or `[1, 2]`: each value is its own label.
fn choices(expr: &Expr) -> syn::Result<Vec<TokenStream2>> {
    let Expr::Array(array) = expr else {
        return Err(syn::Error::new_spanned(
            expr,
            "expected a list like [\"a\", \"b\"]",
        ));
    };
    array
        .elems
        .iter()
        .map(|element| match element {
            Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => {
                let label = text.value();
                Ok(quote! {
                    ::closure_sdk::OptionChoice {
                        name: ::std::string::ToString::to_string(#label),
                        value: ::closure_sdk::ChoiceValue::String(::std::string::ToString::to_string(#label)),
                    }
                })
            }
            Expr::Lit(ExprLit { lit: Lit::Int(int), .. }) => {
                let value = int.base10_parse::<i64>()?;
                let label = value.to_string();
                Ok(quote! {
                    ::closure_sdk::OptionChoice {
                        name: ::std::string::ToString::to_string(#label),
                        value: ::closure_sdk::ChoiceValue::Integer(#value),
                    }
                })
            }
            other => Err(syn::Error::new_spanned(other, "expected a string or a whole number")),
        })
        .collect()
}
