//! Publishing slash commands to Discord.
//!
//! Commands are declared as data in module manifests. Their names and descriptions in other
//! languages come from the module's message catalog by convention:
//!
//! ```ftl
//! command-warn = warn
//!     .description = Warn a member
//! option-warn-reason = reason
//!     .description = Why the member is warned
//! subcommand-warn-add = add
//! option-warn-add-user = user
//! ```
//!
//! A translation that is missing, or that Discord would refuse (uppercase or too long names),
//! is skipped with a warning so one bad string never blocks registering the commands.

use closure_core::error::DiscordError;
use closure_core::event::{ApplicationId, GuildId};
use closure_core::locale::{Args, Locale, Translator};
use closure_core::module::{
    ChoiceValue, CommandSpec, ContextCommandSpec, ContextKind, ModuleId, OptionKind, OptionSpec,
    SubcommandSpec,
};
use closure_core::registry::ModuleRegistry;
use twilight_http::Client;
use twilight_model::application::command::{Command, CommandOption, CommandType};
use twilight_model::application::interaction::InteractionContextType;
use twilight_util::builder::command::{
    AttachmentBuilder, BooleanBuilder, ChannelBuilder, CommandBuilder, IntegerBuilder,
    MentionableBuilder, NumberBuilder, RoleBuilder, StringBuilder, SubCommandBuilder,
    SubCommandGroupBuilder, UserBuilder,
};

use crate::error::map_http_error;

#[derive(Debug, thiserror::Error)]
#[error("command `{command}` of module `{module}` is not valid for Discord: {reason}")]
pub struct CommandBuildError {
    pub module: ModuleId,
    pub command: String,
    pub reason: String,
}

/// Where commands are published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Every server; Discord can take up to an hour to propagate changes.
    Global,
    /// One server, effective immediately. Use this while developing.
    Guild(GuildId),
}

fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.chars().count())
        && name == name.to_lowercase()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn valid_description(text: &str) -> bool {
    (1..=100).contains(&text.chars().count())
}

/// Finds the translations of one catalog entry in every non-default language.
struct Localizer<'a> {
    translator: &'a dyn Translator,
    namespace: &'a str,
}

impl Localizer<'_> {
    fn localized(&self, key: &str, valid: fn(&str) -> bool) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for locale in Locale::ALL.into_iter().filter(|l| *l != Locale::DEFAULT) {
            let Some(text) = self
                .translator
                .exact(locale, self.namespace, key, &Args::new())
            else {
                continue;
            };
            if !valid(&text) {
                tracing::warn!(namespace = self.namespace, key, %locale, "ignoring a translation Discord would refuse");
                continue;
            }
            for tag in locale.discord_tags() {
                out.push(((*tag).to_owned(), text.clone()));
            }
        }
        out
    }

    fn names(&self, key: &str) -> Vec<(String, String)> {
        self.localized(key, valid_name)
    }

    fn descriptions(&self, key: &str) -> Vec<(String, String)> {
        self.localized(&format!("{key}.description"), valid_description)
    }
}

macro_rules! build_option {
    ($builder:ident, $spec:expr, $names:expr, $descriptions:expr) => {
        build_option!($builder, $spec, $names, $descriptions, |builder| builder)
    };
    ($builder:ident, $spec:expr, $names:expr, $descriptions:expr, $limits:expr) => {
        $limits(
            $builder::new(&$spec.name, &$spec.description)
                .required($spec.required)
                .name_localizations($names)
                .description_localizations($descriptions),
        )
        .build()
    };
}

/// The choices of an option whose values are of one kind, as Discord takes them.
fn choices<T>(spec: &OptionSpec, value: impl Fn(&ChoiceValue) -> Option<T>) -> Vec<(String, T)> {
    spec.choices
        .iter()
        .filter_map(|choice| Some((choice.name.clone(), value(&choice.value)?)))
        .collect()
}

fn option(spec: &OptionSpec, localizer: &Localizer<'_>, key: &str) -> CommandOption {
    let mut built = typed_option(spec, localizer, key);
    // Twilight's builders have no switch for autocompletion; the field is set on the result.
    if spec.autocomplete {
        built.autocomplete = Some(true);
    }
    built
}

