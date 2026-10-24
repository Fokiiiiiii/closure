use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::{Action, InteractionResponse};
use closure_core::capability::Capability;
use closure_core::client::DiscordApi;
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{
    ChannelId, CommandInvocation, Event, EventKind, InteractionToken, MessageInfo, UserInfo,
};
use closure_core::locale::Locale;
use closure_core::module::{CommandSpec, EventContext, Module, ModuleId, ModuleManifest};
use closure_core::testing::{MockApi, StaticSettings};
use closure_i18n::TranslatorBuilder;
use serde_json::json;
use tokio::sync::{Barrier, Semaphore};
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use super::*;

const GUILD: u64 = 1;

#[derive(Clone)]
enum Behavior {
    Ok,
    Fail,
    Forbidden,
    Panic,
    Hang,
    /// Blocks until a permit is available.
    Gate(Arc<Semaphore>),
    /// Waits for another handler to arrive: only completes if two run at the same time.
    Rendezvous(Arc<Barrier>),
    /// Sleeps `id % 3` milliseconds, so naive parallelism would reorder events.
    Staggered,
    TryBan,
}

struct Probe {
    manifest: ModuleManifest,
    log: Arc<Mutex<Vec<String>>>,
    behavior: Mutex<Behavior>,
    running: AtomicUsize,
    max_running: AtomicUsize,
}

impl Probe {
    fn new(id: &str, events: &[EventKind], capabilities: &[Capability]) -> Arc<Self> {
        let manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0")
            .events(events.iter().copied())
            .capabilities(capabilities.iter().copied());
        Arc::new(Self {
            manifest,
            log: Arc::new(Mutex::new(Vec::new())),
            behavior: Mutex::new(Behavior::Ok),
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
        })
    }

    fn with_command(id: &str, command: &str) -> Arc<Self> {
        let mut probe = Self::new(id, &[], &[]);
        Arc::get_mut(&mut probe)
            .unwrap()
            .manifest
            .commands
            .push(CommandSpec {
                name: command.into(),
                description: "d".into(),
                options: vec![],
                subcommands: vec![],
                default_member_permissions: None,
            });
        probe
    }

