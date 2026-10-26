use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use twilight_model::gateway::Intents;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::{
    ApplicationMarker, ChannelMarker, GuildMarker, InteractionMarker, MessageMarker, RoleMarker,
    UserMarker,
};

use crate::action::Emoji;
use crate::locale::Locale;
use crate::schedule::ScheduledJob;

pub type GuildId = Id<GuildMarker>;
pub type ChannelId = Id<ChannelMarker>;
pub type UserId = Id<UserMarker>;
pub type RoleId = Id<RoleMarker>;
pub type MessageId = Id<MessageMarker>;
pub type InteractionId = Id<InteractionMarker>;
pub type ApplicationId = Id<ApplicationMarker>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: UserId,
    pub name: String,
    pub bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub guild_id: GuildId,
    pub user: UserInfo,
    pub roles: Vec<RoleId>,
    pub nick: Option<String>,
    /// Unix seconds.
    pub joined_at: Option<i64>,
    /// Unix seconds; present while the member is timed out.
    pub timed_out_until: Option<i64>,
}

/// A file attached to a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub id: u64,
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
    /// `None` when Discord does not know the type.
    pub content_type: Option<String>,
    /// Where Discord serves the file; a message that lost it has an empty one.
    #[serde(default)]
    pub url: String,
}

/// A message in a guild. `created_at` is when it was sent, also on a `MessageUpdated` event.
/// An update is read as a whole message: a field its payload leaves out comes out empty, and a
/// payload without the fields Discord always sends is not delivered at all. Without the content
/// intent `content` is empty; without member data `author_roles` is. Fields added after a
/// release are `#[serde(default)]`, so an event serialised before them still parses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageInfo {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub guild_id: GuildId,
    pub author: UserInfo,
    /// Roles of the author. Empty when the gateway did not include member data.
    pub author_roles: Vec<RoleId>,
    /// Empty when the bot lacks the message content intent or the update did not carry it.
    pub content: String,
    pub mentioned_users: Vec<UserId>,
    #[serde(default)]
    pub mentioned_roles: Vec<RoleId>,
    pub mentions_everyone: bool,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,
    /// Number of embeds (link previews included).
    #[serde(default)]
    pub embed_count: u32,
    #[serde(default)]
    pub sticker_count: u32,
    /// The message this one replies to. `None` for anything that is not a reply.
    #[serde(default)]
    pub reply_to: Option<MessageId>,
    /// Posted by a webhook rather than a user (`author` is then the webhook's user).
    #[serde(default)]
    pub from_webhook: bool,
    /// Unix seconds, when the message was sent.
    #[serde(default)]
    pub created_at: i64,
    /// Unix seconds of the last edit; `None` when it was never edited.
    #[serde(default)]
    pub edited_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionInfo {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub emoji: Emoji,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: ChannelId,
    pub guild_id: GuildId,
    pub name: Option<String>,
    pub parent_id: Option<ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub id: RoleId,
    pub guild_id: GuildId,
    pub name: String,
    pub position: i64,
    pub permissions: Permissions,
}

/// The token of an interaction. It authorises replies for fifteen minutes, so it is
/// redacted from `Debug` output and must never reach logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InteractionToken(String);

impl InteractionToken {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for InteractionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InteractionToken(<redacted>)")
    }
}

