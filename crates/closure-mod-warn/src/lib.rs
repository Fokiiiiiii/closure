//! The `warn` module: `/warn add`, `/warn list` and `/warn clear`.
//!
//! Written with the SDK only, to show what a module author has to write: what the commands do.
//! The SDK checks the permission and the cooldown, reads the options, answers in the user's
//! language and runs the expiry of a warning at the right time.
//!
//! A user's warnings are one entry of the module's storage (`user:{id}`), changed with
//! compare-and-set so two moderators warning at once do not lose each other's warning. Each
//! warning has an id from a counter and a job that removes it after `expire_days`; removing a
//! warning by id does nothing when it is already gone, so the job may safely run twice.

use std::time::Duration;

use closure_sdk::ui::{button, row};
use closure_sdk::{
    Args, Capability, Command, CommandArgs, ComponentCx, Cooldown, CoreError, CustomId, Expect,
    InteractionReply, Level, Locale, ModuleBuilder, ModuleError, PermissionSpec, SdkModule,
    StorageError, Store, UserError, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::message::component::ButtonStyle;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// How many warnings a member can have at once.
const MAX_ACTIVE: usize = 25;
/// How many of the newest warnings `/warn list` shows (a message holds 2000 characters).
const SHOWN: usize = 10;
/// How many characters of a reason `/warn list` shows.
const REASON_SHOWN: usize = 80;
/// The counter that numbers a member's warnings; it never goes down.
const ISSUED: &str = "issued";
/// How often a change is tried again when somebody else changed the same entry meanwhile.
const TRIES: usize = 5;
/// An entry with no warnings left stays this long (and is then removed by the host), so the
/// module does not fill its room in the storage with members who have none.
const EMPTY_KEPT: Duration = Duration::from_secs(3600);

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// After how many days a warning is removed.
    expire_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { expire_days: 30 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Warning {
    id: i64,
    reason: Option<String>,
    by: u64,
}

/// What the job that removes a warning needs.
#[derive(Debug, Serialize, Deserialize)]
struct Expire {
    user: u64,
    id: i64,
}

#[derive(CommandArgs)]
struct Add {
    /// Member to warn
    user: UserId,
    /// Why
    #[arg(max_length = 200)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct Target {
    /// Member
    user: UserId,
}

fn mention(user: u64) -> String {
    format!("<@{user}>")
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

fn entry_key(user: u64) -> String {
    format!("user:{user}")
}

fn expire_key(user: u64, id: i64) -> String {
    format!("expire:{user}:{id}")
}

fn module_error(error: UserError) -> ModuleError {
    match error {
        UserError::Module(error) => error,
        other => ModuleError::other(format!("{other:?}")),
    }
}

/// The warnings of a member, and the revision they were read at.
async fn read(store: &Store, user: u64) -> Result<(Vec<Warning>, Expect), UserError> {
    match store.entry(&entry_key(user)).await? {
        Some(entry) => {
            let list = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(format!("a stored warning list: {error}")))?;
            Ok((list, Expect::Revision(entry.revision)))
        }
        None => Ok((Vec::new(), Expect::Missing)),
    }
}

/// Changes the warnings of a member and returns them as they are now.
async fn change(
    store: &Store,
    user: u64,
    mut edit: impl FnMut(&mut Vec<Warning>) -> Result<(), UserError>,
) -> Result<Vec<Warning>, UserError> {
    for _ in 0..TRIES {
        let (mut list, expect) = read(store, user).await?;
        edit(&mut list)?;
        if list.is_empty() && expect == Expect::Missing {
            return Ok(list);
        }
        let ttl = list.is_empty().then_some(EMPTY_KEPT);
        match store.replace(&entry_key(user), &list, expect, ttl).await {
            Ok(_) => return Ok(list),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(ModuleError::other("the warnings are being changed too often").into())
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= REASON_SHOWN {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(REASON_SHOWN).collect();
    short.push('…');
    short
}

async fn add(cx: closure_sdk::CommandCx, args: Add) -> Result<(), UserError> {
    let moderator = cx.user().id;
    let user = args.user.get();
    if args.user == moderator {
        return Err(UserError::message("warn-self"));
    }
    let config: Config = cx.config()?;
    // Checked here and not again when storing: warnings given at the very same moment can go a
    // few over, and the entry stays far below the limit of the storage.
    if read(cx.store(), user).await?.0.len() >= MAX_ACTIVE {
        return Err(UserError::message("warn-full")
            .with("user", mention(user))
            .with("limit", count(MAX_ACTIVE)));
    }

    let id = cx.store().counter_add(ISSUED, user, 1).await?;
    // The job first: one for a warning that was not stored does nothing, but a warning without
    // its job would never expire.
    let days = u64::from(config.expire_days);
    cx.scheduler()
        .job("expire", &Expire { user, id })?
        .key(&expire_key(user, id))
        .after(Duration::from_secs(days * 86_400))
        .await?;
    let list = change(cx.store(), user, |list| {
        list.push(Warning {
            id,
            reason: args.reason.clone(),
            by: moderator.get(),
        });
        Ok(())
    })
    .await?;
    cx.reply_private_t(
        "warn-added",
        &Args::new()
            .with("user", mention(user))
            .with("active", count(list.len()))
            .with("days", i64::from(config.expire_days)),
    )
    .await
}

async fn list(cx: closure_sdk::CommandCx, args: Target) -> Result<(), UserError> {
    let user = args.user.get();
    let (warnings, _) = read(cx.store(), user).await?;
    if warnings.is_empty() {
        return cx
            .reply_private_t("warn-none", &Args::new().with("user", mention(user)))
            .await;
    }
    let issued = cx.store().counter_total(ISSUED, user).await?;
    let lines: Vec<String> = warnings
        .iter()
        .rev()
        .take(SHOWN)
        .map(|warning| {
            let reason = warning
                .reason
                .as_deref()
                .map_or_else(|| cx.t("warn-no-reason", &Args::new()), shorten);
            cx.t(
                "warn-line",
                &Args::new()
                    .with("id", warning.id)
                    .with("reason", reason)
                    .with("by", mention(warning.by)),
            )
        })
        .collect();
    cx.reply_private_t(
        "warn-list",
        &Args::new()
            .with("user", mention(user))
            .with("active", count(warnings.len()))
            .with("issued", issued)
            .with("lines", lines.join("\n")),
    )
    .await
}

/// Asks before removing anything: the buttons work for the moderator who asked only.
async fn clear(cx: closure_sdk::CommandCx, args: Target) -> Result<(), UserError> {
    let user = args.user.get();
    let (warnings, _) = read(cx.store(), user).await?;
    if warnings.is_empty() {
        return cx
            .reply_private_t("warn-none", &Args::new().with("user", mention(user)))
            .await;
    }
    let me = cx.user().id;
    let confirm = CustomId::new("clear")
        .only_for(me)
        .data(user.to_string())
        .encode(cx.module());
    let cancel = CustomId::new("keep").only_for(me).encode(cx.module());
    cx.reply_with(InteractionReply {
        content: Some(
            cx.t(
                "warn-clear-ask",
                &Args::new()
                    .with("user", mention(user))
                    .with("active", count(warnings.len())),
            ),
        ),
        components: vec![row([
            button(
                ButtonStyle::Danger,
                cx.t("warn-clear-yes", &Args::new()),
                confirm,
            ),
            button(
                ButtonStyle::Secondary,
                cx.t("warn-clear-no", &Args::new()),
                cancel,
            ),
        ])],
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

async fn confirm_clear(cx: ComponentCx) -> Result<(), UserError> {
    // The permission may have been taken away since the buttons were shown.
    cx.require("warn.clear").await?;
    let user: u64 = cx
        .data()
        .parse()
        .map_err(|_| ModuleError::other("the button names no member"))?;
    let mut removed = Vec::new();
    change(cx.store(), user, |list| {
        removed = std::mem::take(list);
        Ok(())
    })
    .await?;
    for warning in &removed {
        cx.scheduler().cancel(&expire_key(user, warning.id)).await?;
    }
    cx.update(InteractionReply {
        content: Some(
            cx.t(
                "warn-cleared",
                &Args::new()
                    .with("user", mention(user))
                    .with("count", count(removed.len())),
            ),
        ),
        ..InteractionReply::default()
    })
    .await
}

async fn keep(cx: ComponentCx) -> Result<(), UserError> {
    cx.update(InteractionReply {
        content: Some(cx.t("warn-kept", &Args::new())),
        ..InteractionReply::default()
    })
    .await
}

/// The module, still open for a test to build on.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("warn", "Warnings", env!("CARGO_PKG_VERSION"))?
            .description("Warn members, look at their warnings, and let warnings expire.")
            .capability(Capability::Storage)
            .permission(PermissionSpec::new(
                "warn.add",
                Level::MODERATOR,
                "Warn members",
            )?)
            .permission(PermissionSpec::new(
                "warn.view",
                Level::MODERATOR,
                "See the warnings of members",
            )?)
            .permission(PermissionSpec::new(
                "warn.clear",
                Level::ADMIN,
                "Remove all warnings of a member",
            )?)
            .config(
                json!({
                    "type": "object",
                    "properties": {
                        "expire_days": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 365,
                            "description": "After how many days a warning is removed."
                        }
                    },
                    "additionalProperties": false
                }),
                json!({ "expire_days": 30 }),
            )
            .group(
                "warn",
                "Warn members and look at their warnings",
                [
                    Command::<Add>::new("add", "Warn a member")
                        .requires("warn.add")
                        .cooldown(Cooldown::per_user(Duration::from_secs(3)))
                        .handler(add),
                    Command::<Target>::new("list", "Show the warnings of a member")
                        .requires("warn.view")
                        .handler(list),
                    Command::<Target>::new("clear", "Remove all warnings of a member")
                        .requires("warn.clear")
                        .handler(clear),
                ],
            )
            .component("clear", confirm_clear)
            .component("keep", keep)
            .job("expire", |cx| async move {
                let Expire { user, id } = cx.payload()?;
                change(cx.store(), user, |list| {
                    list.retain(|warning| warning.id != id);
                    Ok(())
                })
                .await
                .map_err(module_error)?;
                Ok(())
            })
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
