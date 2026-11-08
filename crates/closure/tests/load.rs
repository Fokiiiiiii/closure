//! Load test: several large servers take ordinary chat, then a raid (a flood of joins with spam),
//! then spam that goes on. The real dispatcher, every shipped module and a real PostgreSQL run
//! against a pretend Discord. It prints what was accepted, what was shed and how long the backlog
//! took to clear.
//!
//! It is ignored by default. Run it on an optimised build:
//!
//! ```text
//! DATABASE_URL='postgres:///postgres?host=/tmp&user=chaos' \
//!   cargo test -p closure --release --test load -- --ignored --nocapture
//! ```
//!
//! It makes its own database and drops it at the end. The sizes are set by the environment:
//! `LOAD_GUILDS` (5), `LOAD_MEMBERS` (300000 distinct accounts per server), `LOAD_SCALE` (1.0, a
//! factor on every rate) and `LOAD_SECONDS` (a factor on every phase length, 1.0).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a load test reports to the terminal and panics when its setup is wrong"
)]

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use closure::modules;
use closure_config::{ConfigCompiler, SettingsStore, parse_overrides};
use closure_core::action::{Action, Outcome};
use closure_core::cases::CaseBackend;
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::{
    ChannelId, Event, GuildId, MemberInfo, MessageId, MessageInfo, UserId, UserInfo,
};
use closure_core::history::{HistoryBackend, SavedMessage};
use closure_core::registry::ModuleRegistry;
use closure_core::schedule::SchedulerBackend;
use closure_core::settings::SettingsProvider;
use closure_core::storage::StorageBackend;
use closure_core::testing::MockApi;
use closure_db::Db;
use closure_discord::{Dispatcher, DispatcherConfig};
use closure_i18n::TranslatorBuilder;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Semaphore;

const HEAT: &str = include_str!("../../../docs/examples/heat.yaml");
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
const CHANNELS_PER_GUILD: u64 = 20;
const FIRST_GUILD: u64 = 5_000_000;
const WRITERS: usize = 48;

fn env_number(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A small, fast generator: the load needs variety, not quality.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

/// What one phase asks of each server, per second.
#[derive(Clone, Copy)]
struct Phase {
    name: &'static str,
    seconds: f64,
    chat: f64,
    joins: f64,
    spam: f64,
}

/// Everything the generators share.
struct Counters {
    sent: AtomicU64,
    accepted: AtomicU64,
    shed: AtomicU64,
    ids: AtomicU64,
}

/// Discord as the bot meets it: every request waits for its turn under the limit of the whole bot
/// (50 a second unless told otherwise) and then takes a while to be answered.
struct Limited {
    inner: Arc<MockApi>,
    spacing: Duration,
    latency: Duration,
    next: tokio::sync::Mutex<tokio::time::Instant>,
    calls: AtomicU64,
}

#[async_trait]
impl DiscordApi for Limited {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        if !self.spacing.is_zero() {
            let at = {
                let mut next = self.next.lock().await;
                let at = (*next).max(tokio::time::Instant::now());
                *next = at + self.spacing;
                at
            };
            tokio::time::sleep_until(at).await;
        }
        tokio::time::sleep(self.latency).await;
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.execute(action).await
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }

    async fn guild_owner(&self, guild: GuildId) -> Result<Option<UserId>, DiscordError> {
        self.inner.guild_owner(guild).await
    }
}

fn heat_config() -> Value {
    let indented = HEAT
        .lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!("modules:\n  automod:\n    config:\n{indented}\n");
    let mut config = parse_overrides(&yaml)
        .unwrap()
        .modules
        .get("automod")
        .and_then(|module| module.config.clone())
        .expect("the heat example has a config");
    let rules = config["rules"].as_object_mut().unwrap();
    for (name, rule) in [
        (
            "invites",
            json!({ "triggers": [{ "type": "invites" }],
                    "actions": [{ "do": "delete" }, { "do": "timeout", "duration": "1h" }] }),
        ),
        (
            "scam_words",
            json!({ "triggers": [{ "type": "words", "words": ["free nitro", "airdrop"] }],
                    "actions": [{ "do": "delete" }, { "do": "warn", "reason": "scam" }] }),
        ),
        (
            "flood",
            json!({ "triggers": [{ "type": "spam", "measure": "messages", "amount": 8, "within": "10s" }],
                    "actions": [{ "do": "delete" }, { "do": "timeout", "duration": "10m" }] }),
        ),
        (
            "mentions",
            json!({ "triggers": [{ "type": "spam", "measure": "mentions", "amount": 20, "within": "1m" }],
                    "actions": [{ "do": "delete" }, { "do": "ban", "delete_days": 1, "reason": "mention spam" }] }),
        ),
        (
            "raid_burst",
            json!({ "triggers": [{ "type": "member_join_spam", "amount": 200, "within": "10s" }],
                    "actions": [{ "do": "kick", "reason": "raid" }] }),
        ),
    ] {
        rules.insert(name.to_owned(), rule);
    }
    config
}

