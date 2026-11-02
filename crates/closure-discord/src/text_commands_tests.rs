use std::cell::Cell;

use closure_core::event::{
    AttachmentInfo, CommandOption, CommandOptionValue, Mentionable, UserInfo,
};
use closure_core::locale::Locale;
use closure_core::module::{
    ChoiceValue, CommandSpec, OptionChoice, OptionKind, OptionSpec, SubcommandSpec,
};
use closure_i18n::TranslatorBuilder;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use super::*;

const PREFIX: &str = "!";

#[tokio::test(start_paused = true)]
async fn private_reply_deletion_uses_the_reply_channel() {
    use closure_core::testing::{MockApi, StaticSettings};

    let inner = Arc::new(MockApi::default());
    let api = TextCommandApi::new(
        Arc::clone(&inner) as Arc<dyn DiscordApi>,
        Arc::new(InteractionGuilds::default()),
        Arc::new(StaticSettings::new([])),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
    );
    let reply = inner
        .execute(Action::GetMessage {
            channel_id: Id::new(300),
            message_id: Id::new(55),
        })
        .await
        .unwrap();
    let deletions = Deletions {
        channel: Id::new(3),
        command: Id::new(44),
        command_after: Some(Duration::from_secs(1)),
        reply_after: Some(Duration::from_secs(1)),
    };
    api.schedule_deletions(&deletions, &Reason::new("cleanup").unwrap(), &reply, None);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let deleted: Vec<_> = inner
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::DeleteMessage {
                channel_id,
                message_id,
                ..
            } => Some((channel_id.get(), message_id.get())),
            _ => None,
        })
        .collect();
    assert!(deleted.contains(&(3, 44)), "{deleted:?}");
    assert!(deleted.contains(&(300, 55)), "{deleted:?}");
}

struct ReplyApi(closure_core::testing::MockApi);

#[async_trait]
impl DiscordApi for ReplyApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        let text_reply = match &action {
            Action::EditResponse { token, .. } | Action::Followup { token, .. } => {
                token.text_command_message()
            }
            _ => None,
        };
        let outcome = self.0.execute(action).await?;
        if let Some((channel_id, _)) = text_reply {
            self.0
                .execute(Action::GetMessage {
                    channel_id,
                    message_id: Id::new(55),
                })
                .await
        } else {
            Ok(outcome)
        }
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.0.channel_guild(channel).await
    }

    async fn guild_owner(&self, guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        self.0.guild_owner(guild).await
    }
}

