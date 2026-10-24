//! Turns gateway payloads into the stable [`Event`] model.
//!
//! Anything that is not about a guild (direct messages, interactions outside a guild) is dropped
//! here, so the rest of the pipeline can rely on every event belonging to one.

use closure_core::action::Emoji;
use closure_core::event::{
    ChannelInfo, CommandInvocation, CommandOption, CommandOptionValue, Event, GuildId,
    InteractionToken, MemberInfo, MessageInfo, ReactionInfo, RoleInfo, UserInfo,
};
use closure_core::locale::Locale;
use twilight_gateway::Event as Gateway;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue as TwilightValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::Channel;
use twilight_model::channel::message::{EmojiReactionType, Message};
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::guild::{Member, Role};
use twilight_model::user::User;
use twilight_model::util::Timestamp;

fn user(user: &User) -> UserInfo {
    UserInfo {
        id: user.id,
        name: user.name.clone(),
        bot: user.bot,
    }
}

fn seconds(timestamp: Timestamp) -> i64 {
    timestamp.as_secs()
}

fn member(guild_id: GuildId, member: &Member) -> MemberInfo {
    MemberInfo {
        guild_id,
        user: user(&member.user),
        roles: member.roles.clone(),
        nick: member.nick.clone(),
        joined_at: member.joined_at.map(seconds),
        timed_out_until: member.communication_disabled_until.map(seconds),
    }
}

fn message(message: &Message) -> Option<MessageInfo> {
    Some(MessageInfo {
        id: message.id,
        channel_id: message.channel_id,
        guild_id: message.guild_id?,
        author: user(&message.author),
        author_roles: message
            .member
            .as_ref()
            .map(|member| member.roles.clone())
            .unwrap_or_default(),
        content: message.content.clone(),
        mentioned_users: message.mentions.iter().map(|mention| mention.id).collect(),
        mentions_everyone: message.mention_everyone,
        attachment_count: u32::try_from(message.attachments.len()).unwrap_or(u32::MAX),
    })
}

fn channel(channel: &Channel) -> Option<ChannelInfo> {
    Some(ChannelInfo {
        id: channel.id,
        guild_id: channel.guild_id?,
        name: channel.name.clone(),
        parent_id: channel.parent_id,
    })
}

fn role(guild_id: GuildId, role: &Role) -> RoleInfo {
    RoleInfo {
        id: role.id,
        guild_id,
        name: role.name.clone(),
        position: role.position,
        permissions: role.permissions,
    }
}

fn emoji(emoji: &EmojiReactionType) -> Emoji {
    match emoji {
        EmojiReactionType::Custom { id, name, .. } => Emoji::Custom {
            id: *id,
            name: name.clone(),
        },
        EmojiReactionType::Unicode { name } => Emoji::Unicode {
            value: name.clone(),
        },
    }
}

fn option_value(value: &TwilightValue) -> Option<CommandOptionValue> {
    Some(match value {
        TwilightValue::String(text) => CommandOptionValue::String(text.clone()),
        TwilightValue::Integer(number) => CommandOptionValue::Integer(*number),
        TwilightValue::Number(number) => CommandOptionValue::Number(*number),
        TwilightValue::Boolean(flag) => CommandOptionValue::Boolean(*flag),
        TwilightValue::User(id) => CommandOptionValue::User(*id),
        TwilightValue::Channel(id) => CommandOptionValue::Channel(*id),
        TwilightValue::Role(id) => CommandOptionValue::Role(*id),
        // Attachments, mentionables and autocomplete state are not part of the module model.
        _ => return None,
    })
}

fn options(options: &[CommandDataOption]) -> Vec<CommandOption> {
    options
        .iter()
        .filter_map(|option| {
            Some(CommandOption {
                name: option.name.clone(),
                value: option_value(&option.value)?,
            })
        })
        .collect()
}

