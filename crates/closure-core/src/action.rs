//! The serializable boundary between modules and Discord.
//!
//! Every Discord side effect a module can cause is one [`Action`]. A single choke point
//! ([`ApiClient`](crate::client::ApiClient)) validates the action, checks the module's
//! [`Capability`] and forwards it to a [`DiscordApi`](crate::client::DiscordApi), which keeps
//! authorisation, metrics, auditing and rate-limit handling in one place. Because actions are
//! plain data, scripted (Lua/WASM) modules use the exact same path as native ones.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use twilight_model::channel::Channel;
use twilight_model::channel::message::component::{
    Button, ButtonStyle, SelectMenu, SelectMenuType,
};
use twilight_model::channel::message::{Component, Embed, Message};
use twilight_model::channel::thread::ThreadMember;
use twilight_model::channel::webhook::Webhook;
use twilight_model::guild::audit_log::AuditLog;
use twilight_model::guild::auto_moderation::AutoModerationRule;
use twilight_model::guild::invite::Invite;
use twilight_model::guild::scheduled_event::GuildScheduledEvent;
use twilight_model::guild::{
    Ban, Emoji as GuildEmoji, Guild, Member, Permissions, Role, RolePosition, VerificationLevel,
};
use twilight_model::id::Id;
use twilight_model::id::marker::{
    AutoModerationRuleMarker, EmojiMarker, ScheduledEventMarker, WebhookMarker,
};
use twilight_model::user::User;

use crate::capability::Capability;
use crate::error::DiscordError;
use crate::event::{
    ChannelId, GuildId, InteractionId, InteractionToken, MessageId, RoleId, UserId,
};
use crate::module::{ChoiceValue, OptionChoice};

pub const MAX_MESSAGE_CHARS: usize = 2000;
pub const MAX_EMBEDS: usize = 10;
pub const MAX_BULK_DELETE: usize = 100;
pub const MAX_BAN_DELETE_SECONDS: u32 = 604_800;
pub const MAX_TIMEOUT_SECONDS: u32 = 2_419_200;
pub const MAX_SLOWMODE_SECONDS: u16 = 21_600;
pub const MAX_NICKNAME_CHARS: usize = 32;

/// Why a moderation action happened; written to Discord's audit log.
///
/// Discord rejects empty reasons for audit purposes and truncates beyond 512 characters, so
/// the invariant is enforced at construction and on deserialisation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Reason(String);

impl Reason {
    pub const MAX_CHARS: usize = 512;

    pub fn new(text: impl Into<String>) -> Result<Self, DiscordError> {
        let text = text.into();
        let text = text.trim();
        if text.is_empty() {
            return Err(DiscordError::Invalid("reason must not be empty".into()));
        }
        if text.chars().count() > Self::MAX_CHARS {
            return Err(DiscordError::Invalid(format!(
                "reason exceeds {} characters",
                Self::MAX_CHARS
            )));
        }
        Ok(Self(text.to_owned()))
    }

