//! Turns gateway payloads into the stable [`Event`] model.
//!
//! Anything that is not about a guild (direct messages, interactions outside a guild) is dropped
//! here, so the rest of the pipeline can rely on every event belonging to one.

use closure_core::action::Emoji;
use closure_core::event::{
    AttachmentInfo, AutocompleteInvocation, ChannelInfo, CommandInvocation, CommandOption,
    CommandOptionValue, ComponentInvocation, ContextInvocation, ContextTarget, Event, GuildId,
    InteractionToken, MemberInfo, Mentionable, MessageInfo, ModalSubmission, ReactionInfo, RoleId,
    RoleInfo, RolesChange, ThreadInfo, UserInfo,
};
use closure_core::locale::Locale;
use twilight_gateway::Event as Gateway;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::InteractionDataResolved;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue as TwilightValue,
};
use twilight_model::application::interaction::modal::ModalInteractionComponent;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::{EmojiReactionType, Message, MessageType};
use twilight_model::channel::{Attachment, Channel};
use twilight_model::gateway::payload::incoming::{
    GuildCreate, MemberUpdate, ThreadCreate, ThreadDelete,
};
use twilight_model::guild::{Member, Role};
use twilight_model::id::Id;
use twilight_model::id::marker::GenericMarker;
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

fn attachment(attachment: &Attachment) -> AttachmentInfo {
    AttachmentInfo {
        id: attachment.id.get(),
        filename: attachment.filename.clone(),
        size: attachment.size,
        content_type: attachment.content_type.clone(),
        url: attachment.url.clone(),
    }
}

