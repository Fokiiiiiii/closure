//! Buttons, select menus, forms and pages.
//!
//! Every component carries a `custom_id`, which is how a click finds its way back to the module
//! and the handler that offered it: `module:route:owner:data`. `route` names the handler
//! (`ModuleBuilder::component` / `modal`), `owner` is `*` or the id of the only user who may use
//! it, and `data` is whatever the module wants back (at most about 25 characters when the owner
//! is set: Discord allows 100 characters in all; keep more in the module's storage).

use closure_core::action::ModalSpec;
use closure_core::event::UserId;
use closure_core::module::ModuleId;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption, SelectMenuType, TextInput,
    TextInputStyle,
};

/// What a `custom_id` says, apart from the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomId {
    pub route: String,
    /// The only user who may use the component; `None` for anyone.
    pub owner: Option<UserId>,
    pub data: String,
}

impl CustomId {
    #[must_use]
    pub fn new(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            owner: None,
            data: String::new(),
        }
    }

    /// Only this user may use the component; the others are told it is not theirs.
    #[must_use]
    pub fn only_for(mut self, user: UserId) -> Self {
        self.owner = Some(user);
        self
    }

    #[must_use]
    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.data = data.into();
        self
    }

    /// The text Discord keeps: it starts with the id of the module.
    #[must_use]
    pub fn encode(&self, module: &ModuleId) -> String {
        let owner = self
            .owner
            .map_or_else(|| "*".to_owned(), |user| user.get().to_string());
        format!("{module}:{}:{owner}:{}", self.route, self.data)
    }

    /// Reads what [`Self::encode`] wrote, whatever module it is of.
    #[must_use]
    pub fn parse(custom_id: &str) -> Option<(ModuleId, Self)> {
        let mut parts = custom_id.splitn(4, ':');
        let module = ModuleId::new(parts.next()?).ok()?;
        let route = parts.next()?.to_owned();
        let owner = match parts.next()? {
            "*" => None,
            digits => Some(UserId::new_checked(digits.parse().ok()?)?),
        };
        let data = parts.next().unwrap_or_default().to_owned();
        (!route.is_empty()).then_some((module, Self { route, owner, data }))
    }
}

/// A button that does something when it is clicked.
#[must_use]
pub fn button(style: ButtonStyle, label: impl Into<String>, custom_id: String) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(custom_id),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

/// A button that opens a web page (`https://` only).
#[must_use]
pub fn link_button(label: impl Into<String>, url: impl Into<String>) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Link,
        url: Some(url.into()),
        sku_id: None,
    })
}

/// The same component, greyed out.
#[must_use]
pub fn disabled(mut component: Component) -> Component {
    match &mut component {
        Component::Button(button) => button.disabled = true,
        Component::SelectMenu(menu) => menu.disabled = true,
        _ => {}
    }
    component
}

/// A menu to choose from: `options` are `(label, value)`; the user picks between `min` and
/// `max` of them.
#[must_use]
pub fn select(
    custom_id: String,
    placeholder: Option<&str>,
    options: impl IntoIterator<Item = (String, String)>,
    min: u8,
    max: u8,
) -> Component {
    Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id,
        default_values: None,
        disabled: false,
        kind: SelectMenuType::Text,
        max_values: Some(max),
        min_values: Some(min),
        options: Some(
            options
                .into_iter()
                .map(|(label, value)| SelectMenuOption {
                    default: false,
                    description: None,
                    emoji: None,
                    label,
                    value,
                })
                .collect(),
        ),
        placeholder: placeholder.map(str::to_owned),
        required: None,
    })
}

/// A row of components: up to five buttons, or one select menu.
#[must_use]
pub fn row(components: impl IntoIterator<Item = Component>) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components: components.into_iter().collect(),
    })
}

/// One line of text in a form (`paragraph`: several lines). `name` is how the handler of the
/// form asks for it (`cx.field(name)`).
#[must_use]
pub fn text_input(
    module: &ModuleId,
    name: &str,
    label: &str,
    paragraph: bool,
    required: bool,
    max_length: u16,
) -> Component {
    #[allow(deprecated)]
    let input = TextInput {
        id: None,
        custom_id: format!("{module}:{name}"),
        label: Some(label.to_owned()),
        max_length: Some(max_length),
        min_length: None,
        placeholder: None,
        required: Some(required),
        style: if paragraph {
            TextInputStyle::Paragraph
        } else {
            TextInputStyle::Short
        },
        value: None,
    };
    row([Component::TextInput(input)])
}