/// Splits `/command sub opt:...` into the subcommand name and its options. Subcommand groups
/// (two levels of nesting) are not supported by command specs and are dropped.
fn split_subcommand(raw: &[CommandDataOption]) -> Option<(Option<String>, Vec<CommandOption>)> {
    match raw {
        [
            CommandDataOption {
                name,
                value: TwilightValue::SubCommand(inner),
            },
        ] => Some((Some(name.clone()), options(inner))),
        [
            CommandDataOption {
                value: TwilightValue::SubCommandGroup(_),
                ..
            },
        ] => None,
        other => Some((None, options(other))),
    }
}

fn command(interaction: &Interaction) -> Option<CommandInvocation> {
    if interaction.kind != InteractionType::ApplicationCommand {
        return None;
    }
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    let (subcommand, options) = split_subcommand(&data.options)?;
    Some(CommandInvocation {
        interaction_id: interaction.id,
        application_id: interaction.application_id,
        token: InteractionToken::new(interaction.token.clone()),
        guild_id: interaction.guild_id?,
        channel_id: interaction.channel.as_ref()?.id,
        user: user(interaction.author()?),
        invoker_permissions: interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .unwrap_or_else(twilight_model::guild::Permissions::empty),
        locale: interaction
            .locale
            .as_deref()
            .and_then(Locale::from_discord_tag),
        command: data.name.clone(),
        subcommand,
        options,
    })
}

/// The module-facing event for a gateway event, if it is one modules care about.
pub(crate) fn event(gateway: &Gateway) -> Option<Event> {
    Some(match gateway {
        Gateway::GuildCreate(created) => match &**created {
            GuildCreate::Available(guild) => Event::GuildAvailable {
                guild_id: guild.id,
                name: guild.name.clone(),
                preferred_locale: Locale::from_discord_tag(&guild.preferred_locale),
            },
            GuildCreate::Unavailable(_) => return None,
        },
        // `unavailable` means a Discord outage, not that the bot was removed.
        Gateway::GuildDelete(deleted) if deleted.unavailable != Some(true) => Event::GuildRemoved {
            guild_id: deleted.id,
        },
        Gateway::MemberAdd(added) => Event::MemberJoined(member(added.guild_id, &added.member)),
        Gateway::MemberUpdate(updated) => Event::MemberUpdated(MemberInfo {
            guild_id: updated.guild_id,
            user: user(&updated.user),
            roles: updated.roles.clone(),
            nick: updated.nick.clone(),
            joined_at: updated.joined_at.map(seconds),
            timed_out_until: updated.communication_disabled_until.map(seconds),
        }),
        Gateway::MemberRemove(removed) => Event::MemberLeft {
            guild_id: removed.guild_id,
            user: user(&removed.user),
        },
        Gateway::MessageCreate(created) => Event::MessageCreated(message(&created.0)?),
        Gateway::MessageUpdate(updated) => Event::MessageUpdated(message(&updated.0)?),
        Gateway::MessageDelete(deleted) => Event::MessageDeleted {
            guild_id: deleted.guild_id?,
            channel_id: deleted.channel_id,
            message_id: deleted.id,
        },
        Gateway::MessageDeleteBulk(deleted) => Event::MessagesBulkDeleted {
            guild_id: deleted.guild_id?,
            channel_id: deleted.channel_id,
            message_ids: deleted.ids.clone(),
        },
        Gateway::ReactionAdd(added) => Event::ReactionAdded(reaction(&added.0)?),
        Gateway::ReactionRemove(removed) => Event::ReactionRemoved(reaction(&removed.0)?),
        Gateway::BanAdd(ban) => Event::BanAdded {
            guild_id: ban.guild_id,
            user: user(&ban.user),
        },
        Gateway::BanRemove(ban) => Event::BanRemoved {
            guild_id: ban.guild_id,
            user: user(&ban.user),
        },
        Gateway::ChannelCreate(created) => Event::ChannelCreated(channel(&created.0)?),
        Gateway::ChannelUpdate(updated) => Event::ChannelUpdated(channel(&updated.0)?),
        Gateway::ChannelDelete(deleted) => Event::ChannelDeleted(channel(&deleted.0)?),
        Gateway::RoleCreate(created) => Event::RoleCreated(role(created.guild_id, &created.role)),
        Gateway::RoleUpdate(updated) => Event::RoleUpdated(role(updated.guild_id, &updated.role)),
        Gateway::RoleDelete(deleted) => Event::RoleDeleted {
            guild_id: deleted.guild_id,
            role_id: deleted.role_id,
        },
        Gateway::InteractionCreate(interaction) => Event::Command(command(&interaction.0)?),
        _ => return None,
    })
}