#[tokio::test(start_paused = true)]
async fn ephemeral_text_replies_are_stripped_and_deleted_for_all_answer_types() {
    use closure_core::action::InteractionReply;
    use closure_core::testing::{MockApi, StaticSettings};

    let inner = Arc::new(ReplyApi(MockApi::default()));
    let interactions = Arc::new(InteractionGuilds::default());
    let token = InteractionToken::for_text_command(Id::new(3), Id::new(44));
    assert!(interactions.register(&token, Id::new(1)).is_recorded());
    let api = TextCommandApi::new(
        Arc::clone(&inner) as Arc<dyn DiscordApi>,
        interactions,
        Arc::new(StaticSettings::new([])),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
    );
    let reply = InteractionReply {
        content: Some("reply".into()),
        ephemeral: true,
        components: vec![twilight_model::channel::message::Component::ActionRow(
            twilight_model::channel::message::component::ActionRow {
                id: None,
                components: vec![],
            },
        )],
        ..InteractionReply::default()
    };
    for action in [
        Action::Respond {
            interaction_id: Id::new(44),
            token: token.clone(),
            response: InteractionResponse::Message {
                reply: reply.clone(),
            },
        },
        Action::EditResponse {
            token: token.clone(),
            reply: reply.clone(),
        },
        Action::Followup {
            token: token.clone(),
            reply,
        },
    ] {
        api.execute(action).await.unwrap();
    }
    let actions = inner.0.actions();
    for reply in actions.iter().filter_map(|action| match action {
        Action::Respond {
            response: InteractionResponse::Message { reply },
            ..
        }
        | Action::EditResponse { reply, .. }
        | Action::Followup { reply, .. } => Some(reply),
        _ => None,
    }) {
        assert!(reply.components.is_empty());
    }
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(
        inner
            .0
            .actions()
            .iter()
            .filter(|a| matches!(a, Action::DeleteMessage { .. }))
            .count(),
        0
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(inner.0.actions().iter().filter(|a| matches!(a, Action::DeleteMessage { channel_id, .. } if *channel_id == Id::new(3))).count(), 3);

    let public = Action::Respond {
        interaction_id: Id::new(44),
        token,
        response: InteractionResponse::Message {
            reply: InteractionReply {
                content: Some("public".into()),
                ..InteractionReply::default()
            },
        },
    };
    api.execute(public).await.unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(
        inner
            .0
            .actions()
            .iter()
            .filter(|a| matches!(a, Action::DeleteMessage { .. }))
            .count(),
        3
    );
}

#[tokio::test(start_paused = true)]
async fn configured_reply_delay_applies_to_ephemeral_text_replies() {
    use closure_core::action::InteractionReply;
    use closure_core::settings::GuildPolicy;
    use closure_core::testing::{MockApi, StaticSettings};
    let inner = Arc::new(MockApi::default());
    let interactions = Arc::new(InteractionGuilds::default());
    let token = InteractionToken::for_text_command(Id::new(3), Id::new(44));
    assert!(interactions.register(&token, Id::new(1)).is_recorded());
    let mut settings = StaticSettings::new([]);
    settings.policy = Arc::new(GuildPolicy {
        prefix_delete_reply_after: Some(Duration::from_secs(7)),
        ..GuildPolicy::default()
    });
    let api = TextCommandApi::new(
        Arc::clone(&inner) as Arc<dyn DiscordApi>,
        interactions,
        Arc::new(settings),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
    );
    api.execute(Action::Respond {
        interaction_id: Id::new(44),
        token,
        response: InteractionResponse::Message {
            reply: InteractionReply {
                ephemeral: true,
                content: Some("reply".into()),
                ..InteractionReply::default()
            },
        },
    })
    .await
    .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(
        !inner
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteMessage { .. }))
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        inner
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteMessage { .. }))
    );
}

#[tokio::test]
async fn ephemeral_text_reply_is_refused_when_deletion_queue_is_full() {
    use closure_core::action::InteractionReply;
    use closure_core::testing::{MockApi, StaticSettings};
    let inner = Arc::new(MockApi::default());
    let api = TextCommandApi::new(
        Arc::clone(&inner) as Arc<dyn DiscordApi>,
        Arc::new(InteractionGuilds::default()),
        Arc::new(StaticSettings::new([])),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
    );
    let _permit = Arc::clone(&api.pending)
        .acquire_many_owned(u32::try_from(MAX_PENDING_DELETIONS).unwrap())
        .await
        .unwrap();
    let result = api
        .execute(Action::Followup {
            token: InteractionToken::for_text_command(Id::new(3), Id::new(44)),
            reply: InteractionReply {
                ephemeral: true,
                ..InteractionReply::default()
            },
        })
        .await;
    assert!(result.is_err());
    assert!(inner.actions().is_empty());
}

fn opt(name: &str, kind: OptionKind) -> OptionSpec {
    OptionSpec::new(name, kind)
}

fn required(name: &str, kind: OptionKind) -> OptionSpec {
    opt(name, kind).required()
}

fn spec(name: &str, options: Vec<OptionSpec>, subcommands: Vec<SubcommandSpec>) -> CommandSpec {
    CommandSpec {
        name: name.into(),
        options,
        subcommands,
        default_member_permissions: None,
    }
}

fn sub(name: &str, options: Vec<OptionSpec>) -> SubcommandSpec {
    SubcommandSpec {
        name: name.into(),
        options,
        subcommands: Vec::new(),
    }
}