/// What a `mentionable` option picked: a member or a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mentionable {
    User(UserId),
    Role(RoleId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CommandOptionValue {
    String(String),
    Integer(i64),
    Number(f64),
    Boolean(bool),
    User(UserId),
    Channel(ChannelId),
    Role(RoleId),
    Attachment(AttachmentInfo),
    Mentionable(Mentionable),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandOption {
    pub name: String,
    pub value: CommandOptionValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInvocation {
    pub interaction_id: InteractionId,
    pub application_id: ApplicationId,
    pub token: InteractionToken,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub user: UserInfo,
    /// The roles the invoker holds in the guild.
    pub roles: Vec<RoleId>,
    /// Permissions of the invoker in the channel, as computed by Discord.
    pub invoker_permissions: Permissions,
    /// Locale of the invoking user, when Discord reports one this bot supports.
    pub locale: Option<Locale>,
    pub command: String,
    /// The subcommand, or `group sub` for a subcommand of a group (a space keeps the levels apart).
    pub subcommand: Option<String>,
    pub options: Vec<CommandOption>,
}

/// A click on a button or a choice in a select menu, on a message of the bot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentInvocation {
    pub interaction_id: InteractionId,
    pub application_id: ApplicationId,
    pub token: InteractionToken,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    /// The message the component is on.
    pub message_id: MessageId,
    pub user: UserInfo,
    pub roles: Vec<RoleId>,
    pub invoker_permissions: Permissions,
    pub locale: Option<Locale>,
    /// The id the module gave the component; it starts with the id of the module (`module:..`).
    pub custom_id: String,
    /// What was chosen in a select menu; empty for a button.
    pub values: Vec<String>,
}

/// A modal that the user filled in and sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModalSubmission {
    pub interaction_id: InteractionId,
    pub application_id: ApplicationId,
    pub token: InteractionToken,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub user: UserInfo,
    pub roles: Vec<RoleId>,
    pub invoker_permissions: Permissions,
    pub locale: Option<Locale>,
    /// The id the module gave the modal; it starts with the id of the module (`module:..`).
    pub custom_id: String,
    /// The text of each input, by the input's id, in the order of the modal.
    pub fields: Vec<(String, String)>,
}

impl ModalSubmission {
    /// What the user wrote in the input `custom_id`.
    #[must_use]
    pub fn field(&self, custom_id: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(id, _)| id == custom_id)
            .map(|(_, value)| value.as_str())
    }
}

/// What a context-menu command was used on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContextTarget {
    /// A member or a user (the user's id, name and bot flag).
    User(UserInfo),
    /// A message of the guild.
    Message(MessageInfo),
}

/// A context-menu command (on a user or on a message) used by a member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextInvocation {
    pub interaction_id: InteractionId,
    pub application_id: ApplicationId,
    pub token: InteractionToken,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub user: UserInfo,
    pub roles: Vec<RoleId>,
    pub invoker_permissions: Permissions,
    pub locale: Option<Locale>,
    pub command: String,
    pub target: ContextTarget,
}

/// A user typing in an option of a command: Discord asks the module for choices to offer. It
/// is answered with [`crate::action::InteractionResponse::Autocomplete`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutocompleteInvocation {
    pub interaction_id: InteractionId,
    pub application_id: ApplicationId,
    pub token: InteractionToken,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub user: UserInfo,
    pub roles: Vec<RoleId>,
    pub invoker_permissions: Permissions,
    pub locale: Option<Locale>,
    pub command: String,
    /// The subcommand, or `group sub` (see [`CommandInvocation::subcommand`]).
    pub subcommand: Option<String>,
    /// The option the user is typing in.
    pub focused_option: String,
    /// What the user typed so far.
    pub focused: String,
    /// The other options the user already filled in; the focused one is not among them.
    pub options: Vec<CommandOption>,
}

impl AutocompleteInvocation {
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&CommandOptionValue> {
        self.options
            .iter()
            .find(|option| option.name == name)
            .map(|option| &option.value)
    }
}

/// What every interaction (a command, a click, a submitted modal) has in common: where it came
/// from, who sent it, and what to answer it with.
pub trait InteractionRef: Send + Sync {
    fn interaction_id(&self) -> InteractionId;
    fn token(&self) -> &InteractionToken;
    fn guild_id(&self) -> GuildId;
    fn channel_id(&self) -> ChannelId;
    fn user(&self) -> &UserInfo;
    fn roles(&self) -> &[RoleId];
    fn invoker_permissions(&self) -> Permissions;
    fn locale(&self) -> Option<Locale>;
}

macro_rules! interaction_ref {
    ($ty:ty) => {
        impl InteractionRef for $ty {
            fn interaction_id(&self) -> InteractionId {
                self.interaction_id
            }

            fn token(&self) -> &InteractionToken {
                &self.token
            }

            fn guild_id(&self) -> GuildId {
                self.guild_id
            }

            fn channel_id(&self) -> ChannelId {
                self.channel_id
            }

            fn user(&self) -> &UserInfo {
                &self.user
            }

            fn roles(&self) -> &[RoleId] {
                &self.roles
            }

            fn invoker_permissions(&self) -> Permissions {
                self.invoker_permissions
            }

            fn locale(&self) -> Option<Locale> {
                self.locale
            }
        }
    };
}

interaction_ref!(CommandInvocation);
interaction_ref!(ComponentInvocation);
interaction_ref!(ModalSubmission);
interaction_ref!(AutocompleteInvocation);
interaction_ref!(ContextInvocation);

