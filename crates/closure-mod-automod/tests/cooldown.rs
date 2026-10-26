//! Cooldowns under concurrency: two evaluations of one rule for one member, interleaved where they
//! read and write the cooldown, fire the rule once. The harness keeps its storage to itself, so these
//! build the module's contexts over a storage that yields before every call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use closure_core::action::Action;
use closure_core::cases::{CaseBackend, Cases, MemoryCases};
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::event::GuildId;
use closure_core::history::{HistoryBackend, MemoryHistory, MessageHistory};
use closure_core::locale::KeyTranslator;
use closure_core::module::{EventContext, Module, ModuleId};
use closure_core::permission::GuildPermissions;
use closure_core::settings::GuildPolicy;
use closure_core::storage::{Entry, Expect, MemoryStorage, StorageBackend, StorageError, Store};
use closure_core::testing::MockApi;
use closure_sdk::{Event, Locale};
use common::*;
use serde_json::{Value, json};
use twilight_model::id::Id;

type Boxed<'a, T> = Pin<Box<dyn Future<Output = Result<T, StorageError>> + Send + 'a>>;

/// Storage that yields before each call, so that evaluations running at once interleave wherever one
/// of them waits on the storage.
struct Yielding(MemoryStorage);

impl StorageBackend for Yielding {
    fn get<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        key: &'c str,
    ) -> Boxed<'d, Option<Entry>>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(async move {
            tokio::task::yield_now().await;
            StorageBackend::get(&self.0, module, guild, key).await
        })
    }

    fn put<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        key: &'c str,
        value: serde_json::Value,
        ttl: Option<Duration>,
        expect: Expect,
    ) -> Boxed<'d, i64>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(async move {
            tokio::task::yield_now().await;
            StorageBackend::put(&self.0, module, guild, key, value, ttl, expect).await
        })
    }

    fn delete<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        key: &'c str,
    ) -> Boxed<'d, bool>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        StorageBackend::delete(&self.0, module, guild, key)
    }

    fn list<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        prefix: &'c str,
        limit: usize,
    ) -> Boxed<'d, Vec<(String, Entry)>>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        StorageBackend::list(&self.0, module, guild, prefix, limit)
    }

    fn counter_add<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        counter: &'c str,
        subject: u64,
        delta: i64,
    ) -> Boxed<'d, i64>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        StorageBackend::counter_add(&self.0, module, guild, counter, subject, delta)
    }

    fn counter_get<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        counter: &'c str,
        subject: u64,
        window: Option<Duration>,
    ) -> Boxed<'d, i64>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        StorageBackend::counter_get(&self.0, module, guild, counter, subject, window)
    }

    fn counter_reset<'a, 'b, 'c, 'd>(
        &'a self,
        module: &'b ModuleId,
        guild: GuildId,
        counter: &'c str,
        subject: u64,
    ) -> Boxed<'d, ()>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        StorageBackend::counter_reset(&self.0, module, guild, counter, subject)
    }
}

/// The context the module gets in the guild, over `storage` and the pretend Discord `api`.
fn context(
    module: &impl Module,
    storage: &Arc<Yielding>,
    api: &Arc<MockApi>,
    config: &Value,
) -> EventContext {
    let manifest = module.manifest();
    let id = manifest.id.clone();
    let client = ApiClient::new(
        Arc::clone(api) as Arc<dyn DiscordApi>,
        manifest.capabilities.clone(),
        id.clone(),
    );
    let store = Store::new(
        Arc::clone(storage) as Arc<dyn StorageBackend>,
        &manifest.capabilities,
        id.clone(),
        Id::new(GUILD),
    );
    let cases = Cases::new(
        Arc::new(MemoryCases::with_clock(|| 0)) as Arc<dyn CaseBackend>,
        &manifest.capabilities,
        id.clone(),
        Id::new(GUILD),
    );
    let history = MessageHistory::new(
        Arc::new(MemoryHistory::default()) as Arc<dyn HistoryBackend>,
        &manifest.capabilities,
        id.clone(),
        Id::new(GUILD),
    );
    EventContext::new(
        id,
        Id::new(GUILD),
        client,
        Locale::DEFAULT,
        Arc::new(GuildPolicy::default()),
        Arc::new(config.clone()),
        Arc::new(KeyTranslator),
    )
    .with_bot(Id::new(BOT))
    .with_permissions(Arc::new(GuildPermissions::default()))
    .with_store(store)
    .with_cases(cases)
    .with_history(history)
}

#[tokio::test]
async fn two_evaluations_of_a_rule_with_a_cooldown_fire_it_once_for_the_member() {
    let module = closure_mod_automod::builder().unwrap().build().unwrap();
    let config = json!({ "rules": { "spoilers": {
        "triggers": [{ "type": "words", "words": ["spoiler"] }],
        "actions": [{ "do": "delete" }],
        "cooldown": "1h"
    } } });
    let storage = Arc::new(Yielding(MemoryStorage::default()));
    let api = Arc::new(MockApi::default());
    let first = context(&module, &storage, &api, &config);
    let second = context(&module, &storage, &api, &config);
    let first_event = Event::MessageCreated(custom(MEMBER, CHANNEL, "spoiler"));
    let second_event = Event::MessageCreated(custom(MEMBER, OTHER_CHANNEL, "spoiler"));
    let (a, b) = tokio::join!(
        module.on_event_flow(&first, &first_event),
        module.on_event_flow(&second, &second_event),
    );
    a.unwrap();
    b.unwrap();
    let deletes = api
        .actions()
        .iter()
        .filter(|action| matches!(action, Action::DeleteMessage { .. }))
        .count();
    assert_eq!(
        deletes, 1,
        "the rule fires once for the member, whichever evaluation came first"
    );
}