fn typed_option(spec: &OptionSpec, localizer: &Localizer<'_>, key: &str) -> CommandOption {
    let names = localizer.names(key);
    let descriptions = localizer.descriptions(key);
    // Whole numbers a range may be written with; the manifest checked that it is in order.
    #[allow(clippy::cast_possible_truncation)]
    let whole = |value: f64| value as i64;
    match spec.kind {
        OptionKind::String => build_option!(
            StringBuilder,
            spec,
            names,
            descriptions,
            |mut b: StringBuilder| {
                let picks = choices(spec, |value| match value {
                    ChoiceValue::String(text) => Some(text.clone()),
                    _ => None,
                });
                if !picks.is_empty() {
                    b = b.choices(picks);
                }
                if let Some(min) = spec.min_length {
                    b = b.min_length(min);
                }
                if let Some(max) = spec.max_length {
                    b = b.max_length(max);
                }
                b
            }
        ),
        OptionKind::Integer => build_option!(
            IntegerBuilder,
            spec,
            names,
            descriptions,
            |mut b: IntegerBuilder| {
                let picks = choices(spec, |value| match value {
                    ChoiceValue::Integer(number) => Some(*number),
                    _ => None,
                });
                if !picks.is_empty() {
                    b = b.choices(picks);
                }
                if let Some(min) = spec.min_value {
                    b = b.min_value(whole(min));
                }
                if let Some(max) = spec.max_value {
                    b = b.max_value(whole(max));
                }
                b
            }
        ),
        OptionKind::Number => build_option!(
            NumberBuilder,
            spec,
            names,
            descriptions,
            |mut b: NumberBuilder| {
                let picks = choices(spec, |value| match value {
                    ChoiceValue::Number(number) => Some(*number),
                    _ => None,
                });
                if !picks.is_empty() {
                    b = b.choices(picks);
                }
                if let Some(min) = spec.min_value {
                    b = b.min_value(min);
                }
                if let Some(max) = spec.max_value {
                    b = b.max_value(max);
                }
                b
            }
        ),
        OptionKind::Boolean => build_option!(BooleanBuilder, spec, names, descriptions),
        OptionKind::User => build_option!(UserBuilder, spec, names, descriptions),
        OptionKind::Channel => build_option!(ChannelBuilder, spec, names, descriptions),
        OptionKind::Role => build_option!(RoleBuilder, spec, names, descriptions),
        OptionKind::Mentionable => build_option!(MentionableBuilder, spec, names, descriptions),
        OptionKind::Attachment => build_option!(AttachmentBuilder, spec, names, descriptions),
    }
}

/// A subcommand. `parent` is the catalog path of its command or group, so its name is
/// `subcommand-{parent}-{name}` and its options are `option-{parent}-{name}-{option}`.
fn subcommand_builder(
    parent: &str,
    spec: &SubcommandSpec,
    localizer: &Localizer<'_>,
) -> SubCommandBuilder {
    let path = format!("{parent}-{}", spec.name);
    let key = format!("subcommand-{parent}-{}", spec.name);
    let mut builder = SubCommandBuilder::new(&spec.name, &spec.description)
        .name_localizations(localizer.names(&key))
        .description_localizations(localizer.descriptions(&key));
    for opt in &spec.options {
        let key = format!("option-{path}-{}", opt.name);
        builder = builder.option(option(opt, localizer, &key));
    }
    builder
}

/// A subcommand, or a group of subcommands (which has none of its own options).
fn subcommand(parent: &str, spec: &SubcommandSpec, localizer: &Localizer<'_>) -> CommandOption {
    if spec.subcommands.is_empty() {
        return subcommand_builder(parent, spec, localizer).build();
    }
    let path = format!("{parent}-{}", spec.name);
    let key = format!("subcommand-{parent}-{}", spec.name);
    SubCommandGroupBuilder::new(&spec.name, &spec.description)
        .name_localizations(localizer.names(&key))
        .description_localizations(localizer.descriptions(&key))
        .subcommands(
            spec.subcommands
                .iter()
                .map(|sub| subcommand_builder(&path, sub, localizer)),
        )
        .build()
}

