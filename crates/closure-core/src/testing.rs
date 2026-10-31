//! Test doubles for code written against [`closure_core`](crate).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::audit_log::{AuditLog, AuditLogEntry};
use twilight_model::guild::{Guild, Member, Role};

use crate::action::{Action, AuditLogQuery, InteractionResponse, Outcome, OverwriteTarget};
use crate::client::DiscordApi;
use crate::error::{DiscordError, ModuleError};
use crate::event::{ChannelId, Event, GuildId, RoleId, UserId};
use crate::locale::Locale;
use crate::module::{EventContext, Module, ModuleId, ModuleManifest};
use crate::permission::GuildPermissions;
use crate::settings::{GuildPolicy, ModuleSettings, RuntimeLimits, SettingsProvider};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means another test thread panicked; the data is still usable.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The message Discord answers a send with: a plain text message with the given id.
fn sent_message(channel: ChannelId, id: u64) -> Option<Message> {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(),
        "channel_id": channel.to_string(),
        "content": "",
        "author": { "id": "1", "username": "closure", "discriminator": "0", "avatar": null, "bot": true },
        "attachments": [], "embeds": [], "mention_everyone": false, "mention_roles": [],
        "mentions": [], "pinned": false, "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "type": 0
    }))
    .ok()
}

/// Lets other tasks run once, as a request to Discord would.
async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

/// A role as Discord answers its creation: no permissions, not hoisted, not managed.
fn created_role(id: u64, name: &str) -> Role {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "name": name, "color": 0, "hoist": false,
        "managed": false, "mentionable": false, "permissions": "0", "position": 1,
        "flags": 0, "icon": null, "unicode_emoji": null, "tags": null,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .unwrap_or_else(|error| panic!("a test role: {error}"))
}

/// A guild with the given id and name and nothing else of note.
fn guild_named(guild: GuildId, name: &str, invites_disabled: bool) -> Guild {
    serde_json::from_value(serde_json::json!({
        "id": guild.to_string(),
        "name": name,
        "owner_id": "999999999",
        "roles": [], "emojis": [], "stickers": [],
        "features": if invites_disabled { vec!["INVITES_DISABLED"] } else { Vec::<&str>::new() },
        "afk_timeout": 300, "default_message_notifications": 0,
        "explicit_content_filter": 0, "mfa_level": 0, "nsfw_level": 0,
        "premium_tier": 0, "system_channel_flags": 0, "verification_level": 0,
        "preferred_locale": "en-US", "premium_progress_bar_enabled": false,
        "max_members": null, "max_presences": null, "member_count": null,
        "splash": null, "banner": null, "icon": null, "description": null
    }))
    .unwrap_or_else(|error| panic!("a test guild: {error}"))
}

/// A module that does nothing; handy for exercising registries and configuration.
#[derive(Debug, Clone)]
pub struct StubModule(pub ModuleManifest);

#[async_trait]
impl Module for StubModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.0
    }

    async fn on_event(&self, _: &EventContext, _: &Event) -> Result<(), ModuleError> {
        Ok(())
    }
}

/// A [`DiscordApi`] that records every action and answers with a scripted result.
#[derive(Debug, Default)]
pub struct MockApi {
    actions: Mutex<Vec<Action>>,
    failure: Mutex<Option<DiscordError>>,
    /// The call that fails once, counted from the moment it was set (1 is the next call).
    failing_call: Mutex<Option<(usize, DiscordError)>>,
    /// The errors of the role changes of one role, which the pretend Discord refuses.
    failing_roles: Mutex<HashMap<RoleId, DiscordError>>,
    /// Which guild each known channel belongs to (`None`: a direct message channel).
    channels: Mutex<HashMap<ChannelId, Option<GuildId>>>,
    owners: Mutex<HashMap<GuildId, UserId>>,
    /// Members the pretend Discord knows, by guild and user; `None` is a user who is not one.
    members: Mutex<HashMap<(GuildId, UserId), Option<Member>>>,
    roles: Mutex<HashMap<GuildId, Vec<Role>>>,
    guild_channels: Mutex<HashMap<GuildId, Vec<Channel>>>,
    guild_names: Mutex<HashMap<GuildId, String>>,
    /// The guilds whose invites are switched off by an edit, as the pretend Discord reports them.
    invites_disabled: Mutex<HashSet<GuildId>>,
    lookups: Mutex<usize>,
    /// The entries of the pretend Discord's audit log, oldest first.
    audit_log: Mutex<Vec<AuditLogEntry>>,
}