impl CommandInvocation {
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&CommandOptionValue> {
        self.options
            .iter()
            .find(|option| option.name == name)
            .map(|option| &option.value)
    }
}

/// The stable event model handed to modules. It is deliberately independent of Twilight's
/// payload types so that gateway library upgrades and non-native modules do not leak
/// into module code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    GuildAvailable {
        guild_id: GuildId,
        name: String,
        /// The community locale the guild chose in Discord, when supported by this bot.
        preferred_locale: Option<Locale>,
    },
    GuildRemoved {
        guild_id: GuildId,
    },
    MemberJoined(MemberInfo),
    MemberUpdated(MemberInfo),
    MemberLeft {
        guild_id: GuildId,
        user: UserInfo,
    },
    MessageCreated(MessageInfo),
    MessageUpdated(MessageInfo),
    MessageDeleted {
        guild_id: GuildId,
        channel_id: ChannelId,
        message_id: MessageId,
    },
    MessagesBulkDeleted {
        guild_id: GuildId,
        channel_id: ChannelId,
        message_ids: Vec<MessageId>,
    },
    ReactionAdded(ReactionInfo),
    ReactionRemoved(ReactionInfo),
    BanAdded {
        guild_id: GuildId,
        user: UserInfo,
    },
    BanRemoved {
        guild_id: GuildId,
        user: UserInfo,
    },
    ChannelCreated(ChannelInfo),
    ChannelUpdated(ChannelInfo),
    ChannelDeleted(ChannelInfo),
    RoleCreated(RoleInfo),
    RoleUpdated(RoleInfo),
    RoleDeleted {
        guild_id: GuildId,
        role_id: RoleId,
    },
    Command(CommandInvocation),
    /// A click or a choice on a component of one of the module's messages.
    Component(ComponentInvocation),
    /// A modal of the module that the user sent.
    Modal(ModalSubmission),
    /// The user is typing in an option of a command of the module; answer with the choices.
    Autocomplete(AutocompleteInvocation),
    /// A context-menu command of the module was used on a user or a message.
    ContextCommand(ContextInvocation),
    /// A job the module scheduled is due (see [`crate::schedule`]). Only that module gets it.
    /// Answering [`crate::subscription::Flow::Stop`] says the module cannot handle it and never
    /// will: the job is dropped (a repeating one stops) instead of being finished.
    Scheduled(ScheduledJob),
}

/// Payload-free discriminant of [`Event`], used for subscriptions and metrics labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    GuildAvailable,
    GuildRemoved,
    MemberJoined,
    MemberUpdated,
    MemberLeft,
    MessageCreated,
    MessageUpdated,
    MessageDeleted,
    MessagesBulkDeleted,
    ReactionAdded,
    ReactionRemoved,
    BanAdded,
    BanRemoved,
    ChannelCreated,
    ChannelUpdated,
    ChannelDeleted,
    RoleCreated,
    RoleUpdated,
    RoleDeleted,
    Command,
    Component,
    Modal,
    Autocomplete,
    ContextCommand,
    Scheduled,
}

impl EventKind {
    /// The Gateway intents Discord needs to send events of this kind (besides `GUILDS`, which the
    /// bot always asks for). Some are privileged: they have to be switched on for the bot in the
    /// Discord developer portal.
    #[must_use]
    pub fn required_intents(self) -> Intents {
        match self {
            Self::MemberJoined | Self::MemberUpdated | Self::MemberLeft => Intents::GUILD_MEMBERS,
            Self::MessageCreated
            | Self::MessageUpdated
            | Self::MessageDeleted
            | Self::MessagesBulkDeleted => Intents::GUILD_MESSAGES,
            Self::ReactionAdded | Self::ReactionRemoved => Intents::GUILD_MESSAGE_REACTIONS,
            Self::BanAdded | Self::BanRemoved => Intents::GUILD_MODERATION,
            Self::GuildAvailable
            | Self::GuildRemoved
            | Self::ChannelCreated
            | Self::ChannelUpdated
            | Self::ChannelDeleted
            | Self::RoleCreated
            | Self::RoleUpdated
            | Self::RoleDeleted
            | Self::Command
            | Self::Component
            | Self::Modal
            | Self::Autocomplete
            | Self::ContextCommand
            | Self::Scheduled => Intents::empty(),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GuildAvailable => "guild_available",
            Self::GuildRemoved => "guild_removed",
            Self::MemberJoined => "member_joined",
            Self::MemberUpdated => "member_updated",
            Self::MemberLeft => "member_left",
            Self::MessageCreated => "message_created",
            Self::MessageUpdated => "message_updated",
            Self::MessageDeleted => "message_deleted",
            Self::MessagesBulkDeleted => "messages_bulk_deleted",
            Self::ReactionAdded => "reaction_added",
            Self::ReactionRemoved => "reaction_removed",
            Self::BanAdded => "ban_added",
            Self::BanRemoved => "ban_removed",
            Self::ChannelCreated => "channel_created",
            Self::ChannelUpdated => "channel_updated",
            Self::ChannelDeleted => "channel_deleted",
            Self::RoleCreated => "role_created",
            Self::RoleUpdated => "role_updated",
            Self::RoleDeleted => "role_deleted",
            Self::Command => "command",
            Self::Component => "component",
            Self::Modal => "modal",
            Self::Autocomplete => "autocomplete",
            Self::ContextCommand => "context_command",
            Self::Scheduled => "scheduled",
        }
    }
}

