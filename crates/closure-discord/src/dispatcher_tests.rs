use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::{Action, InteractionResponse};
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{
    ChannelId, CommandInvocation, Event, EventKind, InteractionToken, MessageInfo, UserInfo,
};
use closure_core::interaction::InteractionGuilds;
use closure_core::locale::Locale;
use closure_core::module::{CommandSpec, EventContext, Module, ModuleId, ModuleManifest};
use closure_core::settings::{GuildPolicy, RuntimeLimits, SettingsProvider};
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
    /// Hangs, and needs a moment of real (blocking) work to be torn down once aborted.
    SlowToStop,
    /// Blocks its thread (no `await`) for this long: nothing can abort that.
    BlockThread(Duration),
    /// Blocks until a permit is available.
    Gate(Arc<Semaphore>),
    /// Waits for another handler to arrive: only completes if two run at the same time.
    Rendezvous(Arc<Barrier>),
    /// Sleeps `id % 3` milliseconds, so naive parallelism would reorder events.
    Staggered,
    TryBan,
    /// Answers the command it received, then tries a token it was never given.
    RespondThenForge,
    /// Keeps the client of a message event, then replays it on a command of another guild.
    HoldThenReplay,
    /// Logs the guild policy the module was given.
    ReadPolicy,
}

struct Probe {
    manifest: ModuleManifest,
    log: Arc<Mutex<Vec<String>>>,
    behavior: Mutex<Behavior>,
    running: AtomicUsize,
    max_running: AtomicUsize,

    /// How many handlers were still running when `on_unload` started.
    running_at_unload: AtomicUsize,
    /// A client a module kept from an earlier event.
    held: Mutex<Option<ApiClient>>,
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
            running_at_unload: AtomicUsize::new(usize::MAX),
            held: Mutex::new(None),
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