fn message(message: &Message) -> Option<MessageInfo> {
    Some(MessageInfo {
        removed_attachments: Vec::new(),
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
        mentioned_roles: message.mention_roles.clone(),
        mentions_everyone: message.mention_everyone,
        attachments: message.attachments.iter().map(attachment).collect(),
        embed_count: u32::try_from(message.embeds.len()).unwrap_or(u32::MAX),
        sticker_count: u32::try_from(message.sticker_items.len()).unwrap_or(u32::MAX),
        // Only a reply answers its reference; pins and crossposts point at a message too.
        reply_to: match (message.kind, &message.reference) {
            (MessageType::Reply, Some(reference)) => reference.message_id,
            _ => None,
        },
        from_webhook: message.webhook_id.is_some(),
        created_at: seconds(message.timestamp),
        edited_at: message.edited_timestamp.map(seconds),
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

/// A mentionable is a member or a role; Discord tells which one in the resolved data.
fn mentionable(resolved: &InteractionDataResolved, id: Id<GenericMarker>) -> Option<Mentionable> {
    let role: closure_core::event::RoleId = id.cast();
    if resolved.roles.contains_key(&role) {
        return Some(Mentionable::Role(role));
    }
    let user: closure_core::event::UserId = id.cast();
    (resolved.users.contains_key(&user) || resolved.members.contains_key(&user))
        .then_some(Mentionable::User(user))
}

/// `None` for a value the module model does not have (autocomplete state) or one Discord did
/// not resolve, which is dropped rather than guessed at.
fn option_value(
    value: &TwilightValue,
    resolved: Option<&InteractionDataResolved>,
) -> Option<CommandOptionValue> {
    Some(match value {
        TwilightValue::String(text) => CommandOptionValue::String(text.clone()),
        TwilightValue::Integer(number) => CommandOptionValue::Integer(*number),
        TwilightValue::Number(number) => CommandOptionValue::Number(*number),
        TwilightValue::Boolean(flag) => CommandOptionValue::Boolean(*flag),
        TwilightValue::User(id) => CommandOptionValue::User(*id),
        TwilightValue::Channel(id) => CommandOptionValue::Channel(*id),
        TwilightValue::Role(id) => CommandOptionValue::Role(*id),
        TwilightValue::Attachment(id) => {
            CommandOptionValue::Attachment(attachment(resolved?.attachments.get(id)?))
        }
        TwilightValue::Mentionable(id) => {
            CommandOptionValue::Mentionable(mentionable(resolved?, *id)?)
        }
        _ => return None,
    })
}

fn options(
    options: &[CommandDataOption],
    resolved: Option<&InteractionDataResolved>,
) -> Vec<CommandOption> {
    options
        .iter()
        .filter_map(|option| {
            Some(CommandOption {
                name: option.name.clone(),
                value: option_value(&option.value, resolved)?,
            })
        })
        .collect()
}

/// Splits `/command sub opt:...` into the subcommand name and its options; `/command group sub`
/// gives the name `group sub` (names cannot hold a space, so the two levels stay apart).
fn split_subcommand(
    raw: &[CommandDataOption],
    resolved: Option<&InteractionDataResolved>,
) -> Option<(Option<String>, Vec<CommandOption>)> {
    match raw {
        [
            CommandDataOption {
                name,
                value: TwilightValue::SubCommand(inner),
            },
        ] => Some((Some(name.clone()), options(inner, resolved))),
        [
            CommandDataOption {
                name: group,
                value: TwilightValue::SubCommandGroup(inner),
            },
        ] => match inner.as_slice() {
            [
                CommandDataOption {
                    name,
                    value: TwilightValue::SubCommand(inner),
                },
            ] => Some((Some(format!("{group} {name}")), options(inner, resolved))),
            _ => None,
        },
        other => Some((None, options(other, resolved))),
    }
}

/// What every interaction of a guild member says about who sent it and where.
struct Origin {
    interaction_id: closure_core::event::InteractionId,
    application_id: closure_core::event::ApplicationId,
    token: InteractionToken,
    guild_id: GuildId,
    channel_id: closure_core::event::ChannelId,
    user: UserInfo,
    roles: Vec<closure_core::event::RoleId>,
    invoker_permissions: twilight_model::guild::Permissions,
    locale: Option<Locale>,
}

fn origin(interaction: &Interaction) -> Option<Origin> {
    Some(Origin {
        interaction_id: interaction.id,
        application_id: interaction.application_id,
        token: InteractionToken::new(interaction.token.clone()),
        guild_id: interaction.guild_id?,
        channel_id: interaction.channel.as_ref()?.id,
        user: user(interaction.author()?),
        roles: interaction
            .member
            .as_ref()
            .map(|member| member.roles.clone())
            .unwrap_or_default(),
        invoker_permissions: interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .unwrap_or_else(twilight_model::guild::Permissions::empty),
        locale: interaction
            .locale
            .as_deref()
            .and_then(Locale::from_discord_tag),
    })
}

fn interaction_event(interaction: &Interaction) -> Option<Event> {
    match interaction.kind {
        InteractionType::ApplicationCommand => match &interaction.data {
            Some(InteractionData::ApplicationCommand(data))
                if data.kind != CommandType::ChatInput =>
            {
                context(interaction).map(Event::ContextCommand)
            }
            _ => command(interaction).map(Event::Command),
        },
        InteractionType::ApplicationCommandAutocomplete => {
            autocomplete(interaction).map(Event::Autocomplete)
        }
        InteractionType::MessageComponent => component(interaction).map(Event::Component),
        InteractionType::ModalSubmit => modal(interaction).map(Event::Modal),
        _ => None,
    }
}

fn component(interaction: &Interaction) -> Option<ComponentInvocation> {
    let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
        return None;
    };
    let origin = origin(interaction)?;
    Some(ComponentInvocation {
        interaction_id: origin.interaction_id,
        application_id: origin.application_id,
        token: origin.token,
        guild_id: origin.guild_id,
        channel_id: origin.channel_id,
        message_id: interaction.message.as_ref()?.id,
        user: origin.user,
        roles: origin.roles,
        invoker_permissions: origin.invoker_permissions,
        locale: origin.locale,
        custom_id: data.custom_id.clone(),
        values: data.values.clone(),
    })
}

/// The text inputs of a submitted modal, in the order of the modal, wherever they are nested.
fn modal_fields(components: &[ModalInteractionComponent], fields: &mut Vec<(String, String)>) {
    for component in components {
        match component {
            ModalInteractionComponent::ActionRow(row) => modal_fields(&row.components, fields),
            ModalInteractionComponent::Label(label) => {
                modal_fields(std::slice::from_ref(&*label.component), fields);
            }
            ModalInteractionComponent::TextInput(input) => {
                fields.push((input.custom_id.clone(), input.value.clone()));
            }
            _ => {}
        }
    }
}

fn modal(interaction: &Interaction) -> Option<ModalSubmission> {
    let Some(InteractionData::ModalSubmit(data)) = &interaction.data else {
        return None;
    };
    let origin = origin(interaction)?;
    let mut fields = Vec::new();
    modal_fields(&data.components, &mut fields);
    Some(ModalSubmission {
        interaction_id: origin.interaction_id,
        application_id: origin.application_id,
        token: origin.token,
        guild_id: origin.guild_id,
        channel_id: origin.channel_id,
        user: origin.user,
        roles: origin.roles,
        invoker_permissions: origin.invoker_permissions,
        locale: origin.locale,
        custom_id: data.custom_id.clone(),
        fields,
    })
}

/// The option being typed in and the text so far, also when it is inside a subcommand.
fn focused_of(raw: &[CommandDataOption]) -> Option<(String, String)> {
    raw.iter().find_map(|option| match &option.value {
        TwilightValue::Focused(text, _) => Some((option.name.clone(), text.clone())),
        TwilightValue::SubCommand(inner) | TwilightValue::SubCommandGroup(inner) => {
            focused_of(inner)
        }
        _ => None,
    })
}

fn autocomplete(interaction: &Interaction) -> Option<AutocompleteInvocation> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    let (subcommand, options) = split_subcommand(&data.options, data.resolved.as_ref())?;
    let (focused_option, focused) = focused_of(&data.options)?;
    let origin = origin(interaction)?;
    Some(AutocompleteInvocation {
        interaction_id: origin.interaction_id,
        application_id: origin.application_id,
        token: origin.token,
        guild_id: origin.guild_id,
        channel_id: origin.channel_id,
        user: origin.user,
        roles: origin.roles,
        invoker_permissions: origin.invoker_permissions,
        locale: origin.locale,
        command: data.name.clone(),
        subcommand,
        focused_option,
        focused,
        options,
    })
}

/// A context-menu command: the user or the message it was used on is in the resolved data. A
/// target Discord did not resolve is dropped, as an option of an unknown kind is.
fn context(interaction: &Interaction) -> Option<ContextInvocation> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    let target_id = data.target_id?;
    let resolved = data.resolved.as_ref()?;
    let target = match data.kind {
        CommandType::User => {
            let user_id: closure_core::event::UserId = target_id.cast();
            ContextTarget::User(user(resolved.users.get(&user_id)?))
        }
        CommandType::Message => {
            let message_id: closure_core::event::MessageId = target_id.cast();
            ContextTarget::Message(message(resolved.messages.get(&message_id)?)?)
        }
        _ => return None,
    };
    let origin = origin(interaction)?;
    Some(ContextInvocation {
        interaction_id: origin.interaction_id,
        application_id: origin.application_id,
        token: origin.token,
        guild_id: origin.guild_id,
        channel_id: origin.channel_id,
        user: origin.user,
        roles: origin.roles,
        invoker_permissions: origin.invoker_permissions,
        locale: origin.locale,
        command: data.name.clone(),
        target,
    })
}