impl Event {
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Self::GuildAvailable { .. } => EventKind::GuildAvailable,
            Self::GuildRemoved { .. } => EventKind::GuildRemoved,
            Self::MemberJoined(_) => EventKind::MemberJoined,
            Self::MemberUpdated(_) => EventKind::MemberUpdated,
            Self::MemberLeft { .. } => EventKind::MemberLeft,
            Self::MessageCreated(_) => EventKind::MessageCreated,
            Self::MessageUpdated(_) => EventKind::MessageUpdated,
            Self::MessageDeleted { .. } => EventKind::MessageDeleted,
            Self::MessagesBulkDeleted { .. } => EventKind::MessagesBulkDeleted,
            Self::ReactionAdded(_) => EventKind::ReactionAdded,
            Self::ReactionRemoved(_) => EventKind::ReactionRemoved,
            Self::BanAdded { .. } => EventKind::BanAdded,
            Self::BanRemoved { .. } => EventKind::BanRemoved,
            Self::ChannelCreated(_) => EventKind::ChannelCreated,
            Self::ChannelUpdated(_) => EventKind::ChannelUpdated,
            Self::ChannelDeleted(_) => EventKind::ChannelDeleted,
            Self::RoleCreated(_) => EventKind::RoleCreated,
            Self::RoleUpdated(_) => EventKind::RoleUpdated,
            Self::RoleDeleted { .. } => EventKind::RoleDeleted,
            Self::Command(_) => EventKind::Command,
            Self::Component(_) => EventKind::Component,
            Self::Modal(_) => EventKind::Modal,
            Self::Autocomplete(_) => EventKind::Autocomplete,
            Self::ContextCommand(_) => EventKind::ContextCommand,
            Self::Scheduled(_) => EventKind::Scheduled,
        }
    }

    /// The channel the event happened in, when it is about one.
    #[must_use]
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            Self::MessageCreated(message) | Self::MessageUpdated(message) => {
                Some(message.channel_id)
            }
            Self::MessageDeleted { channel_id, .. }
            | Self::MessagesBulkDeleted { channel_id, .. } => Some(*channel_id),
            Self::ReactionAdded(reaction) | Self::ReactionRemoved(reaction) => {
                Some(reaction.channel_id)
            }
            Self::ChannelCreated(channel)
            | Self::ChannelUpdated(channel)
            | Self::ChannelDeleted(channel) => Some(channel.id),
            Self::Command(invocation) => Some(invocation.channel_id),
            Self::Component(click) => Some(click.channel_id),
            Self::Modal(submission) => Some(submission.channel_id),
            Self::Autocomplete(query) => Some(query.channel_id),
            Self::ContextCommand(invocation) => Some(invocation.channel_id),
            _ => None,
        }
    }

    /// Whether the user the event is about (who wrote the message, joined, was banned, ran the
    /// command) is a bot.
    #[must_use]
    pub fn caused_by_bot(&self) -> bool {
        match self {
            Self::MessageCreated(message) | Self::MessageUpdated(message) => message.author.bot,
            Self::MemberJoined(member) | Self::MemberUpdated(member) => member.user.bot,
            Self::MemberLeft { user, .. }
            | Self::BanAdded { user, .. }
            | Self::BanRemoved { user, .. } => user.bot,
            Self::Command(invocation) => invocation.user.bot,
            Self::Component(click) => click.user.bot,
            Self::Modal(submission) => submission.user.bot,
            Self::Autocomplete(query) => query.user.bot,
            Self::ContextCommand(invocation) => invocation.user.bot,
            _ => false,
        }
    }

    /// The roles of the member the event is about, when the event carries them.
    #[must_use]
    pub fn member_roles(&self) -> &[RoleId] {
        match self {
            Self::MessageCreated(message) | Self::MessageUpdated(message) => &message.author_roles,
            Self::MemberJoined(member) | Self::MemberUpdated(member) => &member.roles,
            Self::Command(invocation) => &invocation.roles,
            Self::Component(click) => &click.roles,
            Self::Modal(submission) => &submission.roles,
            Self::Autocomplete(query) => &query.roles,
            Self::ContextCommand(invocation) => &invocation.roles,
            _ => &[],
        }
    }

    /// Every event is scoped to one guild; events outside a guild (DMs) are dropped when the
    /// gateway payload is converted, so dispatch can always key on this value.
    #[must_use]
    pub fn guild_id(&self) -> GuildId {
        match self {
            Self::GuildAvailable { guild_id, .. }
            | Self::GuildRemoved { guild_id }
            | Self::MemberLeft { guild_id, .. }
            | Self::MessageDeleted { guild_id, .. }
            | Self::MessagesBulkDeleted { guild_id, .. }
            | Self::BanAdded { guild_id, .. }
            | Self::BanRemoved { guild_id, .. }
            | Self::RoleDeleted { guild_id, .. } => *guild_id,
            Self::MemberJoined(info) | Self::MemberUpdated(info) => info.guild_id,
            Self::MessageCreated(info) | Self::MessageUpdated(info) => info.guild_id,
            Self::ReactionAdded(info) | Self::ReactionRemoved(info) => info.guild_id,
            Self::ChannelCreated(info)
            | Self::ChannelUpdated(info)
            | Self::ChannelDeleted(info) => info.guild_id,
            Self::RoleCreated(info) | Self::RoleUpdated(info) => info.guild_id,
            Self::Command(invocation) => invocation.guild_id,
            Self::Component(click) => click.guild_id,
            Self::Modal(submission) => submission.guild_id,
            Self::Autocomplete(query) => query.guild_id,
            Self::ContextCommand(invocation) => invocation.guild_id,
            Self::Scheduled(job) => job.guild_id,
        }
    }

    /// The token to answer the event with, if it is an interaction (a command, a click or a
    /// submitted modal). Discord wants an answer within three seconds.
    #[must_use]
    pub fn interaction_token(&self) -> Option<&InteractionToken> {
        match self {
            Self::Command(invocation) => Some(&invocation.token),
            Self::Component(click) => Some(&click.token),
            Self::Modal(submission) => Some(&submission.token),
            Self::Autocomplete(query) => Some(&query.token),
            Self::ContextCommand(invocation) => Some(&invocation.token),
            _ => None,
        }
    }
}

