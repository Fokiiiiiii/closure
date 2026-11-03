//! The `remind` module: one command, one permission and one setting, built with the SDK.
//!
//! The SDK reads the manifest, checks the permission before the handler runs and answers in the
//! invoker's language; the handler only says what the command does. See `docs/writing-modules.md`.

use closure_sdk::{
    Args, Command, CoreError, Level, Locale, ModuleBuilder, PermissionSpec, SdkModule,
};
use serde::Deserialize;
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("remind", "Remind", env!("CARGO_PKG_VERSION"))?
            .config(
                json!({"type": "object", "additionalProperties": false}),
                json!({}),
            )
            .permission(PermissionSpec::new(
                "remind.use",
                Level::GENERAL,
                "Use reminders",
            )?)
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::Schedule)
            .capability(closure_sdk::Capability::SendMessages)
            .group(
                "remind",
                [
                    Command::<Add>::new("add")
                        .requires("remind.use")
                        .handler(add),
                    Command::<()>::new("list")
                        .requires("remind.use")
                        .handler(list),
                    Command::<Cancel>::new("cancel")
                        .requires("remind.use")
                        .handler(cancel),
                ],
            )
            .on(
                closure_sdk::Subscription::new(closure_sdk::EventKind::GuildAvailable),
                |cx, _| async move {
                    ensure_reconcile(cx.scheduler())
                        .await
                        .map_err(module_error)?;
                    Ok(closure_sdk::Flow::Continue)
                },
            )
            .on(
                closure_sdk::Subscription::new(closure_sdk::EventKind::ConfigChanged),
                |cx, _| async move {
                    ensure_reconcile(cx.scheduler())
                        .await
                        .map_err(module_error)?;
                    Ok(closure_sdk::Flow::Continue)
                },
            )
            .job("deliver", deliver)
            .job("reconcile", reconcile)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[derive(closure_sdk::CommandArgs)]
struct Add {
    when: String,
    #[arg(min_length = 1, max_length = 500)]
    text: String,
}

#[derive(closure_sdk::CommandArgs)]
struct Cancel {
    id: String,
}

#[derive(Clone, serde::Serialize, Deserialize)]
struct Reminder {
    id: u64,
    user: closure_sdk::UserId,
    channel: closure_sdk::ChannelId,
    at: i64,
    text: String,
    #[serde(default)]
    delivering_until: i64,
}

async fn ensure_reconcile(
    scheduler: &closure_core::schedule::Scheduler,
) -> Result<(), closure_core::schedule::ScheduleError> {
    scheduler
        .job("reconcile", &())?
        .key("reconcile")
        .if_absent()
        .every(RECONCILE_INTERVAL, std::time::Duration::ZERO)
        .await
}

fn key(user: closure_sdk::UserId) -> String {
    format!("remind:{}", user.get())
}

fn job_key(user: closure_sdk::UserId, id: u64) -> String {
    format!("deliver:{}:{id}", user.get())
}

fn module_error(error: impl std::fmt::Display) -> closure_sdk::ModuleError {
    closure_sdk::ModuleError::other(error.to_string())
}

async fn entries(
    store: &closure_sdk::Store,
    key: &str,
) -> Result<(Vec<Reminder>, closure_sdk::Expect), closure_sdk::StorageError> {
    match store.entry(key).await? {
        Some(entry) => {
            let values = serde_json::from_value(entry.value)
                .map_err(|e| closure_sdk::StorageError::Value(e.to_string()))?;
            Ok((values, closure_sdk::Expect::Revision(entry.revision)))
        }
        None => Ok((Vec::new(), closure_sdk::Expect::Missing)),
    }
}

async fn remove(
    store: &closure_sdk::Store,
    user: closure_sdk::UserId,
    id: u64,
) -> Result<Option<Reminder>, closure_sdk::StorageError> {
    let key = key(user);
    loop {
        let (mut values, expected) = entries(store, &key).await?;
        let Some(index) = values.iter().position(|item| item.id == id) else {
            return Ok(None);
        };
        let item = values.remove(index);
        let ttl = values.is_empty().then(|| std::time::Duration::from_secs(1));
        match store.replace(&key, &values, expected, ttl).await {
            Ok(_) => return Ok(Some(item)),
            Err(closure_sdk::StorageError::Conflict) => {}
            Err(error) => return Err(error),
        }
    }
}