    fn set(&self, behavior: Behavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn max_running(&self) -> usize {
        self.max_running.load(Ordering::SeqCst)
    }
}

fn label(event: &Event) -> String {
    match event {
        Event::MessageCreated(message) => format!("msg:{}", message.id),
        other => other.kind().as_str().to_owned(),
    }
}

#[async_trait]
impl Module for Probe {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn on_load(&self) -> Result<(), ModuleError> {
        self.log.lock().unwrap().push("load".into());
        Ok(())
    }

    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError> {
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(running, Ordering::SeqCst);
        self.log.lock().unwrap().push(label(event));

        let behavior = self.behavior.lock().unwrap().clone();
        let result = match behavior {
            Behavior::Ok => Ok(()),
            Behavior::Fail => Err(ModuleError::other("boom")),
            Behavior::Forbidden => Err(DiscordError::Forbidden("nope".into()).into()),
            Behavior::Panic => panic!("module bug"),
            Behavior::Hang => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            }
            Behavior::Gate(gate) => {
                let _permit = gate.acquire().await.unwrap();
                Ok(())
            }
            Behavior::Rendezvous(barrier) => {
                barrier.wait().await;
                Ok(())
            }
            Behavior::Staggered => {
                let Event::MessageCreated(message) = event else {
                    return Ok(());
                };
                tokio::time::sleep(Duration::from_millis(message.id.get() % 3)).await;
                Ok(())
            }
            Behavior::TryBan => {
                let outcome = ctx.api.ban(Id::new(GUILD), Id::new(9), 0, reason()).await;
                self.log.lock().unwrap().push(match outcome {
                    Ok(()) => "ban:sent".to_owned(),
                    Err(DiscordError::CapabilityDenied(_)) => "ban:denied".to_owned(),
                    Err(other) => format!("ban:{other}"),
                });
                Ok(())
            }
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn on_unload(&self) {
        self.log.lock().unwrap().push("unload".into());
    }
}

fn reason() -> closure_core::action::Reason {
    closure_core::action::Reason::new("test").unwrap()
}

fn user(id: u64) -> UserInfo {
    UserInfo {
        id: Id::new(id),
        name: "user".into(),
        bot: false,
    }
}

fn message(channel: u64, id: u64) -> Event {
    Event::MessageCreated(MessageInfo {
        id: Id::new(id),
        channel_id: Id::new(channel),
        guild_id: Id::new(GUILD),
        author: user(7),
        author_roles: vec![],
        content: String::new(),
        mentioned_users: vec![],
        mentions_everyone: false,
        attachment_count: 0,
    })
}

fn command(name: &str, locale: Option<Locale>) -> Event {
    Event::Command(CommandInvocation {
        interaction_id: Id::new(50),
        application_id: Id::new(60),
        token: InteractionToken::new("tok"),
        guild_id: Id::new(GUILD),
        channel_id: Id::new(3),
        user: user(7),
        invoker_permissions: Permissions::empty(),
        locale,
        command: name.into(),
        subcommand: None,
        options: vec![],
    })
}

fn limits(queue_capacity: usize, concurrency: usize) -> RuntimeLimits {
    RuntimeLimits {
        handler_timeout: Duration::from_millis(100),
        queue_capacity,
        concurrency,
    }
}

struct Harness {
    dispatcher: Dispatcher,
    api: Arc<MockApi>,
}

async fn harness(
    probes: &[(&Arc<Probe>, bool)],
    limits: RuntimeLimits,
    config: DispatcherConfig,
) -> Harness {
    let registry = Arc::new(
        ModuleRegistry::new(
            probes
                .iter()
                .map(|(probe, _)| Arc::clone(*probe) as Arc<dyn Module>)
                .collect(),
        )
        .unwrap(),
    );
    let mut settings = StaticSettings::new(probes.iter().map(|(probe, enabled)| {
        (
            probe.manifest.id.clone(),
            ModuleSettings {
                enabled: *enabled,
                config: Arc::new(json!({})),
                granted: probe.manifest.capabilities.clone(),
            },
        )
    }));
    settings.limits = limits;
    let api = Arc::new(MockApi::default());
    let translator = Arc::new(TranslatorBuilder::new().unwrap().build());
    let dispatcher = Dispatcher::start(
        registry,
        Arc::new(settings),
        Arc::clone(&api) as Arc<dyn DiscordApi>,
        translator,
        config,
    )
    .await
    .unwrap();
    Harness { dispatcher, api }
}

fn messages_only(id: &str) -> Arc<Probe> {
    Probe::new(id, &[EventKind::MessageCreated], &[])
}

#[tokio::test]
async fn only_subscribed_and_enabled_modules_receive_an_event() {
    let subscribed = messages_only("subscribed");
    let off = messages_only("off");
    let other = Probe::new("other", &[EventKind::BanAdded], &[]);
    let h = harness(
        &[(&subscribed, true), (&off, false), (&other, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(message(3, 100));
    h.dispatcher.flush().await;

    assert_eq!(subscribed.log(), ["load", "msg:100"]);
    assert_eq!(off.log(), ["load"]);
    assert_eq!(other.log(), ["load"]);
}

#[tokio::test(start_paused = true)]
async fn events_of_one_channel_keep_their_order_despite_uneven_handler_times() {
    let probe = messages_only("order");
    probe.set(Behavior::Staggered);
    let h = harness(
        &[(&probe, true)],
        limits(256, 8),
        DispatcherConfig::default(),
    )
    .await;

    for id in 1..=30 {
        h.dispatcher.dispatch(message(3, id));
    }
    h.dispatcher.flush().await;

    let expected: Vec<String> = std::iter::once("load".to_owned())
        .chain((1..=30).map(|id| format!("msg:{id}")))
        .collect();
    assert_eq!(probe.log(), expected);
    assert_eq!(
        probe.max_running(),
        1,
        "one channel is never handled concurrently"
    );
}

fn channels_on_different_lanes(lanes: usize) -> (u64, u64) {
    let first = 100;
    let second = (101..1000)
        .find(|candidate| lane_index(*candidate, lanes) != lane_index(first, lanes))
        .expect("some channel id maps to another lane");
    (first, second)
}

#[tokio::test]
async fn different_channels_are_handled_in_parallel() {
    let probe = messages_only("parallel");
    probe.set(Behavior::Rendezvous(Arc::new(Barrier::new(2))));
    let h = harness(
        &[(&probe, true)],
        limits(64, 8),
        DispatcherConfig::default(),
    )
    .await;
    let (a, b) = channels_on_different_lanes(8);

    h.dispatcher.dispatch(message(a, 1));
    h.dispatcher.dispatch(message(b, 2));

    // Each handler waits for the other; serial processing would hit the module timeout and
    // both would count as failures instead of finishing together.
    tokio::time::timeout(Duration::from_secs(5), async {
        while probe.max_running() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("handlers never ran at the same time");
    h.dispatcher.flush().await;
}

#[tokio::test]
async fn a_full_queue_drops_new_events_instead_of_growing() {
    let probe = messages_only("slow");
    let gate = Arc::new(Semaphore::new(0));
    probe.set(Behavior::Gate(Arc::clone(&gate)));
    let config = DispatcherConfig::default();
    let mut slow_limits = limits(2, 1);
    slow_limits.handler_timeout = Duration::from_secs(30);
    let h = harness(&[(&probe, true)], slow_limits, config).await;

    h.dispatcher.dispatch(message(3, 1));
    while probe.log().len() < 2 {
        tokio::task::yield_now().await;
    }
    for id in 2..=6 {
        h.dispatcher.dispatch(message(3, id));
    }
    assert_eq!(h.dispatcher.pending(), 3, "one running plus a queue of two");

    gate.add_permits(100);
    h.dispatcher.flush().await;
    assert_eq!(probe.log(), ["load", "msg:1", "msg:2", "msg:3"]);
}

#[tokio::test]
async fn a_panicking_module_does_not_affect_others_or_later_events() {
    let bad = messages_only("bad");
    bad.set(Behavior::Panic);
    let good = messages_only("good");
    let h = harness(
        &[(&bad, true), (&good, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.dispatch(message(3, 2));
    h.dispatcher.flush().await;

    assert_eq!(good.log(), ["load", "msg:1", "msg:2"]);
    assert_eq!(bad.log(), ["load", "msg:1", "msg:2"]);
}

#[tokio::test(start_paused = true)]
async fn a_hanging_module_is_abandoned_after_the_timeout() {
    let hung = messages_only("hung");
    hung.set(Behavior::Hang);
    let h = harness(&[(&hung, true)], limits(64, 1), DispatcherConfig::default()).await;

    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.dispatch(message(3, 2));
    h.dispatcher.flush().await;

    assert_eq!(
        hung.log(),
        ["load", "msg:1", "msg:2"],
        "the lane moved on after each timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_module_is_paused_then_given_a_trial_after_the_cooldown() {
    let flaky = messages_only("flaky");
    flaky.set(Behavior::Fail);
    let config = DispatcherConfig {
        breaker: BreakerConfig {
            threshold: 3,
            cooldown: Duration::from_secs(30),
        },
        ..DispatcherConfig::default()
    };
    let h = harness(&[(&flaky, true)], limits(64, 1), config).await;

    for id in 1..=6 {
        h.dispatcher.dispatch(message(3, id));
        h.dispatcher.flush().await;
    }
    assert_eq!(
        flaky.log().len(),
        1 + 3,
        "after three failures calls are skipped"
    );

    tokio::time::sleep(Duration::from_secs(31)).await;
    flaky.set(Behavior::Ok);
    h.dispatcher.dispatch(message(3, 7));
    h.dispatcher.flush().await;
    h.dispatcher.dispatch(message(3, 8));
    h.dispatcher.flush().await;
    assert_eq!(
        &flaky.log()[4..],
        ["msg:7", "msg:8"],
        "the trial succeeded, so calls resume"
    );
}

#[tokio::test]
async fn errors_from_discord_do_not_count_against_a_module() {
    let moderator = messages_only("moderator");
    moderator.set(Behavior::Forbidden);
    let config = DispatcherConfig {
        breaker: BreakerConfig {
            threshold: 2,
            cooldown: Duration::from_secs(30),
        },
        ..DispatcherConfig::default()
    };
    let h = harness(&[(&moderator, true)], limits(64, 1), config).await;

    for id in 1..=10 {
        h.dispatcher.dispatch(message(3, id));
        h.dispatcher.flush().await;
    }
    assert_eq!(moderator.log().len(), 1 + 10);
}

#[tokio::test]
async fn a_module_can_only_do_what_it_was_granted() {
    let without = messages_only("without");
    without.set(Behavior::TryBan);
    let with = Probe::new(
        "with",
        &[EventKind::MessageCreated],
        &[Capability::BanMembers],
    );
    with.set(Behavior::TryBan);
    let h = harness(
        &[(&without, true), (&with, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.flush().await;

    assert_eq!(without.log()[2], "ban:denied");
    assert_eq!(with.log()[2], "ban:sent");
    let actions = h.api.actions();
    assert_eq!(actions.len(), 1, "only the granted module reached Discord");
    assert_eq!(actions[0].name(), "ban");
}

fn reply_text(action: &Action) -> (String, bool) {
    let Action::Respond {
        response: InteractionResponse::Message { reply },
        ..
    } = action
    else {
        panic!("expected a message response, got {action:?}");
    };
    (reply.content.clone().unwrap(), reply.ephemeral)
}

#[tokio::test]
async fn commands_of_a_disabled_module_get_a_private_reply_in_the_invokers_language() {
    let module = Probe::with_command("warn", "warn");
    let h = harness(
        &[(&module, false)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(command("warn", Some(Locale::Ja)));
    h.dispatcher.dispatch(command("warn", Some(Locale::En)));
    h.dispatcher.flush().await;

    let mut replies: Vec<_> = h.api.actions().iter().map(reply_text).collect();
    replies.sort();
    assert_eq!(
        replies,
        [
            (
                "That feature is turned off on this server.".to_owned(),
                true
            ),
            (
                "この機能はこのサーバーでは無効になっています。".to_owned(),
                true
            ),
        ]
    );
    assert_eq!(module.log(), ["load"], "the module never saw the command");
}

#[tokio::test]
async fn a_failing_command_tells_the_user_without_leaking_details() {
    let module = Probe::with_command("warn", "warn");
    module.set(Behavior::Fail);
    let h = harness(
        &[(&module, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(command("warn", Some(Locale::En)));
    h.dispatcher.flush().await;

    let actions = h.api.actions();
    let (text, ephemeral) = reply_text(&actions[0]);
    assert!(text.contains("Something went wrong while running that command"));
    assert!(!text.contains("boom"));
    assert!(ephemeral);
}

#[tokio::test]
async fn a_discord_refusal_is_explained_to_the_user() {
    let module = Probe::with_command("warn", "warn");
    module.set(Behavior::Forbidden);
    let h = harness(
        &[(&module, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(command("warn", Some(Locale::Ja)));
    h.dispatcher.flush().await;

    let (text, _) = reply_text(&h.api.actions()[0]);
    assert_eq!(text, "その操作を行う権限がありません。");
}

#[tokio::test]
async fn unknown_commands_are_answered_instead_of_ignored() {
    let module = Probe::with_command("warn", "warn");
    let h = harness(
        &[(&module, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(command("ghost", None));
    h.dispatcher.flush().await;

    assert_eq!(h.api.actions().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn idle_guilds_release_their_lanes_and_come_back_on_demand() {
    let probe = messages_only("idle");
    let config = DispatcherConfig {
        idle_after: Duration::from_secs(60),
        reap_every: Duration::from_secs(10),
        ..DispatcherConfig::default()
    };
    let h = harness(&[(&probe, true)], limits(64, 4), config).await;

    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.flush().await;
    assert_eq!(h.dispatcher.active_guilds(), 1);

    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(h.dispatcher.active_guilds(), 0);

    h.dispatcher.dispatch(message(3, 2));
    h.dispatcher.flush().await;
    assert_eq!(probe.log(), ["load", "msg:1", "msg:2"]);
}

#[tokio::test(start_paused = true)]
async fn shutdown_finishes_queued_events_and_unloads_modules_in_reverse_order() {
    let base = messages_only("base");
    let mut dependent = Probe::new("dependent", &[EventKind::MessageCreated], &[]);
    Arc::get_mut(&mut dependent)
        .unwrap()
        .manifest
        .dependencies
        .push(ModuleId::new("base").unwrap());
    let h = harness(
        &[(&dependent, true), (&base, true)],
        limits(64, 1),
        DispatcherConfig::default(),
    )
    .await;

    for id in 1..=3 {
        h.dispatcher.dispatch(message(3, id));
    }
    let drained = h.dispatcher.shutdown(Duration::from_secs(5)).await;
    assert!(drained);

    h.dispatcher.dispatch(message(3, 99));
    h.dispatcher.flush().await;

    for probe in [&base, &dependent] {
        let log = probe.log();
        assert_eq!(&log[..4], ["load", "msg:1", "msg:2", "msg:3"], "{log:?}");
        assert_eq!(log.last().unwrap(), "unload");
        assert!(
            !log.contains(&"msg:99".to_owned()),
            "events after shutdown are refused"
        );
    }
}

#[tokio::test]
async fn a_module_that_fails_to_load_stops_startup() {
    struct Broken(ModuleManifest);

    #[async_trait]
    impl Module for Broken {
        fn manifest(&self) -> &ModuleManifest {
            &self.0
        }

        async fn on_load(&self) -> Result<(), ModuleError> {
            Err(ModuleError::other("no database"))
        }

        async fn on_event(&self, _: &EventContext, _: &Event) -> Result<(), ModuleError> {
            Ok(())
        }
    }

    let module: Arc<dyn Module> = Arc::new(Broken(ModuleManifest::new(
        ModuleId::new("broken").unwrap(),
        "broken",
        "0.1.0",
    )));
    let result = Dispatcher::start(
        Arc::new(ModuleRegistry::new(vec![module]).unwrap()),
        Arc::new(StaticSettings::new([])),
        Arc::new(MockApi::default()),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
        DispatcherConfig::default(),
    )
    .await;
    assert!(matches!(result, Err(DispatchError::ModuleLoad { .. })));
}

#[test]
fn lanes_spread_consecutive_snowflakes_evenly() {
    let mut counts = [0usize; 8];
    for id in 0..8000 {
        counts[lane_index(id, 8)] += 1;
    }
    for count in counts {
        assert!((800..=1200).contains(&count), "{counts:?}");
    }
    assert_eq!(lane_index(42, 1), 0);
}

#[test]
fn a_message_and_its_deletion_share_an_ordering_key() {
    let created = message(3, 1);
    let deleted = Event::MessageDeleted {
        guild_id: Id::new(GUILD),
        channel_id: ChannelId::new(3),
        message_id: Id::new(1),
    };
    assert_eq!(ordering_key(&created), ordering_key(&deleted));
}