fn command(
    module: &ModuleId,
    spec: &CommandSpec,
    translator: &dyn Translator,
) -> Result<Command, CommandBuildError> {
    let localizer = Localizer {
        translator,
        namespace: module.as_str(),
    };
    let key = format!("command-{}", spec.name);
    let mut builder = CommandBuilder::new(&spec.name, &spec.description, CommandType::ChatInput)
        .name_localizations(localizer.names(&key))
        .description_localizations(localizer.descriptions(&key))
        // Moderation commands only make sense inside a server.
        .contexts([InteractionContextType::Guild]);
    if let Some(permissions) = spec.default_member_permissions {
        builder = builder.default_member_permissions(permissions);
    }
    for opt in &spec.options {
        let key = format!("option-{}-{}", spec.name, opt.name);
        builder = builder.option(option(opt, &localizer, &key));
    }
    for sub in &spec.subcommands {
        builder = builder.option(subcommand(&spec.name, sub, &localizer));
    }
    builder
        .validate()
        .map(CommandBuilder::build)
        .map_err(|error| CommandBuildError {
            module: module.clone(),
            command: spec.name.clone(),
            reason: error.to_string(),
        })
}

/// A context-menu command: a name, no description and no options.
fn context_command(
    module: &ModuleId,
    spec: &ContextCommandSpec,
    translator: &dyn Translator,
) -> Result<Command, CommandBuildError> {
    let localizer = Localizer {
        translator,
        namespace: module.as_str(),
    };
    let kind = match spec.kind {
        ContextKind::User => CommandType::User,
        ContextKind::Message => CommandType::Message,
    };
    CommandBuilder::new(&spec.name, "", kind)
        .name_localizations(localizer.names(&format!("command-{}", spec.name)))
        .contexts([InteractionContextType::Guild])
        .validate()
        .map(CommandBuilder::build)
        .map_err(|error| CommandBuildError {
            module: module.clone(),
            command: spec.name.clone(),
            reason: error.to_string(),
        })
}

/// Every command of every registered module, slash and context-menu, ready to publish.
pub fn build_commands(
    registry: &ModuleRegistry,
    translator: &dyn Translator,
) -> Result<Vec<Command>, CommandBuildError> {
    let mut commands: Vec<Command> = registry
        .commands()
        .map(|(module, spec)| command(module, spec, translator))
        .collect::<Result<_, _>>()?;
    for (module, spec) in registry.context_commands() {
        commands.push(context_command(module, spec, translator)?);
    }
    Ok(commands)
}