    /// Keeps the client of a message event, then replays it on a command of another guild,
    /// also with a record of interactions of the module's own making.
    async fn hold_then_replay(&self, ctx: &EventContext, event: &Event) {
        match event {
            Event::MessageCreated(_) => {
                *self.held.lock().unwrap() = Some(ctx.api.clone());
            }
            Event::Command(invocation) => {
                let held = self.held.lock().unwrap().clone().expect("a held client");
                let reply = || InteractionReply {
                    content: Some("pong".into()),
                    embeds: vec![],
                    ephemeral: true,
                };
                let plain = held.respond(invocation, reply()).await;
                // In this record the token belongs to the guild the held client is bound to.
                let own_record = Arc::new(InteractionGuilds::default());
                assert!(
                    own_record
                        .register(&invocation.token, Id::new(GUILD))
                        .is_recorded()
                );
                let forged = held
                    .clone()
                    .with_interactions(own_record)
                    .respond(invocation, reply())
                    .await;
                self.log.lock().unwrap().push(format!(
                    "plain:{} forged:{}",
                    matches!(plain, Err(DiscordError::Forbidden(_))),
                    matches!(forged, Err(DiscordError::Forbidden(_)))
                ));
            }
            _ => {}
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
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
        /// Counts a handler as running until it finishes *or is dropped* (aborted).
        struct Running<'a>(&'a AtomicUsize);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        let _running = Running(&self.running);
        self.max_running.fetch_max(running, Ordering::SeqCst);
        self.log.lock().unwrap().push(label(event));

        let behavior = self.behavior.lock().unwrap().clone();
        match behavior {
            Behavior::Ok => Ok(()),
            Behavior::Fail => Err(ModuleError::other("boom")),
            Behavior::Forbidden => Err(DiscordError::Forbidden("nope".into()).into()),
            Behavior::Panic => panic!("module bug"),
            Behavior::Hang => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            }
            Behavior::SlowToStop => {
                /// Dropped (on abort) before the `Running` guard above.
                struct SlowDrop;
                impl Drop for SlowDrop {
                    fn drop(&mut self) {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }
                let _slow = SlowDrop;
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            }
            Behavior::BlockThread(duration) => {
                std::thread::sleep(duration);
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
            Behavior::ReadPolicy => {
                let policy = ctx.policy();
                self.log.lock().unwrap().push(format!(
                    "policy:{:?}:{}",
                    policy.log_channel.map(Id::get),
                    policy.is_moderator(&[Id::new(5)])
                ));
                Ok(())
            }
            Behavior::HoldThenReplay => {
                self.hold_then_replay(ctx, event).await;
                Ok(())
            }
            Behavior::RespondThenForge => {
                let Event::Command(invocation) = event else {
                    return Ok(());
                };
                let reply = || InteractionReply {
                    content: Some("pong".into()),
                    embeds: vec![],
                    ephemeral: true,
                };
                let own = ctx.api.respond(invocation, reply()).await;
                let forged = ctx
                    .api
                    .followup(
                        &CommandInvocation {
                            token: InteractionToken::new("a-token-of-another-guild"),
                            ..invocation.clone()
                        },
                        reply(),
                    )
                    .await;
                self.log.lock().unwrap().push(format!(
                    "own:{} forged:{}",
                    own.is_ok(),
                    matches!(forged, Err(DiscordError::Forbidden(_)))
                ));
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
        }
    }

    async fn on_unload(&self) {
        self.running_at_unload
            .store(self.running(), Ordering::SeqCst);
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

fn message_in(guild: u64, channel: u64, id: u64) -> Event {
    let Event::MessageCreated(mut info) = message(channel, id) else {
        unreachable!("`message` makes a message");
    };
    info.guild_id = Id::new(guild);
    Event::MessageCreated(info)
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
async fn the_guild_wide_budget_drops_new_events_instead_of_growing() {
    let probe = messages_only("slow");
    let gate = Arc::new(Semaphore::new(0));
    probe.set(Behavior::Gate(Arc::clone(&gate)));
    let mut slow_limits = limits(3, 1);
    slow_limits.handler_timeout = Duration::from_secs(30);
    let h = harness(&[(&probe, true)], slow_limits, DispatcherConfig::default()).await;

    h.dispatcher.dispatch(message(3, 1));
    while probe.log().len() < 2 {
        tokio::task::yield_now().await;
    }
    for id in 2..=6 {
        h.dispatcher.dispatch(message(3, id));
    }
    assert_eq!(
        h.dispatcher.pending(),
        3,
        "the budget counts running and waiting events"
    );

    gate.add_permits(100);
    h.dispatcher.flush().await;
    assert_eq!(probe.log(), ["load", "msg:1", "msg:2", "msg:3"]);
}

#[tokio::test]
async fn one_busy_channel_may_use_the_whole_budget_of_the_guild() {
    let probe = messages_only("hot");
    let gate = Arc::new(Semaphore::new(0));
    probe.set(Behavior::Gate(Arc::clone(&gate)));
    let mut wide = limits(8, 8);
    wide.handler_timeout = Duration::from_secs(30);
    let h = harness(&[(&probe, true)], wide, DispatcherConfig::default()).await;

    // Every event concerns the same channel, so they all share one of the eight lanes.
    for id in 1..=20 {
        h.dispatcher.dispatch(message(3, id));
    }
    assert_eq!(
        h.dispatcher.pending(),
        8,
        "a lane is not limited to its share of the budget"
    );

    gate.add_permits(100);
    h.dispatcher.flush().await;
    assert_eq!(probe.log().len(), 1 + 8);
}

#[tokio::test]
async fn commands_are_not_stuck_behind_a_backlog_of_ordinary_events() {
    let slow = messages_only("slow");
    let gate = Arc::new(Semaphore::new(0));
    slow.set(Behavior::Gate(Arc::clone(&gate)));
    let tools = Probe::with_command("tools", "ping");
    let mut wide = limits(64, 4);
    wide.handler_timeout = Duration::from_secs(30);
    let h = harness(
        &[(&slow, true), (&tools, true)],
        wide,
        DispatcherConfig::default(),
    )
    .await;

    // Occupy every lane that ordinary events can use, with a queue behind each.
    for channel in 1..=64 {
        for id in 1..=3 {
            h.dispatcher.dispatch(message(channel, channel * 10 + id));
        }
    }
    h.dispatcher.dispatch(command("ping", None));

    tokio::time::timeout(Duration::from_secs(5), async {
        while !tools.log().contains(&"command".to_owned()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the command waited behind the message backlog");
    assert!(h.dispatcher.pending() > 1, "the backlog is still there");

    gate.add_permits(10_000);
    h.dispatcher.flush().await;
}

#[tokio::test]
async fn a_flood_of_events_does_not_use_up_the_budget_of_commands() {
    let slow = messages_only("slow");
    let gate = Arc::new(Semaphore::new(0));
    slow.set(Behavior::Gate(Arc::clone(&gate)));
    let tools = Probe::with_command("tools", "ping");
    let mut small = limits(4, 2);
    small.handler_timeout = Duration::from_secs(30);
    let h = harness(
        &[(&slow, true), (&tools, true)],
        small,
        DispatcherConfig::default(),
    )
    .await;

    for id in 1..=50 {
        h.dispatcher.dispatch(message(3, id));
    }
    h.dispatcher.dispatch(command("ping", None));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tools.log().contains(&"command".to_owned()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the command was dropped with the flood");

    gate.add_permits(1000);
    h.dispatcher.flush().await;
}

/// Settings whose handler timeout can be changed while the dispatcher runs.
struct Tunable {
    inner: StaticSettings,
    timeout: Mutex<Duration>,
}

impl SettingsProvider for Tunable {
    fn locale(&self, guild: closure_core::event::GuildId) -> Locale {
        self.inner.locale(guild)
    }

    fn limits(&self, guild: closure_core::event::GuildId) -> RuntimeLimits {
        RuntimeLimits {
            handler_timeout: *self.timeout.lock().unwrap(),
            ..self.inner.limits(guild)
        }
    }

    fn policy(&self, guild: closure_core::event::GuildId) -> Arc<GuildPolicy> {
        self.inner.policy(guild)
    }

    fn module(
        &self,
        guild: closure_core::event::GuildId,
        module: &ModuleId,
    ) -> Option<ModuleSettings> {
        self.inner.module(guild, module)
    }
}

#[tokio::test(start_paused = true)]
async fn a_changed_timeout_applies_to_the_next_event_without_waiting_for_idle() {
    let hung = messages_only("hung");
    hung.set(Behavior::Hang);
    let registry =
        Arc::new(ModuleRegistry::new(vec![Arc::clone(&hung) as Arc<dyn Module>]).unwrap());
    let settings = Arc::new(Tunable {
        inner: StaticSettings::new([(
            hung.manifest.id.clone(),
            ModuleSettings {
                enabled: true,
                config: Arc::new(json!({})),
                granted: CapabilitySet::new(),
            },
        )]),
        timeout: Mutex::new(Duration::from_secs(30)),
    });
    let dispatcher = Dispatcher::start(
        registry,
        Arc::clone(&settings) as Arc<dyn SettingsProvider>,
        Arc::new(MockApi::default()),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
        DispatcherConfig::default(),
    )
    .await
    .unwrap();

    let started = tokio::time::Instant::now();
    dispatcher.dispatch(message(3, 1));
    dispatcher.flush().await;
    assert!(started.elapsed() >= Duration::from_secs(30));

    *settings.timeout.lock().unwrap() = Duration::from_millis(100);
    let started = tokio::time::Instant::now();
    dispatcher.dispatch(message(3, 2));
    dispatcher.flush().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the new timeout should apply to the lanes that already exist: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn nothing_is_dispatched_after_shutdown_and_no_lanes_are_left_behind() {
    let probe = messages_only("late");
    let h = harness(
        &[(&probe, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;
    h.dispatcher.dispatch(message(3, 1));
    assert!(h.dispatcher.shutdown(Duration::from_secs(5)).await);

    h.dispatcher.dispatch(message(4, 2));
    h.dispatcher.dispatch(command("ping", None));
    assert_eq!(
        h.dispatcher.active_guilds(),
        0,
        "a late event must not create lanes"
    );
    assert_eq!(h.dispatcher.pending(), 0);
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
async fn modules_are_given_the_guilds_policy() {
    let probe = messages_only("policy");
    probe.set(Behavior::ReadPolicy);
    let registry =
        Arc::new(ModuleRegistry::new(vec![Arc::clone(&probe) as Arc<dyn Module>]).unwrap());
    let mut settings = StaticSettings::new([(
        probe.manifest.id.clone(),
        ModuleSettings {
            enabled: true,
            config: Arc::new(json!({})),
            granted: CapabilitySet::new(),
        },
    )]);
    settings.policy = Arc::new(GuildPolicy {
        log_channel: Some(Id::new(9)),
        moderator_roles: vec![Id::new(5)],
    });
    let dispatcher = Dispatcher::start(
        registry,
        Arc::new(settings),
        Arc::new(MockApi::default()),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
        DispatcherConfig::default(),
    )
    .await
    .unwrap();

    dispatcher.dispatch(message(3, 1));
    dispatcher.flush().await;

    assert_eq!(probe.log(), ["load", "msg:1", "policy:Some(9):true"]);
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
async fn after_the_grace_period_nothing_keeps_running_while_modules_are_unloaded() {
    let slow = messages_only("slow");
    slow.set(Behavior::Hang);
    let mut long = limits(64, 1);
    long.handler_timeout = Duration::from_secs(3600);
    let h = harness(&[(&slow, true)], long, DispatcherConfig::default()).await;

    // The first event hangs; the second waits behind it in the same lane.
    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.dispatch(message(3, 2));
    while slow.running() == 0 {
        tokio::task::yield_now().await;
    }

    let drained = h.dispatcher.shutdown(Duration::from_millis(100)).await;

    assert!(!drained, "the hung call could not drain");
    assert_eq!(
        slow.running(),
        0,
        "the running call must be gone before on_unload"
    );
    assert_eq!(
        slow.log(),
        ["load", "msg:1", "unload"],
        "the queued event never started"
    );
    assert_eq!(
        h.dispatcher.pending(),
        0,
        "the discarded event is counted off"
    );
    tokio::time::timeout(Duration::from_secs(1), h.dispatcher.flush())
        .await
        .expect("flush must not wait for events that were thrown away");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_unload_starts_only_after_an_aborted_handler_is_completely_gone() {
    let slow = messages_only("slow");
    slow.set(Behavior::SlowToStop);
    let mut long = limits(64, 1);
    long.handler_timeout = Duration::from_secs(3600);
    let h = harness(&[(&slow, true)], long, DispatcherConfig::default()).await;

    h.dispatcher.dispatch(message(3, 1));
    while slow.running() == 0 {
        tokio::task::yield_now().await;
    }

    let drained = h.dispatcher.shutdown(Duration::from_millis(50)).await;

    assert!(!drained);
    assert_eq!(
        slow.running_at_unload.load(Ordering::SeqCst),
        0,
        "the handler's destructors were still running when on_unload started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_module_that_blocks_a_thread_gets_no_further_calls_until_it_is_back() {
    let blocker = messages_only("blocker");
    let healthy = messages_only("healthy");
    // Blocks for longer than the time an aborted task is given to stop.
    blocker.set(Behavior::BlockThread(
        TASK_STOP_WAIT + Duration::from_millis(1200),
    ));
    let mut short = limits(64, 4);
    short.handler_timeout = Duration::from_millis(50);
    let h = harness(
        &[(&blocker, true), (&healthy, true)],
        short,
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(message(3, 1));
    // The call overruns its timeout, cannot be stopped, and is given up on.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !h.dispatcher.is_wedged(&blocker.manifest.id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the module is noticed to be blocking");
    assert!(!h.dispatcher.is_wedged(&healthy.manifest.id));

    // Other events go on, to the others, and the blocked module is not asked again.
    h.dispatcher.dispatch(message(4, 2));
    h.dispatcher.dispatch(message(5, 3));
    h.dispatcher.flush().await;
    // Two channels, so two lanes: in no particular order.
    let mut seen = healthy.log();
    seen.sort();
    assert_eq!(seen, ["load", "msg:1", "msg:2", "msg:3"]);
    assert_eq!(blocker.log(), ["load", "msg:1"]);

    // Once the call has returned, the module is asked again.
    tokio::time::timeout(Duration::from_secs(10), async {
        while h.dispatcher.is_wedged(&blocker.manifest.id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the module is let back in");
    blocker.set(Behavior::Ok);
    h.dispatcher.dispatch(message(6, 4));
    h.dispatcher.flush().await;
    assert_eq!(blocker.log(), ["load", "msg:1", "msg:4"]);
}

#[tokio::test]
async fn a_full_queue_is_one_line_of_log_however_many_events_it_drops() {
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let held = messages_only("held");
    let gate = Arc::new(Semaphore::new(0));
    held.set(Behavior::Gate(Arc::clone(&gate)));
    let h = harness(&[(&held, true)], limits(8, 1), DispatcherConfig::default()).await;

    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer({
            let captured = Arc::clone(&captured);
            move || Capture(Arc::clone(&captured))
        })
        .finish();
    // `dispatch` logs on the calling thread.
    tracing::subscriber::with_default(subscriber, || {
        for id in 1..=1000 {
            h.dispatcher.dispatch(message(3, id));
        }
    });

    let log = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let lines = log.matches("the guild's queue is full").count();
    assert_eq!(lines, 1, "{log}");
    gate.add_permits(10_000);
    h.dispatcher.flush().await;
}

/// A module that only counts, for measuring the dispatcher itself.
struct Counting {
    manifest: ModuleManifest,
    handled: AtomicUsize,
    work: Duration,
}

#[async_trait]
impl Module for Counting {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn on_event(&self, _: &EventContext, _: &Event) -> Result<(), ModuleError> {
        if !self.work.is_zero() {
            tokio::time::sleep(self.work).await;
        }
        self.handled.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

async fn counting_dispatcher(work: Duration, limits: RuntimeLimits) -> (Dispatcher, Arc<Counting>) {
    let manifest = ModuleManifest::new(ModuleId::new("counting").unwrap(), "counting", "0.1.0")
        .events([EventKind::MessageCreated]);
    let module = Arc::new(Counting {
        manifest,
        handled: AtomicUsize::new(0),
        work,
    });
    let registry =
        Arc::new(ModuleRegistry::new(vec![Arc::clone(&module) as Arc<dyn Module>]).unwrap());
    let mut settings = StaticSettings::new([(
        module.manifest.id.clone(),
        ModuleSettings {
            enabled: true,
            config: Arc::new(json!({})),
            granted: CapabilitySet::default(),
        },
    )]);
    settings.limits = limits;
    let dispatcher = Dispatcher::start(
        registry,
        Arc::new(settings),
        Arc::new(MockApi::default()),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
        DispatcherConfig::default(),
    )
    .await
    .unwrap();
    (dispatcher, module)
}

/// How many of `count` per second, when it took `elapsed`.
fn per_second(count: usize, elapsed: Duration) -> u128 {
    u128::try_from(count).unwrap_or(0) * 1_000_000 / elapsed.as_micros().max(1)
}

/// Not a test of behaviour: prints what the dispatcher manages on this machine. Run with
/// `cargo test --release -p closure-discord --lib measure -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test"]
async fn measure_throughput_of_events_that_need_no_work() {
    let (queue, module) = counting_dispatcher(Duration::ZERO, limits(10_000, 4)).await;
    let (guilds, per_guild) = (64_u64, 5_000_u64);
    let events = usize::try_from(guilds * per_guild).unwrap();

    let started = Instant::now();
    for id in 1..=per_guild {
        for guild in 1..=guilds {
            queue.dispatch(message_in(guild, 3, id));
        }
    }
    let sent_in = started.elapsed();
    queue.flush().await;
    let all_in = started.elapsed();

    let handled = module.handled.load(Ordering::Relaxed);
    println!(
        "{events} events over {guilds} guilds: accepted {handled}, dispatched in {sent_in:?} ({}/s), all handled in {all_in:?} ({}/s)",
        per_second(events, sent_in),
        per_second(handled, all_in),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test"]
async fn measure_one_guild_flooding_a_module_that_takes_its_time() {
    // What a raid does: one guild, far more events than its queue holds, a module that needs
    // 2 ms for each.
    for (name, capacity, lanes) in [
        ("small", 200, 2),
        ("medium", 1_000, 4),
        ("large", 5_000, 16),
    ] {
        let (queue, module) =
            counting_dispatcher(Duration::from_millis(2), limits(capacity, lanes)).await;
        let events = 1_000_000_usize;

        let started = Instant::now();
        for id in 1..=events {
            let id = u64::try_from(id).unwrap();
            queue.dispatch(message_in(1, 3 + id % 64, id));
        }
        let sent_in = started.elapsed();
        let waiting = queue.pending();
        queue.flush().await;
        let all_in = started.elapsed();

        let handled = module.handled.load(Ordering::Relaxed);
        println!(
            "{name} (queue {capacity}, {lanes} lanes): {events} events dispatched in {sent_in:?} ({}/s); {waiting} were waiting, {handled} handled, {} dropped; drained after {all_in:?}",
            per_second(events, sent_in),
            events - handled,
        );
        assert!(waiting <= capacity, "the queue is bounded");
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

#[tokio::test]
async fn a_send_that_already_took_its_capacity_is_still_collected_after_close() {
    let (sender, mut receiver) = mpsc::channel::<u32>(4);
    // Capacity taken, message not pushed yet: what `try_send` is in the middle of doing.
    let permit = sender.try_reserve().unwrap();

    let mut collected = Vec::new();
    {
        let drain = drain_closed(&mut receiver, |message| collected.push(message));
        let mut drain = std::pin::pin!(drain);
        // One poll runs `close()` and then waits for the sender in progress.
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx)));
        assert!(
            first.await.is_pending(),
            "must wait for the sender in progress"
        );

        permit.send(7);
        drain.await;
    }

    assert_eq!(collected, [7]);
}

#[tokio::test]
async fn a_module_can_answer_the_interaction_it_was_given_but_not_one_it_was_not() {
    let mut module = Probe::with_command("tools", "ping");
    Arc::get_mut(&mut module).unwrap().manifest.capabilities =
        [Capability::RespondToInteractions].into_iter().collect();
    module.set(Behavior::RespondThenForge);
    let h = harness(
        &[(&module, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(command("ping", None));
    h.dispatcher.flush().await;

    assert_eq!(module.log(), ["load", "command", "own:true forged:true"]);
    assert_eq!(
        h.api.actions().len(),
        1,
        "only the module's own answer was sent"
    );
}

#[tokio::test]
async fn a_client_kept_from_one_guild_cannot_answer_an_interaction_of_another() {
    // One module, shared by both guilds, keeps the client it got for a message in guild 1 and
    // uses it, also with a record of its own making, on a command that arrives in guild 2.
    let mut module = Probe::new(
        "tools",
        &[EventKind::MessageCreated],
        &[Capability::RespondToInteractions],
    );
    Arc::get_mut(&mut module)
        .unwrap()
        .manifest
        .commands
        .push(CommandSpec {
            name: "ping".into(),
            description: "d".into(),
            options: vec![],
            subcommands: vec![],
            default_member_permissions: None,
        });
    module.set(Behavior::HoldThenReplay);
    let h = harness(
        &[(&module, true)],
        limits(64, 4),
        DispatcherConfig::default(),
    )
    .await;

    h.dispatcher.dispatch(message(3, 1));
    h.dispatcher.flush().await;
    let Event::Command(mut invocation) = command("ping", None) else {
        unreachable!()
    };
    invocation.guild_id = Id::new(2);
    h.dispatcher.dispatch(Event::Command(invocation));
    h.dispatcher.flush().await;

    assert_eq!(
        module.log(),
        ["load", "msg:1", "command", "plain:true forged:true"]
    );
    assert_eq!(h.api.actions(), [], "nothing was sent to Discord");
}

/// A command of `guild` that carries its own interaction token.
fn command_with(guild: u64, token: &str) -> Event {
    let Event::Command(mut invocation) = command("ping", None) else {
        unreachable!()
    };
    invocation.guild_id = Id::new(guild);
    invocation.token = InteractionToken::new(token);
    Event::Command(invocation)
}

#[tokio::test]
async fn a_command_whose_interaction_cannot_be_recorded_is_not_handed_to_a_module() {
    let module = Probe::with_command("tools", "ping");
    let config = DispatcherConfig {
        interaction_limits: InteractionLimits {
            total: 100,
            per_guild: 1,
        },
        ..DispatcherConfig::default()
    };
    // A queue budget of one command: a reservation that is not given back shows up as a drop.
    let h = harness(&[(&module, true)], limits(1, 4), config).await;

    h.dispatcher.dispatch(command_with(1, "first"));
    h.dispatcher.flush().await;
    h.dispatcher.dispatch(command_with(1, "second")); // over the limit of guild 1
    h.dispatcher.flush().await;
    // The very same interaction again is accepted (it is recorded), but only if the refused
    // command above gave its place in the queue back.
    h.dispatcher.dispatch(command_with(1, "first"));
    h.dispatcher.flush().await;
    h.dispatcher.dispatch(command_with(2, "other-guild"));
    h.dispatcher.flush().await;

    let handled = module.log().iter().filter(|l| *l == "command").count();
    assert_eq!(
        handled, 3,
        "first, first again and the other guild; not second"
    );
    assert_eq!(h.dispatcher.pending(), 0);
    // What is kept per guild must be back at zero too: it decides when lanes are released.
    for guild in [1, 2] {
        let lanes = h.dispatcher.shared.guilds.get(&Id::new(guild)).unwrap();
        assert_eq!(
            lanes.state.pending.load(Ordering::Acquire),
            0,
            "guild {guild}"
        );
        assert_eq!(
            lanes.state.queued_commands.load(Ordering::Acquire),
            0,
            "guild {guild}"
        );
    }
}