async fn add(cx: closure_sdk::CommandCx, args: Add) -> Result<(), closure_sdk::UserError> {
    let duration = closure_sdk::duration::parse(&args.when)
        .map_err(|_| closure_sdk::UserError::message("invalid-time"))?;
    if !(10..=2_592_000).contains(&duration.as_secs()) {
        return Err(closure_sdk::UserError::message("invalid-time"));
    }
    if !(1..=500).contains(&args.text.chars().count()) {
        return Err(closure_sdk::UserError::message("invalid-text"));
    }
    ensure_reconcile(cx.scheduler()).await?;
    let item = Reminder {
        id: cx.invocation().interaction_id.get(),
        user: cx.user().id,
        channel: cx.invocation().channel_id,
        at: cx
            .now()
            .saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)),
        text: args.text,
        delivering_until: 0,
    };
    let job = cx.scheduler().job("deliver", &(item.user, item.id))?;
    let key = key(item.user);
    loop {
        let (mut values, expected) = entries(cx.store(), &key).await?;
        if values.len() >= 10 {
            return Err(closure_sdk::UserError::message("limit"));
        }
        values.push(item.clone());
        match cx.store().replace(&key, &values, expected, None).await {
            Ok(_) => break,
            Err(closure_sdk::StorageError::Conflict) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if let Err(error) = job.key(&job_key(item.user, item.id)).after(duration).await {
        remove(cx.store(), item.user, item.id).await?;
        return Err(error.into());
    }
    cx.reply_private_t("added", &Args::new().with("id", item.id.to_string()))
        .await
}

async fn list(cx: closure_sdk::CommandCx, (): ()) -> Result<(), closure_sdk::UserError> {
    let (items, _) = entries(cx.store(), &key(cx.user().id)).await?;
    if items.is_empty() {
        return cx.reply_private_t("empty", &Args::new()).await;
    }
    let lines = items
        .iter()
        .map(|item| {
            cx.t(
                "line",
                &Args::new()
                    .with("id", item.id.to_string())
                    .with(
                        "at",
                        closure_sdk::duration::timestamp(item.at, closure_sdk::TimeStyle::Relative),
                    )
                    .with("text", item.text.chars().take(200).collect::<String>()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    cx.reply_private(lines).await
}

async fn cancel(cx: closure_sdk::CommandCx, args: Cancel) -> Result<(), closure_sdk::UserError> {
    let id = args
        .id
        .parse::<u64>()
        .map_err(|_| closure_sdk::UserError::message("not-found"))?;
    let removed = remove(cx.store(), cx.user().id, id).await?;
    cx.scheduler().cancel(&job_key(cx.user().id, id)).await?;
    if removed.is_none() {
        return Err(closure_sdk::UserError::message("not-found"));
    }
    cx.reply_private_t("cancelled", &Args::new().with("id", id.to_string()))
        .await
}

async fn deliver(cx: closure_sdk::JobCx) -> Result<(), closure_sdk::ModuleError> {
    let (user, id): (closure_sdk::UserId, u64) = cx.payload()?;
    deliver_item(&cx, user, id).await
}

async fn deliver_item(
    cx: &closure_sdk::JobCx,
    user: closure_sdk::UserId,
    id: u64,
) -> Result<(), closure_sdk::ModuleError> {
    let storage_key = key(user);
    let item = loop {
        let (mut items, expected) = entries(cx.store(), &storage_key)
            .await
            .map_err(module_error)?;
        let Some(item) = items.iter_mut().find(|item| item.id == id) else {
            return Ok(());
        };
        if item.delivering_until > cx.now() {
            return Ok(());
        }
        item.delivering_until = cx.now().saturating_add(60);
        let item = item.clone();
        match cx
            .store()
            .replace(&storage_key, &items, expected, None)
            .await
        {
            Ok(_) => break item,
            Err(closure_sdk::StorageError::Conflict) => {}
            Err(error) => return Err(module_error(error)),
        }
    };
    if cx.now().saturating_sub(item.at) > 86_400 {
        tracing::warn!(id, "expired reminder discarded");
        remove(cx.store(), user, id).await.map_err(module_error)?;
        return Ok(());
    }
    let content = cx.t(
        "delivery",
        &Args::new()
            .with("user", format!("<@{}>", item.user.get()))
            .with("text", item.text.as_str()),
    );
    let action = closure_core::action::Action::SendMessage {
        channel_id: item.channel,
        content: Some(content),
        embeds: Vec::new(),
        reply_to: None,
        mentions: closure_core::action::MentionPolicy::Users(vec![item.user]),
    };
    if let Err(error) = cx.api().execute(action).await {
        tracing::warn!(?error, "reminder delivery failed");
    } else {
        remove(cx.store(), user, id).await.map_err(module_error)?;
    }
    Ok(())
}

async fn reconcile(cx: closure_sdk::JobCx) -> Result<(), closure_sdk::ModuleError> {
    let mut prefixes = vec!["remind:".to_owned()];
    let mut items = Vec::new();
    while let Some(prefix) = prefixes.pop() {
        let entries = cx.store().list(&prefix, 1000).await.map_err(module_error)?;
        let split = entries.len() == 1000;
        for (key, entry) in entries {
            if split && key != prefix {
                continue;
            }
            let values: Vec<Reminder> =
                serde_json::from_value(entry.value).map_err(module_error)?;
            items.extend(values);
        }
        if split {
            for digit in '0'..='9' {
                prefixes.push(format!("{prefix}{digit}"));
            }
        }
    }
    items.sort_by_key(|item| (item.user.get(), item.id));
    let cursor: Option<(u64, u64)> = cx
        .store()
        .get("reconcile-cursor")
        .await
        .map_err(module_error)?;
    let start = cursor.map_or(0, |cursor| {
        items.partition_point(|item| (item.user.get(), item.id) <= cursor)
    });
    let len = items.len();
    for item in items.iter().cycle().skip(start).take(len.min(50)) {
        let future = item.at > cx.now();
        let result = if item.at <= cx.now().saturating_sub(90) {
            deliver_item(&cx, item.user, item.id).await
        } else if future {
            match cx.scheduler().job("deliver", &(item.user, item.id)) {
                Ok(job) => job
                    .key(&job_key(item.user, item.id))
                    .if_absent()
                    .at(item.at)
                    .await
                    .map_err(module_error),
                Err(error) => Err(module_error(error)),
            }
        } else {
            Ok(())
        };
        if future
            && !entries(cx.store(), &key(item.user))
                .await
                .map_err(module_error)?
                .0
                .iter()
                .any(|current| current.id == item.id)
        {
            cx.scheduler()
                .cancel(&job_key(item.user, item.id))
                .await
                .map_err(module_error)?;
        }
        if let Err(error) = result {
            tracing::warn!(?error, "reminder reconciliation failed");
        }
        cx.store()
            .set("reconcile-cursor", &(item.user.get(), item.id))
            .await
            .map_err(module_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use closure_core::{
        action::{Action, MentionPolicy},
        event::CommandOptionValue,
    };
    use closure_sdk::testing::{Harness, Invocation};
    use std::time::Duration;

    fn add_command(when: &str, text: &str) -> Invocation {
        Invocation::new("remind")
            .subcommand("add")
            .option("when", CommandOptionValue::String(when.into()))
            .option("text", CommandOptionValue::String(text.into()))
    }

    #[tokio::test]
    async fn active_delivery_lease_prevents_duplicate() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "leased")).await.unwrap();
        let mut items: serde_json::Value = h.store().get("remind:100").await.unwrap().unwrap();
        items[0]["delivering_until"] = json!(i64::MAX);
        h.store().set("remind:100", &items).await.unwrap();
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(
            !h.actions()
                .iter()
                .any(|a| matches!(a, Action::SendMessage { .. }))
        );
    }

    #[tokio::test]
    async fn concurrent_reconcile_and_delivery_send_once() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "race")).await.unwrap();
        h.scheduler().cancel("reconcile").await.unwrap();
        h.advance(Duration::from_secs(100));
        let (delivery, recovery) = tokio::join!(h.run_due_jobs(), async {
            tokio::task::yield_now().await;
            h.scheduler()
                .job("reconcile", &())
                .unwrap()
                .after(Duration::ZERO)
                .await
                .unwrap();
            h.run_due_jobs().await
        });
        assert!(delivery.into_iter().chain(recovery).all(|r| r.is_ok()));
        assert_eq!(
            h.actions()
                .iter()
                .filter(|a| matches!(a, Action::SendMessage { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancel_removes_delivery_job() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.invoke(add_command("10s", "cancel")).await.unwrap();
        let items: Vec<Reminder> = h.store().get("remind:100").await.unwrap().unwrap();
        h.invoke(
            Invocation::new("remind")
                .subcommand("cancel")
                .option("id", CommandOptionValue::String(items[0].id.to_string())),
        )
        .await
        .unwrap();
        assert!(
            !h.scheduler()
                .cancel(&job_key(items[0].user, items[0].id))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn config_changed_creates_reconcile_job() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.emit(closure_sdk::Event::ConfigChanged {
            guild_id: closure_sdk::GuildId::new(1),
        })
        .await
        .unwrap();
        assert!(h.scheduler().cancel("reconcile").await.unwrap());
    }

    #[tokio::test]
    async fn orphan_is_delivered_by_reconcile() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "orphan")).await.unwrap();
        let item = h
            .store()
            .get::<Vec<Reminder>>("remind:100")
            .await
            .unwrap()
            .unwrap()[0]
            .clone();
        assert!(
            h.scheduler()
                .cancel(&format!("deliver:{}:{}", item.user.get(), item.id))
                .await
                .unwrap()
        );
        h.advance(Duration::from_secs(100));
        h.scheduler()
            .job("reconcile", &())
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(h.actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(text), .. } if text.contains("orphan"))));
    }

    #[tokio::test]
    async fn boundaries_and_limit() {
        let h = Harness::new(builder().unwrap()).unwrap();
        for when in ["9s", "31d", "nonsense"] {
            let reply = h.invoke(add_command(when, "x")).await.unwrap();
            assert!(reply[0].content.contains("10 seconds"));
        }
        for _ in 0..10 {
            h.invoke(add_command("10s", "x")).await.unwrap();
        }
        assert!(
            h.invoke(add_command("10s", "x")).await.unwrap()[0]
                .content
                .contains("10 pending")
        );
    }

    #[tokio::test]
    async fn text_and_reply_boundaries() {
        let h = Harness::new(builder().unwrap()).unwrap();
        for text in [String::new(), "x".repeat(501)] {
            h.invoke(add_command("10s", &text)).await.unwrap();
            assert!(
                h.store()
                    .get::<Vec<Reminder>>("remind:100")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let reply = h
            .invoke(add_command("30d", &"x".repeat(500)))
            .await
            .unwrap();
        assert!(reply[0].ephemeral);
        assert_eq!(
            h.store()
                .get::<Vec<Reminder>>("remind:100")
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        h.set_config(json!({"replies": "public"}));
        assert!(!h.invoke(add_command("10s", "x")).await.unwrap()[0].ephemeral);
    }

    #[tokio::test]
    async fn list_cancel_and_delivery_are_scoped() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(12),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "hello @everyone").channel(12))
            .await
            .unwrap();
        let other_list = h
            .invoke(Invocation::new("remind").subcommand("list").user(101))
            .await
            .unwrap();
        assert!(other_list[0].content.contains("no pending"));
        let own = h
            .invoke(Invocation::new("remind").subcommand("list"))
            .await
            .unwrap();
        let id = own[0].content.split_whitespace().next().unwrap().to_owned();
        let other = h
            .invoke(
                Invocation::new("remind")
                    .subcommand("cancel")
                    .user(101)
                    .option("id", CommandOptionValue::String(id)),
            )
            .await
            .unwrap();
        assert!(other[0].content.contains("No pending"));
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(h.actions().iter().any(|action| matches!(action, Action::SendMessage { channel_id, mentions: MentionPolicy::Users(users), .. } if channel_id.get() == 12 && users.len() == 1 && users[0].get() == 100)));
    }

    #[tokio::test]
    async fn cancelled_jobs_do_not_send() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "hello")).await.unwrap();
        let own = h
            .invoke(Invocation::new("remind").subcommand("list"))
            .await
            .unwrap();
        let id = own[0].content.split_whitespace().next().unwrap().to_owned();
        h.invoke(
            Invocation::new("remind")
                .subcommand("cancel")
                .option("id", CommandOptionValue::String(id)),
        )
        .await
        .unwrap();
        h.advance(Duration::from_secs(10));
        h.scheduler()
            .job("reconcile", &())
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        let jobs = h.run_due_jobs().await;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.into_iter().all(|r| r.is_ok()));
        assert!(
            !h.actions()
                .iter()
                .any(|a| matches!(a, Action::SendMessage { .. }))
        );
    }

    #[tokio::test]
    async fn failed_delivery_is_retried_by_reconcile() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.invoke(add_command("10s", "hello").channel(99))
            .await
            .unwrap();
        h.advance(Duration::from_secs(10));
        let jobs = h.run_due_jobs().await;
        assert!(!jobs.is_empty());
        assert!(jobs[0].is_ok());
        let list = h
            .invoke(Invocation::new("remind").subcommand("list"))
            .await
            .unwrap();
        assert!(list[0].content.contains("hello"));
        h.api().add_channel(
            closure_sdk::ChannelId::new(99),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.advance(Duration::from_secs(90));
        h.scheduler()
            .job("reconcile", &())
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(h.actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(text), .. } if text.contains("hello"))));
        let list = h
            .invoke(Invocation::new("remind").subcommand("list"))
            .await
            .unwrap();
        assert!(list[0].content.contains("no pending"));
    }

    #[tokio::test]
    async fn expired_failure_is_discarded() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.invoke(add_command("10s", "late").channel(99))
            .await
            .unwrap();
        h.advance(Duration::from_secs(86_411));
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        let list = h
            .invoke(Invocation::new("remind").subcommand("list"))
            .await
            .unwrap();
        assert!(list[0].content.contains("no pending"));
    }

    #[tokio::test]
    async fn scheduling_failure_rolls_back_storage() {
        let h = Harness::new(builder().unwrap()).unwrap();
        for _ in 0..closure_core::schedule::MAX_JOBS {
            h.scheduler()
                .job("deliver", &(closure_sdk::UserId::new(100), 1_u64))
                .unwrap()
                .after(Duration::from_secs(60))
                .await
                .unwrap();
        }
        let result = h.invoke(add_command("10s", "x")).await;
        assert!(result.is_err());
        let (items, _) = entries(&h.store(), "remind:100").await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn future_orphan_is_rescheduled() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10m", "future")).await.unwrap();
        let item = h
            .store()
            .get::<Vec<Reminder>>("remind:100")
            .await
            .unwrap()
            .unwrap()[0]
            .clone();
        assert!(
            h.scheduler()
                .cancel(&job_key(item.user, item.id))
                .await
                .unwrap()
        );
        h.scheduler()
            .job("reconcile", &())
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        h.advance(Duration::from_secs(600));
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(h.actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(text), .. } if text.contains("future"))));
    }

    #[tokio::test]
    async fn guild_available_starts_periodic_recovery() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        h.invoke(add_command("10s", "periodic")).await.unwrap();
        let item = h
            .store()
            .get::<Vec<Reminder>>("remind:100")
            .await
            .unwrap()
            .unwrap()[0]
            .clone();
        assert!(
            h.scheduler()
                .cancel(&job_key(item.user, item.id))
                .await
                .unwrap()
        );
        h.emit(closure_sdk::Event::GuildAvailable {
            guild_id: closure_sdk::GuildId::new(1),
            name: "test".into(),
            preferred_locale: None,
        })
        .await
        .unwrap();
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        h.advance(Duration::from_secs(10));
        assert!(
            h.scheduler()
                .cancel(&job_key(item.user, item.id))
                .await
                .unwrap()
        );
        h.advance(Duration::from_secs(290));
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert!(h.actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(text), .. } if text.contains("periodic"))));
    }

    #[tokio::test]
    async fn reconciliation_advances_after_fifty_items() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.api().add_channel(
            closure_sdk::ChannelId::new(5),
            Some(closure_sdk::GuildId::new(1)),
        );
        for user in 100..151 {
            h.invoke(add_command("10s", "batch").user(user))
                .await
                .unwrap();
            let item = h
                .store()
                .get::<Vec<Reminder>>(&format!("remind:{user}"))
                .await
                .unwrap()
                .unwrap()[0]
                .clone();
            assert!(
                h.scheduler()
                    .cancel(&job_key(item.user, item.id))
                    .await
                    .unwrap()
            );
        }
        h.advance(Duration::from_secs(100));
        h.scheduler()
            .job("reconcile", &())
            .unwrap()
            .key("reconcile")
            .every(RECONCILE_INTERVAL, Duration::ZERO)
            .await
            .unwrap();
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert_eq!(
            h.actions()
                .iter()
                .filter(|a| matches!(a, Action::SendMessage { .. }))
                .count(),
            50
        );
        h.advance(RECONCILE_INTERVAL);
        assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
        assert_eq!(
            h.actions()
                .iter()
                .filter(|a| matches!(a, Action::SendMessage { .. }))
                .count(),
            51
        );
    }

    #[tokio::test]
    async fn permission_is_checked_before_all_subcommands() {
        let h = Harness::new(builder().unwrap()).unwrap();
        h.permissions(|permissions| {
            permissions
                .keys
                .entry(closure_sdk::PermissionKey::new("remind.use").unwrap())
                .or_default()
                .deny_users
                .push(closure_sdk::UserId::new(100));
        });
        for command in [
            add_command("10s", "x"),
            Invocation::new("remind").subcommand("list"),
            Invocation::new("remind")
                .subcommand("cancel")
                .option("id", CommandOptionValue::String("1".into())),
        ] {
            let reply = h.invoke(command).await.unwrap();
            assert!(!reply[0].content.contains("pending reminders"));
            assert!(reply[0].ephemeral);
        }
        assert!(
            h.store()
                .get::<Vec<Reminder>>("remind:100")
                .await
                .unwrap()
                .is_none()
        );
    }
}
