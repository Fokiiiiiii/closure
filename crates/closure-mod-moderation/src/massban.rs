use std::collections::HashSet;
use std::fmt::Write;

use closure_sdk::{
    Args, CommandArgs, CommandCx, DiscordError, ModuleError, TargetProblem, UserError, UserId,
};

use crate::{Config, MAX_CHARS, count, delete_seconds};
use closure_sdk::punish::{Moderator, Punisher, Settings};

const MAX_USERS: usize = 50;

#[derive(CommandArgs)]
pub(crate) struct Massban {
    /// User IDs or mentions separated by spaces or commas
    users: String,
    /// Why
    #[arg(max_length = 512)]
    reason: String,
    /// Days of messages to delete, 0 to 7
    #[arg(min = 0, max = 7)]
    delete_days: Option<i64>,
}

fn parse_user(token: &str) -> Option<UserId> {
    let value = if let Some(value) = token.strip_prefix("<@") {
        value
            .strip_suffix('>')?
            .strip_prefix('!')
            .unwrap_or(value.strip_suffix('>')?)
    } else {
        token
    };
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().and_then(UserId::new_checked)
}

fn failure_key(error: &UserError) -> &'static str {
    match error {
        UserError::Module(ModuleError::Discord(
            DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_),
        )) => "massban-forbidden",
        UserError::Module(ModuleError::Discord(DiscordError::RateLimited { .. })) => {
            "massban-rate-limited"
        }
        UserError::Module(ModuleError::Discord(_)) => "massban-discord-failed",
        _ => "massban-failed",
    }
}

pub(crate) async fn massban(cx: CommandCx, args: Massban) -> Result<(), UserError> {
    let tokens: Vec<_> = args
        .users
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .collect();
    if tokens
        .iter()
        .filter_map(|token| parse_user(token))
        .collect::<HashSet<_>>()
        .len()
        > MAX_USERS
    {
        return Err(UserError::message("massban-too-many"));
    }
    if tokens.is_empty() || args.reason.trim().is_empty() {
        return Err(UserError::message("massban-missing"));
    }
    let config: Config = cx.config()?;
    let days = args
        .delete_days
        .unwrap_or(i64::from(config.ban_delete_days));
    if !(0..=7).contains(&days) {
        return Err(UserError::message("ban-delete-days-range"));
    }
    let delete = delete_seconds(days)?;
    cx.defer(true).await?;
    let settings = Settings {
        dm: config.dm_on_action,
        ..Settings::default()
    };
    let punisher =
        Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings).reply_locale(cx.locale());
    let mut seen = HashSet::new();
    let (mut success, mut failed, mut excluded) = (0, 0, 0);
    let mut details = String::new();
    for token in tokens {
        let Some(user) = parse_user(token) else {
            excluded += 1;
            let _ = write!(
                details,
                "\n{}",
                cx.t(
                    "massban-invalid",
                    &Args::new().with("user", crate::shorten(token))
                )
            );
            continue;
        };
        if !seen.insert(user) {
            continue;
        }
        if let Err(error) = cx.check_target(user).await {
            excluded += 1;
            let key = match error {
                UserError::Target(TargetProblem::SelfTarget) => "massban-self",
                UserError::Target(TargetProblem::Owner) => "massban-owner",
                UserError::Target(TargetProblem::Bot) => "massban-bot",
                UserError::Target(TargetProblem::Higher) => "massban-higher",
                _ => "massban-check-failed",
            };
            let _ = write!(
                details,
                "\n{}",
                cx.t(key, &Args::new().with("user", user.get().to_string()))
            );
            continue;
        }
        match punisher
            .ban(user, None, delete, Some(args.reason.clone()))
            .await
        {
            Ok(_) => success += 1,
            Err(error) => {
                failed += 1;
                let key = failure_key(&error);
                let _ = write!(
                    details,
                    "\n{}",
                    cx.t(key, &Args::new().with("user", user.get().to_string()))
                );
            }
        }
    }
    let summary = cx.t(
        "massban-summary",
        &Args::new()
            .with("success", count(success))
            .with("failed", count(failed))
            .with("excluded", count(excluded)),
    );
    cx.reply_private(closure_sdk::embed::truncate(
        &(summary + &details),
        MAX_CHARS,
    ))
    .await
}
