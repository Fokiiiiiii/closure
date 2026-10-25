//! Test doubles for code written against [`closure_core`](crate).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use twilight_model::channel::Message;
use twilight_model::guild::{Member, Role};

use crate::action::{Action, Outcome};
use crate::client::DiscordApi;
use crate::error::{DiscordError, ModuleError};
use crate::event::{ChannelId, Event, GuildId, UserId};
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
    /// Which guild each known channel belongs to (`None`: a direct message channel).
    channels: Mutex<HashMap<ChannelId, Option<GuildId>>>,
    owners: Mutex<HashMap<GuildId, UserId>>,
    /// Members the pretend Discord knows, by guild and user; `None` is a user who is not one.
    members: Mutex<HashMap<(GuildId, UserId), Option<Member>>>,
    roles: Mutex<HashMap<GuildId, Vec<Role>>>,
    lookups: Mutex<usize>,
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
            Action::ListRoles { guild_id } => Ok(lock(&self.roles)
                .get(guild_id)
                .map_or(Outcome::None, |roles| Outcome::Roles(roles.clone()))),
            Action::SendMessage { channel_id, .. } => Ok(self.sent(*channel_id)),
            // The channel of a direct message is not known to the mock; any id will do.
            Action::SendDirectMessage { .. } => Ok(self.sent(ChannelId::new(1))),
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

    /// How often a channel's guild was looked up.
    #[must_use]
    pub fn channel_lookups(&self) -> usize {
        *lock(&self.lookups)
    }
}

#[async_trait]
impl DiscordApi for MockApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
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
        let answer = self.answer(&action);
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