fn group(name: &str, subcommands: Vec<SubcommandSpec>) -> SubcommandSpec {
    SubcommandSpec {
        name: name.into(),
        options: Vec::new(),
        subcommands,
    }
}

fn commands() -> Vec<CommandSpec> {
    let choice = |name: &str, value: &str| OptionChoice {
        name: name.into(),
        value: ChoiceValue::String(value.into()),
    };
    let mut commands = vec![
        spec(
            "warn",
            vec![
                required("user", OptionKind::User),
                opt("reason", OptionKind::String),
            ],
            vec![],
        ),
        spec(
            "timeout",
            vec![
                required("user", OptionKind::User),
                required("duration", OptionKind::String),
                opt("reason", OptionKind::String),
            ],
            vec![],
        ),
        spec(
            "count",
            vec![
                OptionSpec {
                    min_value: Some(1.0),
                    max_value: Some(10.0),
                    ..required("n", OptionKind::Integer)
                },
                opt("f", OptionKind::Number),
                opt("on", OptionKind::Boolean),
            ],
            vec![],
        ),
        spec(
            "pick",
            vec![OptionSpec {
                choices: vec![choice("Spam", "spam"), choice("Raid", "raid")],
                ..required("kind", OptionKind::String)
            }],
            vec![],
        ),
        spec(
            "len",
            vec![OptionSpec {
                min_length: Some(2),
                max_length: Some(5),
                ..required("text", OptionKind::String)
            }],
            vec![],
        ),
        spec(
            "target",
            vec![
                opt("channel", OptionKind::Channel),
                opt("role", OptionKind::Role),
                opt("who", OptionKind::Mentionable),
            ],
            vec![],
        ),
        spec(
            "up",
            vec![
                required("first", OptionKind::Attachment),
                required("second", OptionKind::Attachment),
            ],
            vec![],
        ),
        spec(
            "auto",
            vec![],
            vec![
                sub("status", vec![]),
                group(
                    "rule",
                    vec![sub("add", vec![required("name", OptionKind::String)])],
                ),
            ],
        ),
    ];
    commands.extend(extra_commands());
    commands
}

fn extra_commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "ban",
            vec![
                required("user", OptionKind::User),
                opt("reason", OptionKind::String),
                opt("duration", OptionKind::String),
                opt("delete_days", OptionKind::Integer),
            ],
            vec![],
        ),
        spec(
            "lock",
            vec![
                opt("channel", OptionKind::Channel),
                opt("duration", OptionKind::String),
                opt("reason", OptionKind::String),
            ],
            vec![],
        ),
        spec(
            "pair",
            vec![
                required("first", OptionKind::User),
                required("second", OptionKind::User),
            ],
            vec![],
        ),
    ]
}

fn message(content: &str) -> MessageInfo {
    MessageInfo {
        removed_attachments: Vec::new(),
        previous_content: None,
        id: Id::new(44),
        channel_id: Id::new(3),
        guild_id: Id::new(1),
        author: UserInfo {
            id: Id::new(7),
            name: "mod".into(),
            bot: false,
        },
        author_roles: vec![Id::new(10)],
        content: content.into(),
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments: Vec::new(),
        embed_count: 0,
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: 0,
        edited_at: None,
    }
}

fn file(id: u64, name: &str) -> AttachmentInfo {
    AttachmentInfo {
        id,
        filename: name.into(),
        size: 10,
        content_type: None,
        url: format!("https://cdn.example/{name}"),
    }
}

/// Reads `message` the way the dispatcher does, with the test commands enabled.
fn read_message(message: &MessageInfo, prefix: &str, locale: Locale) -> Reading {
    let all = commands();
    let enabled: Vec<&CommandSpec> = all.iter().collect();
    let translator = TranslatorBuilder::new().unwrap().build();
    Reader {
        prefix,
        application: Id::new(60),
        locale,
        commands: &enabled,
        translator: &translator,
    }
    .read(message, |_| Permissions::SEND_MESSAGES)
}

