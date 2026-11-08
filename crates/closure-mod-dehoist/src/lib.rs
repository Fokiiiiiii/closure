//! Dehoisting: a name that starts with a symbol such as `!` sorts to the top of the member list.
//! The module takes those symbols off the nickname of a member who joins or changes their name,
//! and `/dehoist` does the same for the members already there.

use closure_core::action::{Action, Outcome, Reason};
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
/// The most pages a sweep reads.
const MAX_PAGES: usize = 100;
/// The most names a sweep changes; a larger server is swept again.
const MAX_CHANGES: usize = 500;

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

async fn rename(
    ctx: &EventContext,
    guild_id: GuildId,
    user_id: UserId,
    nickname: String,
) -> Result<(), closure_core::error::DiscordError> {
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

/// A member joined or changed: a hoisting name is cleaned.
async fn on_member(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let (Event::MemberJoined(member) | Event::MemberUpdated(member)) = event else {
        return Ok(Flow::Continue);
    };
    let shown = member.nick.as_deref().unwrap_or(&member.user.name);
    let fallback = ctx.t_in(ctx.locale, "dehoist-fallback", &Args::new());
    let Some(clean) = dehoisted(shown, &fallback) else {
        return Ok(Flow::Continue);
    };
    // A member above the bot cannot be renamed; that is not an error worth more than a trace.
    if let Err(error) = rename(&ctx, member.guild_id, member.user.id, clean).await {
        tracing::debug!(%error, "could not dehoist a member");
    }
    Ok(Flow::Continue)
}

/// Cleans the names of the members already in the server.
async fn sweep(cx: CommandCx, (): ()) -> Result<(), UserError> {
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let fallback = cx.t("dehoist-fallback", &Args::new());
    let (mut changed, mut after) = (0_usize, None);
    'pages: for _ in 0..MAX_PAGES {
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
        let Some(last) = members.last() else { break };
        after = Some(last.user.id);
        let full = members.len() >= usize::from(PAGE);
        for member in &members {
            let shown = member
                .nick
                .as_deref()
                .or(member.user.global_name.as_deref())
                .unwrap_or(&member.user.name);
            let Some(clean) = dehoisted(shown, &fallback) else {
                continue;
            };
            if rename(ctx, ctx.guild_id, member.user.id, clean)
                .await
                .is_ok()
            {
                changed += 1;
                if changed >= MAX_CHANGES {
                    break 'pages;
                }
            }
        }
        if !full {
            break;
        }
    }
    cx.reply_private_t(
        "dehoist-done",
        &Args::new().with("count", i64::try_from(changed).unwrap_or(i64::MAX)),
    )
    .await
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("dehoist", "Dehoist", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ManageNicknames)
            .capability(Capability::ReadGuild)
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
