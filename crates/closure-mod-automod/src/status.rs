//! `/automod status`: whether dry run is on, how many rules run, and each rule and each invalid
//! rule on a line of its own. Shown privately.

use std::collections::BTreeSet;

use closure_sdk::counters::CounterDef;
use closure_sdk::embed::{Color, EmbedBuilder, truncate};
use closure_sdk::{Args, CommandCx, InteractionReply, UserError};

use crate::config::{Config, Entry, Problem, Rule, action_key, event_key, setup, trigger_key};

/// An embed's description holds at most 4096 characters; a little is kept back.
const DESCRIPTION_CHARS: usize = 3_900;

pub async fn status(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let setup = setup(&config, cx.ctx().store());
    let entries = setup.rules;
    let valid: Vec<&Rule> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Valid(rule) => Some(rule),
            Entry::Invalid { .. } => None,
        })
        .collect();
    let enabled = valid.iter().filter(|rule| rule.enabled).count();
    let disabled = valid.len() - enabled;
    let invalid = entries.len() - valid.len();

    let state = cx.t(
        if config.dry_run {
            "state-on"
        } else {
            "state-off"
        },
        &Args::new(),
    );
    let mut lines = vec![
        cx.t("status-dry-run", &Args::new().with("state", state)),
        cx.t(
            "status-counts",
            &Args::new()
                .with("enabled", count(enabled))
                .with("disabled", count(disabled))
                .with("invalid", count(invalid)),
        ),
    ];
    if valid.is_empty() {
        lines.push(cx.t("status-none", &Args::new()));
    }
    let events = events_of(&cx, &valid);
    for rule in &valid {
        lines.push(rule_line(&cx, rule));
    }
    if !events.is_empty() {
        lines.push(cx.t(
            "status-events",
            &Args::new().with("events", events.join(", ")),
        ));
    }
    match &setup.counters_problem {
        Some(problem) => lines.push(problem_text(&cx, problem)),
        None if !setup.counters.is_empty() => {
            lines.push(cx.t("status-counters-heading", &Args::new()));
            lines.extend(setup.counters.iter().map(|def| counter_line(&cx, def)));
        }
        None => {}
    }
    let problems: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Invalid { label, why } => Some(
                cx.t(
                    "status-invalid",
                    &Args::new()
                        .with("label", label.as_str())
                        .with("reason", problem_text(&cx, why)),
                ),
            ),
            Entry::Valid(_) => None,
        })
        .collect();
    if !problems.is_empty() {
        lines.push(cx.t("status-invalid-heading", &Args::new()));
        lines.extend(problems);
    }

    let embed = EmbedBuilder::new()
        .title(cx.t("status-title", &Args::new()))
        .description(truncate(&lines.join("\n"), DESCRIPTION_CHARS))
        .color(Color::Info)
        .build()?;
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed],
        components: Vec::new(),
        ephemeral: true,
    })
    .await
}

fn rule_line(cx: &CommandCx, rule: &Rule) -> String {
    let triggers = rule
        .triggers
        .iter()
        .map(|trigger| cx.t(trigger_key(trigger), &Args::new()))
        .collect::<Vec<_>>()
        .join(", ");
    let actions = rule
        .actions
        .iter()
        .map(|action| cx.t(action_key(action), &Args::new()))
        .collect::<Vec<_>>()
        .join(", ");
    let cooldown = match rule.cooldown {
        Some(cooldown) => format!(
            " {}",
            cx.t(
                "status-cooldown",
                &Args::new().with("time", cx.format_duration(cooldown)),
            )
        ),
        None => String::new(),
    };
    let key = if rule.enabled {
        "status-rule"
    } else {
        "status-rule-off"
    };
    cx.t(
        key,
        &Args::new()
            .with("id", rule.id.as_str())
            .with("name", rule.name.as_str())
            .with("triggers", triggers)
            .with("actions", actions)
            .with("cooldown", cooldown),
    )
}

fn problem_text(cx: &CommandCx, problem: &Problem) -> String {
    match problem {
        Problem::Shape(error) => cx.t("problem-shape", &Args::new().with("error", error.as_str())),
        Problem::Key(key) => cx.t(key, &Args::new()),
        Problem::Counters(reason) => cx.t(
            "problem-counters-sdk",
            &Args::new().with("reason", reason.as_str()),
        ),
    }
}

/// The events the enabled rules listen to, each once, in the server's language.
fn events_of(cx: &CommandCx, valid: &[&Rule]) -> Vec<String> {
    let keys: BTreeSet<&'static str> = valid
        .iter()
        .filter(|rule| rule.enabled)
        .flat_map(|rule| rule.triggers.iter().map(event_key))
        .collect();
    keys.into_iter()
        .map(|key| cx.t(key, &Args::new()))
        .collect()
}

/// A counter on a line: how it is kept, and its triggers.
fn counter_line(cx: &CommandCx, def: &CounterDef) -> String {
    let scope = match (def.per_user, def.per_channel) {
        (true, true) => cx.t("status-scope-both", &Args::new()),
        (true, false) => cx.t("status-scope-user", &Args::new()),
        (false, true) => cx.t("status-scope-channel", &Args::new()),
        (false, false) => cx.t("status-scope-overall", &Args::new()),
    };
    let triggers = def
        .triggers
        .iter()
        .map(|trigger| trigger.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    cx.t(
        "status-counter",
        &Args::new()
            .with("name", def.name.as_str())
            .with("scope", scope)
            .with("triggers", triggers),
    )
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}