fn guild_yaml(log_channel: u64) -> String {
    let channel = log_channel.to_string();
    serde_json::to_string(&json!({
        "modules": {
            "automod": { "enabled": true, "config": heat_config() },
            "logs": { "enabled": true, "config": { "channel": channel } },
            "welcome": { "enabled": true, "config": { "channel": channel, "message": "hi {username}" } },
            "guard": { "enabled": true },
            "persist": { "enabled": true },
            "dehoist": { "enabled": true },
            "honeypot": { "enabled": true },
            "moderation": { "enabled": true },
        }
    }))
    .unwrap()
}

fn guild_id(index: u64) -> GuildId {
    GuildId::new(FIRST_GUILD + index)
}

fn chat_message(counters: &Counters, rng: &mut Rng, guild: u64, members: u64) -> MessageInfo {
    let author = 1_000_000 + rng.below(members);
    let channel = guild * 100 + 10 + rng.below(CHANNELS_PER_GUILD);
    let words = [
        "hello", "anyone", "playing", "tonight", "gg", "lol", "thanks", "maybe",
    ];
    let content = (0..3 + rng.below(8))
        .map(|_| words[rng.below(words.len() as u64) as usize])
        .collect::<Vec<_>>()
        .join(" ");
    message(counters, guild, channel, author, content, Vec::new())
}

fn spam_message(counters: &Counters, rng: &mut Rng, guild: u64, members: u64) -> MessageInfo {
    // A few hundred accounts spam, so that per-account windows fill up.
    let author = 9_000_000 + rng.below(400);
    let channel = guild * 100 + 10 + rng.below(CHANNELS_PER_GUILD);
    match rng.below(4) {
        0 => message(
            counters,
            guild,
            channel,
            author,
            format!("join us discord.gg/raid{}", rng.below(50)),
            Vec::new(),
        ),
        1 => message(
            counters,
            guild,
            channel,
            author,
            "FREE NITRO airdrop, click now".to_owned(),
            Vec::new(),
        ),
        2 => {
            let mentioned = (0..25)
                .map(|_| 1_000_000 + rng.below(members))
                .collect::<Vec<_>>();
            message(
                counters,
                guild,
                channel,
                author,
                "look".to_owned(),
                mentioned,
            )
        }
        _ => message(
            counters,
            guild,
            channel,
            author,
            "the same line again and again".to_owned(),
            Vec::new(),
        ),
    }
}

