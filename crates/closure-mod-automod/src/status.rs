//! `/automod status`: whether dry run is on, how many rules run, and each rule and each invalid
//! rule on a line of its own. Shown privately.

use closure_sdk::embed::{Color, EmbedBuilder, truncate};
use closure_sdk::{Args, CommandCx, InteractionReply, UserError};

use crate::actions::action_key;
use crate::checks::kind_key;
use crate::config::{Config, Entry, Problem, Rule, parse_rules};

/// An embed's description holds at most 4096 characters; a little is kept back.
const DESCRIPTION_CHARS: usize = 3_900;

pub async fn status(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let entries = parse_rules(&config.rules);
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
    for rule in valid {
        lines.push(rule_line(&cx, rule));
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
    let actions = rule
        .actions
        .iter()
        .map(|action| cx.t(action_key(action), &Args::new()))
        .collect::<Vec<_>>()
        .join(", ");
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
            .with("check", cx.t(kind_key(&rule.check), &Args::new()))
            .with("actions", actions),
    )
}

fn problem_text(cx: &CommandCx, problem: &Problem) -> String {
    match problem {
        Problem::Shape(error) => cx.t("problem-shape", &Args::new().with("error", error.as_str())),
        Problem::Key(key) => cx.t(key, &Args::new()),
    }
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}