fn command(interaction: &Interaction) -> Option<CommandInvocation> {
    if interaction.kind != InteractionType::ApplicationCommand {
        return None;
    }
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    let (subcommand, options) = split_subcommand(&data.options, data.resolved.as_ref())?;
    let origin = origin(interaction)?;
    Some(CommandInvocation {
        interaction_id: origin.interaction_id,
        application_id: origin.application_id,
        token: origin.token,
        guild_id: origin.guild_id,
        channel_id: origin.channel_id,
        user: origin.user,
        roles: origin.roles,
        invoker_permissions: origin.invoker_permissions,
        locale: origin.locale,
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
        Gateway::InteractionCreate(interaction) => interaction_event(&interaction.0)?,
        _ => return None,
    })
}

/// What the cache held about the entity an event changes, read before the cache applied the
/// event. Without it, the events that compare old and new state are not sent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Before {
    /// The roles of the member, when the member was cached.
    pub(crate) roles: Option<Vec<RoleId>>,
    /// Whether the thread was archived, when the thread was cached.
    pub(crate) archived: Option<bool>,
}

/// Every event a payload gives: the one it converts to, then the one that compares it with
/// `before` (a role change follows its member update).
pub(crate) fn events(gateway: &Gateway, before: &Before) -> Vec<Event> {
    event(gateway)
        .into_iter()
        .chain(state_change(gateway, before))
        .collect()
}