    /// Builds a reason from generated text, truncating instead of failing.
    #[must_use]
    pub fn truncated(text: &str) -> Self {
        let text = text.trim();
        if text.is_empty() {
            return Self("no reason given".into());
        }
        if text.chars().count() <= Self::MAX_CHARS {
            return Self(text.to_owned());
        }
        let mut cut: String = text.chars().take(Self::MAX_CHARS - 1).collect();
        cut.push('…');
        Self(cut)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Reason {
    type Error = DiscordError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Reason> for String {
    fn from(reason: Reason) -> Self {
        reason.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Emoji {
    Unicode {
        value: String,
    },
    Custom {
        id: Id<EmojiMarker>,
        name: Option<String>,
    },
}

/// Which mentions in an outgoing message may notify people. The default notifies nobody, so
/// user-controlled text echoed by a module can never ping `@everyone`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ids", rename_all = "snake_case")]
pub enum MentionPolicy {
    #[default]
    None,
    Users(Vec<UserId>),
    Roles(Vec<RoleId>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
    Category,
    Announcement,
    Forum,
    Stage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum OverwriteTarget {
    Role(RoleId),
    Member(UserId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverwriteSpec {
    pub target: OverwriteTarget,
    pub allow: Permissions,
    pub deny: Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChannel {
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<ChannelId>,
    pub topic: Option<String>,
    pub nsfw: bool,
    pub slowmode_seconds: u16,
    pub overwrites: Vec<OverwriteSpec>,
}

/// Fields left as `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPatch {
    pub name: Option<String>,
    pub topic: Option<String>,
    pub nsfw: Option<bool>,
    pub slowmode_seconds: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRole {
    pub name: String,
    pub color: u32,
    pub permissions: Permissions,
    pub hoist: bool,
    pub mentionable: bool,
}

/// Fields left as `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePatch {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub permissions: Option<Permissions>,
    pub hoist: Option<bool>,
    pub mentionable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutoModTrigger {
    Keyword {
        keywords: Vec<String>,
        regex_patterns: Vec<String>,
        allow_list: Vec<String>,
    },
    Spam,
    MentionSpam {
        total_limit: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutoModAction {
    BlockMessage { custom_message: Option<String> },
    SendAlert { channel_id: ChannelId },
    Timeout { duration_seconds: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAutoModRule {
    pub name: String,
    pub trigger: AutoModTrigger,
    pub actions: Vec<AutoModAction>,
    pub enabled: bool,
    pub exempt_roles: Vec<RoleId>,
    pub exempt_channels: Vec<ChannelId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLogQuery {
    pub user_id: Option<UserId>,
    /// Discord's numeric audit log action type.
    pub action_type: Option<u16>,
    /// Return entries older than this entry id.
    pub before: Option<u64>,
    pub limit: Option<u16>,
}

/// The token of a webhook. Anyone holding it can post through the webhook, so it is redacted
/// from `Debug` output and must never reach logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WebhookToken(String);

impl WebhookToken {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WebhookToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookToken(<redacted>)")
    }
}

/// Whether a thread started on its own is open to everyone or only to the members added to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Public,
    Private,
}

/// Fields left as `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPatch {
    pub name: Option<String>,
    pub archived: Option<bool>,
    pub locked: Option<bool>,
    pub slowmode_seconds: Option<u16>,
}

pub const MAX_INVITE_AGE_SECONDS: u32 = 604_800;

/// Fields left as `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildPatch {
    pub name: Option<String>,
    pub verification_level: Option<VerificationLevel>,
}

/// Where a scheduled event takes place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventLocation {
    /// A voice channel of the guild.
    Voice { channel_id: ChannelId },
    /// A place outside Discord, named in text.
    External { place: String },
}

/// A scheduled event to create. Times are unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewScheduledEvent {
    pub name: String,
    pub description: Option<String>,
    pub start_unix: i64,
    /// Required for an external event, optional for a voice one.
    pub end_unix: Option<i64>,
    pub location: EventLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledEventStatus {
    Active,
    Completed,
    Canceled,
}

/// Fields left as `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledEventPatch {
    pub name: Option<String>,
    pub status: Option<ScheduledEventStatus>,
}

/// A channel's place in the guild's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPosition {
    pub channel_id: ChannelId,
    pub position: u64,
}

/// The archive times Discord offers: one hour, a day, three days and a week.
const THREAD_ARCHIVE_MINUTES: [u16; 4] = [60, 1440, 4320, 10080];

/// Where to start reading a channel's history, relative to one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MessageCursor {
    Before(MessageId),
    After(MessageId),
    Around(MessageId),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InteractionReply {
    pub content: Option<String>,
    pub embeds: Vec<Embed>,
    pub ephemeral: bool,
    /// Buttons and select menus under the message, in rows (see [`check_components`] for what
    /// Discord accepts). Their `custom_id`s have to start with the id of the module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}

/// A form to show to the user: a title and rows of one text input each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModalSpec {
    /// Starts with the id of the module (`module:..`); the submission comes back under it.
    pub custom_id: String,
    pub title: String,
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionResponse {
    /// Acknowledge now and answer later with a follow-up (needed when work takes over 3 s).
    Defer {
        ephemeral: bool,
    },
    Message {
        reply: InteractionReply,
    },
    /// Answers a click by changing the message the component is on.
    Update {
        reply: InteractionReply,
    },
    /// Acknowledges a click without changing anything (for now).
    DeferUpdate,
    /// Answers with a form.
    Modal(ModalSpec),
    /// Answers an autocomplete request with the choices (at most 25) the user may pick from.
    Autocomplete {
        choices: Vec<OptionChoice>,
    },
}

/// Every Discord operation available to modules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SendMessage {
        channel_id: ChannelId,
        content: Option<String>,
        embeds: Vec<Embed>,
        reply_to: Option<MessageId>,
        #[serde(default)]
        mentions: MentionPolicy,
    },
    /// Privately to a member of `guild_id`: the host checks the membership before sending.
    SendDirectMessage {
        guild_id: GuildId,
        user_id: UserId,
        content: Option<String>,
        embeds: Vec<Embed>,
    },
    EditMessage {
        channel_id: ChannelId,
        message_id: MessageId,
        content: Option<String>,
        embeds: Option<Vec<Embed>>,
    },
    DeleteMessage {
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    },
    BulkDeleteMessages {
        channel_id: ChannelId,
        message_ids: Vec<MessageId>,
        reason: Reason,
    },
    AddReaction {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
    },
    GetMessage {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    /// The newest messages when `cursor` is `None`; otherwise those around, before or after it.
    GetMessages {
        channel_id: ChannelId,
        cursor: Option<MessageCursor>,
        limit: u16,
    },
    /// The first page of the pins: Discord returns up to 50.
    GetPins {
        channel_id: ChannelId,
    },
    PinMessage {
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    },
    UnpinMessage {
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    },
    CrosspostMessage {
        channel_id: ChannelId,
        message_id: MessageId,
    },
    /// One user's reaction with `emoji`. Discord takes no audit log reason for this.
    RemoveReaction {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
        user_id: UserId,
    },
    /// Every reaction on the message, or only those with `emoji`.
    RemoveAllReactions {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Option<Emoji>,
    },
    GetReactionUsers {
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
        after: Option<UserId>,
        limit: u16,
    },
    /// Server-deafens or undeafens the member. Discord refuses it outside voice, as for mutes.
    SetVoiceDeafen {
        guild_id: GuildId,
        user_id: UserId,
        deaf: bool,
        reason: Reason,
    },
    /// Moves a member who is in voice into `channel_id`, a voice or stage channel of the guild.
    MoveMember {
        guild_id: GuildId,
        user_id: UserId,
        channel_id: ChannelId,
        reason: Reason,
    },
    /// Removes a member from voice altogether.
    DisconnectMember {
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    },
    /// A thread in `channel_id`, or one started from `source`, a message of that channel.
    /// Discord takes no audit log reason for threads.
    CreateThread {
        channel_id: ChannelId,
        name: String,
        source: Option<MessageId>,
        kind: ThreadKind,
        auto_archive_minutes: u16,
    },
    EditThread {
        thread_id: ChannelId,
        patch: ThreadPatch,
        reason: Reason,
    },
    /// Makes the bot a member of the thread.
    JoinThread {
        thread_id: ChannelId,
    },
    LeaveThread {
        thread_id: ChannelId,
    },
    AddThreadMember {
        thread_id: ChannelId,
        user_id: UserId,
    },
    RemoveThreadMember {
        thread_id: ChannelId,
        user_id: UserId,
    },
    ListThreadMembers {
        thread_id: ChannelId,
        after: Option<UserId>,
        limit: u16,
    },
    /// `max_age_seconds` of 0 never expires; `max_uses` of 0 has no limit.
    CreateInvite {
        channel_id: ChannelId,
        max_age_seconds: u32,
        max_uses: u16,
        temporary: bool,
        unique: bool,
        reason: Reason,
    },
    ListScheduledEvents {
        guild_id: GuildId,
        with_user_count: bool,
    },
    /// Discord takes no audit log reason for scheduled events.
    CreateScheduledEvent {
        guild_id: GuildId,
        spec: NewScheduledEvent,
    },
    EditScheduledEvent {
        guild_id: GuildId,
        event_id: Id<ScheduledEventMarker>,
        patch: ScheduledEventPatch,
    },
    DeleteScheduledEvent {
        guild_id: GuildId,
        event_id: Id<ScheduledEventMarker>,
    },
    ListEmojis {
        guild_id: GuildId,
    },
    /// `image` is a PNG, JPEG or GIF as a base64 `data:` URI of at most 256 KiB.
    CreateEmoji {
        guild_id: GuildId,
        name: String,
        image: String,
        reason: Reason,
    },
    DeleteEmoji {
        guild_id: GuildId,
        emoji_id: Id<EmojiMarker>,
        reason: Reason,
    },
    /// Members whose name or nickname starts with `query`, at most `limit` of them.
    SearchMembers {
        guild_id: GuildId,
        query: String,
        limit: u16,
    },
    ListMembers {
        guild_id: GuildId,
        after: Option<UserId>,
        limit: u16,
    },
    /// How many members `days` of inactivity would remove. `include_roles` are members with
    /// those roles, which are removed regardless of activity.
    PruneCount {
        guild_id: GuildId,
        days: u16,
        include_roles: Vec<RoleId>,
    },
    /// Removes the members that `PruneCount` would count, and answers with how many there were.
    Prune {
        guild_id: GuildId,
        days: u16,
        include_roles: Vec<RoleId>,
        reason: Reason,
    },
    EditGuild {
        guild_id: GuildId,
        patch: GuildPatch,
        reason: Reason,
    },
    /// Discord takes no audit log reason for reordering.
    ReorderRoles {
        guild_id: GuildId,
        positions: Vec<RolePosition>,
    },
    ReorderChannels {
        guild_id: GuildId,
        positions: Vec<ChannelPosition>,
    },
    /// A direct message channel with a member of `guild_id`; the host checks the membership.
    CreateDirectMessageChannel {
        guild_id: GuildId,
        user_id: UserId,
    },
    CreateWebhook {
        channel_id: ChannelId,
        name: String,
        reason: Reason,
    },
    ListWebhooks {
        channel_id: ChannelId,
    },
    /// The host first checks that the webhook belongs to `guild_id`, since Discord's endpoint
    /// does not name a guild.
    DeleteWebhook {
        guild_id: GuildId,
        webhook_id: Id<WebhookMarker>,
        reason: Reason,
    },
    /// Posts through a webhook. The host first checks that the webhook belongs to `guild_id`.
    ExecuteWebhook {
        guild_id: GuildId,
        webhook_id: Id<WebhookMarker>,
        token: WebhookToken,
        content: Option<String>,
        embeds: Vec<Embed>,
        username: Option<String>,
        avatar_url: Option<String>,
        mentions: MentionPolicy,
    },
    ListInvites {
        guild_id: GuildId,
    },
    /// The host first checks that the invite belongs to `guild_id`, since Discord's endpoint
    /// does not name a guild.
    DeleteInvite {
        guild_id: GuildId,
        code: String,
        reason: Reason,
    },

    GetGuild {
        guild_id: GuildId,
    },
    GetMember {
        guild_id: GuildId,
        user_id: UserId,
    },
    Kick {
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    },
    Ban {
        guild_id: GuildId,
        user_id: UserId,
        delete_message_seconds: u32,
        reason: Reason,
    },
    Unban {
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    },
    /// `duration_seconds: None` lifts an existing timeout.
    Timeout {
        guild_id: GuildId,
        user_id: UserId,
        duration_seconds: Option<u32>,
        reason: Reason,
    },
    /// Server-mutes or unmutes the member in voice channels. Discord refuses it while the
    /// member is not connected to voice.
    SetVoiceMute {
        guild_id: GuildId,
        user_id: UserId,
        mute: bool,
        reason: Reason,
    },
    SetNickname {
        guild_id: GuildId,
        user_id: UserId,
        nickname: Option<String>,
        reason: Reason,
    },
    AddRole {
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    },
    RemoveRole {
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    },
    ListBans {
        guild_id: GuildId,
        after: Option<UserId>,
        limit: u16,
    },

    GetChannel {
        channel_id: ChannelId,
    },
    CreateChannel {
        guild_id: GuildId,
        spec: NewChannel,
        reason: Reason,
    },
    EditChannel {
        channel_id: ChannelId,
        patch: ChannelPatch,
        reason: Reason,
    },
    DeleteChannel {
        channel_id: ChannelId,
        reason: Reason,
    },
    SetPermissionOverwrite {
        channel_id: ChannelId,
        overwrite: OverwriteSpec,
        reason: Reason,
    },
    DeletePermissionOverwrite {
        channel_id: ChannelId,
        target: OverwriteTarget,
        reason: Reason,
    },

    ListRoles {
        guild_id: GuildId,
    },
    CreateRole {
        guild_id: GuildId,
        spec: NewRole,
        reason: Reason,
    },
    EditRole {
        guild_id: GuildId,
        role_id: RoleId,
        patch: RolePatch,
        reason: Reason,
    },
    DeleteRole {
        guild_id: GuildId,
        role_id: RoleId,
        reason: Reason,
    },

    ListAutoModRules {
        guild_id: GuildId,
    },
    CreateAutoModRule {
        guild_id: GuildId,
        spec: NewAutoModRule,
        reason: Reason,
    },
    DeleteAutoModRule {
        guild_id: GuildId,
        rule_id: Id<AutoModerationRuleMarker>,
        reason: Reason,
    },

    GetAuditLog {
        guild_id: GuildId,
        query: AuditLogQuery,
    },

    Respond {
        interaction_id: InteractionId,
        token: InteractionToken,
        response: InteractionResponse,
    },
    Followup {
        token: InteractionToken,
        reply: InteractionReply,
    },
    EditResponse {
        token: InteractionToken,
        reply: InteractionReply,
    },
}

/// The result of a successful [`Action`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Outcome {
    None,
    Message(Box<Message>),
    Guild(Box<Guild>),
    Member(Box<Member>),
    Channel(Box<Channel>),
    Role(Box<Role>),
    Roles(Vec<Role>),
    Bans(Vec<Ban>),
    AutoModRule(Box<AutoModerationRule>),
    AutoModRules(Vec<AutoModerationRule>),
    AuditLog(Box<AuditLog>),
    Messages(Vec<Message>),
    Users(Vec<User>),
    ThreadMembers(Vec<ThreadMember>),
    Invite(Box<Invite>),
    Invites(Vec<Invite>),
    Webhook(Box<Webhook>),
    Webhooks(Vec<Webhook>),
    Members(Vec<Member>),
    /// A number of members, as a prune answers.
    Count(u64),
    ScheduledEvents(Vec<GuildScheduledEvent>),
    ScheduledEvent(Box<GuildScheduledEvent>),
    Emojis(Vec<GuildEmoji>),
    Emoji(Box<GuildEmoji>),
}

fn ensure(condition: bool, message: impl FnOnce() -> String) -> Result<(), DiscordError> {
    if condition {
        Ok(())
    } else {
        Err(DiscordError::Invalid(message()))
    }
}

fn check_len(field: &str, text: &str, min: usize, max: usize) -> Result<(), DiscordError> {
    let length = text.chars().count();
    ensure((min..=max).contains(&length), || {
        format!("{field} must be {min}-{max} characters, got {length}")
    })
}

fn check_message_body(content: Option<&str>, embeds: &[Embed]) -> Result<(), DiscordError> {
    if let Some(content) = content {
        check_len("content", content, 0, MAX_MESSAGE_CHARS)?;
    }
    ensure(embeds.len() <= MAX_EMBEDS, || {
        format!("at most {MAX_EMBEDS} embeds are allowed")
    })
}

/// A new message says something: content or at least one embed.
fn check_new_message(content: Option<&str>, embeds: &[Embed]) -> Result<(), DiscordError> {
    check_message_body(content, embeds)?;
    ensure(
        content.is_some_and(|c| !c.is_empty()) || !embeds.is_empty(),
        || "message needs content or at least one embed".into(),
    )
}

/// Discord takes at most 25 choices; names and string values are 1-100 characters.
fn check_choices(choices: &[OptionChoice]) -> Result<(), DiscordError> {
    ensure(choices.len() <= 25, || {
        format!("at most 25 choices are allowed, got {}", choices.len())
    })?;
    for choice in choices {
        check_len("choice name", &choice.name, 1, 100)?;
        if let ChoiceValue::String(value) = &choice.value {
            check_len("choice value", value, 1, 100)?;
        }
    }
    Ok(())
}

fn check_reply(reply: &InteractionReply, require_body: bool) -> Result<(), DiscordError> {
    check_message_body(reply.content.as_deref(), &reply.embeds)?;
    check_components(&reply.components)?;
    if require_body {
        ensure(
            reply.content.as_deref().is_some_and(|c| !c.is_empty()) || !reply.embeds.is_empty(),
            || "reply needs content or at least one embed".into(),
        )?;
    }
    Ok(())
}

/// What Discord accepts under a message: up to five rows, each of up to five buttons or one
/// select menu of text options.
pub fn check_components(components: &[Component]) -> Result<(), DiscordError> {
    ensure(components.len() <= 5, || {
        "a message has at most 5 rows of components".into()
    })?;
    for row in components {
        let Component::ActionRow(row) = row else {
            return Err(DiscordError::Invalid(
                "components belong in action rows".into(),
            ));
        };
        match row.components.as_slice() {
            [Component::SelectMenu(menu)] => check_select(menu)?,
            buttons if !buttons.is_empty() && buttons.len() <= 5 => {
                for button in buttons {
                    let Component::Button(button) = button else {
                        return Err(DiscordError::Invalid(
                            "a row holds up to 5 buttons or one select menu".into(),
                        ));
                    };
                    check_button(button)?;
                }
            }
            _ => {
                return Err(DiscordError::Invalid(
                    "a row holds up to 5 buttons or one select menu".into(),
                ));
            }
        }
    }
    Ok(())
}

fn check_button(button: &Button) -> Result<(), DiscordError> {
    if let Some(label) = &button.label {
        check_len("button label", label, 1, 80)?;
    }
    match button.style {
        ButtonStyle::Link => ensure(
            button
                .url
                .as_deref()
                .is_some_and(|url| url.starts_with("https://")),
            || "a link button needs an https:// url".into(),
        ),
        ButtonStyle::Primary
        | ButtonStyle::Secondary
        | ButtonStyle::Success
        | ButtonStyle::Danger => {
            check_len(
                "button custom_id",
                button.custom_id.as_deref().unwrap_or_default(),
                1,
                100,
            )?;
            ensure(button.label.is_some() || button.emoji.is_some(), || {
                "a button needs a label or an emoji".into()
            })
        }
        _ => Err(DiscordError::Invalid(
            "this kind of button is not supported".into(),
        )),
    }
}

fn check_select(menu: &SelectMenu) -> Result<(), DiscordError> {
    ensure(menu.kind == SelectMenuType::Text, || {
        "only select menus of text options are supported".into()
    })?;
    check_len("select custom_id", &menu.custom_id, 1, 100)?;
    if let Some(placeholder) = &menu.placeholder {
        check_len("select placeholder", placeholder, 0, 150)?;
    }
    let options = menu.options.as_deref().unwrap_or_default();
    ensure((1..=25).contains(&options.len()), || {
        "a select menu has 1-25 options".into()
    })?;
    for option in options {
        check_len("option label", &option.label, 1, 100)?;
        check_len("option value", &option.value, 1, 100)?;
        if let Some(description) = &option.description {
            check_len("option description", description, 0, 100)?;
        }
    }
    let (min, max) = (menu.min_values.unwrap_or(1), menu.max_values.unwrap_or(1));
    ensure(
        min <= max && usize::from(max) <= options.len() && max <= 25,
        || "a select menu's min and max values do not fit its options".into(),
    )
}

/// A modal has a title, an id, and 1-5 rows of one text input each.
fn check_modal(spec: &ModalSpec) -> Result<(), DiscordError> {
    check_len("modal custom_id", &spec.custom_id, 1, 100)?;
    check_len("modal title", &spec.title, 1, 45)?;
    ensure((1..=5).contains(&spec.components.len()), || {
        "a modal has 1-5 inputs".into()
    })?;
    for row in &spec.components {
        let [Component::ActionRow(row)] = std::slice::from_ref(row) else {
            return Err(DiscordError::Invalid(
                "modal inputs belong in action rows".into(),
            ));
        };
        let [Component::TextInput(input)] = row.components.as_slice() else {
            return Err(DiscordError::Invalid(
                "a modal row holds one text input".into(),
            ));
        };
        check_len("input custom_id", &input.custom_id, 1, 100)?;
        #[allow(deprecated)]
        if let Some(label) = &input.label {
            check_len("input label", label, 1, 45)?;
        }
        if let Some(max) = input.max_length {
            ensure((1..=4000).contains(&max), || {
                "an input holds 1-4000 characters".into()
            })?;
        }
    }
    Ok(())
}

impl Action {
    /// The `custom_id`s of the components and modal this action sends.
    #[must_use]
    pub fn custom_ids(&self) -> Vec<&str> {
        fn of(components: &[Component]) -> Vec<&str> {
            let mut found = Vec::new();
            for component in components {
                match component {
                    Component::ActionRow(row) => found.extend(of(&row.components)),
                    Component::Button(button) => found.extend(button.custom_id.as_deref()),
                    Component::SelectMenu(menu) => found.push(menu.custom_id.as_str()),
                    Component::TextInput(input) => found.push(input.custom_id.as_str()),
                    _ => {}
                }
            }
            found
        }
        match self {
            Self::Respond { response, .. } => match response {
                InteractionResponse::Message { reply } | InteractionResponse::Update { reply } => {
                    of(&reply.components)
                }
                InteractionResponse::Modal(spec) => {
                    let mut ids = vec![spec.custom_id.as_str()];
                    ids.extend(of(&spec.components));
                    ids
                }
                InteractionResponse::Defer { .. }
                | InteractionResponse::DeferUpdate
                | InteractionResponse::Autocomplete { .. } => Vec::new(),
            },
            Self::Followup { reply, .. } | Self::EditResponse { reply, .. } => {
                of(&reply.components)
            }
            _ => Vec::new(),
        }
    }
}

/// A unicode emoji is a short string; an empty one names nothing.
fn check_emoji(emoji: &Emoji) -> Result<(), DiscordError> {
    match emoji {
        Emoji::Unicode { value } => check_len("emoji", value, 1, 64),
        Emoji::Custom { .. } => Ok(()),
    }
}

fn check_scheduled_event(spec: &NewScheduledEvent) -> Result<(), DiscordError> {
    check_len("event name", &spec.name, 1, 100)?;
    if let Some(description) = &spec.description {
        check_len("event description", description, 0, 1000)?;
    }
    match &spec.location {
        EventLocation::Voice { .. } => Ok(()),
        EventLocation::External { place } => {
            check_len("event place", place, 1, 100)?;
            ensure(
                spec.end_unix.is_some_and(|end| end > spec.start_unix),
                || "an external event needs an end after its start".into(),
            )
        }
    }
}

/// Discord takes a PNG, JPEG or GIF of at most 256 KiB, sent as a base64 data URI.
fn check_emoji_image(image: &str) -> Result<(), DiscordError> {
    const PREFIXES: [&str; 3] = [
        "data:image/png;base64,",
        "data:image/jpeg;base64,",
        "data:image/gif;base64,",
    ];
    const MAX_IMAGE_BYTES: usize = 256 * 1024;
    let Some(payload) = PREFIXES
        .iter()
        .find_map(|prefix| image.strip_prefix(prefix))
    else {
        return Err(DiscordError::Invalid(
            "an emoji image is a PNG, JPEG or GIF data URI".into(),
        ));
    };
    // Four base64 characters carry three bytes, so this bounds the decoded size from above.
    ensure(payload.len() / 4 * 3 <= MAX_IMAGE_BYTES, || {
        "an emoji image is at most 256 KiB".into()
    })
}

/// Limits and shapes of the message and thread actions, checked by [`Action::validate`].
#[allow(clippy::too_many_lines)]
fn check_new_limits(action: &Action) -> Result<(), DiscordError> {
    match action {
        Action::GetMessages { limit, .. } => ensure((1..=100).contains(limit), || {
            "message history limit must be 1-100".into()
        }),
        Action::GetReactionUsers { emoji, limit, .. } => {
            check_emoji(emoji)?;
            ensure((1..=100).contains(limit), || {
                "reaction user limit must be 1-100".into()
            })
        }
        Action::RemoveReaction { emoji, .. } => check_emoji(emoji),
        Action::RemoveAllReactions { emoji, .. } => emoji.as_ref().map_or(Ok(()), check_emoji),
        Action::CreateThread {
            name,
            source,
            kind,
            auto_archive_minutes,
            ..
        } => {
            check_len("thread name", name, 1, 100)?;
            ensure(
                THREAD_ARCHIVE_MINUTES.contains(auto_archive_minutes),
                || format!("auto archive must be one of {THREAD_ARCHIVE_MINUTES:?} minutes"),
            )?;
            ensure(source.is_none() || *kind == ThreadKind::Public, || {
                "a thread started from a message is public".into()
            })
        }
        Action::EditThread { patch, .. } => {
            if let Some(name) = &patch.name {
                check_len("thread name", name, 1, 100)?;
            }
            ensure(
                patch
                    .slowmode_seconds
                    .is_none_or(|s| s <= MAX_SLOWMODE_SECONDS),
                || format!("slowmode is at most {MAX_SLOWMODE_SECONDS} seconds"),
            )
        }
        Action::ListThreadMembers { limit, .. } => ensure((1..=100).contains(limit), || {
            "thread member limit must be 1-100".into()
        }),
        Action::CreateInvite {
            max_age_seconds,
            max_uses,
            ..
        } => {
            ensure(*max_age_seconds <= MAX_INVITE_AGE_SECONDS, || {
                format!("an invite lasts at most {MAX_INVITE_AGE_SECONDS} seconds (7 days)")
            })?;
            ensure(*max_uses <= 100, || "an invite has at most 100 uses".into())
        }
        Action::CreateWebhook { name, .. } => check_len("webhook name", name, 1, 80),
        Action::ExecuteWebhook {
            content,
            embeds,
            username,
            avatar_url,
            ..
        } => {
            check_new_message(content.as_deref(), embeds)?;
            if let Some(username) = username {
                check_len("webhook username", username, 1, 80)?;
            }
            avatar_url.as_deref().map_or(Ok(()), |url| {
                ensure(url.starts_with("https://"), || {
                    "a webhook avatar is an https:// url".into()
                })
            })
        }
        Action::SearchMembers { query, limit, .. } => {
            check_len("member search query", query, 1, 100)?;
            ensure((1..=1000).contains(limit), || {
                "member search limit must be 1-1000".into()
            })
        }
        Action::ListMembers { limit, .. } => ensure((1..=1000).contains(limit), || {
            "member list limit must be 1-1000".into()
        }),
        Action::PruneCount { days, .. } | Action::Prune { days, .. } => {
            ensure((1..=30).contains(days), || "prune days must be 1-30".into())
        }
        Action::EditGuild { patch, .. } => patch
            .name
            .as_deref()
            .map_or(Ok(()), |name| check_len("guild name", name, 2, 100)),
        Action::ReorderRoles { positions, .. } => {
            ensure(!positions.is_empty(), || "reorder needs positions".into())?;
            let unique: BTreeSet<_> = positions.iter().map(|position| position.id).collect();
            ensure(unique.len() == positions.len(), || {
                "reorder roles must not repeat a role".into()
            })
        }
        Action::ReorderChannels { positions, .. } => {
            ensure(!positions.is_empty(), || "reorder needs positions".into())?;
            let unique: BTreeSet<_> = positions.iter().map(|p| p.channel_id).collect();
            ensure(unique.len() == positions.len(), || {
                "reorder channels must not repeat a channel".into()
            })
        }
        Action::CreateScheduledEvent { spec, .. } => check_scheduled_event(spec),
        Action::EditScheduledEvent { patch, .. } => patch
            .name
            .as_deref()
            .map_or(Ok(()), |name| check_len("event name", name, 1, 100)),
        Action::CreateEmoji { name, image, .. } => {
            ensure(
                (2..=32).contains(&name.len())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                || "an emoji name is 2-32 letters, digits or '_'".into(),
            )?;
            check_emoji_image(image)
        }
        Action::DeleteInvite { code, .. } => ensure(
            (1..=64).contains(&code.len())
                && code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            || "an invite code is 1-64 letters, digits, '-' or '_'".into(),
        ),
        _ => Ok(()),
    }
}

fn check_timeout_seconds(seconds: u32) -> Result<(), DiscordError> {
    ensure((1..=MAX_TIMEOUT_SECONDS).contains(&seconds), || {
        format!("timeout must be 1-{MAX_TIMEOUT_SECONDS} seconds (28 days)")
    })
}

fn check_auto_mod(spec: &NewAutoModRule) -> Result<(), DiscordError> {
    check_len("auto moderation rule name", &spec.name, 1, 100)?;
    ensure(!spec.actions.is_empty(), || {
        "a rule needs at least one action".into()
    })?;
    ensure(spec.actions.len() <= 3, || {
        "a rule has at most 3 actions".into()
    })?;
    for action in &spec.actions {
        match action {
            AutoModAction::Timeout { duration_seconds } => {
                check_timeout_seconds(*duration_seconds)?;
            }
            AutoModAction::BlockMessage {
                custom_message: Some(message),
            } => check_len("custom message", message, 1, 150)?,
            AutoModAction::BlockMessage {
                custom_message: None,
            }
            | AutoModAction::SendAlert { .. } => {}
        }
    }
    match &spec.trigger {
        AutoModTrigger::Keyword {
            keywords,
            regex_patterns,
            allow_list,
        } => {
            ensure(keywords.len() <= 1000, || "at most 1000 keywords".into())?;
            ensure(regex_patterns.len() <= 10, || {
                "at most 10 regex patterns".into()
            })?;
            ensure(allow_list.len() <= 100, || {
                "at most 100 allow-list entries".into()
            })?;
            ensure(!keywords.is_empty() || !regex_patterns.is_empty(), || {
                "a keyword rule needs keywords or regex patterns".into()
            })?;
            for keyword in keywords {
                check_len("keyword", keyword, 1, 60)?;
            }
            for pattern in regex_patterns {
                check_len("regex pattern", pattern, 1, 260)?;
            }
        }
        AutoModTrigger::Spam => {}
        AutoModTrigger::MentionSpam { total_limit } => {
            ensure((1..=50).contains(total_limit), || {
                "mention limit must be 1-50".into()
            })?;
        }
    }
    Ok(())
}

impl Action {
    /// The capability a module needs to perform this action.
    #[must_use]
    pub fn capability(&self) -> Capability {
        match self {
            Self::CreateInvite { .. } | Self::ListInvites { .. } | Self::DeleteInvite { .. } => {
                Capability::ManageInvites
            }
            Self::EditGuild { .. } => Capability::ManageGuild,
            Self::SetVoiceDeafen { .. } => Capability::DeafenMembers,
            Self::MoveMember { .. } | Self::DisconnectMember { .. } => Capability::MoveMembers,
            Self::CreateThread { .. }
            | Self::EditThread { .. }
            | Self::AddThreadMember { .. }
            | Self::RemoveThreadMember { .. } => Capability::ManageThreads,
            Self::GetMessage { .. }
            | Self::GetMessages { .. }
            | Self::GetPins { .. }
            | Self::GetReactionUsers { .. }
            | Self::ListThreadMembers { .. } => Capability::ReadMessages,
            Self::PinMessage { .. }
            | Self::UnpinMessage { .. }
            | Self::CrosspostMessage { .. }
            | Self::RemoveReaction { .. }
            | Self::RemoveAllReactions { .. } => Capability::ManageMessages,
            Self::SendMessage { .. }
            | Self::EditMessage { .. }
            | Self::JoinThread { .. }
            | Self::LeaveThread { .. }
            | Self::ExecuteWebhook { .. } => Capability::SendMessages,
            Self::CreateWebhook { .. } | Self::ListWebhooks { .. } | Self::DeleteWebhook { .. } => {
                Capability::ManageWebhooks
            }
            Self::SendDirectMessage { .. } | Self::CreateDirectMessageChannel { .. } => {
                Capability::SendDirectMessages
            }
            Self::AddReaction { .. } => Capability::AddReactions,
            Self::DeleteMessage { .. } | Self::BulkDeleteMessages { .. } => {
                Capability::ManageMessages
            }
            Self::GetGuild { .. }
            | Self::GetMember { .. }
            | Self::GetChannel { .. }
            | Self::ListRoles { .. }
            | Self::ListAutoModRules { .. }
            | Self::SearchMembers { .. }
            | Self::ListMembers { .. }
            | Self::ListScheduledEvents { .. }
            | Self::ListEmojis { .. } => Capability::ReadGuild,
            Self::CreateScheduledEvent { .. }
            | Self::EditScheduledEvent { .. }
            | Self::DeleteScheduledEvent { .. } => Capability::ManageEvents,
            Self::CreateEmoji { .. } | Self::DeleteEmoji { .. } => Capability::ManageEmojis,
            Self::ListBans { .. } => Capability::ReadBans,
            Self::Kick { .. } | Self::PruneCount { .. } | Self::Prune { .. } => {
                Capability::KickMembers
            }
            Self::Ban { .. } | Self::Unban { .. } => Capability::BanMembers,
            Self::Timeout { .. } => Capability::TimeoutMembers,
            Self::SetVoiceMute { .. } => Capability::MuteMembers,
            Self::SetNickname { .. } => Capability::ManageNicknames,
            Self::AddRole { .. }
            | Self::RemoveRole { .. }
            | Self::CreateRole { .. }
            | Self::EditRole { .. }
            | Self::DeleteRole { .. }
            | Self::ReorderRoles { .. } => Capability::ManageRoles,
            Self::CreateChannel { .. }
            | Self::EditChannel { .. }
            | Self::DeleteChannel { .. }
            | Self::SetPermissionOverwrite { .. }
            | Self::DeletePermissionOverwrite { .. }
            | Self::ReorderChannels { .. } => Capability::ManageChannels,
            Self::CreateAutoModRule { .. } | Self::DeleteAutoModRule { .. } => {
                Capability::ManageAutoMod
            }
            Self::GetAuditLog { .. } => Capability::ReadAuditLog,
            Self::Respond { .. } | Self::Followup { .. } | Self::EditResponse { .. } => {
                Capability::RespondToInteractions
            }
        }
    }

    /// The guild the action names, if it names one. Actions that identify only a channel, a
    /// message or an interaction return `None`: their guild cannot be known without a lookup
    /// (for channels and messages, see [`channels`](Self::channels); for interaction
    /// responses, [`interaction_token`](Self::interaction_token)).
    #[must_use]
    pub fn guild_id(&self) -> Option<GuildId> {
        match self {
            Self::GetGuild { guild_id }
            | Self::GetMember { guild_id, .. }
            | Self::Kick { guild_id, .. }
            | Self::Ban { guild_id, .. }
            | Self::Unban { guild_id, .. }
            | Self::Timeout { guild_id, .. }
            | Self::SetVoiceMute { guild_id, .. }
            | Self::SetNickname { guild_id, .. }
            | Self::AddRole { guild_id, .. }
            | Self::RemoveRole { guild_id, .. }
            | Self::ListBans { guild_id, .. }
            | Self::CreateChannel { guild_id, .. }
            | Self::ListRoles { guild_id }
            | Self::CreateRole { guild_id, .. }
            | Self::EditRole { guild_id, .. }
            | Self::DeleteRole { guild_id, .. }
            | Self::ListAutoModRules { guild_id }
            | Self::CreateAutoModRule { guild_id, .. }
            | Self::DeleteAutoModRule { guild_id, .. }
            | Self::GetAuditLog { guild_id, .. }
            | Self::SendDirectMessage { guild_id, .. }
            | Self::ListInvites { guild_id }
            | Self::ListScheduledEvents { guild_id, .. }
            | Self::CreateScheduledEvent { guild_id, .. }
            | Self::EditScheduledEvent { guild_id, .. }
            | Self::DeleteScheduledEvent { guild_id, .. }
            | Self::ListEmojis { guild_id }
            | Self::CreateEmoji { guild_id, .. }
            | Self::DeleteEmoji { guild_id, .. }
            | Self::SearchMembers { guild_id, .. }
            | Self::ListMembers { guild_id, .. }
            | Self::PruneCount { guild_id, .. }
            | Self::Prune { guild_id, .. }
            | Self::EditGuild { guild_id, .. }
            | Self::ReorderRoles { guild_id, .. }
            | Self::ReorderChannels { guild_id, .. }
            | Self::CreateDirectMessageChannel { guild_id, .. }
            | Self::DeleteWebhook { guild_id, .. }
            | Self::ExecuteWebhook { guild_id, .. }
            | Self::DeleteInvite { guild_id, .. }
            | Self::SetVoiceDeafen { guild_id, .. }
            | Self::MoveMember { guild_id, .. }
            | Self::DisconnectMember { guild_id, .. } => Some(*guild_id),
            Self::SendMessage { .. }
            | Self::EditMessage { .. }
            | Self::DeleteMessage { .. }
            | Self::BulkDeleteMessages { .. }
            | Self::AddReaction { .. }
            | Self::GetChannel { .. }
            | Self::EditChannel { .. }
            | Self::DeleteChannel { .. }
            | Self::SetPermissionOverwrite { .. }
            | Self::DeletePermissionOverwrite { .. }
            | Self::Respond { .. }
            | Self::Followup { .. }
            | Self::EditResponse { .. }
            | Self::CreateInvite { .. }
            | Self::CreateWebhook { .. }
            | Self::ListWebhooks { .. }
            | Self::GetMessage { .. }
            | Self::GetMessages { .. }
            | Self::GetPins { .. }
            | Self::PinMessage { .. }
            | Self::UnpinMessage { .. }
            | Self::CrosspostMessage { .. }
            | Self::RemoveReaction { .. }
            | Self::RemoveAllReactions { .. }
            | Self::GetReactionUsers { .. }
            | Self::CreateThread { .. }
            | Self::EditThread { .. }
            | Self::JoinThread { .. }
            | Self::LeaveThread { .. }
            | Self::AddThreadMember { .. }
            | Self::RemoveThreadMember { .. }
            | Self::ListThreadMembers { .. } => None,
        }
    }

    /// Every channel the action targets or refers to: the channel of a message action, the
    /// channel being edited, the parent category of a new channel, the alert channel and the
    /// exempt channels of an auto-moderation rule. Which guild each belongs to has to be looked
    /// up; see [`DiscordApi::channel_guild`](crate::client::DiscordApi::channel_guild).
    ///
    /// The match has no wildcard arm, so a new action cannot be added without deciding this.
    #[must_use]
    pub fn channels(&self) -> Vec<ChannelId> {
        match self {
            Self::ReorderChannels { positions, .. } => positions
                .iter()
                .map(|position| position.channel_id)
                .collect(),
            Self::EditThread { thread_id, .. }
            | Self::JoinThread { thread_id }
            | Self::LeaveThread { thread_id }
            | Self::AddThreadMember { thread_id, .. }
            | Self::RemoveThreadMember { thread_id, .. }
            | Self::ListThreadMembers { thread_id, .. } => vec![*thread_id],
            // A text command's reply goes into the channel of its message: that channel must be
            // in the guild, like any other target.
            Self::Respond { token, .. }
            | Self::Followup { token, .. }
            | Self::EditResponse { token, .. } => token
                .text_command_message()
                .map(|(channel, _)| channel)
                .into_iter()
                .collect(),
            Self::SendMessage { channel_id, .. }
            | Self::EditMessage { channel_id, .. }
            | Self::GetMessage { channel_id, .. }
            | Self::GetMessages { channel_id, .. }
            | Self::GetPins { channel_id }
            | Self::PinMessage { channel_id, .. }
            | Self::UnpinMessage { channel_id, .. }
            | Self::CrosspostMessage { channel_id, .. }
            | Self::RemoveReaction { channel_id, .. }
            | Self::RemoveAllReactions { channel_id, .. }
            | Self::GetReactionUsers { channel_id, .. }
            | Self::CreateThread { channel_id, .. }
            | Self::CreateInvite { channel_id, .. }
            | Self::CreateWebhook { channel_id, .. }
            | Self::ListWebhooks { channel_id }
            | Self::MoveMember { channel_id, .. }
            | Self::DeleteMessage { channel_id, .. }
            | Self::BulkDeleteMessages { channel_id, .. }
            | Self::AddReaction { channel_id, .. }
            | Self::GetChannel { channel_id }
            | Self::EditChannel { channel_id, .. }
            | Self::DeleteChannel { channel_id, .. }
            | Self::SetPermissionOverwrite { channel_id, .. }
            | Self::DeletePermissionOverwrite { channel_id, .. } => vec![*channel_id],
            Self::CreateChannel { spec, .. } => spec.parent_id.into_iter().collect(),
            Self::CreateAutoModRule { spec, .. } => spec
                .actions
                .iter()
                .filter_map(|action| match action {
                    AutoModAction::SendAlert { channel_id } => Some(*channel_id),
                    AutoModAction::BlockMessage { .. } | AutoModAction::Timeout { .. } => None,
                })
                .chain(spec.exempt_channels.iter().copied())
                .collect(),
            // These name a guild and nothing else that is a channel, or are bound to an
            // interaction (see `interaction_token`).
            Self::GetGuild { .. }
            | Self::GetMember { .. }
            | Self::Kick { .. }
            | Self::Ban { .. }
            | Self::Unban { .. }
            | Self::Timeout { .. }
            | Self::SetVoiceMute { .. }
            | Self::SetNickname { .. }
            | Self::AddRole { .. }
            | Self::RemoveRole { .. }
            | Self::ListBans { .. }
            | Self::ListRoles { .. }
            | Self::CreateRole { .. }
            | Self::EditRole { .. }
            | Self::DeleteRole { .. }
            | Self::ListAutoModRules { .. }
            | Self::DeleteAutoModRule { .. }
            | Self::GetAuditLog { .. }
            | Self::SendDirectMessage { .. }
            | Self::SetVoiceDeafen { .. }
            | Self::DisconnectMember { .. }
            | Self::ListInvites { .. }
            | Self::DeleteInvite { .. }
            | Self::DeleteWebhook { .. }
            | Self::ExecuteWebhook { .. }
            | Self::SearchMembers { .. }
            | Self::ListMembers { .. }
            | Self::PruneCount { .. }
            | Self::Prune { .. }
            | Self::EditGuild { .. }
            | Self::ReorderRoles { .. }
            | Self::CreateDirectMessageChannel { .. }
            | Self::ListScheduledEvents { .. }
            | Self::CreateScheduledEvent { .. }
            | Self::EditScheduledEvent { .. }
            | Self::DeleteScheduledEvent { .. }
            | Self::ListEmojis { .. }
            | Self::CreateEmoji { .. }
            | Self::DeleteEmoji { .. } => Vec::new(),
        }
    }

    /// The interaction token of the actions that answer an interaction. Which guild a token
    /// belongs to is known only to the host, which registered it when the interaction arrived;
    /// see [`InteractionGuilds`](crate::interaction::InteractionGuilds).
    #[must_use]
    pub fn interaction_token(&self) -> Option<&InteractionToken> {
        match self {
            Self::Respond { token, .. }
            | Self::Followup { token, .. }
            | Self::EditResponse { token, .. } => Some(token),
            Self::SendMessage { .. }
            | Self::EditMessage { .. }
            | Self::DeleteMessage { .. }
            | Self::BulkDeleteMessages { .. }
            | Self::AddReaction { .. }
            | Self::GetGuild { .. }
            | Self::GetMember { .. }
            | Self::Kick { .. }
            | Self::Ban { .. }
            | Self::Unban { .. }
            | Self::Timeout { .. }
            | Self::SetVoiceMute { .. }
            | Self::SetNickname { .. }
            | Self::AddRole { .. }
            | Self::RemoveRole { .. }
            | Self::ListBans { .. }
            | Self::GetChannel { .. }
            | Self::CreateChannel { .. }
            | Self::EditChannel { .. }
            | Self::DeleteChannel { .. }
            | Self::SetPermissionOverwrite { .. }
            | Self::DeletePermissionOverwrite { .. }
            | Self::ListRoles { .. }
            | Self::CreateRole { .. }
            | Self::EditRole { .. }
            | Self::DeleteRole { .. }
            | Self::ListAutoModRules { .. }
            | Self::CreateAutoModRule { .. }
            | Self::DeleteAutoModRule { .. }
            | Self::GetAuditLog { .. }
            | Self::SendDirectMessage { .. }
            | Self::SearchMembers { .. }
            | Self::ListMembers { .. }
            | Self::PruneCount { .. }
            | Self::Prune { .. }
            | Self::EditGuild { .. }
            | Self::ReorderRoles { .. }
            | Self::ReorderChannels { .. }
            | Self::CreateDirectMessageChannel { .. }
            | Self::ListScheduledEvents { .. }
            | Self::CreateScheduledEvent { .. }
            | Self::EditScheduledEvent { .. }
            | Self::DeleteScheduledEvent { .. }
            | Self::ListEmojis { .. }
            | Self::CreateEmoji { .. }
            | Self::DeleteEmoji { .. }
            | Self::CreateWebhook { .. }
            | Self::ListWebhooks { .. }
            | Self::DeleteWebhook { .. }
            | Self::ExecuteWebhook { .. }
            | Self::CreateInvite { .. }
            | Self::ListInvites { .. }
            | Self::DeleteInvite { .. }
            | Self::SetVoiceDeafen { .. }
            | Self::MoveMember { .. }
            | Self::DisconnectMember { .. }
            | Self::GetMessage { .. }
            | Self::GetMessages { .. }
            | Self::GetPins { .. }
            | Self::PinMessage { .. }
            | Self::UnpinMessage { .. }
            | Self::CrosspostMessage { .. }
            | Self::RemoveReaction { .. }
            | Self::RemoveAllReactions { .. }
            | Self::GetReactionUsers { .. }
            | Self::CreateThread { .. }
            | Self::EditThread { .. }
            | Self::JoinThread { .. }
            | Self::LeaveThread { .. }
            | Self::AddThreadMember { .. }
            | Self::RemoveThreadMember { .. }
            | Self::ListThreadMembers { .. } => None,
        }
    }

    /// A stable `snake_case` name for metrics labels and tracing.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::SearchMembers { .. } => "search_members",
            Self::ListMembers { .. } => "list_members",
            Self::PruneCount { .. } => "prune_count",
            Self::Prune { .. } => "prune",
            Self::EditGuild { .. } => "edit_guild",
            Self::ReorderRoles { .. } => "reorder_roles",
            Self::ReorderChannels { .. } => "reorder_channels",
            Self::CreateDirectMessageChannel { .. } => "create_direct_message_channel",
            Self::ListScheduledEvents { .. } => "list_scheduled_events",
            Self::CreateScheduledEvent { .. } => "create_scheduled_event",
            Self::EditScheduledEvent { .. } => "edit_scheduled_event",
            Self::DeleteScheduledEvent { .. } => "delete_scheduled_event",
            Self::ListEmojis { .. } => "list_emojis",
            Self::CreateEmoji { .. } => "create_emoji",
            Self::DeleteEmoji { .. } => "delete_emoji",
            Self::CreateWebhook { .. } => "create_webhook",
            Self::ListWebhooks { .. } => "list_webhooks",
            Self::DeleteWebhook { .. } => "delete_webhook",
            Self::ExecuteWebhook { .. } => "execute_webhook",
            Self::CreateInvite { .. } => "create_invite",
            Self::ListInvites { .. } => "list_invites",
            Self::DeleteInvite { .. } => "delete_invite",
            Self::CreateThread { .. } => "create_thread",
            Self::EditThread { .. } => "edit_thread",
            Self::JoinThread { .. } => "join_thread",
            Self::LeaveThread { .. } => "leave_thread",
            Self::AddThreadMember { .. } => "add_thread_member",
            Self::RemoveThreadMember { .. } => "remove_thread_member",
            Self::ListThreadMembers { .. } => "list_thread_members",
            Self::SetVoiceDeafen { .. } => "set_voice_deafen",
            Self::MoveMember { .. } => "move_member",
            Self::DisconnectMember { .. } => "disconnect_member",
            Self::GetMessage { .. } => "get_message",
            Self::GetMessages { .. } => "get_messages",
            Self::GetPins { .. } => "get_pins",
            Self::PinMessage { .. } => "pin_message",
            Self::UnpinMessage { .. } => "unpin_message",
            Self::CrosspostMessage { .. } => "crosspost_message",
            Self::RemoveReaction { .. } => "remove_reaction",
            Self::RemoveAllReactions { .. } => "remove_all_reactions",
            Self::GetReactionUsers { .. } => "get_reaction_users",
            Self::SendMessage { .. } => "send_message",
            Self::SendDirectMessage { .. } => "send_direct_message",
            Self::EditMessage { .. } => "edit_message",
            Self::DeleteMessage { .. } => "delete_message",
            Self::BulkDeleteMessages { .. } => "bulk_delete_messages",
            Self::AddReaction { .. } => "add_reaction",
            Self::GetGuild { .. } => "get_guild",
            Self::GetMember { .. } => "get_member",
            Self::Kick { .. } => "kick",
            Self::Ban { .. } => "ban",
            Self::Unban { .. } => "unban",
            Self::Timeout { .. } => "timeout",
            Self::SetVoiceMute { .. } => "set_voice_mute",
            Self::SetNickname { .. } => "set_nickname",
            Self::AddRole { .. } => "add_role",
            Self::RemoveRole { .. } => "remove_role",
            Self::ListBans { .. } => "list_bans",
            Self::GetChannel { .. } => "get_channel",
            Self::CreateChannel { .. } => "create_channel",
            Self::EditChannel { .. } => "edit_channel",
            Self::DeleteChannel { .. } => "delete_channel",
            Self::SetPermissionOverwrite { .. } => "set_permission_overwrite",
            Self::DeletePermissionOverwrite { .. } => "delete_permission_overwrite",
            Self::ListRoles { .. } => "list_roles",
            Self::CreateRole { .. } => "create_role",
            Self::EditRole { .. } => "edit_role",
            Self::DeleteRole { .. } => "delete_role",
            Self::ListAutoModRules { .. } => "list_auto_mod_rules",
            Self::CreateAutoModRule { .. } => "create_auto_mod_rule",
            Self::DeleteAutoModRule { .. } => "delete_auto_mod_rule",
            Self::GetAuditLog { .. } => "get_audit_log",
            Self::Respond { .. } => "respond",
            Self::Followup { .. } => "followup",
            Self::EditResponse { .. } => "edit_response",
        }
    }

    /// Rejects actions Discord would refuse anyway, before any request is made.
    // One exhaustive match: a new `Action` must decide how it is checked.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), DiscordError> {
        match self {
            Self::GetMessages { .. }
            | Self::GetReactionUsers { .. }
            | Self::RemoveReaction { .. }
            | Self::RemoveAllReactions { .. }
            | Self::CreateThread { .. }
            | Self::EditThread { .. }
            | Self::ListThreadMembers { .. }
            | Self::CreateInvite { .. }
            | Self::DeleteInvite { .. }
            | Self::CreateWebhook { .. }
            | Self::ExecuteWebhook { .. }
            | Self::SearchMembers { .. }
            | Self::ListMembers { .. }
            | Self::PruneCount { .. }
            | Self::Prune { .. }
            | Self::EditGuild { .. }
            | Self::ReorderRoles { .. }
            | Self::ReorderChannels { .. }
            | Self::CreateScheduledEvent { .. }
            | Self::EditScheduledEvent { .. }
            | Self::CreateEmoji { .. } => check_new_limits(self),
            Self::SendMessage {
                content, embeds, ..
            }
            | Self::SendDirectMessage {
                content, embeds, ..
            } => check_new_message(content.as_deref(), embeds),
            Self::EditMessage {
                content, embeds, ..
            } => check_message_body(content.as_deref(), embeds.as_deref().unwrap_or_default()),
            Self::BulkDeleteMessages { message_ids, .. } => {
                ensure((2..=MAX_BULK_DELETE).contains(&message_ids.len()), || {
                    format!(
                        "bulk delete needs 2-{MAX_BULK_DELETE} messages, got {}",
                        message_ids.len()
                    )
                })?;
                let unique: BTreeSet<_> = message_ids.iter().collect();
                ensure(unique.len() == message_ids.len(), || {
                    "bulk delete message ids must be unique".into()
                })
            }
            Self::Ban {
                delete_message_seconds,
                ..
            } => ensure(*delete_message_seconds <= MAX_BAN_DELETE_SECONDS, || {
                format!("ban message deletion is at most {MAX_BAN_DELETE_SECONDS} seconds (7 days)")
            }),
            Self::Timeout {
                duration_seconds, ..
            } => duration_seconds.map_or(Ok(()), check_timeout_seconds),
            Self::SetNickname { nickname, .. } => nickname.as_deref().map_or(Ok(()), |nick| {
                check_len("nickname", nick, 1, MAX_NICKNAME_CHARS)
            }),
            Self::ListBans { limit, .. } => ensure((1..=1000).contains(limit), || {
                "list_bans limit must be 1-1000".into()
            }),
            Self::CreateChannel { spec, .. } => {
                check_len("channel name", &spec.name, 1, 100)?;
                if let Some(topic) = &spec.topic {
                    check_len("channel topic", topic, 0, 1024)?;
                }
                ensure(spec.slowmode_seconds <= MAX_SLOWMODE_SECONDS, || {
                    format!("slowmode is at most {MAX_SLOWMODE_SECONDS} seconds")
                })
            }
            Self::EditChannel { patch, .. } => {
                if let Some(name) = &patch.name {
                    check_len("channel name", name, 1, 100)?;
                }
                if let Some(topic) = &patch.topic {
                    check_len("channel topic", topic, 0, 1024)?;
                }
                ensure(
                    patch
                        .slowmode_seconds
                        .is_none_or(|s| s <= MAX_SLOWMODE_SECONDS),
                    || format!("slowmode is at most {MAX_SLOWMODE_SECONDS} seconds"),
                )
            }
            Self::CreateRole { spec, .. } => check_len("role name", &spec.name, 1, 100),
            Self::EditRole { patch, .. } => patch
                .name
                .as_deref()
                .map_or(Ok(()), |name| check_len("role name", name, 1, 100)),
            Self::CreateAutoModRule { spec, .. } => check_auto_mod(spec),
            Self::GetAuditLog { query, .. } => ensure(
                query.limit.is_none_or(|limit| (1..=100).contains(&limit)),
                || "audit log limit must be 1-100".into(),
            ),
            Self::Respond { response, .. } => match response {
                InteractionResponse::Defer { .. } | InteractionResponse::DeferUpdate => Ok(()),
                InteractionResponse::Autocomplete { choices } => check_choices(choices),
                InteractionResponse::Message { reply } => check_reply(reply, true),
                InteractionResponse::Update { reply } => check_reply(reply, false),
                InteractionResponse::Modal(spec) => check_modal(spec),
            },
            Self::Followup { reply, .. } => check_reply(reply, true),
            Self::EditResponse { reply, .. } => check_reply(reply, false),
            Self::AddReaction { .. }
            | Self::DeleteMessage { .. }
            | Self::GetGuild { .. }
            | Self::GetMember { .. }
            | Self::Kick { .. }
            | Self::Unban { .. }
            | Self::SetVoiceMute { .. }
            | Self::AddRole { .. }
            | Self::RemoveRole { .. }
            | Self::GetChannel { .. }
            | Self::DeleteChannel { .. }
            | Self::SetPermissionOverwrite { .. }
            | Self::DeletePermissionOverwrite { .. }
            | Self::ListRoles { .. }
            | Self::DeleteRole { .. }
            | Self::ListAutoModRules { .. }
            | Self::DeleteAutoModRule { .. }
            | Self::SetVoiceDeafen { .. }
            | Self::MoveMember { .. }
            | Self::DisconnectMember { .. }
            | Self::GetMessage { .. }
            | Self::GetPins { .. }
            | Self::PinMessage { .. }
            | Self::UnpinMessage { .. }
            | Self::CrosspostMessage { .. }
            | Self::JoinThread { .. }
            | Self::LeaveThread { .. }
            | Self::AddThreadMember { .. }
            | Self::RemoveThreadMember { .. }
            | Self::ListInvites { .. }
            | Self::ListWebhooks { .. }
            | Self::DeleteWebhook { .. }
            | Self::CreateDirectMessageChannel { .. }
            | Self::ListScheduledEvents { .. }
            | Self::DeleteScheduledEvent { .. }
            | Self::ListEmojis { .. }
            | Self::DeleteEmoji { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason() -> Reason {
        Reason::new("spam").unwrap()
    }

    fn id<const N: u64, M>() -> Id<M> {
        Id::new(N)
    }

    #[test]
    fn reason_rejects_empty_and_overlong() {
        assert!(Reason::new("   ").is_err());
        assert!(Reason::new("x".repeat(513)).is_err());
        assert_eq!(Reason::new("  spam ").unwrap().as_str(), "spam");
    }

    #[test]
    fn reason_counts_characters_not_bytes() {
        assert!(Reason::new("あ".repeat(512)).is_ok());
        assert!(Reason::new("あ".repeat(513)).is_err());
    }

    #[test]
    fn truncated_reason_fits_and_marks_the_cut() {
        let reason = Reason::truncated(&"a".repeat(2000));
        assert_eq!(reason.as_str().chars().count(), Reason::MAX_CHARS);
        assert!(reason.as_str().ends_with('…'));
        assert_eq!(Reason::truncated("  ").as_str(), "no reason given");
    }

    #[test]
    fn deserialising_an_invalid_reason_fails() {
        assert!(serde_json::from_str::<Reason>("\"\"").is_err());
    }

    #[test]
    fn ban_window_is_bounded_to_seven_days() {
        let mut action = Action::Ban {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            delete_message_seconds: MAX_BAN_DELETE_SECONDS,
            reason: reason(),
        };
        assert!(action.validate().is_ok());
        if let Action::Ban {
            delete_message_seconds,
            ..
        } = &mut action
        {
            *delete_message_seconds += 1;
        }
        assert!(matches!(action.validate(), Err(DiscordError::Invalid(_))));
    }

    #[test]
    fn timeout_is_bounded_to_twenty_eight_days_and_none_clears() {
        let make = |duration_seconds| Action::Timeout {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            duration_seconds,
            reason: reason(),
        };
        assert!(make(None).validate().is_ok());
        assert!(make(Some(MAX_TIMEOUT_SECONDS)).validate().is_ok());
        assert!(make(Some(0)).validate().is_err());
        assert!(make(Some(MAX_TIMEOUT_SECONDS + 1)).validate().is_err());
    }

    #[test]
    fn voice_mute_needs_mute_members_and_names_its_guild() {
        let action = Action::SetVoiceMute {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            mute: true,
            reason: reason(),
        };
        assert_eq!(action.capability(), Capability::MuteMembers);
        assert_eq!(action.guild_id(), Some(id::<7, _>()));
        assert_eq!(action.name(), "set_voice_mute");
        assert!(action.channels().is_empty());
        assert!(action.validate().is_ok());
    }

    #[test]
    fn voice_mute_round_trips_through_json() {
        let action = Action::SetVoiceMute {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            mute: false,
            reason: reason(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"type\":\"set_voice_mute\""));
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }

    #[test]
    fn bulk_delete_needs_two_to_one_hundred_unique_ids() {
        let make = |ids: Vec<u64>| Action::BulkDeleteMessages {
            channel_id: id::<1, _>(),
            message_ids: ids.into_iter().map(Id::new).collect(),
            reason: reason(),
        };
        assert!(make(vec![1]).validate().is_err());
        assert!(make(vec![1, 1]).validate().is_err());
        assert!(make(vec![1, 2]).validate().is_ok());
        assert!(make((1..=101).collect()).validate().is_err());
    }

    #[test]
    fn empty_message_is_rejected_and_default_mentions_are_none() {
        let action = Action::SendMessage {
            channel_id: id::<1, _>(),
            content: Some(String::new()),
            embeds: vec![],
            reply_to: None,
            mentions: MentionPolicy::default(),
        };
        assert!(action.validate().is_err());
        assert_eq!(MentionPolicy::default(), MentionPolicy::None);
    }

    #[test]
    fn message_length_counts_characters() {
        let make = |content: String| Action::SendMessage {
            channel_id: id::<1, _>(),
            content: Some(content),
            embeds: vec![],
            reply_to: None,
            mentions: MentionPolicy::None,
        };
        assert!(make("あ".repeat(MAX_MESSAGE_CHARS)).validate().is_ok());
        assert!(make("あ".repeat(MAX_MESSAGE_CHARS + 1)).validate().is_err());
    }

    #[test]
    fn guild_scoped_actions_name_their_guild_and_channel_actions_do_not() {
        let ban = Action::Ban {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            delete_message_seconds: 0,
            reason: reason(),
        };
        assert_eq!(ban.guild_id(), Some(id::<7, _>()));
        let delete = Action::DeleteMessage {
            channel_id: id::<1, _>(),
            message_id: id::<2, _>(),
            reason: reason(),
        };
        assert_eq!(delete.guild_id(), None);
    }

    #[test]
    fn every_action_maps_to_a_capability_and_name() {
        let action = Action::Kick {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            reason: reason(),
        };
        assert_eq!(action.capability(), Capability::KickMembers);
        assert_eq!(action.name(), "kick");
    }

    #[test]
    fn action_round_trips_through_json() {
        let action = Action::Unban {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            reason: reason(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"type\":\"unban\""));
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }

    fn button_with(custom_id: &str, label: &str) -> Component {
        Component::Button(Button {
            id: None,
            custom_id: Some(custom_id.into()),
            disabled: false,
            emoji: None,
            label: Some(label.into()),
            style: ButtonStyle::Primary,
            url: None,
            sku_id: None,
        })
    }

    fn row_of(components: Vec<Component>) -> Component {
        Component::ActionRow(twilight_model::channel::message::component::ActionRow {
            id: None,
            components,
        })
    }

    #[test]
    fn components_must_fit_what_discord_accepts() {
        let ok = vec![row_of(vec![
            button_with("m:a", "A"),
            button_with("m:b", "B"),
        ])];
        assert!(check_components(&ok).is_ok());
        assert!(check_components(&[]).is_ok());

        // Too many rows, too many buttons, nothing in a row, a button outside a row.
        assert!(check_components(&vec![row_of(vec![button_with("m:a", "A")]); 6]).is_err());
        let six = (0..6)
            .map(|n| button_with(&format!("m:{n}"), "x"))
            .collect();
        assert!(check_components(&[row_of(six)]).is_err());
        assert!(check_components(&[row_of(vec![])]).is_err());
        assert!(check_components(&[button_with("m:a", "A")]).is_err());
        // Ids and labels that are empty or too long, a link without https.
        assert!(check_components(&[row_of(vec![button_with("", "A")])]).is_err());
        assert!(check_components(&[row_of(vec![button_with(&"m".repeat(101), "A")])]).is_err());
        assert!(check_components(&[row_of(vec![button_with("m:a", &"l".repeat(81))])]).is_err());
        let link = |url: &str| {
            Component::Button(Button {
                id: None,
                custom_id: None,
                disabled: false,
                emoji: None,
                label: Some("site".into()),
                style: ButtonStyle::Link,
                url: Some(url.into()),
                sku_id: None,
            })
        };
        assert!(check_components(&[row_of(vec![link("https://example.org")])]).is_ok());
        assert!(check_components(&[row_of(vec![link("http://example.org")])]).is_err());
    }

    #[test]
    fn select_menus_and_modals_are_checked_too() {
        use twilight_model::channel::message::component::{
            SelectMenuOption, TextInput, TextInputStyle,
        };

        let option = |n: usize| SelectMenuOption {
            default: false,
            description: None,
            emoji: None,
            label: format!("o{n}"),
            value: format!("v{n}"),
        };
        let menu = |options: usize, min: u8, max: u8| {
            row_of(vec![Component::SelectMenu(SelectMenu {
                id: None,
                channel_types: None,
                custom_id: "m:pick".into(),
                default_values: None,
                disabled: false,
                kind: SelectMenuType::Text,
                max_values: Some(max),
                min_values: Some(min),
                options: Some((0..options).map(option).collect()),
                placeholder: None,
                required: None,
            })])
        };
        assert!(check_components(&[menu(3, 1, 2)]).is_ok());
        assert!(check_components(&[menu(0, 1, 1)]).is_err());
        assert!(check_components(&[menu(26, 1, 1)]).is_err());
        assert!(check_components(&[menu(3, 2, 1)]).is_err());
        assert!(check_components(&[menu(3, 1, 4)]).is_err());

        #[allow(deprecated)]
        let input = |id: &str, label: &str| {
            row_of(vec![Component::TextInput(TextInput {
                id: None,
                custom_id: id.into(),
                label: Some(label.into()),
                max_length: Some(100),
                min_length: None,
                placeholder: None,
                required: Some(true),
                style: TextInputStyle::Short,
                value: None,
            })])
        };
        let modal = |title: &str, inputs: Vec<Component>| ModalSpec {
            custom_id: "m:form".into(),
            title: title.into(),
            components: inputs,
        };
        assert!(check_modal(&modal("Report", vec![input("m:why", "Why")])).is_ok());
        assert!(check_modal(&modal("", vec![input("m:why", "Why")])).is_err());
        assert!(check_modal(&modal(&"t".repeat(46), vec![input("m:why", "Why")])).is_err());
        assert!(check_modal(&modal("Report", vec![])).is_err());
        assert!(check_modal(&modal("Report", vec![input("", "Why")])).is_err());
        assert!(check_modal(&modal("Report", vec![button_with("m:a", "A")])).is_err());
    }

    #[test]
    fn autocomplete_choices_are_bounded_like_discord_bounds_them() {
        use crate::module::{ChoiceValue, OptionChoice};
        let respond = |choices: Vec<OptionChoice>| Action::Respond {
            interaction_id: Id::new(1),
            token: InteractionToken::new("t"),
            response: InteractionResponse::Autocomplete { choices },
        };
        let choice = |name: &str| OptionChoice {
            name: name.into(),
            value: ChoiceValue::String(name.into()),
        };
        assert!(respond(Vec::new()).validate().is_ok());
        assert!(
            respond((0..25).map(|n| choice(&n.to_string())).collect())
                .validate()
                .is_ok()
        );
        assert!(
            respond((0..26).map(|n| choice(&n.to_string())).collect())
                .validate()
                .is_err()
        );
        assert!(
            respond(vec![choice(&"x".repeat(101))]).validate().is_err(),
            "name over 100"
        );
        assert!(respond(vec![choice("")]).validate().is_err(), "empty name");
        let long_value = OptionChoice {
            name: "a".into(),
            value: ChoiceValue::String("v".repeat(101)),
        };
        assert!(
            respond(vec![long_value]).validate().is_err(),
            "value over 100"
        );
    }

    #[test]
    fn the_custom_ids_of_an_action_are_all_found() {
        let respond = |response| Action::Respond {
            interaction_id: Id::new(1),
            token: InteractionToken::new("t"),
            response,
        };
        let reply = InteractionReply {
            components: vec![row_of(vec![
                button_with("m:a", "A"),
                button_with("m:b", "B"),
            ])],
            ..InteractionReply::default()
        };
        assert_eq!(
            respond(InteractionResponse::Message {
                reply: reply.clone()
            })
            .custom_ids(),
            ["m:a", "m:b"]
        );
        assert_eq!(
            respond(InteractionResponse::Update {
                reply: reply.clone()
            })
            .custom_ids(),
            ["m:a", "m:b"]
        );
        assert_eq!(
            Action::Followup {
                token: InteractionToken::new("t"),
                reply
            }
            .custom_ids(),
            ["m:a", "m:b"]
        );
        assert!(
            respond(InteractionResponse::DeferUpdate)
                .custom_ids()
                .is_empty()
        );
        assert!(
            respond(InteractionResponse::Defer { ephemeral: true })
                .custom_ids()
                .is_empty()
        );
    }

    #[test]
    fn auto_mod_keyword_rule_needs_a_pattern_and_an_action() {
        let mut spec = NewAutoModRule {
            name: "words".into(),
            trigger: AutoModTrigger::Keyword {
                keywords: vec![],
                regex_patterns: vec![],
                allow_list: vec![],
            },
            actions: vec![AutoModAction::BlockMessage {
                custom_message: None,
            }],
            enabled: true,
            exempt_roles: vec![],
            exempt_channels: vec![],
        };
        assert!(check_auto_mod(&spec).is_err());
        spec.trigger = AutoModTrigger::Keyword {
            keywords: vec!["bad".into()],
            regex_patterns: vec![],
            allow_list: vec![],
        };
        assert!(check_auto_mod(&spec).is_ok());
        spec.actions.clear();
        assert!(check_auto_mod(&spec).is_err());
    }

    #[test]
    fn direct_messages_need_their_own_capability_and_name_their_guild() {
        let action = Action::SendDirectMessage {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            content: Some("hi".into()),
            embeds: vec![],
        };
        assert_eq!(action.capability(), Capability::SendDirectMessages);
        assert_eq!(action.guild_id(), Some(id::<7, _>()));
        assert_eq!(action.name(), "send_direct_message");
        assert!(action.channels().is_empty());
        assert!(action.validate().is_ok());
    }

    fn channel_history(limit: u16) -> Action {
        Action::GetMessages {
            channel_id: id::<1, _>(),
            cursor: Some(MessageCursor::Before(id::<5, _>())),
            limit,
        }
    }

    #[test]
    fn reading_history_needs_read_messages_and_names_its_channel() {
        let action = channel_history(50);
        assert_eq!(action.capability(), Capability::ReadMessages);
        assert_eq!(action.guild_id(), None);
        assert_eq!(action.channels(), vec![id::<1, _>()]);
        assert_eq!(action.name(), "get_messages");
        assert!(action.interaction_token().is_none());
        assert!(action.validate().is_ok());
    }

    #[test]
    fn history_and_reaction_user_limits_are_one_to_one_hundred() {
        for limit in [0, 101] {
            assert!(channel_history(limit).validate().is_err(), "{limit}");
            let users = Action::GetReactionUsers {
                channel_id: id::<1, _>(),
                message_id: id::<2, _>(),
                emoji: Emoji::Unicode {
                    value: "👍".into()
                },
                after: None,
                limit,
            };
            assert!(users.validate().is_err(), "{limit}");
        }
        assert!(channel_history(1).validate().is_ok());
        assert!(channel_history(100).validate().is_ok());
    }

    #[test]
    fn pins_and_reaction_removals_are_moderation_actions() {
        let pin = Action::PinMessage {
            channel_id: id::<1, _>(),
            message_id: id::<2, _>(),
            reason: reason(),
        };
        assert_eq!(pin.capability(), Capability::ManageMessages);
        assert_eq!(pin.name(), "pin_message");
        let remove = Action::RemoveReaction {
            channel_id: id::<1, _>(),
            message_id: id::<2, _>(),
            emoji: Emoji::Unicode {
                value: "👍".into()
            },
            user_id: id::<3, _>(),
        };
        assert_eq!(remove.capability(), Capability::ManageMessages);
        assert_eq!(remove.channels(), vec![id::<1, _>()]);
    }

    #[test]
    fn an_empty_unicode_emoji_is_rejected_before_discord() {
        let action = Action::RemoveReaction {
            channel_id: id::<1, _>(),
            message_id: id::<2, _>(),
            emoji: Emoji::Unicode {
                value: String::new(),
            },
            user_id: id::<3, _>(),
        };
        assert!(matches!(action.validate(), Err(DiscordError::Invalid(_))));
    }

    #[test]
    fn message_actions_round_trip_through_json() {
        let actions = [
            channel_history(10),
            Action::GetMessages {
                channel_id: id::<1, _>(),
                cursor: Some(MessageCursor::Around(id::<5, _>())),
                limit: 100,
            },
            Action::RemoveAllReactions {
                channel_id: id::<1, _>(),
                message_id: id::<2, _>(),
                emoji: None,
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    #[test]
    fn voice_moderation_names_its_guild_and_capability() {
        let deafen = Action::SetVoiceDeafen {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            deaf: true,
            reason: reason(),
        };
        assert_eq!(deafen.capability(), Capability::DeafenMembers);
        assert_eq!(deafen.guild_id(), Some(id::<7, _>()));
        assert!(deafen.channels().is_empty());
        assert_eq!(deafen.name(), "set_voice_deafen");

        let disconnect = Action::DisconnectMember {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            reason: reason(),
        };
        assert_eq!(disconnect.capability(), Capability::MoveMembers);
        assert_eq!(disconnect.guild_id(), Some(id::<7, _>()));
        assert!(disconnect.channels().is_empty());
        assert_eq!(disconnect.name(), "disconnect_member");
    }

    #[test]
    fn moving_a_member_names_the_channel_it_moves_into() {
        let action = Action::MoveMember {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
            channel_id: id::<30, _>(),
            reason: reason(),
        };
        assert_eq!(action.capability(), Capability::MoveMembers);
        assert_eq!(action.guild_id(), Some(id::<7, _>()));
        assert_eq!(action.channels(), vec![id::<30, _>()]);
        assert_eq!(action.name(), "move_member");
        assert!(action.validate().is_ok());
    }

    #[test]
    fn voice_moderation_round_trips_through_json() {
        let actions = [
            Action::SetVoiceDeafen {
                guild_id: id::<1, _>(),
                user_id: id::<2, _>(),
                deaf: false,
                reason: reason(),
            },
            Action::MoveMember {
                guild_id: id::<1, _>(),
                user_id: id::<2, _>(),
                channel_id: id::<3, _>(),
                reason: reason(),
            },
            Action::DisconnectMember {
                guild_id: id::<1, _>(),
                user_id: id::<2, _>(),
                reason: reason(),
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    fn new_thread(minutes: u16) -> Action {
        Action::CreateThread {
            channel_id: id::<1, _>(),
            name: "help".into(),
            source: None,
            kind: ThreadKind::Public,
            auto_archive_minutes: minutes,
        }
    }

    #[test]
    fn a_thread_is_created_in_its_channel_with_an_allowed_archive_time() {
        let action = new_thread(1440);
        assert_eq!(action.capability(), Capability::ManageThreads);
        assert_eq!(action.channels(), vec![id::<1, _>()]);
        assert_eq!(action.guild_id(), None);
        assert_eq!(action.name(), "create_thread");
        assert!(action.validate().is_ok());
        for minutes in [60, 1440, 4320, 10080] {
            assert!(new_thread(minutes).validate().is_ok(), "{minutes}");
        }
        for minutes in [0, 30, 1441, 20160] {
            assert!(new_thread(minutes).validate().is_err(), "{minutes}");
        }
    }

    #[test]
    fn a_thread_started_from_a_message_is_public_and_its_name_is_bounded() {
        let from_message = |kind, name: &str| Action::CreateThread {
            channel_id: id::<1, _>(),
            name: name.into(),
            source: Some(id::<5, _>()),
            kind,
            auto_archive_minutes: 60,
        };
        assert!(from_message(ThreadKind::Public, "help").validate().is_ok());
        assert!(
            from_message(ThreadKind::Private, "help")
                .validate()
                .is_err()
        );
        assert!(from_message(ThreadKind::Public, "").validate().is_err());
        assert!(
            from_message(ThreadKind::Public, &"t".repeat(101))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn thread_edits_are_bounded_and_name_their_thread() {
        let edit = |slowmode_seconds| Action::EditThread {
            thread_id: id::<40, _>(),
            patch: ThreadPatch {
                name: Some("renamed".into()),
                archived: Some(true),
                locked: None,
                slowmode_seconds,
            },
            reason: reason(),
        };
        let ok = edit(Some(MAX_SLOWMODE_SECONDS));
        assert_eq!(ok.capability(), Capability::ManageThreads);
        assert_eq!(ok.channels(), vec![id::<40, _>()]);
        assert!(ok.validate().is_ok());
        assert!(edit(Some(MAX_SLOWMODE_SECONDS + 1)).validate().is_err());
    }

    #[test]
    fn thread_membership_needs_the_matching_capability() {
        let thread = id::<40, _>();
        let cases = [
            (
                Action::JoinThread { thread_id: thread },
                Capability::SendMessages,
            ),
            (
                Action::LeaveThread { thread_id: thread },
                Capability::SendMessages,
            ),
            (
                Action::AddThreadMember {
                    thread_id: thread,
                    user_id: id::<2, _>(),
                },
                Capability::ManageThreads,
            ),
            (
                Action::RemoveThreadMember {
                    thread_id: thread,
                    user_id: id::<2, _>(),
                },
                Capability::ManageThreads,
            ),
            (
                Action::ListThreadMembers {
                    thread_id: thread,
                    after: None,
                    limit: 100,
                },
                Capability::ReadMessages,
            ),
        ];
        for (action, capability) in cases {
            assert_eq!(action.capability(), capability, "{}", action.name());
            assert_eq!(action.channels(), vec![thread], "{}", action.name());
            assert_eq!(action.guild_id(), None, "{}", action.name());
            assert!(action.validate().is_ok(), "{}", action.name());
        }
    }

    #[test]
    fn thread_member_listing_is_limited_to_one_hundred() {
        let list = |limit| Action::ListThreadMembers {
            thread_id: id::<40, _>(),
            after: None,
            limit,
        };
        assert!(list(0).validate().is_err());
        assert!(list(100).validate().is_ok());
        assert!(list(101).validate().is_err());
    }

    #[test]
    fn thread_actions_round_trip_through_json() {
        let actions = [
            new_thread(10080),
            Action::EditThread {
                thread_id: id::<40, _>(),
                patch: ThreadPatch::default(),
                reason: reason(),
            },
            Action::JoinThread {
                thread_id: id::<40, _>(),
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    fn new_invite(max_age_seconds: u32, max_uses: u16) -> Action {
        Action::CreateInvite {
            channel_id: id::<1, _>(),
            max_age_seconds,
            max_uses,
            temporary: false,
            unique: true,
            reason: reason(),
        }
    }

    #[test]
    fn an_invite_is_created_in_its_channel_within_discord_limits() {
        let action = new_invite(3600, 5);
        assert_eq!(action.capability(), Capability::ManageInvites);
        assert_eq!(action.channels(), vec![id::<1, _>()]);
        assert_eq!(action.guild_id(), None);
        assert_eq!(action.name(), "create_invite");
        assert!(action.validate().is_ok());
        // Zero means never expires, zero uses means no limit.
        assert!(new_invite(0, 0).validate().is_ok());
        assert!(new_invite(604_800, 100).validate().is_ok());
        assert!(new_invite(604_801, 5).validate().is_err());
        assert!(new_invite(3600, 101).validate().is_err());
    }

    #[test]
    fn invites_are_listed_and_deleted_per_guild_by_a_safe_code() {
        let list = Action::ListInvites {
            guild_id: id::<7, _>(),
        };
        assert_eq!(list.capability(), Capability::ManageInvites);
        assert_eq!(list.guild_id(), Some(id::<7, _>()));
        assert!(list.channels().is_empty());

        let delete = |code: &str| Action::DeleteInvite {
            guild_id: id::<7, _>(),
            code: code.into(),
            reason: reason(),
        };
        assert_eq!(delete("aB3-_9x").capability(), Capability::ManageInvites);
        assert_eq!(delete("aB3-_9x").guild_id(), Some(id::<7, _>()));
        assert!(delete("aB3-_9x").validate().is_ok());
        // A code goes into a URL path: separators and empty codes are refused.
        for code in ["", "a/b", "a?b", "a b", &"x".repeat(65)] {
            assert!(delete(code).validate().is_err(), "{code:?}");
        }
    }

    fn webhook_post(content: Option<&str>, username: Option<&str>) -> Action {
        Action::ExecuteWebhook {
            guild_id: id::<7, _>(),
            webhook_id: id::<8, _>(),
            token: WebhookToken::new("tok-secret"),
            content: content.map(Into::into),
            embeds: vec![],
            username: username.map(Into::into),
            avatar_url: None,
            mentions: MentionPolicy::None,
        }
    }

    #[test]
    fn webhooks_are_managed_per_channel_and_deleted_per_guild() {
        let create = |name: &str| Action::CreateWebhook {
            channel_id: id::<1, _>(),
            name: name.into(),
            reason: reason(),
        };
        assert_eq!(create("bridge").capability(), Capability::ManageWebhooks);
        assert_eq!(create("bridge").channels(), vec![id::<1, _>()]);
        assert_eq!(create("bridge").name(), "create_webhook");
        assert!(create("bridge").validate().is_ok());
        assert!(create("").validate().is_err());
        assert!(create(&"w".repeat(80)).validate().is_ok());
        assert!(create(&"w".repeat(81)).validate().is_err());

        let list = Action::ListWebhooks {
            channel_id: id::<1, _>(),
        };
        assert_eq!(list.capability(), Capability::ManageWebhooks);
        assert_eq!(list.channels(), vec![id::<1, _>()]);

        let delete = Action::DeleteWebhook {
            guild_id: id::<7, _>(),
            webhook_id: id::<8, _>(),
            reason: reason(),
        };
        assert_eq!(delete.capability(), Capability::ManageWebhooks);
        assert_eq!(delete.guild_id(), Some(id::<7, _>()));
        assert!(delete.channels().is_empty());
    }

    #[test]
    fn a_webhook_post_is_a_message_and_its_token_never_shows_in_debug_output() {
        let post = webhook_post(Some("hello"), Some("relay"));
        assert_eq!(post.capability(), Capability::SendMessages);
        assert_eq!(post.guild_id(), Some(id::<7, _>()));
        assert!(post.channels().is_empty());
        assert_eq!(post.name(), "execute_webhook");
        assert!(post.validate().is_ok());
        assert!(!format!("{post:?}").contains("tok-secret"));
        assert!(!format!("{:?}", WebhookToken::new("tok-secret")).contains("tok-secret"));
        assert_eq!(WebhookToken::new("tok-secret").expose(), "tok-secret");
    }

    #[test]
    fn a_webhook_post_needs_a_body_and_bounded_names_and_avatars() {
        assert!(webhook_post(None, None).validate().is_err());
        assert!(webhook_post(Some(""), None).validate().is_err());
        assert!(
            webhook_post(Some("hi"), Some(&"n".repeat(81)))
                .validate()
                .is_err()
        );
        assert!(webhook_post(Some("hi"), Some("")).validate().is_err());
        let with_avatar = |avatar_url: &str| Action::ExecuteWebhook {
            guild_id: id::<7, _>(),
            webhook_id: id::<8, _>(),
            token: WebhookToken::new("t"),
            content: Some("hi".into()),
            embeds: vec![],
            username: None,
            avatar_url: Some(avatar_url.into()),
            mentions: MentionPolicy::None,
        };
        assert!(with_avatar("https://example.org/a.png").validate().is_ok());
        assert!(with_avatar("http://example.org/a.png").validate().is_err());
    }

    #[test]
    fn webhook_actions_round_trip_through_json() {
        let action = webhook_post(Some("hi"), None);
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }

    #[test]
    fn member_searches_and_listings_are_read_and_bounded() {
        let search = |query: &str, limit| Action::SearchMembers {
            guild_id: id::<7, _>(),
            query: query.into(),
            limit,
        };
        assert_eq!(search("ana", 10).capability(), Capability::ReadGuild);
        assert_eq!(search("ana", 10).guild_id(), Some(id::<7, _>()));
        assert!(search("ana", 10).validate().is_ok());
        assert!(search("", 10).validate().is_err());
        assert!(search("ana", 0).validate().is_err());
        assert!(search("ana", 1000).validate().is_ok());
        assert!(search("ana", 1001).validate().is_err());

        let list = Action::ListMembers {
            guild_id: id::<7, _>(),
            after: Some(id::<3, _>()),
            limit: 1000,
        };
        assert_eq!(list.name(), "list_members");
        assert!(list.validate().is_ok());
        let too_many = Action::ListMembers {
            guild_id: id::<7, _>(),
            after: None,
            limit: 1001,
        };
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn prunes_are_bounded_to_thirty_days_and_count_as_kicks() {
        let prune = |days| Action::PruneCount {
            guild_id: id::<7, _>(),
            days,
            include_roles: vec![],
        };
        assert_eq!(prune(7).capability(), Capability::KickMembers);
        assert!(prune(0).validate().is_err());
        assert!(prune(30).validate().is_ok());
        assert!(prune(31).validate().is_err());
        let real = Action::Prune {
            guild_id: id::<7, _>(),
            days: 7,
            include_roles: vec![],
            reason: reason(),
        };
        assert_eq!(real.capability(), Capability::KickMembers);
        assert_eq!(real.name(), "prune");
    }

    #[test]
    fn editing_the_guild_needs_manage_guild_and_a_valid_name() {
        let edit = |name: Option<&str>| Action::EditGuild {
            guild_id: id::<7, _>(),
            patch: GuildPatch {
                name: name.map(Into::into),
                verification_level: None,
            },
            reason: reason(),
        };
        assert_eq!(edit(Some("New")).capability(), Capability::ManageGuild);
        assert!(edit(None).validate().is_ok());
        assert!(edit(Some("N")).validate().is_err());
        assert!(edit(Some(&"n".repeat(101))).validate().is_err());
    }

    #[test]
    fn reordering_names_its_targets_and_refuses_duplicates() {
        let roles = Action::ReorderRoles {
            guild_id: id::<7, _>(),
            positions: vec![RolePosition {
                id: id::<10, _>(),
                position: 2,
            }],
        };
        assert_eq!(roles.capability(), Capability::ManageRoles);
        assert!(roles.validate().is_ok());
        let none = Action::ReorderRoles {
            guild_id: id::<7, _>(),
            positions: vec![],
        };
        assert!(none.validate().is_err());

        let channels = |ids: [u64; 2]| Action::ReorderChannels {
            guild_id: id::<7, _>(),
            positions: ids
                .map(|id| ChannelPosition {
                    channel_id: Id::new(id),
                    position: 0,
                })
                .to_vec(),
        };
        assert_eq!(channels([1, 2]).capability(), Capability::ManageChannels);
        assert_eq!(
            channels([1, 2]).channels(),
            vec![id::<1, _>(), id::<2, _>()]
        );
        assert!(channels([1, 2]).validate().is_ok());
        assert!(channels([1, 1]).validate().is_err());
    }

    #[test]
    fn a_direct_message_channel_needs_its_capability_and_guild() {
        let action = Action::CreateDirectMessageChannel {
            guild_id: id::<7, _>(),
            user_id: id::<2, _>(),
        };
        assert_eq!(action.capability(), Capability::SendDirectMessages);
        assert_eq!(action.guild_id(), Some(id::<7, _>()));
        assert!(action.channels().is_empty());
        assert_eq!(action.name(), "create_direct_message_channel");
    }

    #[test]
    fn the_new_guild_actions_round_trip_through_json() {
        let actions = [
            Action::ListMembers {
                guild_id: id::<1, _>(),
                after: None,
                limit: 5,
            },
            Action::EditGuild {
                guild_id: id::<1, _>(),
                patch: GuildPatch::default(),
                reason: reason(),
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    fn event_at(start: i64, end: Option<i64>, location: EventLocation) -> NewScheduledEvent {
        NewScheduledEvent {
            name: "game night".into(),
            description: None,
            start_unix: start,
            end_unix: end,
            location,
        }
    }

    #[test]
    fn scheduled_events_are_read_and_managed_per_guild() {
        let list = Action::ListScheduledEvents {
            guild_id: id::<7, _>(),
            with_user_count: true,
        };
        assert_eq!(list.capability(), Capability::ReadGuild);
        assert_eq!(list.guild_id(), Some(id::<7, _>()));
        assert!(list.validate().is_ok());

        let create = Action::CreateScheduledEvent {
            guild_id: id::<7, _>(),
            spec: event_at(
                1_800_000_000,
                None,
                EventLocation::Voice {
                    channel_id: id::<3, _>(),
                },
            ),
        };
        assert_eq!(create.capability(), Capability::ManageEvents);
        assert!(create.channels().is_empty());
        assert!(create.validate().is_ok());

        let edit = Action::EditScheduledEvent {
            guild_id: id::<7, _>(),
            event_id: id::<9, _>(),
            patch: ScheduledEventPatch {
                name: None,
                status: Some(ScheduledEventStatus::Canceled),
            },
        };
        assert_eq!(edit.capability(), Capability::ManageEvents);
        assert_eq!(edit.name(), "edit_scheduled_event");
        assert!(edit.validate().is_ok());

        let delete = Action::DeleteScheduledEvent {
            guild_id: id::<7, _>(),
            event_id: id::<9, _>(),
        };
        assert_eq!(delete.capability(), Capability::ManageEvents);
    }

    #[test]
    fn an_external_event_needs_a_place_and_an_end_after_its_start() {
        let external = |end, place: &str| Action::CreateScheduledEvent {
            guild_id: id::<7, _>(),
            spec: event_at(
                100,
                end,
                EventLocation::External {
                    place: place.into(),
                },
            ),
        };
        assert!(external(Some(200), "the square").validate().is_ok());
        assert!(external(None, "the square").validate().is_err());
        assert!(external(Some(100), "the square").validate().is_err());
        assert!(external(Some(200), "").validate().is_err());
    }

    #[test]
    fn scheduled_event_names_and_descriptions_are_bounded() {
        let mut spec = event_at(
            100,
            None,
            EventLocation::Voice {
                channel_id: id::<3, _>(),
            },
        );
        spec.name = String::new();
        let create = |spec: NewScheduledEvent| Action::CreateScheduledEvent {
            guild_id: id::<7, _>(),
            spec,
        };
        assert!(create(spec.clone()).validate().is_err());
        spec.name = "ok".into();
        spec.description = Some("d".repeat(1001));
        assert!(create(spec).validate().is_err());
    }

    #[test]
    fn emoji_are_listed_created_and_deleted_with_discord_rules() {
        assert_eq!(
            Action::ListEmojis {
                guild_id: id::<7, _>()
            }
            .capability(),
            Capability::ReadGuild
        );
        let image = "data:image/png;base64,iVBORw0KGgo=";
        let create = |name: &str, image: &str| Action::CreateEmoji {
            guild_id: id::<7, _>(),
            name: name.into(),
            image: image.into(),
            reason: reason(),
        };
        assert_eq!(
            create("party", image).capability(),
            Capability::ManageEmojis
        );
        assert!(create("party", image).validate().is_ok());
        assert!(create("p", image).validate().is_err());
        assert!(create("bad name", image).validate().is_err());
        assert!(
            create("party", "https://example.org/a.png")
                .validate()
                .is_err()
        );
        assert!(
            create("party", "data:image/bmp;base64,AAAA")
                .validate()
                .is_err()
        );
        // The decoded picture is at most 256 KiB.
        let too_big = format!("data:image/png;base64,{}", "A".repeat(350_000));
        assert!(create("party", &too_big).validate().is_err());

        let delete = Action::DeleteEmoji {
            guild_id: id::<7, _>(),
            emoji_id: id::<4, _>(),
            reason: reason(),
        };
        assert_eq!(delete.capability(), Capability::ManageEmojis);
        assert_eq!(delete.guild_id(), Some(id::<7, _>()));
    }

    #[test]
    fn the_member_guild_and_scheduled_event_actions_round_trip_through_json() {
        let actions = [
            Action::SearchMembers {
                guild_id: id::<1, _>(),
                query: "ana".into(),
                limit: 10,
            },
            Action::Prune {
                guild_id: id::<1, _>(),
                days: 7,
                include_roles: vec![id::<3, _>()],
                reason: reason(),
            },
            Action::ReorderChannels {
                guild_id: id::<1, _>(),
                positions: vec![ChannelPosition {
                    channel_id: id::<2, _>(),
                    position: 4,
                }],
            },
            Action::CreateScheduledEvent {
                guild_id: id::<1, _>(),
                spec: event_at(
                    100,
                    Some(200),
                    EventLocation::External {
                        place: "square".into(),
                    },
                ),
            },
            Action::EditScheduledEvent {
                guild_id: id::<1, _>(),
                event_id: id::<5, _>(),
                patch: ScheduledEventPatch {
                    name: None,
                    status: Some(ScheduledEventStatus::Active),
                },
            },
            Action::CreateEmoji {
                guild_id: id::<1, _>(),
                name: "party".into(),
                image: "data:image/gif;base64,R0lGOD".into(),
                reason: reason(),
            },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    #[test]
    fn direct_message_body_is_checked_like_a_message() {
        let make = |content: Option<String>| Action::SendDirectMessage {
            guild_id: id::<1, _>(),
            user_id: id::<2, _>(),
            content,
            embeds: vec![],
        };
        assert!(make(None).validate().is_err());
        assert!(make(Some(String::new())).validate().is_err());
        assert!(
            make(Some("あ".repeat(MAX_MESSAGE_CHARS)))
                .validate()
                .is_ok()
        );
        assert!(
            make(Some("あ".repeat(MAX_MESSAGE_CHARS + 1)))
                .validate()
                .is_err()
        );
    }
}
