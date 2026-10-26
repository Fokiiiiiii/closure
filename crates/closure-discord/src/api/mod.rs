//! Performs [`Action`]s through Twilight's HTTP client.

mod automod;
mod channels;
mod emojis;
mod guilds;
mod interactions;
mod invites;
mod members;
mod messages;
mod roles;
mod scheduled_events;
mod threads;
mod voice;
mod webhooks;
#[cfg(test)]
mod wire_tests;

use std::collections::HashMap;
use std::fmt;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use closure_core::action::{Action, Outcome};
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::{ApplicationId, ChannelId, GuildId, UserId};
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;
use twilight_http::Client;
use twilight_http::response::Response;
use twilight_http::response::marker::ListBody;

pub use guilds::joined_guilds;

use crate::error::map_http_error;
use crate::hierarchy::{CacheView, Preflight, preflight};

/// How reads that failed transiently are repeated. Writes are never retried: repeating a ban
/// or a message that may have gone through would be worse than reporting the failure.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    /// Delay before the first retry; doubled for each further one.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_millis(250),
        }
    }
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The production [`DiscordApi`].
pub struct TwilightApi {
    http: Arc<Client>,
    application_id: ApplicationId,
    cache: Option<Arc<dyn CacheView>>,
    retry: RetryPolicy,
    clock: Clock,
    /// Guilds of channels learned from Discord. A channel never changes guild, so an entry
    /// cannot go stale; the map is emptied when it reaches [`KNOWN_CHANNELS`].
    known_channels: Mutex<HashMap<ChannelId, GuildId>>,
    /// Lookups of a channel's guild that are under way, so concurrent first lookups of one
    /// channel make one request.
    lookups: Mutex<HashMap<ChannelId, LookupCell>>,
    /// What the bot said in reply to text commands.
    text: interactions::TextReplies,
}

/// How many channel-to-guild answers from Discord are remembered.
const KNOWN_CHANNELS: usize = 10_000;

impl fmt::Debug for TwilightApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwilightApi")
            .field("application_id", &self.application_id)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl TwilightApi {
    #[must_use]
    pub fn new(http: Arc<Client>, application_id: ApplicationId) -> Self {
        Self {
            http,
            application_id,
            cache: None,
            retry: RetryPolicy::default(),
            clock: Arc::new(system_clock),
            known_channels: Mutex::new(HashMap::new()),
            lookups: Mutex::new(HashMap::new()),
            text: interactions::TextReplies::default(),
        }
    }

    /// Enables the role hierarchy pre-check.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<dyn CacheView>) -> Self {
        self.cache = Some(cache);
        self
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Replaces the clock (unix seconds); for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    #[must_use]
    pub fn http(&self) -> &Arc<Client> {
        &self.http
    }
}

/// Awaits a request and reads its body as a model.
pub(crate) async fn read<T, F>(request: F) -> Result<T, DiscordError>
where
    F: IntoFuture<Output = Result<Response<T>, twilight_http::Error>>,
    T: DeserializeOwned + Unpin + Send + 'static,
{
    let response = request
        .into_future()
        .await
        .map_err(|error| map_http_error(&error))?;
    response.model().await.map_err(unreadable)
}

/// Awaits a request whose body is a list of models.
pub(crate) async fn read_list<T, F>(request: F) -> Result<Vec<T>, DiscordError>
where
    F: IntoFuture<Output = Result<Response<ListBody<T>>, twilight_http::Error>>,
    T: DeserializeOwned + Unpin + Send + 'static,
{
    let response = request
        .into_future()
        .await
        .map_err(|error| map_http_error(&error))?;
    response.models().await.map_err(unreadable)
}

/// Awaits a request and ignores its body.
pub(crate) async fn run<T, F>(request: F) -> Result<Outcome, DiscordError>
where
    F: IntoFuture<Output = Result<Response<T>, twilight_http::Error>>,
{
    request
        .into_future()
        .await
        .map(|_| Outcome::None)
        .map_err(|error| map_http_error(&error))
}

fn unreadable(error: impl fmt::Display) -> DiscordError {
    DiscordError::Fatal(format!("could not read Discord's response: {error}"))
}