fn message(
    counters: &Counters,
    guild: u64,
    channel: u64,
    author: u64,
    content: String,
    mentioned: Vec<u64>,
) -> MessageInfo {
    MessageInfo {
        id: MessageId::new(counters.ids.fetch_add(1, Ordering::Relaxed)),
        channel_id: ChannelId::new(channel),
        guild_id: guild_id(guild),
        author: UserInfo {
            id: UserId::new(author),
            name: "member".into(),
            bot: false,
        },
        author_roles: Vec::new(),
        content,
        mentioned_users: mentioned.into_iter().map(UserId::new).collect(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments: Vec::new(),
        removed_attachments: Vec::new(),
        previous_content: None,
        embed_count: 0,
        embeds_text: String::new(),
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: i64::try_from(now_ms() / 1000).unwrap(),
        edited_at: None,
    }
}

/// An account made just now.
fn join(counters: &Counters, guild: u64) -> Event {
    let id = ((now_ms() - DISCORD_EPOCH_MS) << 22)
        | (counters.ids.fetch_add(1, Ordering::Relaxed) & 0x3f_ffff);
    Event::MemberJoined(MemberInfo {
        guild_id: guild_id(guild),
        user: UserInfo {
            id: UserId::new(id),
            name: "newcomer".into(),
            bot: false,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: Some(i64::try_from(now_ms() / 1000).unwrap()),
        timed_out_until: None,
    })
}

/// Runs one server's side of a phase: events at the asked rates, in steps of 10 ms.
async fn drive(
    dispatcher: Dispatcher,
    db: Arc<Db>,
    counters: Arc<Counters>,
    writers: Arc<Semaphore>,
    guild: u64,
    members: u64,
    phase: Phase,
    scale: f64,
) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (guild + 1));
    let started = Instant::now();
    let mut due = [0.0_f64; 3];
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    while started.elapsed().as_secs_f64() < phase.seconds {
        tick.tick().await;
        for (slot, rate) in [phase.chat, phase.joins, phase.spam]
            .into_iter()
            .enumerate()
        {
            due[slot] += rate * scale * 0.01;
        }
        let whole = due.map(|count| count as u64);
        for (slot, count) in whole.into_iter().enumerate() {
            due[slot] -= count as f64;
            for _ in 0..count {
                let (event, saved) = match slot {
                    0 => {
                        let info = chat_message(&counters, &mut rng, guild, members);
                        let saved = SavedMessage::from_info(&info);
                        (Event::MessageCreated(info), Some(saved))
                    }
                    1 => (join(&counters, guild), None),
                    _ => {
                        let info = spam_message(&counters, &mut rng, guild, members);
                        let saved = SavedMessage::from_info(&info);
                        (Event::MessageCreated(info), Some(saved))
                    }
                };
                counters.sent.fetch_add(1, Ordering::Relaxed);
                if dispatcher.try_dispatch(event) {
                    counters.accepted.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.shed.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(saved) = saved
                    && let Ok(permit) = Arc::clone(&writers).try_acquire_owned()
                {
                    let db = Arc::clone(&db);
                    tokio::spawn(async move {
                        let _ = db.save(&saved).await;
                        drop(permit);
                    });
                }
            }
        }
    }
}

fn resident_megabytes() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .map_or(0.0, |kilobytes| kilobytes / 1024.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a load test: run it on purpose, see the module documentation"]
async fn several_large_servers_take_a_raid_and_then_spam() {
    let guilds = env_number("LOAD_GUILDS", 5.0) as u64;
    let members = env_number("LOAD_MEMBERS", 300_000.0) as u64;
    let scale = env_number("LOAD_SCALE", 1.0);
    let length = env_number("LOAD_SECONDS", 1.0);
    let drain_limit = env_number("LOAD_DRAIN", 60.0);

    // A database of its own.
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a test server");
    let admin_options = PgConnectOptions::from_str(&url).unwrap();
    // Only digits and a fixed prefix go into the statements below.
    let name = format!("closure_load_{}", std::process::id());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(admin_options.clone())
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(24)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(admin_options.clone().database(&name))
        .await
        .unwrap();
    let db = Arc::new(Db::from_pool(pool.clone()));
    db.migrate().await.unwrap();

    // The real modules, with settings for every server.
    let registry = Arc::new(ModuleRegistry::new(modules().unwrap()).unwrap());
    let mut catalogs = TranslatorBuilder::new().unwrap();
    catalogs.add_registry(&registry).unwrap();
    let store = Arc::new(SettingsStore::new(
        ConfigCompiler::new(Arc::clone(&registry)).unwrap(),
    ));
    let api = Arc::new(MockApi::default());
    let rps = env_number("LOAD_API_RPS", 50.0);
    let limited = Arc::new(Limited {
        inner: Arc::clone(&api),
        spacing: if rps > 0.0 {
            Duration::from_secs_f64(1.0 / rps)
        } else {
            Duration::ZERO
        },
        latency: Duration::from_millis(env_number("LOAD_API_MS", 60.0) as u64),
        next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        calls: AtomicU64::new(0),
    });
    for guild in 0..guilds {
        let log_channel = guild * 100 + 5;
        store
            .set_guild_yaml(guild_id(guild), &guild_yaml(log_channel))
            .unwrap_or_else(|error| panic!("the settings of a server are invalid: {error}"));
        db.register_guild(guild_id(guild)).await.unwrap();
        api.set_guild_name(guild_id(guild), "large server");
        api.add_channel(ChannelId::new(log_channel), Some(guild_id(guild)));
        for channel in 0..CHANNELS_PER_GUILD {
            api.add_channel(
                ChannelId::new(guild * 100 + 10 + channel),
                Some(guild_id(guild)),
            );
        }
    }
    let dispatcher = Dispatcher::start_with_services(
        registry,
        Arc::clone(&store) as Arc<dyn SettingsProvider>,
        Arc::clone(&limited) as Arc<dyn DiscordApi>,
        Arc::new(catalogs.build()),
        DispatcherConfig {
            bot_id: Some(UserId::new(1)),
            ..DispatcherConfig::default()
        },
        Arc::clone(&db) as Arc<dyn StorageBackend>,
        Arc::clone(&db) as Arc<dyn SchedulerBackend>,
        Arc::clone(&db) as Arc<dyn CaseBackend>,
        Arc::clone(&db) as Arc<dyn HistoryBackend>,
    )
    .await
    .unwrap();
    for guild in 0..guilds {
        dispatcher.dispatch(Event::GuildAvailable {
            guild_id: guild_id(guild),
            name: "large server".into(),
            preferred_locale: None,
        });
    }

    let counters = Arc::new(Counters {
        sent: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
        shed: AtomicU64::new(0),
        ids: AtomicU64::new(1_000_000_000),
    });
    let writers = Arc::new(Semaphore::new(WRITERS));
    let peak_pending = Arc::new(AtomicUsize::new(0));
    let peak_memory = Arc::new(AtomicU64::new(0));

    // Samples the backlog and the memory while the phases run.
    let sampler = {
        let (dispatcher, peak_pending, peak_memory) = (
            dispatcher.clone(),
            Arc::clone(&peak_pending),
            Arc::clone(&peak_memory),
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                peak_pending.fetch_max(dispatcher.pending(), Ordering::Relaxed);
                peak_memory.fetch_max(resident_megabytes() as u64, Ordering::Relaxed);
            }
        })
    };

    let phases = [
        Phase {
            name: "ordinary chat",
            seconds: 15.0 * length,
            chat: 100.0,
            joins: 5.0,
            spam: 0.0,
        },
        Phase {
            name: "raid",
            seconds: 10.0 * length,
            chat: 100.0,
            joins: 2_000.0,
            spam: 400.0,
        },
        Phase {
            name: "spam goes on",
            seconds: 15.0 * length,
            chat: 100.0,
            joins: 20.0,
            spam: 1_500.0,
        },
    ];
    println!(
        "\n{guilds} servers, {members} accounts each, rates x{scale}, memory at start {:.0} MB",
        resident_megabytes()
    );
    println!(
        "{:<14} {:>8} {:>9} {:>9} {:>7} {:>11} {:>9} {:>8}",
        "phase", "seconds", "sent", "accepted", "shed", "backlog max", "drain s", "left"
    );
    let overall = Instant::now();
    for phase in phases {
        let before = (
            counters.sent.load(Ordering::Relaxed),
            counters.accepted.load(Ordering::Relaxed),
            counters.shed.load(Ordering::Relaxed),
        );
        peak_pending.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let drivers: Vec<_> = (0..guilds)
            .map(|guild| {
                tokio::spawn(drive(
                    dispatcher.clone(),
                    Arc::clone(&db),
                    Arc::clone(&counters),
                    Arc::clone(&writers),
                    guild,
                    members,
                    phase,
                    scale,
                ))
            })
            .collect();
        for driver in drivers {
            driver.await.unwrap();
        }
        let sending = started.elapsed();
        let draining = Instant::now();
        while dispatcher.pending() > 0 && draining.elapsed() < Duration::from_secs_f64(drain_limit)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let drain = draining.elapsed().as_secs_f64();
        let (sent, accepted, shed) = (
            counters.sent.load(Ordering::Relaxed) - before.0,
            counters.accepted.load(Ordering::Relaxed) - before.1,
            counters.shed.load(Ordering::Relaxed) - before.2,
        );
        println!(
            "{:<14} {:>8.1} {:>9} {:>9} {:>7} {:>11} {:>9.1} {:>8}",
            phase.name,
            sending.as_secs_f64(),
            sent,
            accepted,
            shed,
            peak_pending.load(Ordering::Relaxed),
            drain,
            dispatcher.pending()
        );
    }
    sampler.abort();

    // What the modules did about it.
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for action in api.actions() {
        *by_kind.entry(action.name().to_owned()).or_default() += 1;
    }
    println!(
        "\nwhat the modules did ({} actions):",
        by_kind.values().sum::<usize>()
    );
    for (kind, count) in &by_kind {
        println!("  {kind:<28} {count}");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT relname::text, n_tup_ins FROM pg_stat_user_tables WHERE n_tup_ins > 0 ORDER BY n_tup_ins DESC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    println!("\nrows written:");
    for (table, count) in rows {
        println!("  {table:<28} {count}");
    }
    let wedged: Vec<String> = modules()
        .unwrap()
        .iter()
        .map(|module| module.manifest().id.clone())
        .filter(|id| dispatcher.is_wedged(id))
        .map(|id| id.to_string())
        .collect();
    println!(
        "\ntotal {:.1} s, memory peak {} MB, nothing wedged: {}",
        overall.elapsed().as_secs_f64(),
        peak_memory.load(Ordering::Relaxed),
        dispatcher.active_guilds() > 0 && wedged.is_empty()
    );

    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}