/// A form: its `custom_id` brings the submission back to the module's `modal` handler. Every
/// input is a row made with [`text_input`].
#[must_use]
pub fn modal(
    custom_id: String,
    title: impl Into<String>,
    inputs: impl IntoIterator<Item = Component>,
) -> ModalSpec {
    ModalSpec {
        custom_id,
        title: title.into(),
        components: inputs.into_iter().collect(),
    }
}

/// Previous and next buttons for a list of pages, with the position in the middle.
///
/// The buttons carry the page they lead to; the handler of `route` reads it with
/// `cx.page_requested()`, answers with `cx.update(..)` and a pager for the new page.
#[derive(Debug, Clone)]
pub struct Pager {
    pub route: String,
    /// The page shown, counting from 0.
    pub page: usize,
    pub pages: usize,
    /// Only this user may turn the pages.
    pub owner: Option<UserId>,
}

impl Pager {
    #[must_use]
    pub fn new(route: impl Into<String>, page: usize, pages: usize) -> Self {
        Self {
            route: route.into(),
            page,
            pages,
            owner: None,
        }
    }

    #[must_use]
    pub fn only_for(mut self, user: UserId) -> Self {
        self.owner = Some(user);
        self
    }

    /// The row of buttons: back (off on the first page), "3/7", forward (off on the last).
    #[must_use]
    pub fn row(&self, module: &ModuleId) -> Component {
        let target = |page: usize| {
            let id = CustomId {
                route: self.route.clone(),
                owner: self.owner,
                data: page.to_string(),
            };
            id.encode(module)
        };
        let back = button(
            ButtonStyle::Secondary,
            "◀",
            target(self.page.saturating_sub(1)),
        );
        let next = button(ButtonStyle::Secondary, "▶", target(self.page + 1));
        let position = format!("{}/{}", self.page + 1, self.pages.max(1));
        // The middle button only shows where we are.
        let here = disabled(button(
            ButtonStyle::Secondary,
            position,
            format!("{module}:{}:*:here", self.route),
        ));
        row([
            if self.page == 0 { disabled(back) } else { back },
            here,
            if self.page + 1 >= self.pages {
                disabled(next)
            } else {
                next
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    fn module() -> ModuleId {
        ModuleId::new("warn").unwrap()
    }

    #[test]
    fn a_custom_id_round_trips_and_keeps_colons_in_its_data() {
        let id = CustomId::new("confirm").only_for(Id::new(7)).data("a:b:c");
        let text = id.encode(&module());
        assert_eq!(text, "warn:confirm:7:a:b:c");
        assert_eq!(CustomId::parse(&text), Some((module(), id)));

        let anyone = CustomId::new("page").data("3");
        assert_eq!(anyone.encode(&module()), "warn:page:*:3");
        assert_eq!(
            CustomId::parse("warn:page:*:3").map(|(_, id)| id),
            Some(anyone)
        );
        // Without data, and things that are not ours.
        assert_eq!(
            CustomId::parse("warn:x:*:").map(|(_, id)| id.data),
            Some(String::new())
        );
        for bad in [
            "",
            "warn",
            "warn:x",
            "warn::*:d",
            "Warn:x:*:d",
            "warn:x:abc:d",
            "warn:x:0:d",
        ] {
            assert_eq!(CustomId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_pager_turns_pages_and_switches_off_what_is_not_there() {
        let page_ids = |pager: &Pager| {
            let Component::ActionRow(row) = pager.row(&module()) else {
                panic!("a row")
            };
            row.components
                .iter()
                .map(|component| match component {
                    Component::Button(b) => (
                        b.custom_id.clone().unwrap(),
                        b.disabled,
                        b.label.clone().unwrap(),
                    ),
                    _ => panic!("buttons"),
                })
                .collect::<Vec<_>>()
        };
        let first = page_ids(&Pager::new("list", 0, 3));
        assert!(first[0].1);
        assert_eq!(first[1].2, "1/3");
        assert_eq!(
            first[2],
            ("warn:list:*:1".to_owned(), false, "▶".to_owned())
        );
        let middle = page_ids(&Pager::new("list", 1, 3).only_for(Id::new(7)));
        assert_eq!(
            middle[0],
            ("warn:list:7:0".to_owned(), false, "◀".to_owned())
        );
        assert_eq!(middle[2].0, "warn:list:7:2");
        let last = page_ids(&Pager::new("list", 2, 3));
        assert!(last[2].1 && !last[0].1);
        assert!(page_ids(&Pager::new("list", 0, 1))[2].1);
    }
}