fn read(content: &str) -> Reading {
    read_message(&message(content), PREFIX, Locale::En)
}

fn run(content: &str) -> CommandInvocation {
    match read(content) {
        Reading::Run(invocation) => invocation,
        other => panic!("expected a command for {content:?}, got {other:?}"),
    }
}

fn usage(content: &str) -> String {
    match read(content) {
        Reading::Usage { text, .. } => text,
        other => panic!("expected a usage line for {content:?}, got {other:?}"),
    }
}

fn values(content: &str) -> Vec<CommandOptionValue> {
    run(content)
        .options
        .into_iter()
        .map(|option| option.value)
        .collect()
}

fn text(value: &str) -> CommandOptionValue {
    CommandOptionValue::String(value.into())
}

#[test]
fn a_message_without_the_prefix_is_not_a_command() {
    assert_eq!(read("warn <@5> spam"), Reading::Ignore);
    assert_eq!(read(" !warn <@5>"), Reading::Ignore);
    assert_eq!(read("hello !warn <@5>"), Reading::Ignore);
}

#[test]
fn a_bare_prefix_or_only_spaces_after_it_run_nothing() {
    assert_eq!(read("!"), Reading::Ignore);
    assert_eq!(read("!   \t "), Reading::Ignore);
}

#[test]
fn bots_and_webhooks_never_run_text_commands() {
    let mut bot = message("!warn <@5>");
    bot.author.bot = true;
    assert_eq!(read_message(&bot, PREFIX, Locale::En), Reading::Ignore);

    let mut hook = message("!warn <@5>");
    hook.from_webhook = true;
    assert_eq!(read_message(&hook, PREFIX, Locale::En), Reading::Ignore);
}

#[test]
fn the_command_word_is_case_insensitive_and_the_invocation_uses_the_declared_name() {
    assert_eq!(run("!WARN <@5>").command, "warn");
    assert_eq!(run("!Warn <@5>").command, "warn");
}

#[test]
fn unknown_command_words_are_ignored_silently() {
    assert_eq!(read("!nope <@5>"), Reading::Ignore);
    assert_eq!(read("!warnings"), Reading::Ignore);
}

#[test]
fn the_prefix_is_matched_literally_whatever_it_contains() {
    let content = |text: &str| read_message(&message(text), "$.", Locale::En);
    assert!(matches!(content("$.warn <@5>"), Reading::Run(_)));
    assert_eq!(content("$warn <@5>"), Reading::Ignore);
    assert_eq!(content("warn <@5>"), Reading::Ignore);

    let star = |text: &str| read_message(&message(text), ".*", Locale::En);
    assert!(matches!(star(".*warn <@5>"), Reading::Run(_)));
    assert_eq!(star("wordwarn <@5>"), Reading::Ignore);
}

#[test]
fn the_invocation_is_the_slash_form_with_the_message_as_its_interaction() {
    let author = message("!warn <@5> spam");
    let all = commands();
    let enabled: Vec<&CommandSpec> = all.iter().collect();
    let translator = TranslatorBuilder::new().unwrap().build();
    let reading = Reader {
        prefix: PREFIX,
        application: Id::new(60),
        locale: Locale::Ja,
        commands: &enabled,
        translator: &translator,
    }
    .read(&author, |_| Permissions::ADMINISTRATOR);
    let Reading::Run(invocation) = reading else {
        panic!("expected a command, got {reading:?}");
    };
    assert_eq!(
        invocation,
        CommandInvocation {
            interaction_id: Id::new(44),
            application_id: Id::new(60),
            token: InteractionToken::for_text_command(Id::new(3), Id::new(44)),
            guild_id: Id::new(1),
            channel_id: Id::new(3),
            user: author.author.clone(),
            roles: vec![Id::new(10)],
            invoker_permissions: Permissions::ADMINISTRATOR,
            // The guild's language is used when the invocation names none, as for slash commands.
            locale: None,
            command: "warn".into(),
            subcommand: None,
            options: vec![
                CommandOption {
                    name: "user".into(),
                    value: CommandOptionValue::User(Id::new(5)),
                },
                CommandOption {
                    name: "reason".into(),
                    value: text("spam"),
                },
            ],
        }
    );
}