/// Replaces the published commands of `scope` with `commands`. The call is idempotent: the
/// result is the same however often it runs. Returns how many commands Discord now has.
///
/// Commands of modules that are disabled in a guild stay visible; invoking one gets a private
/// "that feature is turned off" answer from the dispatcher.
pub async fn register_commands(
    http: &Client,
    application_id: ApplicationId,
    scope: CommandScope,
    commands: &[Command],
) -> Result<usize, DiscordError> {
    let interaction = http.interaction(application_id);
    let response = match scope {
        CommandScope::Global => interaction.set_global_commands(commands).await,
        CommandScope::Guild(guild) => interaction.set_guild_commands(guild, commands).await,
    }
    .map_err(|error| map_http_error(&error))?;
    let published = response.models().await.map_err(|error| {
        DiscordError::Fatal(format!("could not read Discord's response: {error}"))
    })?;
    Ok(published.len())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::module::{Module, ModuleManifest, OptionChoice};
    use closure_core::testing::StubModule;
    use closure_i18n::TranslatorBuilder;
    use twilight_model::application::command::CommandOptionType;
    use twilight_model::guild::Permissions;

    use super::*;

    const EN: &str = "
command-warn = warn
    .description = Warn a member
option-warn-user = user
    .description = Member to warn
subcommand-warn-list = list
    .description = List warnings
";
    const JA: &str = "
command-warn = 警告
    .description = メンバーに警告します
option-warn-user = ユーザー
    .description = 警告する相手
subcommand-warn-list = 一覧
    .description = 警告の一覧を表示します
";

    fn unlocalized(map: Option<&std::collections::HashMap<String, String>>) -> bool {
        map.is_none_or(std::collections::HashMap::is_empty)
    }

    fn option_spec(name: &str, kind: OptionKind, required: bool) -> OptionSpec {
        let option = OptionSpec::new(name, format!("{name} option"), kind);
        if required { option.required() } else { option }
    }

    fn registry(spec: CommandSpec) -> ModuleRegistry {
        let manifest =
            ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0").command(spec);
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        ModuleRegistry::new(vec![module]).unwrap()
    }

    fn translator(en: &str, ja: &str) -> impl Translator {
        let mut builder = TranslatorBuilder::new().unwrap();
        builder.add_catalog("warn", Locale::En, en).unwrap();
        builder.add_catalog("warn", Locale::Ja, ja).unwrap();
        builder.build()
    }

    fn warn_spec() -> CommandSpec {
        CommandSpec {
            name: "warn".into(),
            description: "Warn a member".into(),
            options: vec![
                option_spec("user", OptionKind::User, true),
                option_spec("note", OptionKind::String, false),
            ],
            subcommands: vec![],
            default_member_permissions: Some(Permissions::MODERATE_MEMBERS),
        }
    }

    fn built(spec: CommandSpec, en: &str, ja: &str) -> Command {
        let commands = build_commands(&registry(spec), &translator(en, ja)).unwrap();
        assert_eq!(commands.len(), 1);
        commands.into_iter().next().unwrap()
    }

    #[test]
    fn commands_carry_their_japanese_names_and_descriptions() {
        let command = built(warn_spec(), EN, JA);
        assert_eq!(command.name, "warn");
        assert_eq!(command.description, "Warn a member");
        assert_eq!(command.name_localizations.as_ref().unwrap()["ja"], "警告");
        assert_eq!(
            command.description_localizations.as_ref().unwrap()["ja"],
            "メンバーに警告します"
        );
        let user = &command.options[0];
        assert_eq!(user.kind, CommandOptionType::User);
        assert!(user.required.unwrap());
        assert_eq!(user.name_localizations.as_ref().unwrap()["ja"], "ユーザー");
        assert_eq!(
            user.description_localizations.as_ref().unwrap()["ja"],
            "警告する相手"
        );
    }

    #[test]
    fn commands_are_guild_only_and_keep_their_default_permissions() {
        let command = built(warn_spec(), EN, JA);
        assert_eq!(command.contexts, Some(vec![InteractionContextType::Guild]));
        assert_eq!(
            command.default_member_permissions,
            Some(Permissions::MODERATE_MEMBERS)
        );
        assert_eq!(command.kind, CommandType::ChatInput);
    }

    #[test]
    fn untranslated_text_simply_has_no_localization() {
        let command = built(warn_spec(), EN, "");
        assert!(unlocalized(command.name_localizations.as_ref()));
        assert!(unlocalized(command.options[0].name_localizations.as_ref()));
    }

    #[test]
    fn translations_discord_would_refuse_are_skipped_not_fatal() {
        let ja = format!(
            "command-warn = Warn\n    .description = {}",
            "a".repeat(101)
        );
        let command = built(warn_spec(), EN, &ja);
        assert!(
            unlocalized(command.name_localizations.as_ref()),
            "uppercase name"
        );
        assert!(unlocalized(command.description_localizations.as_ref()));
    }

    #[test]
    fn subcommands_are_nested_with_their_own_localizations() {
        let spec = CommandSpec {
            options: vec![],
            subcommands: vec![SubcommandSpec {
                name: "list".into(),
                description: "List warnings".into(),
                options: vec![option_spec("user", OptionKind::User, true)],
                subcommands: vec![],
            }],
            ..warn_spec()
        };
        let command = built(spec, EN, JA);
        let list = &command.options[0];
        assert_eq!(list.kind, CommandOptionType::SubCommand);
        assert_eq!(list.name_localizations.as_ref().unwrap()["ja"], "一覧");
        assert_eq!(list.options.as_ref().unwrap()[0].name, "user");
    }

    #[test]
    fn every_option_kind_maps_to_its_discord_type() {
        let kinds = [
            (OptionKind::String, CommandOptionType::String),
            (OptionKind::Integer, CommandOptionType::Integer),
            (OptionKind::Number, CommandOptionType::Number),
            (OptionKind::Boolean, CommandOptionType::Boolean),
            (OptionKind::User, CommandOptionType::User),
            (OptionKind::Channel, CommandOptionType::Channel),
            (OptionKind::Role, CommandOptionType::Role),
            (OptionKind::Mentionable, CommandOptionType::Mentionable),
            (OptionKind::Attachment, CommandOptionType::Attachment),
        ];
        for (kind, expected) in kinds {
            let spec = CommandSpec {
                options: vec![option_spec("value", kind, true)],
                ..warn_spec()
            };
            assert_eq!(built(spec, EN, JA).options[0].kind, expected, "{kind:?}");
        }
    }

    #[test]
    fn choices_ranges_and_lengths_are_published() {
        let mut level = OptionSpec::new("level", "How serious", OptionKind::Integer);
        level.min_value = Some(1.0);
        level.max_value = Some(5.0);
        let mut ratio = OptionSpec::new("ratio", "A ratio", OptionKind::Number);
        ratio.min_value = Some(0.5);
        let mut note = OptionSpec::new("note", "A note", OptionKind::String);
        note.min_length = Some(2);
        note.max_length = Some(40);
        let mut kind = OptionSpec::new("kind", "Which", OptionKind::String);
        kind.choices = vec![
            OptionChoice {
                name: "Spam".into(),
                value: ChoiceValue::String("spam".into()),
            },
            OptionChoice {
                name: "Abuse".into(),
                value: ChoiceValue::String("abuse".into()),
            },
        ];
        let spec = CommandSpec {
            options: vec![level, ratio, note, kind],
            ..warn_spec()
        };
        let command = built(spec, EN, JA);
        let [level, ratio, note, kind] = &command.options[..] else {
            panic!("four options");
        };
        assert_eq!(
            level.min_value,
            Some(twilight_model::application::command::CommandOptionValue::Integer(1))
        );
        assert_eq!(
            level.max_value,
            Some(twilight_model::application::command::CommandOptionValue::Integer(5))
        );
        assert_eq!(
            ratio.min_value,
            Some(twilight_model::application::command::CommandOptionValue::Number(0.5))
        );
        assert_eq!((note.min_length, note.max_length), (Some(2), Some(40)));
        let names: Vec<&str> = kind
            .choices
            .iter()
            .flatten()
            .map(|choice| choice.name.as_str())
            .collect();
        assert_eq!(names, ["Spam", "Abuse"]);
        assert_eq!(level.choices.iter().flatten().count(), 0);
        assert_eq!(note.choices.iter().flatten().count(), 0);
    }

    #[test]
    fn context_commands_are_published_with_their_kind_and_no_description() {
        let manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0")
            .context_command(ContextCommandSpec {
                name: "report".into(),
                kind: ContextKind::Message,
            });
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = ModuleRegistry::new(vec![module]).unwrap();
        let commands = build_commands(&registry, &translator(EN, JA)).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].kind, CommandType::Message);
        assert_eq!(commands[0].name, "report");
        assert_eq!(commands[0].description, "");
        assert!(commands[0].options.is_empty());
    }

    #[test]
    fn autocomplete_options_are_published_with_the_flag() {
        let mut reason = option_spec("reason", OptionKind::String, false);
        reason.autocomplete = true;
        let spec = CommandSpec {
            options: vec![reason, option_spec("note", OptionKind::String, false)],
            ..warn_spec()
        };
        let command = built(spec, EN, JA);
        assert_eq!(command.options[0].autocomplete, Some(true));
        assert_ne!(command.options[1].autocomplete, Some(true));
    }

    #[test]
    fn subcommand_groups_are_published_with_their_subcommands() {
        let group = SubcommandSpec {
            name: "list".into(),
            description: "List warnings".into(),
            options: vec![],
            subcommands: vec![SubcommandSpec {
                name: "add".into(),
                description: "Add one".into(),
                options: vec![option_spec("user", OptionKind::User, true)],
                subcommands: vec![],
            }],
        };
        let spec = CommandSpec {
            options: vec![],
            subcommands: vec![group],
            ..warn_spec()
        };
        let command = built(spec, EN, JA);
        let list = &command.options[0];
        assert_eq!(list.kind, CommandOptionType::SubCommandGroup);
        assert_eq!(list.name_localizations.as_ref().unwrap()["ja"], "一覧");
        let add = &list.options.as_ref().unwrap()[0];
        assert_eq!(add.kind, CommandOptionType::SubCommand);
        assert_eq!(add.options.as_ref().unwrap()[0].name, "user");
    }
}