fn state_change(gateway: &Gateway, before: &Before) -> Option<Event> {
    match gateway {
        Gateway::MemberUpdate(updated) => roles_changed(updated, before.roles.as_deref()),
        Gateway::ThreadCreate(created) => thread_created(created),
        Gateway::ThreadUpdate(updated) => archive_change(&updated.0, before.archived),
        Gateway::ThreadDelete(deleted) => Some(thread_deleted(deleted)),
        _ => None,
    }
}

fn roles_changed(updated: &MemberUpdate, previous: Option<&[RoleId]>) -> Option<Event> {
    let previous = previous?;
    let added: Vec<RoleId> = updated
        .roles
        .iter()
        .filter(|role| !previous.contains(role))
        .copied()
        .collect();
    let removed: Vec<RoleId> = previous
        .iter()
        .filter(|role| !updated.roles.contains(role))
        .copied()
        .collect();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    Some(Event::RolesChanged(RolesChange {
        guild_id: updated.guild_id,
        user: user(&updated.user),
        added,
        removed,
    }))
}

fn thread(channel: &Channel) -> Option<ThreadInfo> {
    let metadata = channel.thread_metadata.as_ref();
    Some(ThreadInfo {
        id: channel.id,
        guild_id: channel.guild_id?,
        parent_id: channel.parent_id,
        name: channel.name.clone().unwrap_or_default(),
        owner_id: channel.owner_id,
        archived: metadata.is_some_and(|metadata| metadata.archived),
        locked: metadata.is_some_and(|metadata| metadata.locked),
        created_at: metadata
            .and_then(|metadata| metadata.create_timestamp)
            .map(seconds),
    })
}

/// Discord marks a thread that was just made; the threads the bot learns of at start-up or when it
/// is added to one come without the mark.
fn thread_created(created: &ThreadCreate) -> Option<Event> {
    if created.0.newly_created != Some(true) {
        return None;
    }
    Some(Event::ThreadCreated(thread(&created.0)?))
}

fn archive_change(channel: &Channel, previous: Option<bool>) -> Option<Event> {
    let previous = previous?;
    let archived = channel.thread_metadata.as_ref()?.archived;
    if previous == archived {
        return None;
    }
    let info = thread(channel)?;
    Some(if archived {
        Event::ThreadArchived(info)
    } else {
        Event::ThreadUnarchived(info)
    })
}