#[test]
fn the_permissions_are_computed_only_for_a_command_that_runs() {
    let asked = Cell::new(false);
    let all = commands();
    let enabled: Vec<&CommandSpec> = all.iter().collect();
    let translator = TranslatorBuilder::new().unwrap().build();
    let reader = Reader {
        prefix: PREFIX,
        application: Id::new(60),
        locale: Locale::En,
        commands: &enabled,
        translator: &translator,
    };
    let answer = |_: &MessageInfo| {
        asked.set(true);
        Permissions::empty()
    };
    assert_eq!(reader.read(&message("!nope"), answer), Reading::Ignore);
    assert!(!asked.get());
    assert!(matches!(
        reader.read(&message("!warn <@5>"), answer),
        Reading::Run(_)
    ));
    assert!(asked.get());
}

#[test]
fn mentions_of_a_user_are_read_in_every_form() {
    assert_eq!(values("!warn <@5>"), [CommandOptionValue::User(Id::new(5))]);
    assert_eq!(
        values("!warn <@!5>"),
        [CommandOptionValue::User(Id::new(5))]
    );
    assert_eq!(values("!warn 5"), [CommandOptionValue::User(Id::new(5))]);
    assert_eq!(
        values("!warn \"<@5>\""),
        [CommandOptionValue::User(Id::new(5))]
    );
}

#[test]
fn a_role_mention_or_a_plain_name_is_not_a_user() {
    assert_eq!(usage("!warn <@&5>"), "Usage: !warn <user> [reason]");
    assert_eq!(usage("!warn @5"), "Usage: !warn <user> [reason]");
    assert_eq!(usage("!warn <5>"), "Usage: !warn <user> [reason]");
    assert_eq!(usage("!warn 0"), "Usage: !warn <user> [reason]");
    assert_eq!(usage("!warn <@5"), "Usage: !warn <user> [reason]");
}

#[test]
fn the_last_string_option_takes_the_rest_of_the_line() {
    assert_eq!(
        values("!warn <@5>   being   rude  "),
        [CommandOptionValue::User(Id::new(5)), text("being   rude")]
    );
    assert_eq!(
        values("!warn <@5> spam extra"),
        [CommandOptionValue::User(Id::new(5)), text("spam extra")]
    );
}

#[test]
fn a_quoted_word_is_one_value_and_keeps_its_spaces() {
    assert_eq!(
        values("!warn <@5> \"being rude\""),
        [CommandOptionValue::User(Id::new(5)), text("being rude")]
    );
    assert_eq!(
        values("!warn <@5> \"say \\\"hi\\\"\""),
        [CommandOptionValue::User(Id::new(5)), text("say \"hi\"")]
    );
}

#[test]
fn a_quoted_word_that_is_not_the_last_one_leaves_the_rest_to_the_next_option() {
    assert_eq!(
        usage("!warn <@5> \"being\" rude"),
        "Usage: !warn <user> [reason]"
    );
    assert_eq!(
        values("!timeout <@5> \"2 hours\" being rude"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("2 hours"),
            text("being rude"),
        ]
    );
}

#[test]
fn an_unclosed_quote_runs_to_the_end_of_the_line() {
    assert_eq!(
        values("!warn <@5> \"being rude"),
        [CommandOptionValue::User(Id::new(5)), text("being rude")]
    );
}

#[test]
fn a_string_option_before_others_takes_one_word_like_any_other_option() {
    assert_eq!(
        values("!timeout <@5> 1h being rude"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("1h"),
            text("being rude"),
        ]
    );
    assert_eq!(
        values("!timeout <@5> 1h"),
        [CommandOptionValue::User(Id::new(5)), text("1h")]
    );
}

