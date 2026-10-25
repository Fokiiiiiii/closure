use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::{
    ApplicationMarker, ChannelMarker, GuildMarker, InteractionMarker, MessageMarker, RoleMarker,
    UserMarker,
};

use crate::action::Emoji;
use crate::locale::Locale;

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
    pub mentions_everyone: bool,
    pub attachment_count: u32,
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
    pub subcommand: Option<String>,
    pub options: Vec<CommandOption>,
}

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
}

impl EventKind {
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
        }
    }
}

pub type EventKinds = BTreeSet<EventKind>;

#[cfg(test)]
mod tests {
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
    fn interaction_token_is_redacted_in_debug() {
        let token = InteractionToken::new("secret-token");
        assert!(!format!("{token:?}").contains("secret-token"));
        assert_eq!(token.expose(), "secret-token");
    }
}