/// Reads are safe to repeat; everything else may have taken effect before it failed.
fn is_read(action: &Action) -> bool {
    matches!(
        action,
        Action::GetMessage { .. }
            | Action::GetMessages { .. }
            | Action::GetPins { .. }
            | Action::GetReactionUsers { .. }
            | Action::ListThreadMembers { .. }
            | Action::ListInvites { .. }
            | Action::ListWebhooks { .. }
            | Action::SearchMembers { .. }
            | Action::ListMembers { .. }
            | Action::PruneCount { .. }
            | Action::ListScheduledEvents { .. }
            | Action::ListEmojis { .. }
            | Action::GetGuild { .. }
            | Action::GetMember { .. }
            | Action::ListBans { .. }
            | Action::GetChannel { .. }
            | Action::ListRoles { .. }
            | Action::ListAutoModRules { .. }
            | Action::GetAuditLog { .. }
    )
}

fn error_label(error: &DiscordError) -> &'static str {
    match error {
        DiscordError::Invalid(_) => "invalid",
        DiscordError::CapabilityDenied(_) => "capability_denied",
        DiscordError::Forbidden(_) => "forbidden",
        DiscordError::HierarchyViolation(_) => "hierarchy",
        DiscordError::NotFound => "not_found",
        DiscordError::DirectMessagesClosed => "direct_messages_closed",
        DiscordError::NotInVoice => "not_in_voice",
        DiscordError::RateLimited { .. } => "rate_limited",
        DiscordError::Transient(_) => "transient",
        DiscordError::Fatal(_) => "fatal",
    }
}

#[async_trait]
impl DiscordApi for TwilightApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        let name = action.name();
        let started = Instant::now();

        let result = self.execute_checked(&action).await;

        let outcome = result.as_ref().map_or_else(error_label, |_| "ok");
        metrics::counter!("closure_discord_actions_total", "action" => name, "result" => outcome)
            .increment(1);
        metrics::histogram!("closure_discord_action_seconds", "action" => name)
            .record(started.elapsed().as_secs_f64());
        result
    }

    async fn guild_owner(&self, guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        Ok(self
            .cache
            .as_ref()
            .and_then(|cache| cache.guild_owner(guild)))
    }

    /// Answered from the cache, then from earlier answers of Discord; otherwise Discord is asked
    /// (a `GetChannel`, counted and timed like any other action). A channel does not move
    /// between guilds, so a remembered answer cannot go stale. Concurrent first lookups of the
    /// same channel make one request and share its answer.
    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        let lookup = |source: &'static str| {
            metrics::counter!("closure_channel_guild_lookups_total", "source" => source)
                .increment(1);
        };
        if let Some(guild) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.channel_guild(channel))
        {
            lookup("cache");
            return Ok(Some(guild));
        }
        if let Some(guild) = self
            .known_channels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&channel)
            .copied()
        {
            lookup("memory");
            return Ok(Some(guild));
        }
        // Whoever comes first asks; the others who want the same channel meanwhile wait for that
        // answer, a failure included, instead of asking again.
        let flight = Flight::join(&self.lookups, channel);
        let mut asked = false;
        let answer = flight
            .cell
            .get_or_init(|| {
                asked = true;
                lookup("discord");
                self.ask_discord(channel)
            })
            .await
            .clone();
        if !asked {
            lookup("shared");
        }
        answer
    }
}

/// A lookup of the guild of a channel that is under way, shared by everyone who wants it.
type LookupCell = Arc<OnceCell<Result<Option<GuildId>, DiscordError>>>;

/// One caller's share in a lookup. The last one to leave takes the lookup off the list, also when
/// the callers are given up on (their future dropped) before the answer came.
struct Flight<'a> {
    lookups: &'a Mutex<HashMap<ChannelId, LookupCell>>,
    channel: ChannelId,
    cell: LookupCell,
}

impl<'a> Flight<'a> {
    fn join(lookups: &'a Mutex<HashMap<ChannelId, LookupCell>>, channel: ChannelId) -> Self {
        let cell = Arc::clone(
            lookups
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(channel)
                .or_default(),
        );
        Self {
            lookups,
            channel,
            cell,
        }
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut lookups = self.lookups.lock().unwrap_or_else(PoisonError::into_inner);
        // Joining happens under this lock too, so no new share appears while this is decided: the
        // list's own reference and this one are all that is left when nobody else waits.
        if Arc::strong_count(&self.cell) <= 2
            && lookups
                .get(&self.channel)
                .is_some_and(|listed| Arc::ptr_eq(listed, &self.cell))
        {
            lookups.remove(&self.channel);
        }
    }
}

