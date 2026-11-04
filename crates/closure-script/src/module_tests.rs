use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use closure_core::action::Action;
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::client::ApiClient;
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, Event, GuildId};
use closure_core::locale::{KeyTranslator, Locale};
use closure_core::module::{EventContext, Module, ModuleId, ModuleManifest};
use closure_core::settings::GuildPolicy;
use closure_core::storage::{MemoryStorage, Store};
use closure_core::testing::MockApi;
use serde_json::{Value, json};

use crate::runtime::{FakeRuntime, ScriptRuntime};
use crate::{Grants, Limits, ScriptModule};

fn setup(
    runtime: Arc<dyn ScriptRuntime>,
    granted: Vec<Capability>,
    limits: Limits,
) -> (ScriptModule, EventContext, Arc<MockApi>) {
    let id = ModuleId::new("script").unwrap();
    let guild = GuildId::new(1);
    let capabilities = CapabilitySet::all();
    let manifest = ModuleManifest::new(id.clone(), "script", "1").capabilities(Capability::ALL);
    let grants = Grants::from([(id.clone(), granted)]);
    let module = ScriptModule::new(manifest, &grants, runtime, limits).unwrap();
    let api = Arc::new(MockApi::default());
    api.add_channel(ChannelId::new(2), Some(guild));
    api.add_channel(ChannelId::new(3), Some(GuildId::new(2)));
    let client = ApiClient::new(api.clone(), capabilities.clone(), id.clone()).scoped_to(guild);
    let store = Store::new(
        Arc::new(MemoryStorage::default()),
        &capabilities,
        id.clone(),
        guild,
    );
    let ctx = EventContext::new(
        id,
        guild,
        client,
        Locale::En,
        Arc::new(GuildPolicy::default()),
        Arc::new(json!({})),
        Arc::new(KeyTranslator),
    )
    .with_store(store);
    (module, ctx, api)
}

fn event() -> Event {
    Event::ConfigChanged {
        guild_id: GuildId::new(1),
    }
}

fn send(channel: u64, text: &str) -> Value {
    json!({"type":"send_message", "channel_id":channel.to_string(), "content":text, "embeds":[], "reply_to":null})
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn builtin_loader_executes_only_granted_actions() {
    use crate::{BuiltinRuntimes, load_dir};
    use closure_core::module::ModuleRuntime;
    use std::fmt::Write;

    let root = std::env::temp_dir().join(format!("closure-wasm-loader-{}", std::process::id()));
    let dir = root.join("script");
    std::fs::create_dir_all(&dir).unwrap();
    let mut manifest = ModuleManifest::new(ModuleId::new("script").unwrap(), "script", "1")
        .capabilities([Capability::SendMessages]);
    manifest.runtime = ModuleRuntime::Wasm;
    let mut spec = serde_json::to_value(manifest).unwrap();
    spec["entry"] = json!("main.wasm");
    std::fs::write(dir.join("module.yaml"), serde_json::to_vec(&spec).unwrap()).unwrap();
    let output = serde_json::to_string(&json!([send(2, "hello")])).unwrap();
    let encoded = output.bytes().fold(String::new(), |mut result, b| {
        write!(result, "\\{b:02x}").unwrap();
        result
    });
    let packed = (1024_u64 << 32) | output.len() as u64;
    let wat = format!(
        r#"(module (memory (export "memory") 1)
        (data (i32.const 1024) "{encoded}")
        (func (export "alloc") (param i32) (result i32) i32.const 0)
        (func (export "call") (param i32 i32) (result i64) i64.const {packed}))"#
    );
    std::fs::write(dir.join("main.wasm"), wat).unwrap();
    let id = ModuleId::new("script").unwrap();
    for granted in [true, false] {
        let grants = if granted {
            Grants::from([(id.clone(), vec![Capability::SendMessages])])
        } else {
            Grants::new()
        };
        let loaded = load_dir(&root, &grants, &BuiltinRuntimes);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let (_, ctx, api) = setup(
            Arc::new(FakeRuntime::new(|_, _, _| Ok(json!([])))),
            vec![],
            Limits::default(),
        );
        loaded.modules[0].on_event(&ctx, &event()).await.unwrap();
        assert_eq!(api.actions().len(), usize::from(granted));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn denied_capabilities_emit_one_warning_per_module() {
    use std::io::Write;
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || writer.clone()),
    );
    tracing::subscriber::set_global_default(subscriber).unwrap();
    {
        let id = ModuleId::new("warning_test").unwrap();
        let manifest = ModuleManifest::new(id, "script", "1")
            .capabilities([Capability::Storage, Capability::SendMessages]);
        let runtime = Arc::new(FakeRuntime::new(|_, _, _| Ok(json!([]))));
        let module =
            ScriptModule::new(manifest, &Grants::new(), runtime, Limits::default()).unwrap();
        assert_eq!(module.manifest().capabilities.iter().count(), 0);
    }
    let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| line.contains("script capabilities were not granted")
                && line.contains("warning_test"))
            .count(),
        1
    );
}

