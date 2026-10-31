use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use twilight_model::channel::Channel;
use twilight_model::channel::message::{Embed, Message};
use twilight_model::channel::thread::ThreadMember;
use twilight_model::channel::webhook::Webhook;
use twilight_model::guild::invite::Invite;
use twilight_model::guild::scheduled_event::GuildScheduledEvent;
use twilight_model::guild::{Emoji as GuildEmoji, RolePosition};
use twilight_model::guild::{Guild, Member};
use twilight_model::id::Id;
use twilight_model::id::marker::{EmojiMarker, ScheduledEventMarker, WebhookMarker};
use twilight_model::user::User;

use crate::action::{
    Action, ChannelPosition, Emoji, GuildPatch, InteractionReply, InteractionResponse,
    MentionPolicy, MessageCursor, ModalSpec, NewScheduledEvent, Outcome, Reason,
    ScheduledEventPatch, ThreadKind, ThreadPatch, WebhookToken,
};
use crate::capability::CapabilitySet;
use crate::error::DiscordError;
use crate::event::{ChannelId, GuildId, InteractionRef, MessageId, RoleId, UserId};
use crate::interaction::InteractionGuilds;
use crate::module::{ModuleId, OptionChoice};

/// The single door to Discord. Implemented by the Twilight-backed client in production and by
/// [`MockApi`](crate::testing::MockApi) in tests.
#[async_trait]
pub trait DiscordApi: Send + Sync {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError>;

    /// The guild `channel` belongs to, or `None` for a channel outside any guild (a direct
    /// message). Fails if Discord cannot say, e.g. the channel does not exist.
    ///
    /// This is what lets [`ApiClient`] keep actions that identify only a channel or a message
    /// inside the guild the client is bound to.
    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError>;

    /// The owner of `guild`, when it is known without asking Discord (from the cache).
    async fn guild_owner(&self, _guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        Ok(None)
    }
}

/// What a module holds instead of a raw HTTP client.
///
/// [`ApiClient::execute`] rejects actions the module has no capability for, validates the rest
/// and only then forwards them, so a buggy or hostile module cannot exceed its grant.
#[derive(Clone)]
pub struct ApiClient {
    api: Arc<dyn DiscordApi>,
    granted: Arc<CapabilitySet>,
    module: ModuleId,
    guild: Option<GuildId>,
    interactions: Option<Arc<InteractionGuilds>>,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("module", &self.module)
            .field("granted", &self.granted)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

fn unexpected(action: &'static str) -> DiscordError {
    DiscordError::Fatal(format!("unexpected outcome for `{action}`"))
}

impl ApiClient {
    #[must_use]
    pub fn new(api: Arc<dyn DiscordApi>, granted: CapabilitySet, module: ModuleId) -> Self {
        Self {
            api,
            granted: Arc::new(granted),
            module,
            guild: None,
            interactions: None,
        }
    }

    /// Tells the client which guild each interaction token belongs to. A client that is bound
    /// to a guild and has no such record refuses every interaction response.
    ///
    /// Only the host's setup does this, before the client is bound to a guild: once it is bound,
    /// or already has a record, the call changes nothing. A module holds a bound client and
    /// could otherwise swap in a record of its own, in which any token belongs to any guild.
    #[must_use]
    pub fn with_interactions(mut self, interactions: Arc<InteractionGuilds>) -> Self {
        if self.guild.is_some() || self.interactions.is_some() {
            tracing::warn!(module = %self.module, "the interaction record of a bound client cannot be replaced");
        } else {
            self.interactions = Some(interactions);
        }
        self
    }

    /// Restricts the client to one guild: an action that targets a different guild is refused.
    ///
    /// Capabilities are granted per guild, so without this a module enabled with `ban_members`
    /// in one guild could ban in any other. An action that names a guild is compared directly.
    /// One that identifies only a channel or a message is checked by asking
    /// [`DiscordApi::channel_guild`], and refused when the channel is in another guild, outside
    /// any guild, or its guild cannot be determined; this also covers the channels an action
    /// merely refers to (a parent category, an alert or exempt channel). Responses to an
    /// interaction are answered only when the host recorded the token as coming from the
    /// bound guild (see [`with_interactions`](Self::with_interactions)).
    #[must_use]
    pub fn scoped_to(mut self, guild: GuildId) -> Self {
        // Only the first binding counts: a module holds a (cloneable) scoped client and must not
        // be able to rebind its own copy to another guild.
        self.guild.get_or_insert(guild);
        self
    }

    /// The owner of the guild the client is bound to, if the host knows. Needs no capability: it
    /// is what the host itself knows, not something fetched for the module.
    pub async fn guild_owner(&self) -> Result<Option<UserId>, DiscordError> {
        match self.guild {
            Some(guild) => self.api.guild_owner(guild).await,
            None => Ok(None),
        }
    }

    pub async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        let name = action.name();
        let capability = action.capability();
        if !self.granted.contains(capability) {
            tracing::warn!(module = %self.module, action = name, %capability, "capability denied");
            return Err(DiscordError::CapabilityDenied(capability));
        }
        if let (Some(scope), Some(target)) = (self.guild, action.guild_id())
            && scope != target
        {
            tracing::warn!(module = %self.module, action = name, %scope, %target, "action targets another guild");
            return Err(DiscordError::Forbidden(
                "the action targets a guild other than the one being served".into(),
            ));
        }
        action.validate()?;
        // A click is delivered to the module whose id its `custom_id` starts with, so a module
        // that could name another's id could make that module act on a click it never offered.
        let own_prefix = format!("{}:", self.module);
        if let Some(id) = action
            .custom_ids()
            .into_iter()
            .find(|id| !id.starts_with(&own_prefix))
        {
            tracing::warn!(module = %self.module, action = name, custom_id = id, "custom_id outside the module's own");
            return Err(DiscordError::Invalid(format!(
                "a custom_id has to start with `{own_prefix}`"
            )));
        }
        if let Some(scope) = self.guild {
            self.check_scope(scope, &action).await?;
        }