impl TwilightApi {
    /// Asks Discord (a `GetChannel`, counted and timed like any other action) and remembers the
    /// guild it names.
    async fn ask_discord(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        match self
            .execute(Action::GetChannel {
                channel_id: channel,
            })
            .await?
        {
            Outcome::Channel(found) => {
                if let Some(guild) = found.guild_id {
                    let mut known = self
                        .known_channels
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if known.len() >= KNOWN_CHANNELS {
                        known.clear();
                    }
                    known.insert(channel, guild);
                }
                Ok(found.guild_id)
            }
            _ => Err(DiscordError::Fatal(
                "unexpected outcome for `get_channel`".into(),
            )),
        }
    }

    #[cfg(test)]
    fn lookups_in_flight(&self) -> usize {
        self.lookups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl TwilightApi {
    async fn execute_checked(&self, action: &Action) -> Result<Outcome, DiscordError> {
        // A rank problem seen in the cache is only a suspicion (the cache may be stale): the
        // request is sent anyway, and the suspicion explains a 403 should one come back.
        let mut suspicion = None;
        if let Some(cache) = &self.cache {
            match preflight(cache.as_ref(), action) {
                Preflight::Pass => {}
                Preflight::Reject(error) => return Err(error),
                Preflight::Suspect(error) => suspicion = Some(error),
            }
        }
        let retries = if is_read(action) {
            self.retry.max_retries
        } else {
            0
        };
        let mut attempt = 0;
        loop {
            match self.perform(action).await {
                Err(DiscordError::Transient(reason)) if attempt < retries => {
                    let delay = self
                        .retry
                        .base_delay
                        .saturating_mul(2u32.saturating_pow(attempt));
                    tracing::warn!(action = action.name(), attempt, %reason, ?delay, "retrying read");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(DiscordError::Forbidden(detail)) => {
                    return Err(suspicion.unwrap_or(DiscordError::Forbidden(detail)));
                }
                other => return other,
            }
        }
    }

    // One exhaustive match: adding an `Action` without implementing it fails to compile.
    #[allow(clippy::too_many_lines)]
    async fn perform(&self, action: &Action) -> Result<Outcome, DiscordError> {
        match action {
            Action::ListScheduledEvents {
                guild_id,
                with_user_count,
            } => {
                self.list_scheduled_events(*guild_id, *with_user_count)
                    .await
            }
            Action::CreateScheduledEvent { guild_id, spec } => {
                self.create_scheduled_event(*guild_id, spec).await
            }
            Action::EditScheduledEvent {
                guild_id,
                event_id,
                patch,
            } => self.edit_scheduled_event(*guild_id, *event_id, patch).await,
            Action::DeleteScheduledEvent { guild_id, event_id } => {
                self.delete_scheduled_event(*guild_id, *event_id).await
            }
            Action::ListEmojis { guild_id } => self.list_emojis(*guild_id).await,
            Action::CreateEmoji {
                guild_id,
                name,
                image,
                reason,
            } => self.create_emoji(*guild_id, name, image, reason).await,
            Action::DeleteEmoji {
                guild_id,
                emoji_id,
                reason,
            } => self.delete_emoji(*guild_id, *emoji_id, reason).await,
            Action::SearchMembers {
                guild_id,
                query,
                limit,
            } => self.search_members(*guild_id, query, *limit).await,
            Action::ListMembers {
                guild_id,
                after,
                limit,
            } => self.list_members(*guild_id, *after, *limit).await,
            Action::PruneCount {
                guild_id,
                days,
                include_roles,
            } => self.prune_count(*guild_id, *days, include_roles).await,
            Action::Prune {
                guild_id,
                days,
                include_roles,
                reason,
            } => self.prune(*guild_id, *days, include_roles, reason).await,
            Action::EditGuild {
                guild_id,
                patch,
                reason,
            } => self.edit_guild(*guild_id, patch, reason).await,
            Action::ReorderRoles {
                guild_id,
                positions,
            } => self.reorder_roles(*guild_id, positions).await,
            Action::ReorderChannels {
                guild_id,
                positions,
            } => self.reorder_channels(*guild_id, positions).await,
            Action::CreateDirectMessageChannel { guild_id, user_id } => {
                self.create_direct_message_channel(*guild_id, *user_id)
                    .await
            }
            Action::CreateWebhook {
                channel_id,
                name,
                reason,
            } => self.create_webhook(*channel_id, name, reason).await,
            Action::ListWebhooks { channel_id } => self.list_webhooks(*channel_id).await,
            Action::DeleteWebhook {
                guild_id,
                webhook_id,
                reason,
            } => self.delete_webhook(*guild_id, *webhook_id, reason).await,
            Action::ExecuteWebhook {
                guild_id,
                webhook_id,
                token,
                content,
                embeds,
                username,
                avatar_url,
                mentions,
            } => {
                self.execute_webhook(
                    *guild_id,
                    *webhook_id,
                    token,
                    content.as_deref(),
                    embeds,
                    username.as_deref(),
                    avatar_url.as_deref(),
                    mentions,
                )
                .await
            }
            Action::CreateInvite {
                channel_id,
                max_age_seconds,
                max_uses,
                temporary,
                unique,
                reason,
            } => {
                self.create_invite(
                    *channel_id,
                    *max_age_seconds,
                    *max_uses,
                    *temporary,
                    *unique,
                    reason,
                )
                .await
            }
            Action::ListInvites { guild_id } => self.list_invites(*guild_id).await,
            Action::DeleteInvite {
                guild_id,
                code,
                reason,
            } => self.delete_invite(*guild_id, code, reason).await,
            Action::CreateThread {
                channel_id,
                name,
                source,
                kind,
                auto_archive_minutes,
            } => {
                self.create_thread(*channel_id, name, *source, *kind, *auto_archive_minutes)
                    .await
            }
            Action::EditThread {
                thread_id,
                patch,
                reason,
            } => self.edit_thread(*thread_id, patch, reason).await,
            Action::JoinThread { thread_id } => self.join_thread(*thread_id).await,
            Action::LeaveThread { thread_id } => self.leave_thread(*thread_id).await,
            Action::AddThreadMember { thread_id, user_id } => {
                self.add_thread_member(*thread_id, *user_id).await
            }
            Action::RemoveThreadMember { thread_id, user_id } => {
                self.remove_thread_member(*thread_id, *user_id).await
            }
            Action::ListThreadMembers {
                thread_id,
                after,
                limit,
            } => self.list_thread_members(*thread_id, *after, *limit).await,
            Action::SetVoiceDeafen {
                guild_id,
                user_id,
                deaf,
                reason,
            } => {
                self.set_voice_deafen(*guild_id, *user_id, *deaf, reason)
                    .await
            }
            Action::MoveMember {
                guild_id,
                user_id,
                channel_id,
                reason,
            } => {
                self.move_member(*guild_id, *user_id, *channel_id, reason)
                    .await
            }
            Action::DisconnectMember {
                guild_id,
                user_id,
                reason,
            } => self.disconnect_member(*guild_id, *user_id, reason).await,
            Action::GetMessage {
                channel_id,
                message_id,
            } => self.get_message(*channel_id, *message_id).await,
            Action::GetMessages {
                channel_id,
                cursor,
                limit,
            } => self.get_messages(*channel_id, *cursor, *limit).await,
            Action::GetPins { channel_id } => self.get_pins(*channel_id).await,
            Action::PinMessage {
                channel_id,
                message_id,
                reason,
            } => self.pin_message(*channel_id, *message_id, reason).await,
            Action::UnpinMessage {
                channel_id,
                message_id,
                reason,
            } => self.unpin_message(*channel_id, *message_id, reason).await,
            Action::CrosspostMessage {
                channel_id,
                message_id,
            } => self.crosspost_message(*channel_id, *message_id).await,
            Action::RemoveReaction {
                channel_id,
                message_id,
                emoji,
                user_id,
            } => {
                self.remove_reaction(*channel_id, *message_id, emoji, *user_id)
                    .await
            }
            Action::RemoveAllReactions {
                channel_id,
                message_id,
                emoji,
            } => {
                self.remove_all_reactions(*channel_id, *message_id, emoji.as_ref())
                    .await
            }
            Action::GetReactionUsers {
                channel_id,
                message_id,
                emoji,
                after,
                limit,
            } => {
                self.get_reaction_users(*channel_id, *message_id, emoji, *after, *limit)
                    .await
            }
            Action::SendMessage {
                channel_id,
                content,
                embeds,
                reply_to,
                mentions,
            } => {
                self.send_message(*channel_id, content.as_deref(), embeds, *reply_to, mentions)
                    .await
            }
            Action::SendDirectMessage {
                guild_id,
                user_id,
                content,
                embeds,
            } => {
                self.send_direct_message(*guild_id, *user_id, content.as_deref(), embeds)
                    .await
            }
            Action::EditMessage {
                channel_id,
                message_id,
                content,
                embeds,
            } => {
                self.edit_message(
                    *channel_id,
                    *message_id,
                    content.as_deref(),
                    embeds.as_deref(),
                )
                .await
            }
            Action::DeleteMessage {
                channel_id,
                message_id,
                reason,
            } => self.delete_message(*channel_id, *message_id, reason).await,
            Action::BulkDeleteMessages {
                channel_id,
                message_ids,
                reason,
            } => self.bulk_delete(*channel_id, message_ids, reason).await,
            Action::AddReaction {
                channel_id,
                message_id,
                emoji,
            } => self.add_reaction(*channel_id, *message_id, emoji).await,

            Action::GetGuild { guild_id } => self.get_guild(*guild_id).await,
            Action::GetMember { guild_id, user_id } => self.get_member(*guild_id, *user_id).await,
            Action::Kick {
                guild_id,
                user_id,
                reason,
            } => self.kick(*guild_id, *user_id, reason).await,
            Action::Ban {
                guild_id,
                user_id,
                delete_message_seconds,
                reason,
            } => {
                self.ban(*guild_id, *user_id, *delete_message_seconds, reason)
                    .await
            }
            Action::Unban {
                guild_id,
                user_id,
                reason,
            } => self.unban(*guild_id, *user_id, reason).await,
            Action::Timeout {
                guild_id,
                user_id,
                duration_seconds,
                reason,
            } => {
                self.timeout(*guild_id, *user_id, *duration_seconds, reason)
                    .await
            }
            Action::SetVoiceMute {
                guild_id,
                user_id,
                mute,
                reason,
            } => {
                self.set_voice_mute(*guild_id, *user_id, *mute, reason)
                    .await
            }
            Action::SetNickname {
                guild_id,
                user_id,
                nickname,
                reason,
            } => {
                self.set_nickname(*guild_id, *user_id, nickname.as_deref(), reason)
                    .await
            }
            Action::AddRole {
                guild_id,
                user_id,
                role_id,
                reason,
            } => self.add_role(*guild_id, *user_id, *role_id, reason).await,
            Action::RemoveRole {
                guild_id,
                user_id,
                role_id,
                reason,
            } => {
                self.remove_role(*guild_id, *user_id, *role_id, reason)
                    .await
            }
            Action::ListBans {
                guild_id,
                after,
                limit,
            } => self.list_bans(*guild_id, *after, *limit).await,

            Action::GetChannel { channel_id } => self.get_channel(*channel_id).await,
            Action::CreateChannel {
                guild_id,
                spec,
                reason,
            } => self.create_channel(*guild_id, spec, reason).await,
            Action::EditChannel {
                channel_id,
                patch,
                reason,
            } => self.edit_channel(*channel_id, patch, reason).await,
            Action::DeleteChannel { channel_id, reason } => {
                self.delete_channel(*channel_id, reason).await
            }
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite,
                reason,
            } => {
                self.set_permission_overwrite(*channel_id, overwrite, reason)
                    .await
            }
            Action::DeletePermissionOverwrite {
                channel_id,
                target,
                reason,
            } => {
                self.delete_permission_overwrite(*channel_id, *target, reason)
                    .await
            }

            Action::ListRoles { guild_id } => self.list_roles(*guild_id).await,
            Action::CreateRole {
                guild_id,
                spec,
                reason,
            } => self.create_role(*guild_id, spec, reason).await,
            Action::EditRole {
                guild_id,
                role_id,
                patch,
                reason,
            } => self.edit_role(*guild_id, *role_id, patch, reason).await,
            Action::DeleteRole {
                guild_id,
                role_id,
                reason,
            } => self.delete_role(*guild_id, *role_id, reason).await,

            Action::ListAutoModRules { guild_id } => self.list_auto_mod_rules(*guild_id).await,
            Action::CreateAutoModRule {
                guild_id,
                spec,
                reason,
            } => self.create_auto_mod_rule(*guild_id, spec, reason).await,
            Action::DeleteAutoModRule {
                guild_id,
                rule_id,
                reason,
            } => self.delete_auto_mod_rule(*guild_id, *rule_id, reason).await,

            Action::GetAuditLog { guild_id, query } => self.get_audit_log(*guild_id, query).await,

            Action::Respond {
                interaction_id,
                token,
                response,
            } => self.respond(*interaction_id, token, response).await,
            Action::Followup { token, reply } => self.followup(token, reply).await,
            Action::EditResponse { token, reply } => self.edit_response(token, reply).await,
        }
    }
}