#[tokio::test]
async fn events_actions_and_denials_keep_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&seen);
    let runtime = Arc::new(FakeRuntime::new(move |input, _, _| {
        observed.lock().unwrap().push(input);
        Ok(
            json!([send(2, "first"), {"type":"get_guild", "guild_id":"1"}, send(3, "wrong guild"), send(2, &"x".repeat(2001)), send(2, "last")]),
        )
    }));
    let (module, ctx, api) = setup(runtime, vec![Capability::SendMessages], Limits::default());
    module.on_load().await.unwrap();
    module.on_event_flow(&ctx, &event()).await.unwrap();
    module.on_unload().await;
    assert_eq!(
        seen.lock().unwrap()[0],
        serde_json::to_value(event()).unwrap()
    );
    let actions = api.actions();
    assert_eq!(actions.len(), 2);
    assert!(
        matches!(&actions[0], Action::SendMessage { content: Some(text), .. } if text == "first")
    );
    assert!(
        matches!(&actions[1], Action::SendMessage { content: Some(text), .. } if text == "last")
    );
}

#[tokio::test]
async fn command_autocomplete_and_job_reach_runtime() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&seen);
    let runtime = Arc::new(FakeRuntime::new(move |input, _, _| {
        observed.lock().unwrap().push(input);
        Ok(json!([]))
    }));
    let (module, ctx, _) = setup(runtime, vec![], Limits::default());
    let common = json!({"interaction_id":"10", "application_id":"11", "token":"test", "guild_id":"1", "channel_id":"2", "user":{"id":"12", "name":"user", "bot":false}, "roles":[], "invoker_permissions":"0", "locale":null, "command":"test", "subcommand":null, "options":[]});
    let mut auto = common.clone();
    auto["focused_option"] = json!("name");
    auto["focused"] = json!("x");
    let events = [
        json!({"type":"command", "data":common}),
        json!({"type":"autocomplete", "data":auto}),
        json!({"type":"scheduled", "data":{"id":1, "guild_id":"1", "module":"script", "name":"task", "payload":{"x":1}, "attempt":1, "token":1}}),
    ];
    for value in &events {
        let event: Event = serde_json::from_value(value.clone()).unwrap();
        module.on_event(&ctx, &event).await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), events);
}