        let started = Instant::now();
        let result = self.api.execute(action).await;
        tracing::debug!(
            module = %self.module,
            action = name,
            ok = result.is_ok(),
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "discord action"
        );
        result
    }

    /// Refuses `action` unless everything it touches belongs to `scope`: every channel it
    /// targets or refers to, and the interaction it answers.
    async fn check_scope(&self, scope: GuildId, action: &Action) -> Result<(), DiscordError> {
        for channel in action.channels() {
            match self.api.channel_guild(channel).await? {
                Some(guild) if guild == scope => {}
                found => {
                    tracing::warn!(module = %self.module, action = action.name(), %scope, ?found, "action targets a channel of another guild");
                    return Err(DiscordError::Forbidden(
                        "the action targets a channel that is not in the guild being served".into(),
                    ));
                }
            }
        }
        if let Some(token) = action.interaction_token() {
            let known = self
                .interactions
                .as_ref()
                .and_then(|interactions| interactions.guild_of(token));
            if known != Some(scope) {
                tracing::warn!(module = %self.module, action = action.name(), %scope, "action answers an interaction of another guild, or an unknown one");
                return Err(DiscordError::Forbidden(
                    "the action answers an interaction that is not from the guild being served"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    pub async fn send_message(
        &self,
        channel_id: ChannelId,
        content: impl Into<String> + Send,
    ) -> Result<Message, DiscordError> {
        let action = Action::SendMessage {
            channel_id,
            content: Some(content.into()),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::None,
        };
        match self.execute(action).await? {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("send_message")),
        }
    }

    /// Posts `embeds` to the channel, without text and without mentions. Needs the capability
    /// `SendMessages`, as [`Self::send_message`] does.
    pub async fn send_embeds(
        &self,
        channel_id: ChannelId,
        embeds: Vec<Embed>,
    ) -> Result<Message, DiscordError> {
        let action = Action::SendMessage {
            channel_id,
            content: None,
            embeds,
            reply_to: None,
            mentions: MentionPolicy::None,
        };
        match self.execute(action).await? {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("send_embeds")),
        }
    }

    /// Privately to a member of the guild the client is bound to; the host checks that the user
    /// is one.
    pub async fn send_direct_message(
        &self,
        user_id: UserId,
        content: impl Into<String> + Send,
        embeds: Vec<Embed>,
    ) -> Result<Message, DiscordError> {
        let Some(guild_id) = self.guild else {
            return Err(DiscordError::Invalid(
                "direct messages need a client bound to a guild".into(),
            ));
        };
        let action = Action::SendDirectMessage {
            guild_id,
            user_id,
            content: Some(content.into()),
            embeds,
        };
        match self.execute(action).await? {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("send_direct_message")),
        }
    }

    pub async fn delete_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteMessage {
            channel_id,
            message_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn bulk_delete_messages(
        &self,
        channel_id: ChannelId,
        message_ids: Vec<MessageId>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::BulkDeleteMessages {
            channel_id,
            message_ids,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn add_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
    ) -> Result<(), DiscordError> {
        self.execute(Action::AddReaction {
            channel_id,
            message_id,
            emoji,
        })
        .await
        .map(drop)
    }

    pub async fn get_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
    ) -> Result<Member, DiscordError> {
        match self
            .execute(Action::GetMember { guild_id, user_id })
            .await?
        {
            Outcome::Member(member) => Ok(*member),
            _ => Err(unexpected("get_member")),
        }
    }

    pub async fn get_guild(&self, guild_id: GuildId) -> Result<Guild, DiscordError> {
        match self.execute(Action::GetGuild { guild_id }).await? {
            Outcome::Guild(guild) => Ok(*guild),
            _ => Err(unexpected("get_guild")),
        }
    }

    pub async fn kick(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Kick {
            guild_id,
            user_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn ban(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        delete_message_seconds: u32,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Ban {
            guild_id,
            user_id,
            delete_message_seconds,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn unban(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Unban {
            guild_id,
            user_id,
            reason,
        })
        .await
        .map(drop)
    }

    /// Times a member out for `duration_seconds`, or lifts the timeout when `None`.
    pub async fn timeout(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        duration_seconds: Option<u32>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Timeout {
            guild_id,
            user_id,
            duration_seconds,
            reason,
        })
        .await
        .map(drop)
    }

    /// Server-mutes or unmutes a member in voice channels.
    pub async fn set_voice_mute(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        mute: bool,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::SetVoiceMute {
            guild_id,
            user_id,
            mute,
            reason,
        })
        .await
        .map(drop)
    }

    /// Server-deafens or undeafens a member in voice channels.
    pub async fn set_voice_deafen(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        deaf: bool,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::SetVoiceDeafen {
            guild_id,
            user_id,
            deaf,
            reason,
        })
        .await
        .map(drop)
    }

    /// Moves a member who is in voice into `channel_id`, which must be in the guild.
    pub async fn move_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        channel_id: ChannelId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::MoveMember {
            guild_id,
            user_id,
            channel_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn disconnect_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DisconnectMember {
            guild_id,
            user_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn get_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Result<Message, DiscordError> {
        match self
            .execute(Action::GetMessage {
                channel_id,
                message_id,
            })
            .await?
        {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("get_message")),
        }
    }

    /// The newest messages of a channel, or those around, before or after `cursor`.
    pub async fn get_messages(
        &self,
        channel_id: ChannelId,
        cursor: Option<MessageCursor>,
        limit: u16,
    ) -> Result<Vec<Message>, DiscordError> {
        match self
            .execute(Action::GetMessages {
                channel_id,
                cursor,
                limit,
            })
            .await?
        {
            Outcome::Messages(messages) => Ok(messages),
            _ => Err(unexpected("get_messages")),
        }
    }

    pub async fn get_pins(&self, channel_id: ChannelId) -> Result<Vec<Message>, DiscordError> {
        match self.execute(Action::GetPins { channel_id }).await? {
            Outcome::Messages(messages) => Ok(messages),
            _ => Err(unexpected("get_pins")),
        }
    }

    pub async fn pin_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::PinMessage {
            channel_id,
            message_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn unpin_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::UnpinMessage {
            channel_id,
            message_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn crosspost_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Result<Message, DiscordError> {
        match self
            .execute(Action::CrosspostMessage {
                channel_id,
                message_id,
            })
            .await?
        {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("crosspost_message")),
        }
    }

    pub async fn remove_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
        user_id: UserId,
    ) -> Result<(), DiscordError> {
        self.execute(Action::RemoveReaction {
            channel_id,
            message_id,
            emoji,
            user_id,
        })
        .await
        .map(drop)
    }

    /// Every reaction on the message, or only those with `emoji` when one is given.
    pub async fn remove_all_reactions(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Option<Emoji>,
    ) -> Result<(), DiscordError> {
        self.execute(Action::RemoveAllReactions {
            channel_id,
            message_id,
            emoji,
        })
        .await
        .map(drop)
    }

    pub async fn get_reaction_users(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: Emoji,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Vec<User>, DiscordError> {
        match self
            .execute(Action::GetReactionUsers {
                channel_id,
                message_id,
                emoji,
                after,
                limit,
            })
            .await?
        {
            Outcome::Users(users) => Ok(users),
            _ => Err(unexpected("get_reaction_users")),
        }
    }

    /// A thread in `channel_id`, or one started from `source`, a message of that channel.
    pub async fn create_thread(
        &self,
        channel_id: ChannelId,
        name: String,
        source: Option<MessageId>,
        kind: ThreadKind,
        auto_archive_minutes: u16,
    ) -> Result<Channel, DiscordError> {
        match self
            .execute(Action::CreateThread {
                channel_id,
                name,
                source,
                kind,
                auto_archive_minutes,
            })
            .await?
        {
            Outcome::Channel(channel) => Ok(*channel),
            _ => Err(unexpected("create_thread")),
        }
    }

    pub async fn edit_thread(
        &self,
        thread_id: ChannelId,
        patch: ThreadPatch,
        reason: Reason,
    ) -> Result<Channel, DiscordError> {
        match self
            .execute(Action::EditThread {
                thread_id,
                patch,
                reason,
            })
            .await?
        {
            Outcome::Channel(channel) => Ok(*channel),
            _ => Err(unexpected("edit_thread")),
        }
    }

    pub async fn join_thread(&self, thread_id: ChannelId) -> Result<(), DiscordError> {
        self.execute(Action::JoinThread { thread_id })
            .await
            .map(drop)
    }

    pub async fn leave_thread(&self, thread_id: ChannelId) -> Result<(), DiscordError> {
        self.execute(Action::LeaveThread { thread_id })
            .await
            .map(drop)
    }

    pub async fn add_thread_member(
        &self,
        thread_id: ChannelId,
        user_id: UserId,
    ) -> Result<(), DiscordError> {
        self.execute(Action::AddThreadMember { thread_id, user_id })
            .await
            .map(drop)
    }

    pub async fn remove_thread_member(
        &self,
        thread_id: ChannelId,
        user_id: UserId,
    ) -> Result<(), DiscordError> {
        self.execute(Action::RemoveThreadMember { thread_id, user_id })
            .await
            .map(drop)
    }

    pub async fn list_thread_members(
        &self,
        thread_id: ChannelId,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Vec<ThreadMember>, DiscordError> {
        match self
            .execute(Action::ListThreadMembers {
                thread_id,
                after,
                limit,
            })
            .await?
        {
            Outcome::ThreadMembers(members) => Ok(members),
            _ => Err(unexpected("list_thread_members")),
        }
    }

    /// An invite to `channel_id`. `max_age_seconds` of 0 never expires; `max_uses` of 0 is unlimited.
    pub async fn create_invite(
        &self,
        channel_id: ChannelId,
        max_age_seconds: u32,
        max_uses: u16,
        temporary: bool,
        unique: bool,
        reason: Reason,
    ) -> Result<Invite, DiscordError> {
        match self
            .execute(Action::CreateInvite {
                channel_id,
                max_age_seconds,
                max_uses,
                temporary,
                unique,
                reason,
            })
            .await?
        {
            Outcome::Invite(invite) => Ok(*invite),
            _ => Err(unexpected("create_invite")),
        }
    }

    pub async fn list_invites(&self, guild_id: GuildId) -> Result<Vec<Invite>, DiscordError> {
        match self.execute(Action::ListInvites { guild_id }).await? {
            Outcome::Invites(invites) => Ok(invites),
            _ => Err(unexpected("list_invites")),
        }
    }

    pub async fn delete_invite(
        &self,
        guild_id: GuildId,
        code: String,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteInvite {
            guild_id,
            code,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn create_webhook(
        &self,
        channel_id: ChannelId,
        name: String,
        reason: Reason,
    ) -> Result<Webhook, DiscordError> {
        match self
            .execute(Action::CreateWebhook {
                channel_id,
                name,
                reason,
            })
            .await?
        {
            Outcome::Webhook(webhook) => Ok(*webhook),
            _ => Err(unexpected("create_webhook")),
        }
    }

    pub async fn list_webhooks(&self, channel_id: ChannelId) -> Result<Vec<Webhook>, DiscordError> {
        match self.execute(Action::ListWebhooks { channel_id }).await? {
            Outcome::Webhooks(webhooks) => Ok(webhooks),
            _ => Err(unexpected("list_webhooks")),
        }
    }

    pub async fn delete_webhook(
        &self,
        guild_id: GuildId,
        webhook_id: Id<WebhookMarker>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteWebhook {
            guild_id,
            webhook_id,
            reason,
        })
        .await
        .map(drop)
    }

    /// Posts `content` or `embeds` through a webhook, without notifying anyone.
    pub async fn execute_webhook(
        &self,
        guild_id: GuildId,
        webhook_id: Id<WebhookMarker>,
        token: WebhookToken,
        content: Option<String>,
        embeds: Vec<Embed>,
        username: Option<String>,
    ) -> Result<Message, DiscordError> {
        match self
            .execute(Action::ExecuteWebhook {
                guild_id,
                webhook_id,
                token,
                content,
                embeds,
                username,
                avatar_url: None,
                mentions: MentionPolicy::None,
            })
            .await?
        {
            Outcome::Message(message) => Ok(*message),
            _ => Err(unexpected("execute_webhook")),
        }
    }

    pub async fn search_members(
        &self,
        guild_id: GuildId,
        query: String,
        limit: u16,
    ) -> Result<Vec<Member>, DiscordError> {
        match self
            .execute(Action::SearchMembers {
                guild_id,
                query,
                limit,
            })
            .await?
        {
            Outcome::Members(members) => Ok(members),
            _ => Err(unexpected("search_members")),
        }
    }

    pub async fn list_members(
        &self,
        guild_id: GuildId,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Vec<Member>, DiscordError> {
        match self
            .execute(Action::ListMembers {
                guild_id,
                after,
                limit,
            })
            .await?
        {
            Outcome::Members(members) => Ok(members),
            _ => Err(unexpected("list_members")),
        }
    }

    /// How many members `days` of inactivity would remove, without removing them.
    pub async fn prune_count(
        &self,
        guild_id: GuildId,
        days: u16,
        include_roles: Vec<RoleId>,
    ) -> Result<u64, DiscordError> {
        match self
            .execute(Action::PruneCount {
                guild_id,
                days,
                include_roles,
            })
            .await?
        {
            Outcome::Count(count) => Ok(count),
            _ => Err(unexpected("prune_count")),
        }
    }

    /// Removes the members a prune would count, and answers with how many were removed.
    pub async fn prune(
        &self,
        guild_id: GuildId,
        days: u16,
        include_roles: Vec<RoleId>,
        reason: Reason,
    ) -> Result<u64, DiscordError> {
        match self
            .execute(Action::Prune {
                guild_id,
                days,
                include_roles,
                reason,
            })
            .await?
        {
            Outcome::Count(count) => Ok(count),
            _ => Err(unexpected("prune")),
        }
    }

    pub async fn edit_guild(
        &self,
        guild_id: GuildId,
        patch: GuildPatch,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::EditGuild {
            guild_id,
            patch,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn reorder_roles(
        &self,
        guild_id: GuildId,
        positions: Vec<RolePosition>,
    ) -> Result<(), DiscordError> {
        self.execute(Action::ReorderRoles {
            guild_id,
            positions,
        })
        .await
        .map(drop)
    }

    pub async fn reorder_channels(
        &self,
        guild_id: GuildId,
        positions: Vec<ChannelPosition>,
    ) -> Result<(), DiscordError> {
        self.execute(Action::ReorderChannels {
            guild_id,
            positions,
        })
        .await
        .map(drop)
    }

    /// A direct message channel with a member of the guild the client is bound to.
    pub async fn create_direct_message_channel(
        &self,
        user_id: UserId,
    ) -> Result<Channel, DiscordError> {
        let Some(guild_id) = self.guild else {
            return Err(DiscordError::Invalid(
                "direct message channels need a client bound to a guild".into(),
            ));
        };
        match self
            .execute(Action::CreateDirectMessageChannel { guild_id, user_id })
            .await?
        {
            Outcome::Channel(channel) => Ok(*channel),
            _ => Err(unexpected("create_direct_message_channel")),
        }
    }

    pub async fn list_scheduled_events(
        &self,
        guild_id: GuildId,
        with_user_count: bool,
    ) -> Result<Vec<GuildScheduledEvent>, DiscordError> {
        match self
            .execute(Action::ListScheduledEvents {
                guild_id,
                with_user_count,
            })
            .await?
        {
            Outcome::ScheduledEvents(events) => Ok(events),
            _ => Err(unexpected("list_scheduled_events")),
        }
    }

    pub async fn create_scheduled_event(
        &self,
        guild_id: GuildId,
        spec: NewScheduledEvent,
    ) -> Result<GuildScheduledEvent, DiscordError> {
        match self
            .execute(Action::CreateScheduledEvent { guild_id, spec })
            .await?
        {
            Outcome::ScheduledEvent(event) => Ok(*event),
            _ => Err(unexpected("create_scheduled_event")),
        }
    }

    pub async fn edit_scheduled_event(
        &self,
        guild_id: GuildId,
        event_id: Id<ScheduledEventMarker>,
        patch: ScheduledEventPatch,
    ) -> Result<GuildScheduledEvent, DiscordError> {
        match self
            .execute(Action::EditScheduledEvent {
                guild_id,
                event_id,
                patch,
            })
            .await?
        {
            Outcome::ScheduledEvent(event) => Ok(*event),
            _ => Err(unexpected("edit_scheduled_event")),
        }
    }

    pub async fn delete_scheduled_event(
        &self,
        guild_id: GuildId,
        event_id: Id<ScheduledEventMarker>,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteScheduledEvent { guild_id, event_id })
            .await
            .map(drop)
    }

    pub async fn list_emojis(&self, guild_id: GuildId) -> Result<Vec<GuildEmoji>, DiscordError> {
        match self.execute(Action::ListEmojis { guild_id }).await? {
            Outcome::Emojis(emojis) => Ok(emojis),
            _ => Err(unexpected("list_emojis")),
        }
    }

    /// `image` is a PNG, JPEG or GIF as a base64 `data:` URI of at most 256 KiB.
    pub async fn create_emoji(
        &self,
        guild_id: GuildId,
        name: String,
        image: String,
        reason: Reason,
    ) -> Result<GuildEmoji, DiscordError> {
        match self
            .execute(Action::CreateEmoji {
                guild_id,
                name,
                image,
                reason,
            })
            .await?
        {
            Outcome::Emoji(emoji) => Ok(*emoji),
            _ => Err(unexpected("create_emoji")),
        }
    }

    pub async fn delete_emoji(
        &self,
        guild_id: GuildId,
        emoji_id: Id<EmojiMarker>,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::DeleteEmoji {
            guild_id,
            emoji_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn add_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::AddRole {
            guild_id,
            user_id,
            role_id,
            reason,
        })
        .await
        .map(drop)
    }

    pub async fn remove_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        reason: Reason,
    ) -> Result<(), DiscordError> {
        self.execute(Action::RemoveRole {
            guild_id,
            user_id,
            role_id,
            reason,
        })
        .await
        .map(drop)
    }

    /// Answers a slash command with a message.
    pub async fn respond(
        &self,
        invocation: &(impl InteractionRef + ?Sized),
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.respond_with(invocation, InteractionResponse::Message { reply })
            .await
    }

    /// Answers an autocomplete request with the choices the user may pick from.
    pub async fn autocomplete(
        &self,
        invocation: &(impl InteractionRef + ?Sized),
        choices: Vec<OptionChoice>,
    ) -> Result<(), DiscordError> {
        self.respond_with(invocation, InteractionResponse::Autocomplete { choices })
            .await
    }

    /// Answers a click by changing the message the component is on.
    pub async fn update(
        &self,
        click: &(impl InteractionRef + ?Sized),
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.respond_with(click, InteractionResponse::Update { reply })
            .await
    }

    /// Answers with a form.
    pub async fn modal(
        &self,
        interaction: &(impl InteractionRef + ?Sized),
        spec: ModalSpec,
    ) -> Result<(), DiscordError> {
        self.respond_with(interaction, InteractionResponse::Modal(spec))
            .await
    }

    async fn respond_with(
        &self,
        interaction: &(impl InteractionRef + ?Sized),
        response: InteractionResponse,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Respond {
            interaction_id: interaction.interaction_id(),
            token: interaction.token().clone(),
            response,
        })
        .await
        .map(drop)
    }

    /// Acknowledges a slash command so the answer can follow after Discord's 3 second limit.
    pub async fn defer(
        &self,
        invocation: &(impl InteractionRef + ?Sized),
        ephemeral: bool,
    ) -> Result<(), DiscordError> {
        self.respond_with(invocation, InteractionResponse::Defer { ephemeral })
            .await
    }

    /// Acknowledges a click; the message stays as it is until [`Self::edit_response`].
    pub async fn defer_update(
        &self,
        click: &(impl InteractionRef + ?Sized),
    ) -> Result<(), DiscordError> {
        self.respond_with(click, InteractionResponse::DeferUpdate)
            .await
    }

    pub async fn followup(
        &self,
        invocation: &(impl InteractionRef + ?Sized),
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.execute(Action::Followup {
            token: invocation.token().clone(),
            reply,
        })
        .await
        .map(drop)
    }

    /// Replaces the answer given before (or the message a click was on, after a deferral).
    pub async fn edit_response(
        &self,
        invocation: &(impl InteractionRef + ?Sized),
        reply: InteractionReply,
    ) -> Result<(), DiscordError> {
        self.execute(Action::EditResponse {
            token: invocation.token().clone(),
            reply,
        })
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{
        AutoModAction, AutoModTrigger, ChannelKind, ChannelPatch, InteractionReply,
        InteractionResponse, NewAutoModRule, NewChannel, OverwriteSpec, OverwriteTarget,
    };
    use crate::capability::Capability;
    use crate::event::InteractionToken;
    use crate::testing::MockApi;
    use twilight_model::guild::Permissions;
    use twilight_model::id::Id;

    fn client(api: &Arc<MockApi>, granted: &[Capability]) -> ApiClient {
        ApiClient::new(
            Arc::clone(api) as Arc<dyn DiscordApi>,
            granted.iter().copied().collect(),
            ModuleId::new("test").unwrap(),
        )
    }

    #[tokio::test]
    async fn the_owner_of_the_bound_guild_is_known_without_a_capability() {
        let api = Arc::new(MockApi::default());
        api.set_owner(Id::new(1), Id::new(7));
        api.set_owner(Id::new(2), Id::new(8));
        let bound = client(&api, &[]).scoped_to(Id::new(1));
        assert_eq!(bound.guild_owner().await, Ok(Some(Id::new(7))));
        // A client bound to a guild never learns about another one's owner.
        assert_eq!(
            client(&api, &[]).scoped_to(Id::new(2)).guild_owner().await,
            Ok(Some(Id::new(8)))
        );
        assert_eq!(client(&api, &[]).guild_owner().await, Ok(None));
    }

    #[tokio::test]
    async fn a_module_cannot_name_a_component_after_another_module() {
        use twilight_model::channel::message::Component;
        use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};

        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::RespondToInteractions]);
        let invocation = |custom_id: &str| InteractionReply {
            content: Some("hi".into()),
            components: vec![Component::ActionRow(ActionRow {
                id: None,
                components: vec![Component::Button(Button {
                    id: None,
                    custom_id: Some(custom_id.into()),
                    disabled: false,
                    emoji: None,
                    label: Some("go".into()),
                    style: ButtonStyle::Primary,
                    url: None,
                    sku_id: None,
                })],
            })],
            ..InteractionReply::default()
        };
        let action = |reply| Action::Followup {
            token: InteractionToken::new("t"),
            reply,
        };
        // The client of module `test` may name `test:..` and nothing else.
        assert!(client.execute(action(invocation("test:go"))).await.is_ok());
        for other in ["warn:go", "tes:go", "test", "Test:go", "testing:go"] {
            let error = client.execute(action(invocation(other))).await.unwrap_err();
            assert!(
                matches!(error, DiscordError::Invalid(_)),
                "{other}: {error:?}"
            );
        }
        assert_eq!(api.actions().len(), 1);
    }

    fn reason() -> Reason {
        Reason::new("test").unwrap()
    }

    #[tokio::test]
    async fn ungranted_capability_is_denied_without_reaching_discord() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::SendMessages]);
        let result = client.ban(Id::new(1), Id::new(2), 0, reason()).await;
        assert_eq!(
            result,
            Err(DiscordError::CapabilityDenied(Capability::BanMembers))
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn a_scoped_client_refuses_other_guilds_even_with_the_capability() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::KickMembers]).scoped_to(Id::new(1));

        let elsewhere = client.kick(Id::new(2), Id::new(9), reason()).await;
        assert!(
            matches!(elsewhere, Err(DiscordError::Forbidden(_))),
            "{elsewhere:?}"
        );
        assert_eq!(api.actions(), []);

        client.kick(Id::new(1), Id::new(9), reason()).await.unwrap();
        assert_eq!(api.actions().len(), 1);
    }

    #[tokio::test]
    async fn a_module_cannot_rebind_its_scoped_client_to_another_guild() {
        let api = Arc::new(MockApi::default());
        let given = client(&api, &[Capability::BanMembers]).scoped_to(Id::new(1));

        let rebound = given.clone().scoped_to(Id::new(2));
        let result = rebound.ban(Id::new(2), Id::new(9), 0, reason()).await;
        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);

        rebound
            .ban(Id::new(1), Id::new(9), 0, reason())
            .await
            .unwrap();
    }