#[test]
fn a_missing_required_option_answers_with_the_usage_line_and_runs_nothing() {
    assert_eq!(usage("!warn"), "Usage: !warn <user> [reason]");
    assert_eq!(
        usage("!timeout <@5>"),
        "Usage: !timeout <user> <duration> [reason]"
    );
}

#[test]
fn the_usage_line_is_localized_in_the_guild_language() {
    let reading = read_message(&message("!warn"), PREFIX, Locale::Ja);
    let Reading::Usage { text, .. } = reading else {
        panic!("expected a usage line, got {reading:?}");
    };
    assert_eq!(text, "使い方: !warn <user> [reason]");
}

#[test]
fn an_unparsable_value_answers_with_the_usage_line() {
    assert_eq!(usage("!count abc"), "Usage: !count <n> [f] [on]");
    assert_eq!(usage("!count 2.5"), "Usage: !count <n> [f] [on]");
}

#[test]
fn integers_and_numbers_are_parsed_by_kind() {
    assert_eq!(
        values("!count 3 1.5 yes"),
        [
            CommandOptionValue::Integer(3),
            CommandOptionValue::Number(1.5),
            CommandOptionValue::Boolean(true),
        ]
    );
    assert_eq!(
        values("!count 3 -2"),
        [
            CommandOptionValue::Integer(3),
            CommandOptionValue::Number(-2.0)
        ]
    );
    assert_eq!(usage("!count 3 nan"), "Usage: !count <n> [f] [on]");
}

#[test]
fn integers_respect_their_range() {
    assert_eq!(values("!count 1"), [CommandOptionValue::Integer(1)]);
    assert_eq!(values("!count 10"), [CommandOptionValue::Integer(10)]);
    assert_eq!(usage("!count 0"), "Usage: !count <n> [f] [on]");
    assert_eq!(usage("!count 11"), "Usage: !count <n> [f] [on]");
}

#[test]
fn booleans_accept_the_usual_words_in_any_case() {
    for (word, expected) in [
        ("true", true),
        ("YES", true),
        ("on", true),
        ("1", true),
        ("False", false),
        ("no", false),
        ("off", false),
        ("0", false),
    ] {
        assert_eq!(
            values(&format!("!count 2 1 {word}")).last(),
            Some(&CommandOptionValue::Boolean(expected)),
            "{word}"
        );
    }
    assert_eq!(usage("!count 2 1 maybe"), "Usage: !count <n> [f] [on]");
}

#[test]
fn choices_are_enforced_and_the_chosen_value_is_the_canonical_one() {
    assert_eq!(values("!pick SPAM"), [text("spam")]);
    assert_eq!(values("!pick raid"), [text("raid")]);
    assert_eq!(usage("!pick other"), "Usage: !pick <kind>");
}

#[test]
fn string_length_limits_count_characters() {
    assert_eq!(values("!len ab"), [text("ab")]);
    assert_eq!(values("!len あいう"), [text("あいう")]);
    assert_eq!(usage("!len a"), "Usage: !len <text>");
    assert_eq!(usage("!len abcdef"), "Usage: !len <text>");
}

#[test]
fn channels_roles_and_mentionables_are_read_in_their_mention_forms() {
    assert_eq!(
        values("!target <#7> <@&8> <@!9>"),
        [
            CommandOptionValue::Channel(Id::new(7)),
            CommandOptionValue::Role(Id::new(8)),
            CommandOptionValue::Mentionable(Mentionable::User(Id::new(9))),
        ]
    );
    assert_eq!(
        values("!target 7 8 <@&9>"),
        [
            CommandOptionValue::Channel(Id::new(7)),
            CommandOptionValue::Role(Id::new(8)),
            CommandOptionValue::Mentionable(Mentionable::Role(Id::new(9))),
        ]
    );
    assert_eq!(
        values("!target <@5>"),
        [CommandOptionValue::Mentionable(Mentionable::User(Id::new(
            5
        )))]
    );
    assert_eq!(
        usage("!target <#7> <#8>"),
        "Usage: !target [channel] [role] [who]"
    );
}

