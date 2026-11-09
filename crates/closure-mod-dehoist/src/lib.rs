//! Dehoisting: a name that starts with a symbol such as `!` sorts to the top of the member list.
//! The module takes those symbols off the nickname of a member who joins or changes their name,
//! and `/dehoist` does the same for the members already there.

use std::future::Future;

use closure_core::action::{Action, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{Event, EventKind, GuildId, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Capability, Command, CommandCx, CoreError, Intents, Level, Locale, ModuleBuilder,
    ModuleError, PermissionSpec, SdkModule, Subscription, UserError,
};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// Discord's limit for a nickname.
const MAX_NICKNAME: usize = 32;
/// Members read from Discord at a time.
const PAGE: u16 = 1_000;
/// The most pages a sweep reads in one run.
const MAX_PAGES: usize = 100;
/// Names a sweep tries to change, counting the ones Discord refuses; a larger server is swept again.
const MAX_ATTEMPTS: usize = 500;
/// Refusals (no permission) in a row after which a sweep stops.
const MAX_REFUSALS: usize = 5;
/// Where the next sweep resumes: the last member the previous one went past.
const CURSOR: &str = "sweep-cursor";

/// The symbols that sort before every letter and digit.
fn hoisting(c: char) -> bool {
    ('!'..='/').contains(&c)
}

/// The name without its leading symbols, or `None` when it does not start with one.
fn dehoisted(name: &str, fallback: &str) -> Option<String> {
    if !name.trim_start().starts_with(hoisting) {
        return None;
    }
    let clean = name.trim_start_matches(|c: char| hoisting(c) || c.is_whitespace());
    let clean = if clean.is_empty() { fallback } else { clean };
    Some(clean.chars().take(MAX_NICKNAME).collect())
}

/// The name a member is shown by. An event carries no global name, so an event without a nickname
/// has nothing to judge by.
fn shown_name<'a>(
    nick: Option<&'a str>,
    global_name: Option<&'a str>,
    username: Option<&'a str>,
) -> Option<&'a str> {
    nick.or(global_name).or(username)
}

async fn rename(
    ctx: &EventContext,
    guild_id: GuildId,
    user_id: UserId,
    nickname: String,
) -> Result<(), DiscordError> {
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "dehoist-reason", &Args::new()));
    ctx.api
        .execute(Action::SetNickname {
            guild_id,
            user_id,
            nickname: Some(nickname),
            reason,
        })
        .await
        .map(drop)
}

async fn on_member(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let (Event::MemberJoined(member) | Event::MemberUpdated(member)) = event else {
        return Ok(Flow::Continue);
    };
    let fallback = ctx.t_in(ctx.locale, "dehoist-fallback", &Args::new());
    let Some(clean) = shown_name(member.nick.as_deref(), None, None)
        .and_then(|shown| dehoisted(shown, &fallback))
    else {
        return Ok(Flow::Continue);
    };
    // A member above the bot cannot be renamed; that is not an error worth more than a trace.
    if let Err(error) = rename(&ctx, member.guild_id, member.user.id, clean).await {
        tracing::debug!(%error, "could not dehoist a member");
    }
    Ok(Flow::Continue)
}

/// Whether a sweep ran out of something before it reached the end of the members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Budget,
    Refused,
}

/// What a sweep has done so far; `last` is the last member it is past.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pass {
    changed: usize,
    attempts: usize,
    refusals: usize,
    last: Option<UserId>,
    stop: Option<Stop>,
}

/// Cleans the shown names of one page, in order, until the attempt budget or a run of refusals
/// stops it.
async fn clean_page<F, Fut>(
    shown: &[(UserId, Option<String>)],
    fallback: &str,
    pass: &mut Pass,
    mut rename: F,
) where
    F: FnMut(UserId, String) -> Fut,
    Fut: Future<Output = Result<(), DiscordError>>,
{
    for (id, name) in shown {
        let Some(clean) = name.as_deref().and_then(|name| dehoisted(name, fallback)) else {
            pass.last = Some(*id);
            continue;
        };
        if pass.attempts >= MAX_ATTEMPTS {
            pass.stop = Some(Stop::Budget);
            return;
        }
        pass.attempts += 1;
        match rename(*id, clean).await {
            Ok(()) => {
                pass.changed += 1;
                pass.refusals = 0;
                pass.last = Some(*id);
            }
            Err(DiscordError::Forbidden(_)) => {
                pass.refusals += 1;
                if pass.refusals >= MAX_REFUSALS {
                    pass.stop = Some(Stop::Refused);
                    return;
                }
                pass.last = Some(*id);
            }
            Err(_) => {
                pass.refusals = 0;
                pass.last = Some(*id);
            }
        }
    }
}

async fn sweep(cx: CommandCx, (): ()) -> Result<(), UserError> {
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let fallback = cx.t("dehoist-fallback", &Args::new());
    let store = ctx.store();
    let mut after = store
        .get::<u64>(CURSOR)
        .await?
        .and_then(UserId::new_checked);
    let mut pass = Pass::default();
    let mut finished = false;
    for _ in 0..MAX_PAGES {
        let Ok(Outcome::Members(members)) = ctx
            .api
            .execute(Action::ListMembers {
                guild_id: ctx.guild_id,
                after,
                limit: PAGE,
            })
            .await
        else {
            break;
        };
        if members.is_empty() {
            finished = true;
            break;
        }
        let shown: Vec<(UserId, Option<String>)> = members
            .iter()
            .map(|member| {
                let name = shown_name(
                    member.nick.as_deref(),
                    member.user.global_name.as_deref(),
                    Some(member.user.name.as_str()),
                );
                (member.user.id, name.map(str::to_owned))
            })
            .collect();
        clean_page(&shown, &fallback, &mut pass, |user, clean| {
            rename(ctx, ctx.guild_id, user, clean)
        })
        .await;
        if pass.stop.is_some() {
            break;
        }
        if members.len() < usize::from(PAGE) {
            finished = true;
            break;
        }
        after = members.last().map(|member| member.user.id);
    }
    if finished {
        store.delete(CURSOR).await?;
    } else if let Some(last) = pass.last {
        store.set(CURSOR, &last.get()).await?;
    }
    let count = i64::try_from(pass.changed).unwrap_or(i64::MAX);
    let key = match pass.stop {
        Some(Stop::Refused) => "dehoist-refused",
        None if finished => "dehoist-done",
        Some(Stop::Budget) | None => "dehoist-partial",
    };
    cx.reply_private_t(key, &Args::new().with("count", count))
        .await
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("dehoist", "Dehoist", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ManageNicknames)
            .capability(Capability::ReadGuild)
            .capability(Capability::Storage)
            .intents(Intents::GUILD_MEMBERS)
            .permission(PermissionSpec::new(
                "dehoist.sweep",
                Level::MODERATOR,
                "permission-dehoist-sweep",
            )?)
            .on(Subscription::new(EventKind::MemberJoined), on_member)
            .on(Subscription::new(EventKind::MemberUpdated), on_member)
            .command(
                Command::<()>::new("dehoist")
                    .requires("dehoist.sweep")
                    .handler(sweep),
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