#[tokio::test]
async fn host_kv_and_execute_use_grants_and_scoped_services() {
    let runtime = Arc::new(FakeRuntime::new(|_, host, _| {
        host.set("key", json!({"v":1}))?;
        assert_eq!(host.get("key")?, json!({"v":1}));
        assert_eq!(host.list("k", 10)?[0]["value"], json!({"v":1}));
        assert!(host.delete("key")?);
        assert_eq!(host.get("key")?, Value::Null);
        assert!(
            host.execute(json!({"type":"get_guild", "guild_id":"1"}))
                .unwrap_err()
                .contains("capability")
        );
        host.execute(send(2, "host"))?;
        Ok(json!([]))
    }));
    let (module, ctx, api) = setup(
        runtime,
        vec![Capability::Storage, Capability::SendMessages],
        Limits::default(),
    );
    module.on_event(&ctx, &event()).await.unwrap();
    assert_eq!(api.actions().len(), 1);
    assert_eq!(ctx.store().get::<Value>("key").await.unwrap(), None);
}

#[tokio::test]
async fn host_output_limit_cannot_be_swallowed_and_opens_breaker() {
    let runtime = Arc::new(FakeRuntime::new(|_, host, _| {
        let _ = host.set("key", json!("oversized"));
        Ok(json!([]))
    }));
    let limits = Limits {
        output_bytes: 4,
        ..Limits::default()
    };
    let (module, ctx, _) = setup(runtime, vec![Capability::Storage], limits);
    for _ in 0..5 {
        assert!(module.on_event(&ctx, &event()).await.is_err());
    }
    assert!(
        module
            .on_event(&ctx, &event())
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
    assert_eq!(ctx.store().get::<Value>("key").await.unwrap(), None);
}

#[tokio::test]
async fn storage_without_owner_grant_is_denied() {
    let runtime = Arc::new(FakeRuntime::new(|_, host, _| {
        assert!(host.set("key", json!(1)).unwrap_err().contains("storage"));
        Ok(json!([]))
    }));
    let (module, ctx, _) = setup(runtime, vec![], Limits::default());
    module.on_event(&ctx, &event()).await.unwrap();
    assert_eq!(ctx.store().get::<Value>("key").await.unwrap(), None);
}

#[tokio::test]
async fn host_deadline_and_other_module_are_isolated() {
    let runtime = Arc::new(FakeRuntime::new(|_, host, budget| {
        while Instant::now() < budget.deadline {
            std::thread::yield_now();
        }
        let _ = host.set("late", json!(1));
        Ok(json!([]))
    }));
    let limits = Limits {
        wall_ms: 20,
        ..Limits::default()
    };
    let (bad, ctx, _) = setup(runtime, vec![Capability::Storage], limits);
    let good_runtime = Arc::new(FakeRuntime::new(|_, _, _| Ok(json!([]))));
    let (good, good_ctx, _) = setup(good_runtime, vec![], Limits::default());
    let started = Instant::now();
    let event = event();
    let (failed, succeeded) =
        tokio::join!(bad.on_event(&ctx, &event), good.on_event(&good_ctx, &event));
    assert!(failed.is_err());
    assert!(succeeded.is_ok());
    assert!(started.elapsed() < Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(ctx.store().get::<Value>("late").await.unwrap(), None);
}

struct SlowApi;

#[async_trait::async_trait]
impl DiscordApi for SlowApi {
    async fn execute(&self, _: Action) -> Result<closure_core::action::Outcome, DiscordError> {
        unreachable!()
    }

    async fn channel_guild(&self, _: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(Some(GuildId::new(1)))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn slow_host_call_times_out_without_blocking_tokio_worker() {
    let runtime = Arc::new(FakeRuntime::new(|_, host, _| {
        let _ = host.execute(send(2, "slow"));
        Ok(json!([]))
    }));
    let limits = Limits {
        wall_ms: 30,
        ..Limits::default()
    };
    let (module, mut ctx, _) = setup(runtime, vec![Capability::SendMessages], limits);
    ctx.api = ApiClient::new(Arc::new(SlowApi), CapabilitySet::all(), ctx.module.clone())
        .scoped_to(ctx.guild_id);
    let start = Instant::now();
    assert!(module.on_event(&ctx, &event()).await.is_err());
    assert!(start.elapsed() < Duration::from_millis(500));
}