    /// All ten actions that identify their target by a channel, aimed at `channel`.
    // One of each channel-addressed action, so the guild boundary is tested for all of them.
    #[allow(clippy::too_many_lines)]
    fn channel_actions(channel: u64) -> Vec<Action> {
        let channel_id = Id::new(channel);
        let message_id = Id::new(5);
        vec![
            Action::SendMessage {
                channel_id,
                content: Some("hi".into()),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            },
            Action::EditMessage {
                channel_id,
                message_id,
                content: Some("edited".into()),
                embeds: None,
            },
            Action::DeleteMessage {
                channel_id,
                message_id,
                reason: reason(),
            },
            Action::BulkDeleteMessages {
                channel_id,
                message_ids: vec![Id::new(5), Id::new(6)],
                reason: reason(),
            },
            Action::AddReaction {
                channel_id,
                message_id,
                emoji: Emoji::Unicode {
                    value: "👍".into()
                },
            },
            Action::GetMessage {
                channel_id,
                message_id,
            },
            Action::GetMessages {
                channel_id,
                cursor: None,
                limit: 10,
            },
            Action::GetPins { channel_id },
            Action::CreateThread {
                channel_id,
                name: "help".into(),
                source: None,
                kind: ThreadKind::Public,
                auto_archive_minutes: 60,
            },
            Action::EditThread {
                thread_id: channel_id,
                patch: ThreadPatch::default(),
                reason: reason(),
            },
            Action::JoinThread {
                thread_id: channel_id,
            },
            Action::ListThreadMembers {
                thread_id: channel_id,
                after: None,
                limit: 10,
            },
            Action::PinMessage {
                channel_id,
                message_id,
                reason: reason(),
            },
            Action::UnpinMessage {
                channel_id,
                message_id,
                reason: reason(),
            },
            Action::CrosspostMessage {
                channel_id,
                message_id,
            },
            Action::RemoveReaction {
                channel_id,
                message_id,
                emoji: Emoji::Unicode {
                    value: "👍".into()
                },
                user_id: Id::new(9),
            },
            Action::RemoveAllReactions {
                channel_id,
                message_id,
                emoji: None,
            },
            Action::GetReactionUsers {
                channel_id,
                message_id,
                emoji: Emoji::Unicode {
                    value: "👍".into()
                },
                after: None,
                limit: 10,
            },
            Action::GetChannel { channel_id },
            Action::EditChannel {
                channel_id,
                patch: ChannelPatch {
                    name: Some("renamed".into()),
                    ..ChannelPatch::default()
                },
                reason: reason(),
            },
            Action::DeleteChannel {
                channel_id,
                reason: reason(),
            },
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite: OverwriteSpec {
                    target: OverwriteTarget::Role(Id::new(7)),
                    allow: Permissions::empty(),
                    deny: Permissions::SEND_MESSAGES,
                },
                reason: reason(),
            },
            Action::DeletePermissionOverwrite {
                channel_id,
                target: OverwriteTarget::Role(Id::new(7)),
                reason: reason(),
            },
        ]
    }

    #[tokio::test]
    async fn a_scoped_client_refuses_channels_of_other_guilds_and_outside_guilds() {
        let api = Arc::new(MockApi::default());
        api.add_channel(Id::new(100), Some(Id::new(1)));
        api.add_channel(Id::new(200), Some(Id::new(2)));
        api.add_channel(Id::new(300), None);
        let client = client(&api, &Capability::ALL).scoped_to(Id::new(1));

        for (channel, what) in [(200, "another guild"), (300, "no guild"), (999, "unknown")] {
            for action in channel_actions(channel) {
                let name = action.name();
                let result = client.execute(action).await;
                if channel == 999 {
                    assert_eq!(result, Err(DiscordError::NotFound), "{name} / {what}");
                } else {
                    assert!(
                        matches!(result, Err(DiscordError::Forbidden(_))),
                        "{name} / {what}: {result:?}"
                    );
                }
            }
        }
        assert_eq!(api.actions(), [], "nothing reached Discord");

        for action in channel_actions(100) {
            let name = action.name();
            client
                .execute(action)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(api.actions().len(), channel_actions(100).len());
    }

    #[test]
    fn every_channel_addressed_action_reports_its_channel() {
        for action in channel_actions(100) {
            assert_eq!(action.channels(), [Id::new(100)], "{}", action.name());
            assert_eq!(action.guild_id(), None, "{}", action.name());
        }
    }

    fn new_channel(parent_id: Option<u64>) -> Action {
        Action::CreateChannel {
            guild_id: Id::new(1),
            spec: NewChannel {
                name: "new".into(),
                kind: ChannelKind::Text,
                parent_id: parent_id.map(Id::new),
                topic: None,
                nsfw: false,
                slowmode_seconds: 0,
                overwrites: Vec::new(),
            },
            reason: reason(),
        }
    }

    fn new_rule(alert: Option<u64>, exempt: Option<u64>) -> Action {
        Action::CreateAutoModRule {
            guild_id: Id::new(1),
            spec: NewAutoModRule {
                name: "rule".into(),
                trigger: AutoModTrigger::Spam,
                actions: std::iter::once(AutoModAction::BlockMessage {
                    custom_message: None,
                })
                .chain(alert.map(|channel| AutoModAction::SendAlert {
                    channel_id: Id::new(channel),
                }))
                .collect(),
                enabled: true,
                exempt_roles: Vec::new(),
                exempt_channels: exempt.map(Id::new).into_iter().collect(),
            },
            reason: reason(),
        }
    }

    #[tokio::test]
    async fn voice_moves_and_reorders_never_reach_another_guilds_channels() {
        let api = Arc::new(MockApi::default());
        api.add_channel(Id::new(30), Some(Id::new(2)));
        api.add_channel(Id::new(31), Some(Id::new(1)));
        let client = client(&api, &[Capability::MoveMembers, Capability::ManageChannels])
            .scoped_to(Id::new(1));

        let move_away = client
            .move_member(Id::new(1), Id::new(20), Id::new(30), reason())
            .await;
        assert!(
            matches!(move_away, Err(DiscordError::Forbidden(_))),
            "{move_away:?}"
        );
        let reorder = client
            .reorder_channels(
                Id::new(1),
                vec![
                    ChannelPosition {
                        channel_id: Id::new(31),
                        position: 0,
                    },
                    ChannelPosition {
                        channel_id: Id::new(30),
                        position: 1,
                    },
                ],
            )
            .await;
        assert!(
            matches!(reorder, Err(DiscordError::Forbidden(_))),
            "{reorder:?}"
        );
        assert!(api.actions().is_empty());

        client
            .move_member(Id::new(1), Id::new(20), Id::new(31), reason())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn channels_an_action_merely_refers_to_must_also_be_in_the_guild() {
        let api = Arc::new(MockApi::default());
        api.add_channel(Id::new(100), Some(Id::new(1)));
        api.add_channel(Id::new(200), Some(Id::new(2)));
        let client = client(&api, &Capability::ALL).scoped_to(Id::new(1));

        for action in [
            new_channel(Some(200)),
            new_rule(Some(200), None),
            new_rule(None, Some(200)),
            new_rule(Some(100), Some(200)),
        ] {
            let name = action.name();
            let result = client.execute(action).await;
            assert!(
                matches!(result, Err(DiscordError::Forbidden(_))),
                "{name}: {result:?}"
            );
        }
        assert_eq!(api.actions(), []);

        for action in [
            new_channel(None),
            new_channel(Some(100)),
            new_rule(None, None),
            new_rule(Some(100), Some(100)),
        ] {
            let name = action.name();
            client
                .execute(action)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert_eq!(api.actions().len(), 4);
    }

    fn respond(token: &str) -> Action {
        Action::Respond {
            interaction_id: Id::new(1),
            token: InteractionToken::new(token),
            response: InteractionResponse::Defer { ephemeral: false },
        }
    }

    fn followup(token: &str) -> Action {
        Action::Followup {
            token: InteractionToken::new(token),
            reply: InteractionReply {
                content: Some("done".into()),
                embeds: Vec::new(),
                components: Vec::new(),
                ephemeral: false,
            },
        }
    }

    fn edit_response(token: &str) -> Action {
        Action::EditResponse {
            token: InteractionToken::new(token),
            reply: InteractionReply {
                content: Some("done".into()),
                embeds: Vec::new(),
                components: Vec::new(),
                ephemeral: false,
            },
        }
    }

    #[tokio::test]
    async fn a_client_answers_only_interactions_of_its_own_guild() {
        let api = Arc::new(MockApi::default());
        let interactions = Arc::new(InteractionGuilds::default());
        assert!(
            interactions
                .register(&InteractionToken::new("token-1"), Id::new(1))
                .is_recorded()
        );
        assert!(
            interactions
                .register(&InteractionToken::new("token-2"), Id::new(2))
                .is_recorded()
        );
        let client = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&interactions))
            .scoped_to(Id::new(1));

        for make in [respond, followup, edit_response] {
            for token in ["token-2", "forged"] {
                let action = make(token);
                let name = action.name();
                let result = client.execute(action).await;
                assert!(
                    matches!(result, Err(DiscordError::Forbidden(_))),
                    "{name} / {token}: {result:?}"
                );
            }
            client.execute(make("token-1")).await.unwrap();
        }
        assert_eq!(
            api.actions().len(),
            3,
            "only the own guild's three went through"
        );
    }

    #[tokio::test]
    async fn a_bound_client_cannot_be_given_a_record_of_its_own_making() {
        let api = Arc::new(MockApi::default());
        let real = Arc::new(InteractionGuilds::default());
        assert!(
            real.register(&InteractionToken::new("token-of-guild-2"), Id::new(2))
                .is_recorded()
        );
        let forged = Arc::new(InteractionGuilds::default());
        assert!(
            forged
                .register(&InteractionToken::new("token-of-guild-2"), Id::new(1))
                .is_recorded()
        );

        let with_record = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&real))
            .scoped_to(Id::new(1));
        let without_record =
            client(&api, &[Capability::RespondToInteractions]).scoped_to(Id::new(1));

        for client in [with_record, without_record] {
            // What a module holding the client could try.
            let client = client.with_interactions(Arc::clone(&forged));
            for make in [respond, followup, edit_response] {
                let action = make("token-of-guild-2");
                let name = action.name();
                let result = client.execute(action).await;
                assert!(
                    matches!(result, Err(DiscordError::Forbidden(_))),
                    "{name}: {result:?}"
                );
            }
        }
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn a_module_cannot_carry_its_permission_from_one_guild_to_another() {
        // One module, shared by both guilds: it may respond in guild 1 but not in guild 2. It
        // keeps guild 1's client and uses it with a token it received in guild 2.
        let api = Arc::new(MockApi::default());
        let interactions = Arc::new(InteractionGuilds::default());
        assert!(
            interactions
                .register(&InteractionToken::new("token-of-guild-2"), Id::new(2))
                .is_recorded()
        );
        let allowed_in_1 = client(&api, &[Capability::RespondToInteractions])
            .with_interactions(Arc::clone(&interactions))
            .scoped_to(Id::new(1));

        let result = allowed_in_1.execute(respond("token-of-guild-2")).await;

        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn without_a_record_of_interactions_a_scoped_client_answers_none() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::RespondToInteractions]).scoped_to(Id::new(1));
        let result = client.execute(respond("anything")).await;
        assert!(
            matches!(result, Err(DiscordError::Forbidden(_))),
            "{result:?}"
        );

        // Host code (no scope) is not confined.
        let host = self::client(&api, &[Capability::RespondToInteractions]);
        host.execute(respond("anything")).await.unwrap();
    }

    #[tokio::test]
    async fn a_lookup_failure_refuses_the_action_instead_of_letting_it_through() {
        struct Failing;
        #[async_trait]
        impl DiscordApi for Failing {
            async fn execute(&self, _: Action) -> Result<Outcome, DiscordError> {
                panic!("the action must not be sent");
            }
            async fn channel_guild(&self, _: ChannelId) -> Result<Option<GuildId>, DiscordError> {
                Err(DiscordError::Transient("discord is down".into()))
            }
        }
        let client = ApiClient::new(
            Arc::new(Failing),
            [Capability::ManageChannels].into_iter().collect(),
            ModuleId::new("test").unwrap(),
        )
        .scoped_to(Id::new(1));

        let result = client
            .execute(Action::DeleteChannel {
                channel_id: Id::new(100),
                reason: reason(),
            })
            .await;
        assert_eq!(
            result,
            Err(DiscordError::Transient("discord is down".into()))
        );
    }

    #[tokio::test]
    async fn actions_that_name_a_guild_and_unscoped_clients_need_no_lookup() {
        let api = Arc::new(MockApi::default());
        let scoped = client(&api, &[Capability::KickMembers]).scoped_to(Id::new(1));
        scoped.kick(Id::new(1), Id::new(9), reason()).await.unwrap();
        assert_eq!(api.channel_lookups(), 0);

        let unscoped = client(&api, &[Capability::ManageChannels]);
        unscoped
            .execute(Action::DeleteChannel {
                channel_id: Id::new(100),
                reason: reason(),
            })
            .await
            .unwrap();
        assert_eq!(
            api.channel_lookups(),
            0,
            "host code is not confined to a guild"
        );
    }

    #[tokio::test]
    async fn invalid_action_is_rejected_without_reaching_discord() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::BanMembers]);
        let result = client.ban(Id::new(1), Id::new(2), u32::MAX, reason()).await;
        assert!(matches!(result, Err(DiscordError::Invalid(_))));
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn granted_valid_action_is_forwarded_once() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::KickMembers]);
        client.kick(Id::new(1), Id::new(2), reason()).await.unwrap();
        assert_eq!(api.actions().len(), 1);
        assert_eq!(api.actions()[0].name(), "kick");
    }

    #[tokio::test]
    async fn discord_failures_are_propagated() {
        let api = Arc::new(MockApi::default());
        api.fail_with(DiscordError::NotFound);
        let client = client(&api, &[Capability::KickMembers]);
        let result = client.kick(Id::new(1), Id::new(2), reason()).await;
        assert_eq!(result, Err(DiscordError::NotFound));
    }

    #[tokio::test]
    async fn an_embed_is_posted_without_text_or_mentions() {
        let api = Arc::new(MockApi::default());
        let channel = serde_json::from_value::<Channel>(serde_json::json!({
            "id": "5",
            "type": 0,
            "name": "general"
        }))
        .unwrap();
        api.set_channels(Id::new(1), vec![channel]);
        let client = client(&api, &[Capability::SendMessages]).scoped_to(Id::new(1));
        let embed = serde_json::from_value::<Embed>(serde_json::json!({
            "type": "rich",
            "description": "hi"
        }))
        .unwrap();

        client
            .send_embeds(Id::new(5), vec![embed.clone()])
            .await
            .unwrap();

        assert_eq!(
            api.actions(),
            [Action::SendMessage {
                channel_id: Id::new(5),
                content: None,
                embeds: vec![embed],
                reply_to: None,
                mentions: MentionPolicy::None,
            }]
        );
    }

    #[tokio::test]
    async fn a_direct_message_needs_its_own_capability() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::SendMessages]).scoped_to(Id::new(1));
        let result = client
            .send_direct_message(Id::new(9), "hi", Vec::new())
            .await;
        assert_eq!(
            result,
            Err(DiscordError::CapabilityDenied(
                Capability::SendDirectMessages
            ))
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn a_direct_message_goes_to_the_user_in_the_bound_guild() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::SendDirectMessages]).scoped_to(Id::new(1));
        let message = client
            .send_direct_message(Id::new(9), "hi", Vec::new())
            .await
            .unwrap();
        assert_eq!(message.content, "");
        assert_eq!(
            api.actions(),
            [Action::SendDirectMessage {
                guild_id: Id::new(1),
                user_id: Id::new(9),
                content: Some("hi".into()),
                embeds: Vec::new(),
            }]
        );
    }

    #[tokio::test]
    async fn a_direct_message_of_another_guild_or_without_a_guild_is_refused() {
        let api = Arc::new(MockApi::default());
        let scoped = client(&api, &[Capability::SendDirectMessages]).scoped_to(Id::new(1));
        let elsewhere = Action::SendDirectMessage {
            guild_id: Id::new(2),
            user_id: Id::new(9),
            content: Some("hi".into()),
            embeds: Vec::new(),
        };
        assert!(matches!(
            scoped.execute(elsewhere).await,
            Err(DiscordError::Forbidden(_))
        ));

        let unbound = client(&api, &[Capability::SendDirectMessages]);
        let result = unbound
            .send_direct_message(Id::new(9), "hi", Vec::new())
            .await;
        assert!(
            matches!(result, Err(DiscordError::Invalid(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);
    }

    #[tokio::test]
    async fn an_empty_direct_message_is_rejected_before_discord() {
        let api = Arc::new(MockApi::default());
        let client = client(&api, &[Capability::SendDirectMessages]).scoped_to(Id::new(1));
        let result = client.send_direct_message(Id::new(9), "", Vec::new()).await;
        assert!(
            matches!(result, Err(DiscordError::Invalid(_))),
            "{result:?}"
        );
        assert_eq!(api.actions(), []);
    }
}
