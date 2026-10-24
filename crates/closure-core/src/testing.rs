//! Test doubles for code written against [`closure_core`](crate).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;

use crate::action::{Action, Outcome};
use crate::client::DiscordApi;
use crate::error::{DiscordError, ModuleError};
use crate::event::{Event, GuildId};
use crate::locale::Locale;
use crate::module::{EventContext, Module, ModuleId, ModuleManifest};
use crate::settings::{GuildPolicy, ModuleSettings, RuntimeLimits, SettingsProvider};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means another test thread panicked; the data is still usable.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
}

impl MockApi {
    /// Every action received so far, in order.
    #[must_use]
    pub fn actions(&self) -> Vec<Action> {
        lock(&self.actions).clone()
    }

    /// Makes every following call fail with `error`.
    pub fn fail_with(&self, error: DiscordError) {
        *lock(&self.failure) = Some(error);
    }
}

#[async_trait]
impl DiscordApi for MockApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        lock(&self.actions).push(action);
        match lock(&self.failure).clone() {
            Some(error) => Err(error),
            None => Ok(Outcome::None),
        }
    }
}

/// A fixed [`SettingsProvider`] for tests: the same locale and module settings for all guilds.
#[derive(Debug, Clone)]
pub struct StaticSettings {
    pub locale: Locale,
    pub limits: RuntimeLimits,
    pub policy: Arc<GuildPolicy>,
    pub modules: Arc<HashMap<ModuleId, ModuleSettings>>,
}

impl StaticSettings {
    #[must_use]
    pub fn new(modules: impl IntoIterator<Item = (ModuleId, ModuleSettings)>) -> Self {
        Self {
            locale: Locale::DEFAULT,
            limits: RuntimeLimits::default(),
            policy: Arc::new(GuildPolicy::default()),
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

    fn module(&self, _guild: GuildId, module: &ModuleId) -> Option<ModuleSettings> {
        self.modules.get(module).cloned()
    }
}