#[test]
fn typed_options_can_be_reordered_before_the_reason() {
    assert_eq!(
        values("!ban 7d <@5> spam"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("spam"),
            text("7d")
        ]
    );
    assert_eq!(
        values("!timeout 7d <@5> spam"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("7d"),
            text("spam")
        ]
    );
    assert_eq!(
        values("!lock 10m <#7> reason"),
        [
            CommandOptionValue::Channel(Id::new(7)),
            text("10m"),
            text("reason")
        ]
    );
    assert_eq!(
        values("!lock <#7> 10m reason"),
        values("!lock 10m <#7> reason")
    );
    assert_eq!(
        values("!ban <@5> spam 7d"),
        [CommandOptionValue::User(Id::new(5)), text("spam 7d")]
    );
    assert_eq!(
        values("!ban <@5> 7d 10m  spam"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("10m  spam"),
            text("7d")
        ]
    );
    assert!(matches!(read("!ban 7d spam"), Reading::Usage { .. }));
    assert_eq!(
        values("!pair <@5> <@6>"),
        [
            CommandOptionValue::User(Id::new(5)),
            CommandOptionValue::User(Id::new(6))
        ]
    );
    assert!(matches!(
        read("!pair <@5> <@6> <@7>"),
        Reading::Usage { .. }
    ));
}

#[test]
fn quoted_reason_allows_following_typed_options() {
    assert_eq!(
        values("!ban <@5> \"being rude\" 7d"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("being rude"),
            text("7d")
        ]
    );
    assert_eq!(
        values("!ban <@5> \"being rude\" 7d 3"),
        [
            CommandOptionValue::User(Id::new(5)),
            text("being rude"),
            text("7d"),
            CommandOptionValue::Integer(3)
        ]
    );
    assert!(matches!(
        read("!ban <@5> \"being rude\" invalid"),
        Reading::Usage { .. }
    ));
    assert_eq!(
        values("!ban <@5> spam 7d"),
        [CommandOptionValue::User(Id::new(5)), text("spam 7d")]
    );
}

