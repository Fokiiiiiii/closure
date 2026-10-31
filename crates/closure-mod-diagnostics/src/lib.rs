//! The `diagnostics` module: `/ping`.
//!
//! Written with the SDK only, and the smallest complete example of a module: a manifest, message
//! catalogs in every supported language, typed configuration, and commands that answer with
//! embeds.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use closure_core::latency::LatencySummary;
use closure_sdk::{
    Args, Color, Command, CommandCx, CoreError, EmbedBuilder, InteractionReply, Locale,
    ModuleBuilder, SdkModule, UserError,
};
use serde::Deserialize;
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// How many times `/ping` measures the API, the deferral included.
const PINGS: usize = 5;
/// Discord's epoch, 2015-01-01, in Unix milliseconds: the base of every snowflake's time.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
/// How far the clocks may differ before the delay is not shown at all.
const CLOCK_TOLERANCE_MS: u64 = 60_000;

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// Replies are only visible to the person who ran the command.
    ephemeral: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { ephemeral: true }
    }
}

/// How long after Discord created the interaction the bot saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delay {
    /// The local clock is behind Discord's, so the delay is shown as zero.
    ClockBehind,
    /// Whole milliseconds.
    After(u64),
}

/// Unix milliseconds at which Discord created the interaction: the snowflake's time bits.
fn created_ms(interaction_id: u64) -> u64 {
    (interaction_id >> 22) + DISCORD_EPOCH_MS
}

/// The delay from creation to `now_ms`, or `None` when the clocks are too far apart to say.
fn delay(interaction_id: u64, now_ms: u64) -> Option<Delay> {
    let created = created_ms(interaction_id);
    if now_ms >= created {
        let after = now_ms - created;
        (after <= CLOCK_TOLERANCE_MS).then_some(Delay::After(after))
    } else {
        ((created - now_ms) <= CLOCK_TOLERANCE_MS).then_some(Delay::ClockBehind)
    }
}

/// The system clock in Unix milliseconds; zero if it is before the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// A duration in whole milliseconds, rounded to the nearest.
fn millis(duration: Duration) -> u128 {
    (duration.as_micros() + 500) / 1000
}

/// "mean (min–max)" in whole milliseconds.
fn range(mean: Duration, min: Duration, max: Duration) -> String {
    format!("{} ms ({}–{} ms)", millis(mean), millis(min), millis(max))
}

/// A summary as "mean (min–max)", or a dash when nothing was measured.
fn summary_text(summary: Option<LatencySummary>) -> String {
    summary.map_or_else(
        || "—".to_owned(),
        |summary| range(summary.mean, summary.min, summary.max),
    )
}

async fn ping(cx: CommandCx, (): ()) -> Result<(), UserError> {
    // Read before the measured calls, which would otherwise count in the delay.
    let received_ms = now_ms();
    let config: Config = cx.config()?;
    let mut samples = Vec::with_capacity(PINGS);
    let mut last_error = None;
    for call in 1..=PINGS {
        let started = Instant::now();
        // Edits go straight to the API: the SDK's own answer phases would end after the first one.
        let outcome = if call == 1 {
            cx.defer(config.ephemeral).await
        } else {
            let args = Args::new()
                .with("done", call.to_string())
                .with("total", PINGS.to_string());
            let progress = InteractionReply {
                content: Some(cx.t("ping-progress", &args)),
                embeds: Vec::new(),
                components: Vec::new(),
                ephemeral: config.ephemeral,
            };
            cx.api()
                .edit_response(cx.invocation(), progress)
                .await
                .map_err(UserError::from)
        };
        match outcome {
            Ok(()) => samples.push(started.elapsed()),
            Err(error) => last_error = Some(error),
        }
    }
    if samples.is_empty()
        && let Some(error) = last_error
    {
        return Err(error);
    }

    let calls = cx.t(
        "ping-calls",
        &Args::new()
            .with("succeeded", samples.len().to_string())
            .with("total", PINGS.to_string()),
    );
    let mut embed = EmbedBuilder::new()
        .title(cx.t("ping-title", &Args::new()))
        .color(Color::Success)
        .field("Gateway", summary_text(cx.ctx().gateway_latency()), true)
        .field(
            "API",
            format!("{}\n{calls}", summary_text(LatencySummary::of(&samples))),
            true,
        );
    if let Some(delay) = delay(cx.invocation().interaction_id.get(), received_ms) {
        let text = match delay {
            Delay::ClockBehind => cx.t("ping-clock-behind", &Args::new()),
            Delay::After(ms) => format!("{ms} ms"),
        };
        embed = embed.field("Delay", text, true);
    }
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed.build()?],
        components: Vec::new(),
        ephemeral: config.ephemeral,
    })
    .await
}

