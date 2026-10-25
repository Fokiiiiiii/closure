//! The serializable boundary between modules and Discord.
//!
//! Every Discord side effect a module can cause is one [`Action`]. A single choke point
//! ([`ApiClient`](crate::client::ApiClient)) validates the action, checks the module's
//! [`Capability`] and forwards it to a [`DiscordApi`](crate::client::DiscordApi), which keeps
//! authorisation, metrics, auditing and rate-limit handling in one place. Because actions are
//! plain data, scripted (Lua/WASM) modules use the exact same path as native ones.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use twilight_model::channel::Channel;
use twilight_model::channel::message::component::{
    Button, ButtonStyle, SelectMenu, SelectMenuType,
};
use twilight_model::channel::message::{Component, Embed, Message};
use twilight_model::guild::audit_log::AuditLog;
use twilight_model::guild::auto_moderation::AutoModerationRule;
use twilight_model::guild::{Ban, Guild, Member, Permissions, Role};
use twilight_model::id::Id;
use twilight_model::id::marker::{AutoModerationRuleMarker, EmojiMarker};

use crate::capability::Capability;
use crate::error::DiscordError;
use crate::event::{
    ChannelId, GuildId, InteractionId, InteractionToken, MessageId, RoleId, UserId,
};

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
                InteractionResponse::Defer { .. } | InteractionResponse::DeferUpdate => Vec::new(),
            },
            Self::Followup { reply, .. } | Self::EditResponse { reply, .. } => {
                of(&reply.components)
            }
            _ => Vec::new(),
        }
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
            Self::SendMessage { .. } | Self::EditMessage { .. } => Capability::SendMessages,
            Self::SendDirectMessage { .. } => Capability::SendDirectMessages,
            Self::AddReaction { .. } => Capability::AddReactions,
            Self::DeleteMessage { .. } | Self::BulkDeleteMessages { .. } => {
                Capability::ManageMessages
            }
            Self::GetGuild { .. }
            | Self::GetMember { .. }
            | Self::GetChannel { .. }
            | Self::ListRoles { .. }
            | Self::ListAutoModRules { .. } => Capability::ReadGuild,
            Self::ListBans { .. } => Capability::ReadBans,
            Self::Kick { .. } => Capability::KickMembers,
            Self::Ban { .. } | Self::Unban { .. } => Capability::BanMembers,
            Self::Timeout { .. } => Capability::TimeoutMembers,
            Self::SetVoiceMute { .. } => Capability::MuteMembers,
            Self::SetNickname { .. } => Capability::ManageNicknames,
            Self::AddRole { .. }
            | Self::RemoveRole { .. }
            | Self::CreateRole { .. }
            | Self::EditRole { .. }
            | Self::DeleteRole { .. } => Capability::ManageRoles,
            Self::CreateChannel { .. }
            | Self::EditChannel { .. }
            | Self::DeleteChannel { .. }
            | Self::SetPermissionOverwrite { .. }
            | Self::DeletePermissionOverwrite { .. } => Capability::ManageChannels,
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
            | Self::SendDirectMessage { guild_id, .. } => Some(*guild_id),
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
            | Self::EditResponse { .. } => None,
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
            Self::SendMessage { channel_id, .. }
            | Self::EditMessage { channel_id, .. }
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
            | Self::Respond { .. }
            | Self::Followup { .. }
            | Self::EditResponse { .. } => Vec::new(),
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
            | Self::SendDirectMessage { .. } => None,
        }
    }

    /// A stable `snake_case` name for metrics labels and tracing.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
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
    pub fn validate(&self) -> Result<(), DiscordError> {
        match self {
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
            | Self::DeleteAutoModRule { .. } => Ok(()),
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