impl MockApi {
    /// What the pretend Discord answers an action with: what was declared for the members and
    /// roles it asks about, nothing for everything else.
    fn answer(&self, action: &Action) -> Result<Outcome, DiscordError> {
        match action {
            Action::GetMember { guild_id, user_id } => {
                match lock(&self.members).get(&(*guild_id, *user_id)) {
                    Some(Some(member)) => Ok(Outcome::Member(Box::new(member.clone()))),
                    Some(None) => Err(DiscordError::NotFound),
                    None => Ok(Outcome::None),
                }
            }
            Action::ListRoles { guild_id } => Ok(Outcome::Roles(
                lock(&self.roles).get(guild_id).cloned().unwrap_or_default(),
            )),
            // A created role gets the next id, so a test can tell the roles it made apart.
            Action::CreateRole { guild_id, spec, .. } => {
                let id = 500 + u64::try_from(lock(&self.actions).len()).unwrap_or(0);
                let role = created_role(id, &spec.name);
                lock(&self.roles)
                    .entry(*guild_id)
                    .or_default()
                    .push(role.clone());
                Ok(Outcome::Role(Box::new(role)))
            }
            Action::GetChannel { channel_id } => Ok(lock(&self.guild_channels)
                .values()
                .flatten()
                .find(|channel| channel.id == *channel_id)
                .map_or(Outcome::None, |channel| {
                    Outcome::Channel(Box::new(channel.clone()))
                })),
            Action::ListChannels { guild_id } => Ok(Outcome::Channels(
                lock(&self.guild_channels)
                    .get(guild_id)
                    .cloned()
                    .unwrap_or_default(),
            )),
            Action::GetGuild { guild_id } => {
                let disabled = lock(&self.invites_disabled).contains(guild_id);
                let names = lock(&self.guild_names);
                Ok(match names.get(guild_id) {
                    Some(name) => Outcome::Guild(Box::new(guild_named(*guild_id, name, disabled))),
                    None if disabled => Outcome::Guild(Box::new(guild_named(*guild_id, "", true))),
                    None => Outcome::None,
                })
            }
            Action::EditGuild {
                guild_id, patch, ..
            } => {
                if let Some(disabled) = patch.invites_disabled {
                    let mut flagged = lock(&self.invites_disabled);
                    if disabled {
                        flagged.insert(*guild_id);
                    } else {
                        flagged.remove(guild_id);
                    }
                }
                Ok(Outcome::None)
            }
            Action::SendMessage { channel_id, .. } => Ok(self.sent(*channel_id)),
            // The channel of a direct message is not known to the mock; any id will do.
            Action::SendDirectMessage { .. } => Ok(self.sent(ChannelId::new(1))),
            Action::GetMessage {
                channel_id,
                message_id,
            }
            | Action::CrosspostMessage {
                channel_id,
                message_id,
            } => Ok(sent_message(*channel_id, message_id.get())
                .map_or(Outcome::None, |message| Outcome::Message(Box::new(message)))),
            // The pretend Discord holds nothing else: lists come back empty, counts as zero.
            Action::GetMessages { .. } | Action::GetPins { .. } => Ok(Outcome::Messages(vec![])),
            Action::GetReactionUsers { .. } => Ok(Outcome::Users(vec![])),
            Action::ListThreadMembers { .. } => Ok(Outcome::ThreadMembers(vec![])),
            Action::ListInvites { .. } => Ok(Outcome::Invites(vec![])),
            Action::ListWebhooks { .. } => Ok(Outcome::Webhooks(vec![])),
            Action::SearchMembers { .. } | Action::ListMembers { .. } => {
                Ok(Outcome::Members(vec![]))
            }
            Action::PruneCount { .. } | Action::Prune { .. } => Ok(Outcome::Count(0)),
            Action::ListScheduledEvents { .. } => Ok(Outcome::ScheduledEvents(vec![])),
            Action::ListEmojis { .. } => Ok(Outcome::Emojis(vec![])),
            // A text command's reply is a message sent in the channel of the command.
            Action::Respond {
                token,
                response: InteractionResponse::Message { .. },
                ..
            } => Ok(token
                .text_command_message()
                .map_or(Outcome::None, |(channel, _)| self.sent(channel))),
            Action::GetAuditLog { query, .. } => {
                Ok(Outcome::AuditLog(Box::new(self.audit_log_for(query))))
            }
            // Writes that return a new object are recorded but answered with `Outcome::None`.
            _ => Ok(Outcome::None),
        }
    }