fn reaction(reaction: &twilight_model::gateway::GatewayReaction) -> Option<ReactionInfo> {
    Some(ReactionInfo {
        guild_id: reaction.guild_id?,
        channel_id: reaction.channel_id,
        message_id: reaction.message_id,
        user_id: reaction.user_id,
        emoji: emoji(&reaction.emoji),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use twilight_model::gateway::payload::incoming::{
        BanAdd, GuildDelete, InteractionCreate, MessageCreate, MessageDelete,
    };

    use super::*;

    fn user_json(id: &str, bot: bool) -> Value {
        json!({ "id": id, "username": "alice", "discriminator": "0", "avatar": null, "bot": bot })
    }

    fn message_json(guild: Option<&str>) -> Value {
        let mut message = json!({
            "id": "100", "channel_id": "3", "content": "hello", "author": user_json("7", false),
            "attachments": [{ "id": "1", "filename": "a.png", "size": 1, "url": "u", "proxy_url": "p" }],
            "embeds": [], "mention_everyone": true, "mention_roles": [],
            "mentions": [{ "id": "8", "username": "bob", "discriminator": "0", "avatar": null, "public_flags": 0 }],
            "pinned": false, "timestamp": "2026-10-24T00:00:00.000000+00:00",
            "edited_timestamp": null, "tts": false, "type": 0,
            "member": { "roles": ["5", "6"], "joined_at": "2026-10-24T00:00:00.000000+00:00",
                        "deaf": false, "mute": false, "flags": 0 }
        });
        if let Some(guild) = guild {
            message["guild_id"] = json!(guild);
        }
        message
    }

    fn gateway_message(guild: Option<&str>) -> Gateway {
        let message: Message = serde_json::from_value(message_json(guild)).unwrap();
        Gateway::MessageCreate(Box::new(MessageCreate(message)))
    }

    #[test]
    fn guild_messages_become_message_events() {
        let Some(Event::MessageCreated(info)) = event(&gateway_message(Some("1"))) else {
            panic!("expected a message event");
        };
        assert_eq!(info.id.get(), 100);
        assert_eq!(info.guild_id.get(), 1);
        assert_eq!(info.channel_id.get(), 3);
        assert_eq!(info.author.id.get(), 7);
        assert_eq!(
            info.author_roles
                .iter()
                .map(|r| r.get())
                .collect::<Vec<_>>(),
            [5, 6]
        );
        assert_eq!(
            info.mentioned_users
                .iter()
                .map(|u| u.get())
                .collect::<Vec<_>>(),
            [8]
        );
        assert!(info.mentions_everyone);
        assert_eq!(info.attachment_count, 1);
        assert_eq!(info.content, "hello");
    }

    #[test]
    fn direct_messages_are_dropped() {
        assert_eq!(event(&gateway_message(None)), None);
    }

    #[test]
    fn a_guild_outage_is_not_a_removal() {
        let outage = Gateway::GuildDelete(GuildDelete {
            id: twilight_model::id::Id::new(1),
            unavailable: Some(true),
        });
        assert_eq!(event(&outage), None);

        let removed = Gateway::GuildDelete(GuildDelete {
            id: twilight_model::id::Id::new(1),
            unavailable: None,
        });
        assert!(matches!(event(&removed), Some(Event::GuildRemoved { .. })));
    }

    #[test]
    fn bans_and_deletions_carry_their_guild() {
        let ban: BanAdd =
            serde_json::from_value(json!({ "guild_id": "1", "user": user_json("9", false) }))
                .unwrap();
        assert!(matches!(
            event(&Gateway::BanAdd(ban)),
            Some(Event::BanAdded { user, .. }) if user.id.get() == 9
        ));

        let in_dm = MessageDelete {
            channel_id: twilight_model::id::Id::new(3),
            guild_id: None,
            id: twilight_model::id::Id::new(100),
        };
        assert_eq!(event(&Gateway::MessageDelete(in_dm)), None);
    }

    fn command_json(options: &Value) -> Value {
        json!({
            "app_permissions": "0", "application_id": "60", "channel": { "id": "3", "type": 0 },
            "data": { "id": "70", "name": "warn", "type": 1, "options": options },
            "guild_id": "1", "id": "50", "locale": "ja-JP", "guild_locale": "en-US",
            "member": { "permissions": "8", "roles": [], "joined_at": "2026-10-24T00:00:00.000000+00:00",
                        "deaf": false, "mute": false, "flags": 0, "user": user_json("7", false) },
            "token": "tok", "type": 2, "version": 1, "entitlements": [],
            "authorizing_integration_owners": {}
        })
    }

    fn gateway_command(options: &Value) -> Gateway {
        let interaction: Interaction = serde_json::from_value(command_json(options)).unwrap();
        Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)))
    }

    #[test]
    fn commands_carry_the_invokers_language_permissions_and_options() {
        let gateway = gateway_command(&json!([
            { "name": "user", "type": 6, "value": "9" },
            { "name": "reason", "type": 3, "value": "spam" },
            { "name": "count", "type": 4, "value": 3 }
        ]));
        let Some(Event::Command(invocation)) = event(&gateway) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.command, "warn");
        assert_eq!(invocation.subcommand, None);
        assert_eq!(invocation.locale, Some(Locale::Ja));
        assert_eq!(invocation.user.id.get(), 7);
        assert_eq!(
            invocation.invoker_permissions,
            twilight_model::guild::Permissions::ADMINISTRATOR
        );
        assert_eq!(invocation.token.expose(), "tok");
        assert_eq!(
            invocation.option("reason"),
            Some(&CommandOptionValue::String("spam".into()))
        );
        assert_eq!(
            invocation.option("count"),
            Some(&CommandOptionValue::Integer(3))
        );
        assert!(
            matches!(invocation.option("user"), Some(CommandOptionValue::User(id)) if id.get() == 9)
        );
    }

    #[test]
    fn subcommands_are_split_from_their_options() {
        let gateway = gateway_command(&json!([
            { "name": "add", "type": 1, "options": [{ "name": "reason", "type": 3, "value": "spam" }] }
        ]));
        let Some(Event::Command(invocation)) = event(&gateway) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.subcommand.as_deref(), Some("add"));
        assert_eq!(invocation.options.len(), 1);
        assert_eq!(invocation.options[0].name, "reason");
    }

    #[test]
    fn subcommand_groups_are_not_supported_and_dropped() {
        let gateway = gateway_command(&json!([
            { "name": "group", "type": 2, "options": [{ "name": "add", "type": 1, "options": [] }] }
        ]));
        assert_eq!(event(&gateway), None);
    }

    #[test]
    fn unknown_locales_are_left_unset() {
        let mut interaction = command_json(&json!([]));
        interaction["locale"] = json!("fr");
        let interaction: Interaction = serde_json::from_value(interaction).unwrap();
        let gateway = Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)));
        let Some(Event::Command(invocation)) = event(&gateway) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.locale, None);
    }
}
