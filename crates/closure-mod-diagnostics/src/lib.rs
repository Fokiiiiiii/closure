//! The `diagnostics` module: `/ping` and `/about`.
//!
//! It is deliberately tiny and doubles as the reference for writing a module: a manifest that
//! declares everything the host needs to know, message catalogs in every supported language,
//! typed configuration read through [`EventContext::parse_config`], and Discord access only
//! through the capability-gated client in the context.
//!
//! `/about` also satisfies the AGPL-3.0 requirement to offer the source code to people who use
//! the bot over a network.

use async_trait::async_trait;
use closure_core::action::InteractionReply;
use closure_core::capability::Capability;
use closure_core::error::{CoreError, ModuleError};
use closure_core::event::{Event, EventKind};
use closure_core::locale::{Args, Locale};
use closure_core::module::{CommandSpec, EventContext, Module, ModuleId, ModuleManifest};
use serde::Deserialize;
use serde_json::json;

/// Where the source code of this program can be obtained.
pub const SOURCE_URL: &str = "https://github.com/Fokiiiiiii/closure";

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// Replies are only visible to the person who ran the command.
    ephemeral: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { ephemeral: true }
    }
}

fn command(name: &str, description: &str) -> CommandSpec {
    CommandSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        options: Vec::new(),
        subcommands: Vec::new(),
        default_member_permissions: None,
    }
}

#[derive(Debug)]
pub struct DiagnosticsModule {
    manifest: ModuleManifest,
}

impl DiagnosticsModule {
    pub fn new() -> Result<Self, CoreError> {
        let manifest = ModuleManifest::new(
            ModuleId::new("diagnostics")?,
            "Diagnostics",
            env!("CARGO_PKG_VERSION"),
        )
        .description("Liveness and version commands.")
        .events([EventKind::Command])
        .capabilities([Capability::RespondToInteractions])
        .command(command("ping", "Check that the bot is alive"))
        .command(command(
            "about",
            "Show the version and where to find the source code",
        ))
        .config(
            json!({
                "type": "object",
                "properties": {
                    "ephemeral": {
                        "type": "boolean",
                        "description": "Only show replies to the person who ran the command."
                    }
                },
                "additionalProperties": false
            }),
            json!({ "ephemeral": true }),
        )
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA)
        .enabled_by_default();
        Ok(Self { manifest })
    }
}