    /// The answer to a message sent to `channel`. Each message gets an id of its own, so a test
    /// can tell them apart. Called before the action is recorded.
    fn sent(&self, channel: ChannelId) -> Outcome {
        let id = u64::try_from(lock(&self.actions).len()).unwrap_or(u64::MAX) + 1;
        sent_message(channel, id)
            .map_or(Outcome::None, |message| Outcome::Message(Box::new(message)))
    }

    /// Adds an entry to the guild's audit log, as Discord would have it.
    pub fn add_audit_entry(&self, entry: AuditLogEntry) {
        lock(&self.audit_log).push(entry);
    }

    /// The audit log entries of one kind (all of them when the query names no kind), newest first.
    fn audit_log_for(&self, query: &AuditLogQuery) -> AuditLog {
        let entries: Vec<AuditLogEntry> = lock(&self.audit_log)
            .iter()
            .rev()
            .filter(|entry| {
                query
                    .action_type
                    .is_none_or(|kind| u16::from(entry.action_type) == kind)
            })
            .cloned()
            .collect();
        AuditLog {
            application_commands: Vec::new(),
            auto_moderation_rules: Vec::new(),
            entries,
            guild_scheduled_events: Vec::new(),
            integrations: Vec::new(),
            threads: Vec::new(),
            users: Vec::new(),
            webhooks: Vec::new(),
        }
    }

    /// Every action received so far, in order.
    #[must_use]
    pub fn actions(&self) -> Vec<Action> {
        lock(&self.actions).clone()
    }

    /// Makes every following call fail with `error`.
    pub fn fail_with(&self, error: DiscordError) {
        *lock(&self.failure) = Some(error);
    }

    /// Makes the `n`th following call fail once with `error`. A call failed this way is not
    /// recorded in [`Self::actions`]: it did not get through.
    pub fn fail_nth(&self, n: usize, error: DiscordError) {
        *lock(&self.failing_call) = Some((n, error));
    }

    /// Declares that `channel` belongs to `guild`. Channels never declared do not exist.
    pub fn add_channel(&self, channel: ChannelId, guild: Option<GuildId>) {
        lock(&self.channels).insert(channel, guild);
    }

    /// Declares who owns `guild`.
    pub fn set_owner(&self, guild: GuildId, owner: UserId) {
        lock(&self.owners).insert(guild, owner);
    }

    /// Declares a member of `guild`; a member asked for without a declaration is an error of
    /// the mock, not of Discord.
    pub fn set_member(&self, guild: GuildId, member: Member) {
        lock(&self.members).insert((guild, member.user.id), Some(member));
    }

    /// Declares that `user` is not a member of `guild`.
    pub fn set_not_a_member(&self, guild: GuildId, user: UserId) {
        lock(&self.members).insert((guild, user), None);
    }

    /// Declares the roles of `guild`.
    pub fn set_roles(&self, guild: GuildId, roles: Vec<Role>) {
        lock(&self.roles).insert(guild, roles);
    }