/// The module's declaration; the bot builds it with the other modules.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("diagnostics", "Diagnostics", env!("CARGO_PKG_VERSION"))?
            .description("Liveness and version commands.")
            .config(
                json!({
                    "type": "object",
                    "properties": {
                        "ephemeral": {
                            "type": "boolean",
                            "description": "Only show replies to the person who ran the command.",
                            "x-ui": { "level": "basic" }
                        }
                    },
                    "additionalProperties": false
                }),
                json!({ "ephemeral": true }),
            )
            .command(Command::<()>::new("ping", "Check that the bot is alive").handler(ping))
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

/// The module, ready to be registered.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use closure_core::latency::LatencySummary;
    use closure_core::module::Module;
    use closure_sdk::testing::{Harness, Invocation, Reply, ReplyKind};
    use closure_sdk::{Capability, DiscordError, Locale};
    use serde_json::json;

    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// An interaction id that Discord would have created at `unix_ms`, with some low bits.
    fn snowflake_at(unix_ms: u64) -> u64 {
        ((unix_ms - DISCORD_EPOCH_MS) << 22) | 0x3039
    }

    fn harness() -> Harness {
        Harness::new(builder().unwrap()).unwrap()
    }

    fn field(reply: &Reply, name: &str) -> Option<String> {
        reply.embeds[0]
            .fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.clone())
    }

    #[test]
    fn the_delay_is_the_time_since_discord_created_the_interaction() {
        let created = 1_700_000_000_000;
        assert_eq!(
            delay(snowflake_at(created), created + 250),
            Some(Delay::After(250))
        );
    }

    #[test]
    fn a_clock_behind_the_interaction_shows_a_zero_delay() {
        let created = 1_700_000_000_000;
        assert_eq!(
            delay(snowflake_at(created), created - 1_000),
            Some(Delay::ClockBehind)
        );
    }

    #[test]
    fn a_clock_more_than_a_minute_off_hides_the_delay() {
        let created = 1_700_000_000_000;
        let id = snowflake_at(created);
        assert_eq!(delay(id, created + 60_000), Some(Delay::After(60_000)));
        assert_eq!(delay(id, created + 60_001), None);
        assert_eq!(delay(id, created - 60_000), Some(Delay::ClockBehind));
        assert_eq!(delay(id, created - 60_001), None);
    }

    #[test]
    fn milliseconds_are_rounded_to_the_nearest() {
        assert_eq!(millis(Duration::from_micros(1_499)), 1);
        assert_eq!(millis(Duration::from_micros(1_500)), 2);
    }

    #[test]
    fn a_range_reads_the_mean_then_the_bounds() {
        assert_eq!(range(ms(12), ms(10), ms(15)), "12 ms (10–15 ms)");
    }

    #[test]
    fn an_unmeasured_gateway_shows_a_dash() {
        assert_eq!(summary_text(None), "—");
        let summary = LatencySummary::of(&[ms(10), ms(20)]);
        assert_eq!(summary_text(summary), "15 ms (10–20 ms)");
    }

    #[tokio::test]
    async fn ping_defers_then_edits_five_measurements_into_an_embed() {
        let h = harness();
        let replies = h.invoke(Invocation::new("ping")).await.unwrap();
        let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
        assert_eq!(
            kinds,
            [
                ReplyKind::Deferred,
                ReplyKind::Edit,
                ReplyKind::Edit,
                ReplyKind::Edit,
                ReplyKind::Edit,
                ReplyKind::Edit,
            ]
        );
        let last = replies.last().unwrap();
        assert_eq!(last.embeds[0].title.as_deref(), Some("Pong!"));
        assert_eq!(field(last, "Gateway").as_deref(), Some("—"));
        assert!(field(last, "API").unwrap().contains("5 of 5 succeeded"));
        // The harness's interaction ids are not live snowflakes, so the clock check hides it.
        assert_eq!(field(last, "Delay"), None);
    }

    #[tokio::test]
    async fn the_gateway_value_set_through_the_harness_is_shown() {
        let h = harness();
        h.set_gateway_latency(0, ms(12));
        h.set_gateway_latency(1, ms(20));
        let replies = h.invoke(Invocation::new("ping")).await.unwrap();
        assert_eq!(
            field(replies.last().unwrap(), "Gateway").as_deref(),
            Some("16 ms (12–20 ms)")
        );
    }

    #[tokio::test]
    async fn a_failed_call_is_not_counted() {
        let h = harness();
        h.api().fail_nth(2, DiscordError::Transient("test".into()));
        let replies = h.invoke(Invocation::new("ping")).await.unwrap();
        assert!(
            field(replies.last().unwrap(), "API")
                .unwrap()
                .contains("4 of 5 succeeded")
        );
    }

    #[tokio::test]
    async fn ping_fails_when_no_call_succeeds() {
        let h = harness();
        h.api().fail_with(DiscordError::Fatal("test".into()));
        assert!(h.invoke(Invocation::new("ping")).await.is_err());
    }

    #[tokio::test]
    async fn the_ephemeral_setting_decides_whether_the_reply_is_private() {
        let h = harness();
        assert!(h.invoke(Invocation::new("ping")).await.unwrap()[0].ephemeral);
        h.set_config(json!({ "ephemeral": false }));
        assert!(!h.invoke(Invocation::new("ping")).await.unwrap()[0].ephemeral);
    }

    #[tokio::test]
    async fn the_sdk_replies_setting_overrides_the_module_setting() {
        let h = harness();
        h.set_config(json!({ "ephemeral": true, "replies": "public" }));
        assert!(!h.invoke(Invocation::new("ping")).await.unwrap()[0].ephemeral);
        h.set_config(json!({ "ephemeral": false, "replies": "private" }));
        assert!(h.invoke(Invocation::new("ping")).await.unwrap()[0].ephemeral);
    }

    #[tokio::test]
    async fn ping_is_in_japanese_for_a_japanese_invoker() {
        let h = harness();
        let replies = h
            .invoke(Invocation::new("ping").locale(Locale::Ja))
            .await
            .unwrap();
        assert!(replies[1].content.contains("計測中"));
        assert!(
            field(replies.last().unwrap(), "API")
                .unwrap()
                .contains("5 回中 5 回成功")
        );
    }

    #[test]
    fn the_manifest_keeps_its_commands_and_capability() {
        let module = module().unwrap();
        let manifest = module.manifest();
        let names: Vec<&str> = manifest.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["ping"]);
        assert!(
            manifest
                .capabilities
                .contains(Capability::RespondToInteractions)
        );
        assert!(manifest.permissions.is_empty());
    }

    #[test]
    fn both_languages_define_the_same_messages() {
        assert_eq!(
            closure_i18n::missing_keys(EN, JA).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            closure_i18n::missing_keys(JA, EN).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_default_configuration_satisfies_its_own_schema() {
        let manifest = module().unwrap().manifest().clone();
        let config: Config = serde_json::from_value(manifest.default_config).unwrap();
        assert!(config.ephemeral);
    }
}