fn thread_deleted(deleted: &ThreadDelete) -> Event {
    Event::ThreadDeleted {
        guild_id: deleted.guild_id,
        thread_id: deleted.id,
        parent_id: deleted.parent_id,
    }
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
        BanAdd, GuildDelete, InteractionCreate, MemberUpdate, MessageCreate, MessageDelete,
        MessageUpdate, ThreadCreate, ThreadDelete, ThreadUpdate,
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
        assert_eq!(info.attachments.len(), 1);
        assert_eq!(info.content, "hello");
        assert!(!info.from_webhook);
        assert_eq!(info.created_at, 1_792_800_000);
        assert_eq!(info.edited_at, None);
    }

    /// Converts a message payload, as the gateway hands it over.
    fn converted(json: Value) -> MessageInfo {
        let payload: Message = serde_json::from_value(json).unwrap();
        message(&payload).unwrap()
    }

    #[test]
    fn attachments_keep_their_size_and_content_type_when_discord_has_one() {
        let mut json = message_json(Some("1"));
        json["attachments"] = json!([
            { "id": "1", "filename": "a.png", "size": 1, "url": "u", "proxy_url": "p", "content_type": "image/png" },
            { "id": "2", "filename": "b.bin", "size": 2048, "url": "u", "proxy_url": "p" }
        ]);
        let info = converted(json);
        assert_eq!(
            info.attachments,
            [
                AttachmentInfo {
                    id: 1,
                    filename: "a.png".into(),
                    size: 1,
                    content_type: Some("image/png".into()),
                    url: "u".into(),
                },
                AttachmentInfo {
                    id: 2,
                    filename: "b.bin".into(),
                    size: 2048,
                    content_type: None,
                    url: "u".into(),
                },
            ]
        );
    }

    #[test]
    fn embeds_stickers_and_mentioned_roles_are_counted_or_listed() {
        let mut json = message_json(Some("1"));
        json["embeds"] =
            json!([{ "type": "rich", "title": "t" }, { "type": "rich", "description": "d" }]);
        json["sticker_items"] = json!([{ "id": "5", "format_type": 1, "name": "s" }]);
        json["mention_roles"] = json!(["12", "13"]);
        let info = converted(json);
        assert_eq!(info.embed_count, 2);
        assert_eq!(info.sticker_count, 1);
        assert_eq!(
            info.mentioned_roles
                .iter()
                .map(|r| r.get())
                .collect::<Vec<_>>(),
            [12, 13]
        );
    }

    #[test]
    fn a_reply_names_the_message_it_answers() {
        let mut json = message_json(Some("1"));
        json["type"] = json!(19);
        json["message_reference"] =
            json!({ "message_id": "55", "channel_id": "3", "guild_id": "1" });
        assert_eq!(
            converted(json).reply_to,
            Some(twilight_model::id::Id::new(55))
        );
    }

    #[test]
    fn a_message_that_only_points_at_another_one_is_not_a_reply() {
        // A pinned-message notice refers to the pinned message without replying to it.
        let mut json = message_json(Some("1"));
        json["type"] = json!(6);
        json["message_reference"] =
            json!({ "message_id": "55", "channel_id": "3", "guild_id": "1" });
        assert_eq!(converted(json).reply_to, None);
    }

    #[test]
    fn a_webhook_message_is_marked_and_has_no_member_roles() {
        let mut json = message_json(Some("1"));
        json.as_object_mut().unwrap().remove("member");
        json["webhook_id"] = json!("77");
        let info = converted(json);
        assert!(info.from_webhook);
        assert!(info.author_roles.is_empty());
    }

    #[test]
    fn an_update_leaves_empty_what_the_payload_did_not_carry() {
        // An update carries its text, empty included; without member data there are no roles.
        let mut json = message_json(Some("1"));
        json["content"] = json!("");
        json["attachments"] = json!([]);
        json["edited_timestamp"] = json!("2026-10-24T01:00:00.000000+00:00");
        json.as_object_mut().unwrap().remove("member");
        let update: MessageUpdate = serde_json::from_value(json).unwrap();
        let Some(Event::MessageUpdated(info)) = event(&Gateway::MessageUpdate(Box::new(update)))
        else {
            panic!("expected a message update event");
        };
        assert_eq!(info.content, "");
        assert!(info.author_roles.is_empty());
        assert!(info.attachments.is_empty());
        assert_eq!(info.created_at, 1_792_800_000);
        assert_eq!(info.edited_at, Some(1_792_803_600));
    }

    #[test]
    fn an_update_missing_required_fields_does_not_parse_at_all() {
        // Twilight reads an update as a whole message, so a partial one (an embed-only update)
        // never reaches the converter. This pins that assumption.
        let partial = json!({ "id": "100", "channel_id": "3", "guild_id": "1", "embeds": [] });
        assert!(serde_json::from_value::<MessageUpdate>(partial).is_err());
    }

    fn member_update(roles: &[&str]) -> Gateway {
        let update: MemberUpdate = serde_json::from_value(json!({
            "guild_id": "1", "roles": roles, "pending": false, "user": user_json("7", false),
            "joined_at": "2026-10-24T00:00:00.000000+00:00"
        }))
        .unwrap();
        Gateway::MemberUpdate(Box::new(update))
    }

    fn before_roles(roles: &[u64]) -> Before {
        Before {
            roles: Some(roles.iter().map(|id| Id::new(*id)).collect()),
            ..Before::default()
        }
    }

    fn roles_of(events: &[Event]) -> Option<(Vec<u64>, Vec<u64>)> {
        events.iter().find_map(|event| match event {
            Event::RolesChanged(change) => Some((
                change.added.iter().map(|id| id.get()).collect(),
                change.removed.iter().map(|id| id.get()).collect(),
            )),
            _ => None,
        })
    }

    #[test]
    fn a_role_change_is_the_difference_to_the_cached_roles() {
        let events = events(&member_update(&["6", "8"]), &before_roles(&[5, 7]));
        assert!(matches!(events.first(), Some(Event::MemberUpdated(_))));
        assert_eq!(roles_of(&events), Some((vec![6, 8], vec![5, 7])));
        let Some(Event::RolesChanged(change)) = events.get(1) else {
            panic!("expected a role change after the member update");
        };
        assert_eq!(change.guild_id, Id::new(1));
        assert_eq!(change.user.id, Id::new(7));
    }

    #[test]
    fn no_role_change_is_said_without_the_roles_the_bot_knew_before() {
        let events = events(&member_update(&["6"]), &Before::default());
        assert!(matches!(events.as_slice(), [Event::MemberUpdated(_)]));
    }

    #[test]
    fn an_update_that_keeps_the_roles_says_nothing_about_them() {
        let events = events(&member_update(&["5"]), &before_roles(&[5]));
        assert!(matches!(events.as_slice(), [Event::MemberUpdated(_)]));
    }

    fn thread_json(id: &str, archived: bool, newly_created: Option<bool>) -> Value {
        json!({
            "id": id, "type": 11, "guild_id": "1", "name": "t", "parent_id": "2", "owner_id": "7",
            "thread_metadata": {
                "archived": archived, "auto_archive_duration": 1440, "locked": false,
                "archive_timestamp": "2026-10-24T00:00:00.000000+00:00",
                "create_timestamp": "2026-10-24T00:00:00.000000+00:00"
            },
            "newly_created": newly_created
        })
    }

    fn thread_created(newly_created: Option<bool>) -> Gateway {
        let channel: Channel =
            serde_json::from_value(thread_json("20", false, newly_created)).unwrap();
        Gateway::ThreadCreate(Box::new(ThreadCreate(channel)))
    }

    fn thread_updated(archived: bool) -> Gateway {
        let channel: Channel = serde_json::from_value(thread_json("20", archived, None)).unwrap();
        Gateway::ThreadUpdate(Box::new(ThreadUpdate(channel)))
    }

    fn before_archived(archived: Option<bool>) -> Before {
        Before {
            archived,
            ..Before::default()
        }
    }

    #[test]
    fn a_thread_made_now_is_announced_and_one_the_bot_learns_of_is_not() {
        let announced = events(&thread_created(Some(true)), &Before::default());
        let [Event::ThreadCreated(thread)] = announced.as_slice() else {
            panic!("expected one thread creation, got {announced:?}");
        };
        assert_eq!(thread.id, Id::new(20));
        assert_eq!(thread.parent_id, Some(Id::new(2)));
        assert_eq!(thread.owner_id, Some(Id::new(7)));
        assert!(!thread.archived && thread.created_at.is_some());

        assert!(events(&thread_created(None), &Before::default()).is_empty());
        assert!(events(&thread_created(Some(false)), &Before::default()).is_empty());
    }

    #[test]
    fn a_thread_archived_or_unarchived_compares_with_the_cached_state() {
        let archived = events(&thread_updated(true), &before_archived(Some(false)));
        assert!(matches!(archived.as_slice(), [Event::ThreadArchived(t)] if t.archived));

        let unarchived = events(&thread_updated(false), &before_archived(Some(true)));
        assert!(matches!(unarchived.as_slice(), [Event::ThreadUnarchived(t)] if !t.archived));

        assert!(events(&thread_updated(true), &before_archived(Some(true))).is_empty());
        assert!(events(&thread_updated(false), &before_archived(Some(false))).is_empty());
        // Not cached before: whether it was archived is unknown, so no event.
        assert!(events(&thread_updated(true), &Before::default()).is_empty());
        assert!(events(&thread_updated(false), &Before::default()).is_empty());
    }

    #[test]
    fn a_deleted_thread_names_its_guild_and_parent() {
        let deleted: ThreadDelete = serde_json::from_value(json!({
            "guild_id": "1", "id": "20", "type": 11, "parent_id": "2"
        }))
        .unwrap();
        let events = events(&Gateway::ThreadDelete(deleted), &Before::default());
        assert!(matches!(
            events.as_slice(),
            [Event::ThreadDeleted { guild_id, thread_id, parent_id }]
                if *guild_id == Id::new(1) && *thread_id == Id::new(20) && *parent_id == Id::new(2)
        ));
    }

    #[test]
    fn a_thread_outside_any_guild_is_dropped() {
        let mut channel: Channel =
            serde_json::from_value(thread_json("20", false, Some(true))).unwrap();
        channel.guild_id = None;
        let gateway = Gateway::ThreadCreate(Box::new(ThreadCreate(channel)));
        assert!(events(&gateway, &Before::default()).is_empty());
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
    fn attachment_and_mentionable_options_take_what_discord_resolved_for_them() {
        let mut json = command_json(&json!([
            { "name": "file", "type": 11, "value": "55" },
            { "name": "who", "type": 9, "value": "9" },
            { "name": "pick", "type": 9, "value": "10" },
            { "name": "gone", "type": 11, "value": "56" }
        ]));
        json["data"]["resolved"] = json!({
            "attachments": { "55": {
                "id": "55", "filename": "a.png", "size": 12, "content_type": "image/png",
                "url": "https://cdn.example/a.png", "proxy_url": "https://proxy.example/a.png"
            } },
            "users": { "9": user_json("9", false) },
            "roles": { "10": {
                "id": "10", "name": "mods", "color": 0, "colors": { "primary_color": 0 },
                "hoist": false, "managed": false, "mentionable": true, "position": 1,
                "permissions": "0", "flags": 0
            } }
        });
        let interaction: Interaction = serde_json::from_value(json).unwrap();
        let gateway = Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)));
        let Some(Event::Command(invocation)) = event(&gateway) else {
            panic!("expected a command event");
        };
        assert_eq!(
            invocation.option("file"),
            Some(&CommandOptionValue::Attachment(AttachmentInfo {
                id: 55,
                filename: "a.png".into(),
                size: 12,
                content_type: Some("image/png".into()),
                url: "https://cdn.example/a.png".into(),
            }))
        );
        assert_eq!(
            invocation.option("who"),
            Some(&CommandOptionValue::Mentionable(Mentionable::User(
                twilight_model::id::Id::new(9)
            )))
        );
        assert_eq!(
            invocation.option("pick"),
            Some(&CommandOptionValue::Mentionable(Mentionable::Role(
                twilight_model::id::Id::new(10)
            )))
        );
        // An attachment Discord did not resolve is not guessed at.
        assert_eq!(invocation.option("gone"), None);
    }

    #[test]
    fn an_autocomplete_request_names_the_focused_option_and_keeps_the_others() {
        let mut json = command_json(&json!([
            { "name": "user", "type": 6, "value": "9" },
            { "name": "reason", "type": 3, "value": "sp", "focused": true }
        ]));
        json["type"] = json!(4);
        let interaction: Interaction = serde_json::from_value(json).unwrap();
        let gateway = Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)));
        let Some(Event::Autocomplete(query)) = event(&gateway) else {
            panic!("expected an autocomplete event");
        };
        assert_eq!(query.command, "warn");
        assert_eq!(query.subcommand, None);
        assert_eq!(query.focused_option, "reason");
        assert_eq!(query.focused, "sp");
        assert_eq!(query.token.expose(), "tok");
        assert_eq!(
            query.options.len(),
            1,
            "the focused option is not among them"
        );
        assert_eq!(
            query.option("user"),
            Some(&CommandOptionValue::User(twilight_model::id::Id::new(9)))
        );
    }

    fn context_gateway(kind: u64, target: &str, resolved: Value) -> Gateway {
        let mut json = command_json(&json!([]));
        json["data"]["type"] = json!(kind);
        json["data"]["name"] = json!("report");
        json["data"]["target_id"] = json!(target);
        json["data"]["resolved"] = resolved;
        let interaction: Interaction = serde_json::from_value(json).unwrap();
        Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)))
    }

    #[test]
    fn a_context_command_on_a_user_carries_the_user_it_was_used_on() {
        let gateway = context_gateway(2, "9", json!({ "users": { "9": user_json("9", false) } }));
        let Some(Event::ContextCommand(invocation)) = event(&gateway) else {
            panic!("expected a context command event");
        };
        assert_eq!(invocation.command, "report");
        assert_eq!(
            invocation.target,
            ContextTarget::User(UserInfo {
                id: twilight_model::id::Id::new(9),
                name: "alice".into(),
                bot: false,
            })
        );
    }

    #[test]
    fn a_context_command_on_a_message_carries_the_message_it_was_used_on() {
        let mut message = message_json(Some("1"));
        message["id"] = json!("80");
        let gateway = context_gateway(3, "80", json!({ "messages": { "80": message } }));
        let Some(Event::ContextCommand(invocation)) = event(&gateway) else {
            panic!("expected a context command event");
        };
        let ContextTarget::Message(target) = invocation.target else {
            panic!("expected a message target");
        };
        assert_eq!(target.id.get(), 80);
        assert_eq!(target.content, "hello");
    }

    #[test]
    fn a_context_command_whose_target_discord_did_not_resolve_is_dropped() {
        let gateway = context_gateway(2, "9", json!({}));
        assert_eq!(event(&gateway), None);
    }

    fn user_interaction(kind: u8, data: &Value, extra: &Value) -> Gateway {
        let mut json = json!({
            "app_permissions": "0", "application_id": "60", "channel": { "id": "3", "type": 0 },
            "data": data,
            "guild_id": "1", "id": "51", "locale": "en-US", "guild_locale": "en-US",
            "member": { "permissions": "0", "roles": ["10", "11"], "joined_at": "2026-10-24T00:00:00.000000+00:00",
                        "deaf": false, "mute": false, "flags": 0, "user": user_json("7", false) },
            "token": "tok2", "type": kind, "version": 1, "entitlements": [],
            "authorizing_integration_owners": {}
        });
        if let (Some(map), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
            map.extend(extra.clone());
        }
        let interaction: Interaction = serde_json::from_value(json).unwrap();
        Gateway::InteractionCreate(Box::new(InteractionCreate(interaction)))
    }

    #[test]
    fn a_click_carries_its_id_values_message_and_who_clicked() {
        let message = json!({
            "id": "900", "channel_id": "3", "author": user_json("99", true), "content": "hi",
            "timestamp": "2026-10-24T00:00:00.000000+00:00", "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0
        });
        let gateway = user_interaction(
            3,
            &json!({ "custom_id": "warn:confirm:7", "component_type": 3, "values": ["a", "b"] }),
            &json!({ "message": message }),
        );
        let Some(Event::Component(click)) = event(&gateway) else {
            panic!("expected a click");
        };
        assert_eq!(click.custom_id, "warn:confirm:7");
        assert_eq!(click.values, ["a", "b"]);
        assert_eq!(click.message_id.get(), 900);
        assert_eq!(click.user.id.get(), 7);
        assert_eq!(
            click.roles.iter().map(|r| r.get()).collect::<Vec<_>>(),
            [10, 11]
        );
        assert_eq!(click.token.expose(), "tok2");
        assert_eq!(click.locale, Some(Locale::En));
    }

    #[test]
    fn a_submitted_modal_gives_the_text_of_every_input_wherever_it_is() {
        let gateway = user_interaction(
            5,
            &json!({
                "custom_id": "warn:reason",
                "components": [
                    { "type": 1, "id": 1, "components": [
                        { "type": 4, "id": 2, "custom_id": "why", "value": "spam" } ] },
                    { "type": 18, "id": 3, "component": { "type": 4, "id": 4, "custom_id": "note", "value": "twice" } }
                ]
            }),
            &json!({}),
        );
        let Some(Event::Modal(form)) = event(&gateway) else {
            panic!("expected a modal");
        };
        assert_eq!(form.custom_id, "warn:reason");
        assert_eq!(form.field("why"), Some("spam"));
        assert_eq!(form.field("note"), Some("twice"));
        assert_eq!(form.field("other"), None);
        assert_eq!(form.channel_id.get(), 3);
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
    fn a_subcommand_of_a_group_is_named_by_both_levels() {
        let gateway = gateway_command(&json!([
            { "name": "list", "type": 2, "options": [{
                "name": "add", "type": 1, "options": [{ "name": "user", "type": 6, "value": "9" }]
            }] }
        ]));
        let Some(Event::Command(invocation)) = event(&gateway) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.subcommand.as_deref(), Some("list add"));
        assert_eq!(
            invocation.option("user"),
            Some(&CommandOptionValue::User(twilight_model::id::Id::new(9)))
        );
    }

    #[test]
    fn a_plain_subcommand_and_a_grouped_one_of_one_command_are_both_named() {
        let plain = gateway_command(&json!([
            { "name": "status", "type": 1, "options": [] }
        ]));
        let Some(Event::Command(invocation)) = event(&plain) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.subcommand.as_deref(), Some("status"));

        let grouped = gateway_command(&json!([
            { "name": "counters", "type": 2, "options": [{
                "name": "view", "type": 1,
                "options": [{ "name": "counter", "type": 3, "value": "strikes" }]
            }] }
        ]));
        let Some(Event::Command(invocation)) = event(&grouped) else {
            panic!("expected a command event");
        };
        assert_eq!(invocation.subcommand.as_deref(), Some("counters view"));
        assert_eq!(
            invocation.option("counter"),
            Some(&CommandOptionValue::String("strikes".into()))
        );
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