    /// Makes every role change of `role`, and every overwrite for it, fail with `error`, as Discord
    /// does for a role above the bot's.
    pub fn fail_role(&self, role: RoleId, error: DiscordError) {
        lock(&self.failing_roles).insert(role, error);
    }

    /// Lets the role changes of every role succeed again.
    pub fn recover_roles(&self) {
        lock(&self.failing_roles).clear();
    }

    /// Declares the channels of `guild`, with their permission overwrites.
    pub fn set_channels(&self, guild: GuildId, channels: Vec<Channel>) {
        for channel in &channels {
            lock(&self.channels).insert(channel.id, Some(guild));
        }
        lock(&self.guild_channels).insert(guild, channels);
    }

    /// Declares the name of `guild`, which the pretend Discord gives when the guild is asked for.
    pub fn set_guild_name(&self, guild: GuildId, name: &str) {
        lock(&self.guild_names).insert(guild, name.to_owned());
    }

    /// How often a channel's guild was looked up.
    #[must_use]
    pub fn channel_lookups(&self) -> usize {
        *lock(&self.lookups)
    }
}

#[async_trait]
impl DiscordApi for MockApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        // A real request waits for Discord, so other tasks may run between two calls.
        yield_now().await;
        let injected = {
            let mut slot = lock(&self.failing_call);
            match slot.take() {
                Some((1, error)) => Some(error),
                Some((n, error)) => {
                    *slot = Some((n - 1, error));
                    None
                }
                None => None,
            }
        };
        if let Some(error) = injected {
            return Err(error);
        }
        let role_failure = match &action {
            Action::AddRole { role_id, .. } | Action::RemoveRole { role_id, .. } => {
                lock(&self.failing_roles).get(role_id).cloned()
            }
            Action::SetPermissionOverwrite { overwrite, .. } => match overwrite.target {
                OverwriteTarget::Role(role) => lock(&self.failing_roles).get(&role).cloned(),
                OverwriteTarget::Member(_) => None,
            },
            _ => None,
        };
        if let Some(error) = role_failure {
            crate::dump::record(&action);
            lock(&self.actions).push(action);
            return Err(error);
        }
        let answer = self.answer(&action);
        crate::dump::record(&action);
        lock(&self.actions).push(action);
        match lock(&self.failure).clone() {
            Some(error) => Err(error),
            None => answer,
        }
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        *lock(&self.lookups) += 1;
        lock(&self.channels)
            .get(&channel)
            .copied()
            .ok_or(DiscordError::NotFound)
    }

    async fn guild_owner(&self, guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        Ok(lock(&self.owners).get(&guild).copied())
    }
}

/// A fixed [`SettingsProvider`] for tests: the same locale and module settings for all guilds.
#[derive(Debug, Clone)]
pub struct StaticSettings {
    pub locale: Locale,
    pub limits: RuntimeLimits,
    pub policy: Arc<GuildPolicy>,
    pub permissions: Arc<GuildPermissions>,
    pub modules: Arc<HashMap<ModuleId, ModuleSettings>>,
}

impl StaticSettings {
    #[must_use]
    pub fn new(modules: impl IntoIterator<Item = (ModuleId, ModuleSettings)>) -> Self {
        Self {
            locale: Locale::DEFAULT,
            limits: RuntimeLimits::default(),
            policy: Arc::new(GuildPolicy::default()),
            permissions: Arc::new(GuildPermissions::default()),
            modules: Arc::new(modules.into_iter().collect()),
        }
    }
}

impl SettingsProvider for StaticSettings {
    fn locale(&self, _guild: GuildId) -> Locale {
        self.locale
    }

    fn limits(&self, _guild: GuildId) -> RuntimeLimits {
        self.limits
    }

    fn policy(&self, _guild: GuildId) -> Arc<GuildPolicy> {
        Arc::clone(&self.policy)
    }

    fn permissions(&self, _guild: GuildId) -> Arc<GuildPermissions> {
        Arc::clone(&self.permissions)
    }

    fn module(&self, _guild: GuildId, module: &ModuleId) -> Option<ModuleSettings> {
        self.modules.get(module).cloned()
    }
}