#[async_trait]
impl Module for DiagnosticsModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError> {
        let Event::Command(invocation) = event else {
            return Ok(());
        };
        let key = match invocation.command.as_str() {
            "ping" => "ping-reply",
            "about" => "about-reply",
            _ => return Ok(()),
        };
        let config: Config = ctx.parse_config()?;
        let args = Args::new()
            .with("version", env!("CARGO_PKG_VERSION"))
            .with("url", SOURCE_URL);
        let content = ctx.t_in(ctx.reply_locale(invocation), key, &args);
        ctx.api
            .respond(
                invocation,
                InteractionReply {
                    content: Some(content),
                    embeds: Vec::new(),
                    components: Vec::new(),
                    ephemeral: config.ephemeral,
                },
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::action::{Action, InteractionResponse};
    use closure_core::capability::CapabilitySet;
    use closure_core::client::{ApiClient, DiscordApi};
    use closure_core::event::{CommandInvocation, InteractionToken, UserInfo};
    use closure_core::registry::ModuleRegistry;
    use closure_core::settings::GuildPolicy;
    use closure_core::testing::MockApi;
    use closure_i18n::TranslatorBuilder;
    use twilight_model::guild::Permissions;
    use twilight_model::id::Id;

    use super::*;

    fn invocation(command: &str, locale: Option<Locale>) -> Event {
        Event::Command(CommandInvocation {
            interaction_id: Id::new(1),
            application_id: Id::new(2),
            token: InteractionToken::new("token"),
            guild_id: Id::new(3),
            channel_id: Id::new(4),
            user: UserInfo {
                id: Id::new(5),
                name: "user".into(),
                bot: false,
            },
            roles: vec![],
            invoker_permissions: Permissions::empty(),
            locale,
            command: command.into(),
            subcommand: None,
            options: Vec::new(),
        })
    }

    /// Runs the module the way the dispatcher does and returns what it sent to Discord.
    async fn run(event: &Event, guild_language: Locale, config: serde_json::Value) -> Vec<Action> {
        let module = DiagnosticsModule::new().unwrap();
        let registry =
            ModuleRegistry::new(vec![Arc::new(DiagnosticsModule::new().unwrap())]).unwrap();
        let mut translators = TranslatorBuilder::new().unwrap();
        translators.add_registry(&registry).unwrap();

        let api = Arc::new(MockApi::default());
        let granted: CapabilitySet = module.manifest().capabilities.clone();
        let ctx = EventContext::new(
            module.manifest().id.clone(),
            Id::new(3),
            ApiClient::new(
                Arc::clone(&api) as Arc<dyn DiscordApi>,
                granted,
                module.manifest().id.clone(),
            ),
            guild_language,
            Arc::new(GuildPolicy::default()),
            Arc::new(config),
            Arc::new(translators.build()),
        );
        module.on_event(&ctx, event).await.unwrap();
        api.actions()
    }

    fn reply(action: &Action) -> (String, bool) {
        let Action::Respond {
            response: InteractionResponse::Message { reply },
            ..
        } = action
        else {
            panic!("expected a message reply, got {action:?}");
        };
        (reply.content.clone().unwrap(), reply.ephemeral)
    }

    #[tokio::test]
    async fn ping_answers_privately_in_the_invokers_language() {
        let defaults = json!({ "ephemeral": true });
        let english = run(
            &invocation("ping", Some(Locale::En)),
            Locale::Ja,
            defaults.clone(),
        )
        .await;
        assert_eq!(
            reply(&english[0]),
            ("Pong! I'm up and running.".to_owned(), true)
        );

        let japanese = run(&invocation("ping", Some(Locale::Ja)), Locale::En, defaults).await;
        assert_eq!(reply(&japanese[0]).0, "Pong! 正常に動作しています。");
    }

    #[tokio::test]
    async fn without_an_invoker_locale_the_guild_language_is_used() {
        let actions = run(&invocation("ping", None), Locale::Ja, json!({})).await;
        assert!(reply(&actions[0]).0.contains("正常に動作"));
    }

    #[tokio::test]
    async fn about_names_the_version_and_where_the_source_is() {
        let actions = run(
            &invocation("about", Some(Locale::En)),
            Locale::En,
            json!({}),
        )
        .await;
        let (text, _) = reply(&actions[0]);
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains(SOURCE_URL), "{text}");
        assert!(text.contains("AGPL-3.0"), "{text}");
        assert!(!text.contains('{'), "an argument was left unfilled: {text}");
    }

    #[tokio::test]
    async fn replies_can_be_made_public_by_configuration() {
        let actions = run(
            &invocation("ping", None),
            Locale::En,
            json!({ "ephemeral": false }),
        )
        .await;
        assert!(!reply(&actions[0]).1);
    }

    #[tokio::test]
    async fn other_commands_and_events_are_ignored() {
        assert!(
            run(&invocation("warn", None), Locale::En, json!({}))
                .await
                .is_empty()
        );
        let unrelated = Event::GuildRemoved {
            guild_id: Id::new(3),
        };
        assert!(run(&unrelated, Locale::En, json!({})).await.is_empty());
    }

    #[tokio::test]
    async fn invalid_configuration_is_reported_not_ignored() {
        let module = DiagnosticsModule::new().unwrap();
        let ctx = EventContext::new(
            module.manifest().id.clone(),
            Id::new(3),
            ApiClient::new(
                Arc::new(MockApi::default()),
                module.manifest().capabilities.clone(),
                module.manifest().id.clone(),
            ),
            Locale::En,
            Arc::new(GuildPolicy::default()),
            Arc::new(json!({ "ephemeral": "yes" })),
            Arc::new(closure_core::locale::KeyTranslator),
        );
        let result = module.on_event(&ctx, &invocation("ping", None)).await;
        assert!(matches!(result, Err(ModuleError::Config(_))));
    }

    #[test]
    fn the_manifest_is_valid_and_its_commands_are_registered() {
        let registry =
            ModuleRegistry::new(vec![Arc::new(DiagnosticsModule::new().unwrap())]).unwrap();
        let names: Vec<&str> = registry.commands().map(|(_, c)| c.name.as_str()).collect();
        assert_eq!(names, ["ping", "about"]);
        assert!(registry.command_owner("about").is_some());
    }

    #[test]
    fn both_languages_define_the_same_messages() {
        assert_eq!(
            closure_i18n::missing_keys(EN, JA).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            closure_i18n::missing_keys(JA, EN).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_default_configuration_satisfies_its_own_schema() {
        let manifest = DiagnosticsModule::new().unwrap().manifest;
        let config: Config = serde_json::from_value(manifest.default_config).unwrap();
        assert!(config.ephemeral);
    }
}