#[test]
fn real_moderation_commands_accept_reordered_options() {
    use closure_core::module::Module;
    let module = closure_mod_moderation::module().unwrap();
    let commands: Vec<_> = module.manifest().commands.iter().collect();
    let translator = TranslatorBuilder::new().unwrap().build();
    let reader = Reader {
        prefix: PREFIX,
        application: Id::new(60),
        locale: Locale::En,
        commands: &commands,
        translator: &translator,
    };
    for (input, expected) in [
        (
            "!ban <@5> \"being rude\" 7d",
            vec![
                ("user", CommandOptionValue::User(Id::new(5))),
                ("reason", text("being rude")),
                ("duration", text("7d")),
            ],
        ),
        (
            "!ban <@5> \"being rude\" 7d 3",
            vec![
                ("user", CommandOptionValue::User(Id::new(5))),
                ("reason", text("being rude")),
                ("duration", text("7d")),
                ("delete_days", CommandOptionValue::Integer(3)),
            ],
        ),
        (
            "!ban 7d <@5> spam",
            vec![
                ("user", CommandOptionValue::User(Id::new(5))),
                ("reason", text("spam")),
                ("duration", text("7d")),
            ],
        ),
        (
            "!timeout 7d <@5> spam",
            vec![
                ("user", CommandOptionValue::User(Id::new(5))),
                ("duration", text("7d")),
                ("reason", text("spam")),
            ],
        ),
        (
            "!lock 10m <#7> reason",
            vec![
                ("channel", CommandOptionValue::Channel(Id::new(7))),
                ("duration", text("10m")),
                ("reason", text("reason")),
            ],
        ),
    ] {
        let Reading::Run(invocation) = reader.read(&message(input), |_| Permissions::empty())
        else {
            panic!("{input}")
        };
        assert_eq!(
            invocation.options,
            expected
                .into_iter()
                .map(|(name, value)| CommandOption {
                    name: name.into(),
                    value
                })
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn extra_words_without_an_option_to_take_them_are_a_usage_error() {
    assert_eq!(
        usage("!target <#7> <@&8> <@9> extra"),
        "Usage: !target [channel] [role] [who]"
    );
}

#[test]
fn attachments_are_taken_in_order_and_do_not_use_up_words() {
    let mut with_files = message("!up");
    with_files.attachments = vec![file(1, "a.txt"), file(2, "b.txt")];
    let Reading::Run(invocation) = read_message(&with_files, PREFIX, Locale::En) else {
        panic!("expected a command");
    };
    assert_eq!(
        invocation.options,
        [
            CommandOption {
                name: "first".into(),
                value: CommandOptionValue::Attachment(file(1, "a.txt")),
            },
            CommandOption {
                name: "second".into(),
                value: CommandOptionValue::Attachment(file(2, "b.txt")),
            },
        ]
    );

    let mut one = message("!up");
    one.attachments = vec![file(1, "a.txt")];
    let Reading::Usage { text, .. } = read_message(&one, PREFIX, Locale::En) else {
        panic!("a required attachment is missing, so the usage line answers");
    };
    assert_eq!(text, "Usage: !up <first> <second>");
}

#[test]
fn subcommands_and_groups_resolve_to_their_path() {
    let status = run("!auto status");
    assert_eq!(status.subcommand.as_deref(), Some("status"));
    assert!(status.options.is_empty());

    let add = run("!auto rule add my rule");
    assert_eq!(add.subcommand.as_deref(), Some("rule add"));
    assert_eq!(
        add.options,
        [CommandOption {
            name: "name".into(),
            value: text("my rule"),
        }]
    );
}

#[test]
fn a_plain_subcommand_and_a_group_of_one_command_both_read_as_text() {
    let all = [spec(
        "tree",
        vec![],
        vec![
            sub("status", vec![]),
            group(
                "counters",
                vec![sub("view", vec![required("counter", OptionKind::String)])],
            ),
        ],
    )];
    let enabled: Vec<&CommandSpec> = all.iter().collect();
    let translator = TranslatorBuilder::new().unwrap().build();
    let reader = Reader {
        prefix: PREFIX,
        application: Id::new(60),
        locale: Locale::En,
        commands: &enabled,
        translator: &translator,
    };
    let at = |content: &str| reader.read(&message(content), |_| Permissions::SEND_MESSAGES);

    let Reading::Run(status) = at("!tree status") else {
        panic!("the plain subcommand runs");
    };
    assert_eq!(status.subcommand.as_deref(), Some("status"));

    let Reading::Run(view) = at("!tree counters view strikes") else {
        panic!("the subcommand of the group runs");
    };
    assert_eq!(view.subcommand.as_deref(), Some("counters view"));
    assert_eq!(
        view.options,
        [CommandOption {
            name: "counter".into(),
            value: text("strikes"),
        }]
    );
}

#[test]
fn a_missing_or_unknown_subcommand_answers_with_the_choices() {
    assert_eq!(usage("!auto"), "Usage: !auto <status|rule>");
    assert_eq!(usage("!auto nope"), "Usage: !auto <status|rule>");
    assert_eq!(usage("!auto rule"), "Usage: !auto rule <add>");
    assert_eq!(usage("!auto rule nope"), "Usage: !auto rule <add>");
}

#[test]
fn the_usage_line_uses_the_prefix_the_guild_chose() {
    let reading = read_message(&message("$.warn"), "$.", Locale::En);
    let Reading::Usage { text, .. } = reading else {
        panic!("expected a usage line, got {reading:?}");
    };
    assert_eq!(text, "Usage: $.warn <user> [reason]");
}