pub type EventKinds = BTreeSet<EventKind>;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user() -> UserInfo {
        UserInfo {
            id: Id::new(7),
            name: "u".into(),
            bot: false,
        }
    }

    #[test]
    fn kind_and_guild_id_follow_the_payload() {
        let event = Event::BanAdded {
            guild_id: Id::new(1),
            user: user(),
        };
        assert_eq!(event.kind(), EventKind::BanAdded);
        assert_eq!(event.guild_id(), Id::new(1));
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = Event::MessageDeleted {
            guild_id: Id::new(1),
            channel_id: Id::new(2),
            message_id: Id::new(3),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"message_deleted\""));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn messages_serialised_before_the_richer_kit_still_parse() {
        let old = json!({
            "id": "1", "channel_id": "2", "guild_id": "3",
            "author": { "id": "4", "name": "u", "bot": false },
            "author_roles": [], "content": "hi", "mentioned_users": [],
            "mentions_everyone": false, "attachment_count": 2
        });
        let info: MessageInfo = serde_json::from_value(old).unwrap();
        assert!(info.attachments.is_empty());
        assert!(info.mentioned_roles.is_empty());
        assert_eq!(info.created_at, 0);
        assert_eq!(info.edited_at, None);
    }

    #[test]
    fn interaction_token_is_redacted_in_debug() {
        let token = InteractionToken::new("secret-token");
        assert!(!format!("{token:?}").contains("secret-token"));
        assert_eq!(token.expose(), "secret-token");
    }
}
