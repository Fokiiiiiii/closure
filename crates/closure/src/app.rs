use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use closure::message_history::{retention_cutoff, unix_now};
use closure::modules;
use closure_api::{ApiState, GatewayStatus, Readiness, ServerLimits, router};
use closure_config::{ConfigCompiler, SettingsStore, parse_overrides};
use closure_core::action::{Action, Outcome};
use closure_core::cases::CaseBackend;
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::event::{Event as ModuleEvent, GuildId, UserId};
use closure_core::history::HistoryBackend;
use closure_core::latency::GatewayLatency;
use closure_core::locale::Translator;
use closure_core::permission::Actor;
use closure_core::registry::ModuleRegistry;
use closure_core::schedule::SchedulerBackend;
use closure_core::settings::SettingsProvider;
use closure_core::storage::StorageBackend;
use closure_db::{Db, DbOptions};
use closure_discord::hierarchy::{CacheView, InMemoryCacheView};
use closure_discord::{
    CommandScope, Dispatcher, DispatcherConfig, GatewayConfig, GatewayHandle, GatewayHealth,
    TwilightApi, build_commands, gateway_intents, joined_guilds, new_cache, register_commands,
    run_jobs, start_gateway,
};
use closure_i18n::TranslatorBuilder;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use secrecy::SecretString;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::ApplicationMarker;

use crate::args::Args;
use crate::changes::ConfigChanges;
use crate::hooks::{AppHooks, reconcile_guilds};
use crate::sync;

const GATEWAY_STOP_TIMEOUT: Duration = Duration::from_secs(15);
const DISPATCHER_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const ATTACHMENT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

struct GatewayReadiness(Arc<GatewayHealth>);

struct CachedDirectory {
    cache: InMemoryCacheView,
    health: Arc<GatewayHealth>,
}

struct CachedAccess {
    cache: InMemoryCacheView,
    api: Arc<TwilightApi>,
    health: Arc<dyn Readiness>,
}

impl closure_api::MemberAccess for CachedAccess {
    fn member(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Actor>, ()>> + Send + '_>>
    {
        Box::pin(async move {
            self.ensure_ready(guild)?;
            let roles = if let Some(roles) = self.cache.member_roles(guild, user) {
                roles
            } else {
                match self
                    .api
                    .execute(Action::GetMember {
                        guild_id: guild,
                        user_id: user,
                    })
                    .await
                {
                    Ok(Outcome::Member(member)) => member.roles,
                    Err(DiscordError::NotFound) => {
                        self.ensure_ready(guild)?;
                        return Ok(None);
                    }
                    _ => return Err(()),
                }
            };
            let owner = self.cache.guild_owner(guild).ok_or(())? == user;
            let everyone = self
                .cache
                .0
                .role(Id::new(guild.get()))
                .ok_or(())?
                .permissions;
            let mut permissions = everyone;
            for role in &roles {
                permissions |= self.cache.0.role(*role).ok_or(())?.permissions;
            }
            self.ensure_ready(guild)?;
            Ok(Some(Actor {
                user,
                roles,
                administrator: permissions.contains(Permissions::ADMINISTRATOR),
                owner,
            }))
        })
    }
}

impl CachedAccess {
    fn ensure_ready(&self, guild: GuildId) -> Result<(), ()> {
        let status = self.health.gateway();
        if status.total == 0
            || status.ready != status.total
            || self
                .cache
                .0
                .guild(guild)
                .is_none_or(|guild| guild.unavailable() == Some(true))
        {
            return Err(());
        }
        Ok(())
    }
}

impl closure_api::GuildDirectory for CachedDirectory {
    fn guild(&self, id: GuildId) -> Option<closure_api::Guild> {
        if self.health.total() == 0 || self.health.ready() != self.health.total() {
            return None;
        }
        let guild = self.cache.0.guild(id)?;
        if guild.unavailable() == Some(true) {
            return None;
        }
        Some(closure_api::Guild {
            id: id.to_string(),
            name: guild.name().to_owned(),
            icon: guild.icon().map(ToString::to_string),
        })
    }

    fn directory(&self, id: GuildId) -> Option<closure_api::Directory> {
        self.guild(id)?;
        let cache = &self.cache.0;
        let channel_ids = cache
            .guild_channels(id)
            .map(|channels| channels.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let role_ids = cache
            .guild_roles(id)
            .map(|roles| roles.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut channels = Vec::new();
        for channel_id in channel_ids {
            let Some(channel) = cache.channel(channel_id) else {
                continue;
            };
            let kind = match channel.kind {
                ChannelType::GuildText => "text",
                ChannelType::GuildVoice => "voice",
                ChannelType::GuildCategory => "category",
                ChannelType::GuildAnnouncement => "announcement",
                ChannelType::GuildForum => "forum",
                ChannelType::GuildStageVoice => "stage",
                _ => continue,
            };
            let Some(name) = channel.name.clone() else {
                continue;
            };
            let Some(position) = channel.position else {
                continue;
            };
            channels.push(closure_api::Channel {
                id: channel.id.to_string(),
                name,
                kind,
                parent_id: channel.parent_id.map(|id| id.to_string()),
                position: i64::from(position),
            });
        }
        let mut roles = Vec::new();
        for role_id in role_ids {
            let Some(role) = cache.role(role_id) else {
                continue;
            };
            roles.push(closure_api::Role {
                id: role.id.to_string(),
                name: role.name.clone(),
                color: role.colors.primary_color,
                position: role.position,
                managed: role.managed,
            });
        }
        Some(closure_api::Directory { channels, roles })
    }
}

impl Readiness for GatewayReadiness {
    fn gateway(&self) -> GatewayStatus {
        GatewayStatus {
            ready: self.0.ready(),
            total: self.0.total(),
        }
    }
}

/// Resolves when the process is asked to stop (Ctrl-C, or SIGTERM from a supervisor).
async fn termination() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a working signal handler this arm must never win the race.
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    () = ctrl_c => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

fn serve_api(
    address: SocketAddr,
    state: ApiState,
    shutdown: CancellationToken,
    listener: tokio::net::TcpListener,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        closure_api::serve(listener, router(state), ServerLimits::default(), shutdown).await;
        tracing::info!(%address, "the HTTP API stopped");
    })
}

/// What every other part is built from.
struct Core {
    registry: Arc<ModuleRegistry>,
    translator: Arc<dyn Translator>,
    store: Arc<SettingsStore>,
}

fn build_core(bot_config: Option<&Path>, scripts_dir: &Path) -> Result<Core> {
    let mut all = modules()?;
    let grants = closure_script::load_grants(scripts_dir).unwrap_or_else(|error| {
        tracing::error!(%error, "could not load script grants");
        closure_script::Grants::new()
    });
    let loaded = closure_script::load_dir(scripts_dir, &grants, &closure_script::BuiltinRuntimes);
    for error in loaded.errors {
        tracing::error!(path = %error.path.display(), error = %error.message, "could not load script");
    }
    let mut pending: Vec<Arc<dyn closure_core::module::Module>> = loaded
        .modules
        .into_iter()
        .map(|module| Arc::new(module) as Arc<dyn closure_core::module::Module>)
        .collect();
    while !pending.is_empty() {
        let previous = pending.len();
        let mut deferred = Vec::new();
        let mut errors = Vec::new();
        for module in pending {
            let mut candidate = all.clone();
            candidate.push(Arc::clone(&module));
            let valid = (|| -> Result<()> {
                let registry = Arc::new(ModuleRegistry::new(candidate.clone())?);
                let mut catalogs = TranslatorBuilder::new()?;
                catalogs.add_registry(&registry)?;
                ConfigCompiler::new(registry)?;
                Ok(())
            })();
            match valid {
                Ok(()) => all = candidate,
                Err(error) => {
                    errors.push(error);
                    deferred.push(module);
                }
            }
        }
        if deferred.len() == previous {
            for (module, error) in deferred.iter().zip(errors) {
                tracing::error!(module = %module.manifest().id, %error, "could not register script");
            }
            break;
        }
        pending = deferred;
    }
    let registry =
        Arc::new(ModuleRegistry::new(all).context("the set of modules is inconsistent")?);
    let mut catalogs = TranslatorBuilder::new()?;
    catalogs.add_registry(&registry)?;
    let mut compiler = ConfigCompiler::new(Arc::clone(&registry))
        .context("a module's configuration schema is unusable")?;
    if let Some(path) = bot_config {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the bot configuration {}", path.display()))?;
        let defaults = parse_overrides(&text)
            .with_context(|| format!("{} is not a configuration document", path.display()))?;
        compiler = compiler
            .with_bot_defaults(&defaults)
            .with_context(|| format!("{} is not a valid configuration", path.display()))?;
    }
    let store = Arc::new(SettingsStore::new(compiler));
    Ok(Core {
        registry,
        translator: Arc::new(catalogs.build()),
        store,
    })
}

async fn publish_commands(
    args: &Args,
    http: &Client,
    application_id: Id<ApplicationMarker>,
    core: &Core,
) -> Result<()> {
    if args.skip_command_registration {
        tracing::info!("not publishing slash commands");
        return Ok(());
    }
    let commands = build_commands(&core.registry, core.translator.as_ref())?;
    let scope = args.command_guild.map_or(CommandScope::Global, |guild| {
        CommandScope::Guild(Id::new(guild.get()))
    });
    let published = register_commands(http, application_id, scope, &commands)
        .await
        .context("could not publish slash commands")?;
    tracing::info!(published, ?scope, "slash commands published");
    if matches!(scope, CommandScope::Global) {
        clear_guild_commands(http, application_id).await;
    }
    Ok(())
}

/// Empties the commands each guild has of its own. A run that published to one guild left them there,
/// and Discord shows them next to the global ones, so every command would appear twice.
async fn clear_guild_commands(http: &Client, application_id: Id<ApplicationMarker>) {
    let guilds = match joined_guilds(http).await {
        Ok(guilds) => guilds,
        Err(error) => {
            tracing::warn!(%error, "could not read the guild list, so guild commands were not cleared");
            return;
        }
    };
    for guild in guilds {
        let scope = CommandScope::Guild(guild);
        if let Err(error) = register_commands(http, application_id, scope, &[]).await {
            tracing::warn!(guild = guild.get(), %error, "could not clear the commands of a guild");
        }
    }
}

/// Starts the HTTP API, or reports that it is disabled when no admin token was given.
async fn start_api(
    args: &Args,
    core: &Core,
    db: &Db,
    health: Arc<GatewayHealth>,
    access: CachedAccess,
    metrics: PrometheusHandle,
    shutdown: &CancellationToken,
) -> Result<Option<JoinHandle<()>>> {
    let Some(token) = &args.api_admin_token else {
        tracing::warn!("API_ADMIN_TOKEN is not set; the HTTP API is disabled");
        return Ok(None);
    };
    if !args.api_bind.ip().is_loopback() {
        tracing::warn!(address = %args.api_bind, "the HTTP API is reachable from other machines; put TLS in front of it");
    }
    let state = ApiState::new(
        Arc::clone(&core.store),
        db.clone(),
        Arc::new(GatewayReadiness(Arc::clone(&health))),
        Arc::new(CachedDirectory {
            cache: access.cache.clone(),
            health,
        }),
        Arc::new(access),
        move || metrics.render(),
        &SecretString::from(token.clone()),
    )?;
    let listener = tokio::net::TcpListener::bind(args.api_bind)
        .await
        .with_context(|| format!("could not listen on {}", args.api_bind))?;
    tracing::info!(address = %args.api_bind, "HTTP API listening");
    Ok(Some(serve_api(
        args.api_bind,
        state,
        shutdown.clone(),
        listener,
    )))
}

/// How often expired entries and old minute counts of modules are removed.
const STORAGE_PURGE_EVERY: Duration = Duration::from_secs(3600);

/// Removes what modules no longer need from the database, now and then, until `shutdown`.
fn start_storage_upkeep(
    db: Db,
    message_history_days: u32,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match db.purge_module_storage().await {
                Ok((entries, minutes)) if entries + minutes > 0 => {
                    tracing::info!(entries, minutes, "removed what modules no longer need");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "could not remove expired module data"),
            }
            match db.purge_scheduled_jobs().await {
                Ok(jobs) if jobs > 0 => {
                    tracing::info!(jobs, "removed scheduled jobs that were given up on");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "could not remove old scheduled jobs"),
            }
            let cutoff = retention_cutoff(unix_now(), message_history_days);
            match db.purge_before(cutoff).await {
                Ok(messages) if messages > 0 => {
                    tracing::info!(messages, "removed messages past the history retention");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "could not remove old messages"),
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(STORAGE_PURGE_EVERY) => {}
            }
        }
    })
}

/// What has been started and has to be stopped again, however the run ends.
#[derive(Default)]
struct Started {
    dispatcher: Option<Dispatcher>,
    gateway: Option<GatewayHandle>,
    api_task: Option<JoinHandle<()>>,
    jobs_task: Option<JoinHandle<()>>,
    changes_task: Option<JoinHandle<()>>,
    hooks: Option<Arc<AppHooks>>,
}

/// Starts everything that talks to Discord and waits until the process is asked to stop.
/// Returns whether the gateway stopped on its own. Whatever was started before an error is
/// left in `started`, so the caller can still shut it down in order.
async fn serve(
    args: &Args,
    core: &Core,
    db: &Db,
    metrics: PrometheusHandle,
    shutdown: &CancellationToken,
    started: &mut Started,
    config_changes: Arc<ConfigChanges>,
) -> Result<bool> {
    let application_id = Id::new(args.application_id.get());
    let http = Arc::new(Client::new(args.discord_token.clone()));
    let cache = new_cache();
    // Shared by the Gateway task, which writes it, and the dispatcher, which hands it to modules.
    let latency = Arc::new(GatewayLatency::default());
    let api = Arc::new(
        TwilightApi::new(Arc::clone(&http), application_id)
            .with_webhooks(crate::webhooks::load(&args.webhook_file)?)
            .with_cache(Arc::new(InMemoryCacheView(Arc::clone(&cache)))),
    );
    let dispatcher = started
        .dispatcher
        .insert(
            Dispatcher::start_with_services(
                Arc::clone(&core.registry),
                Arc::clone(&core.store) as Arc<dyn SettingsProvider>,
                Arc::clone(&api) as Arc<dyn DiscordApi>,
                Arc::clone(&core.translator),
                // A bot's user id is its application id.
                DispatcherConfig {
                    bot_id: Some(Id::new(args.application_id.get())),
                    gateway_latency: Arc::clone(&latency),
                    ..DispatcherConfig::default()
                },
                Arc::new(db.clone()) as Arc<dyn StorageBackend>,
                Arc::new(db.clone()) as Arc<dyn SchedulerBackend>,
                Arc::new(db.clone()) as Arc<dyn CaseBackend>,
                Arc::new(db.clone()) as Arc<dyn HistoryBackend>,
            )
            .await
            .context("a module failed to load")?,
        )
        .clone();
    started.jobs_task = Some(tokio::spawn(run_jobs(
        Arc::new(db.clone()),
        dispatcher.clone(),
        shutdown.clone(),
    )));
    let dispatch_changes = dispatcher.clone();
    started.changes_task = Some(tokio::spawn(forward_config_changes(
        Arc::clone(&config_changes),
        move |guild_id| dispatch_changes.try_dispatch(ModuleEvent::ConfigChanged { guild_id }),
        shutdown.clone(),
    )));

    publish_commands(args, &http, application_id, core).await?;

    match joined_guilds(&http).await {
        Ok(guilds) => match reconcile_guilds(db, &guilds).await {
            Ok(left) => tracing::info!(guilds = guilds.len(), left, "reconciled the guild list"),
            Err(error) => tracing::warn!(%error, "could not reconcile the guild list"),
        },
        Err(error) => tracing::warn!(%error, "could not read the guild list from Discord"),
    }

    let hooks = Arc::new(AppHooks::new(
        db.clone(),
        Arc::clone(&core.store),
        args.message_history_days,
        Arc::clone(&config_changes),
    ));
    started.hooks = Some(Arc::clone(&hooks));
    let gateway = started.gateway.insert(
        start_gateway(
            &http,
            GatewayConfig {
                token: args.discord_token.clone(),
                intents: gateway_intents(&core.registry),
                latency,
            },
            Arc::clone(&cache),
            dispatcher,
            hooks,
            shutdown.clone(),
        )
        .await
        .context("could not start the gateway")?,
    );
    started.api_task = start_api(
        args,
        core,
        db,
        Arc::clone(gateway.health()),
        CachedAccess {
            cache: InMemoryCacheView(cache),
            api,
            health: Arc::new(GatewayReadiness(Arc::clone(gateway.health()))),
        },
        metrics,
        shutdown,
    )
    .await?;

    Ok(tokio::select! {
        () = termination() => false,
        () = gateway.closed() => true,
    })
}

const HAND_OVER_ATTEMPTS: usize = 4;
const HAND_OVER_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Hands the modules of a guild that its configuration changed, until shutdown. The event is
/// dispatched like any other, so it reaches only the modules that are enabled in the guild.
/// A refused hand-over (the dispatcher is full or closing) is retried a few times; after that the
/// change is dropped with a warning, so a busy dispatcher never holds up the reload.
async fn forward_config_changes(
    changes: Arc<ConfigChanges>,
    mut hand_over: impl FnMut(GuildId) -> bool + Send + 'static,
    shutdown: CancellationToken,
) {
    loop {
        let guild_id = tokio::select! {
            () = shutdown.cancelled() => return,
            guild = changes.next() => guild,
        };
        for attempt in 1..=HAND_OVER_ATTEMPTS {
            if hand_over(guild_id) {
                break;
            }
            if attempt == HAND_OVER_ATTEMPTS {
                tracing::warn!(%guild_id, "the dispatcher refused a configuration change; the modules were not told");
                break;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(HAND_OVER_RETRY_DELAY) => {}
            }
        }
    }
}

/// Waits for a background task that was told to stop. One that does not stop in time is aborted:
/// dropping its handle would leave it running, holding database connections that `db.close()`
/// then waits for.
async fn stop_task(mut task: JoinHandle<()>, name: &str) {
    if tokio::time::timeout(TASK_STOP_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        tracing::warn!(task = name, "did not stop in time; aborting it");
        task.abort();
    }
}

pub async fn run(args: Args) -> Result<()> {
    let metrics = PrometheusBuilder::new()
        .install_recorder()
        .context("could not install the metrics recorder")?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        ?args,
        "starting closure"
    );

    let core = build_core(args.bot_config.as_deref(), &args.scripts_dir)?;
    let db = Db::connect(&args.database_url, &DbOptions::default())
        .await
        .context("could not connect to the database")?;
    db.migrate()
        .await
        .context("could not apply the database migrations")?;

    let shutdown = CancellationToken::new();
    let config_changes = Arc::new(ConfigChanges::default());
    // Installs report through this queue; `serve` hands what waits in it to the modules.
    let reports = Arc::clone(&config_changes);
    core.store.set_change_hook(move |guild| reports.push(guild));
    let sync_task = sync::start(db.clone(), Arc::clone(&core.store), shutdown.clone())
        .await
        .context("could not load guild configurations")?;

    let upkeep_task = start_storage_upkeep(db.clone(), args.message_history_days, shutdown.clone());
    let mut started = Started::default();
    let outcome = serve(
        &args,
        &core,
        &db,
        metrics,
        &shutdown,
        &mut started,
        config_changes,
    )
    .await;
    match &outcome {
        Ok(false) => tracing::info!("shutting down"),
        Ok(true) => tracing::error!("the gateway stopped on its own"),
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "start-up failed; stopping what was started");
        }
    }

    // Stop taking new work first, then let what is in flight finish.
    shutdown.cancel();
    if let Some(gateway) = &started.gateway
        && tokio::time::timeout(GATEWAY_STOP_TIMEOUT, gateway.closed())
            .await
            .is_err()
    {
        tracing::warn!("the gateway did not stop in time");
    }
    // The jobs go first: the dispatcher must not be asked to run any once it is shutting down.
    if let Some(task) = started.changes_task {
        stop_task(task, "configuration changes").await;
    }
    if let Some(task) = started.jobs_task {
        stop_task(task, "scheduled jobs").await;
    }
    if let Some(dispatcher) = &started.dispatcher {
        dispatcher.shutdown(DISPATCHER_DRAIN_TIMEOUT).await;
    }
    if let Some(hooks) = &started.hooks {
        hooks.finish_attachments(ATTACHMENT_STOP_TIMEOUT).await;
    }
    if let Some(task) = started.api_task {
        stop_task(task, "HTTP API").await;
    }
    stop_task(sync_task, "configuration sync").await;
    stop_task(upkeep_task, "storage upkeep").await;
    db.close().await;
    tracing::info!("stopped");

    match outcome {
        Ok(true) => bail!(
            "the gateway stopped unexpectedly; check the bot token and that the requested intents are enabled in the developer portal"
        ),
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use closure_api::MemberAccess;

    struct TestReadiness(GatewayStatus);
    impl Readiness for TestReadiness {
        fn gateway(&self) -> GatewayStatus {
            self.0
        }
    }

    #[tokio::test]
    async fn cached_access_refuses_disconnected_gateway() {
        use twilight_model::gateway::{
            event::Event,
            payload::incoming::{GuildCreate, MemberAdd},
        };
        let cache = new_cache();
        let guild: twilight_model::guild::Guild = serde_json::from_value(serde_json::json!({
            "id":"10", "name":"g", "owner_id":"7", "roles":[{"id":"10","name":"everyone","permissions":"0","position":0,"color":0,"colors":{"primary_color":0},"flags":0,"hoist":false,"managed":false,"mentionable":false}],
            "emojis":[], "features":[], "stickers":[], "afk_timeout":300,
            "default_message_notifications":0, "explicit_content_filter":0, "mfa_level":0,
            "nsfw_level":0, "premium_tier":0, "system_channel_flags":0, "verification_level":0,
            "preferred_locale":"en-US", "premium_progress_bar_enabled":false,
            "joined_at":"2026-10-24T00:00:00.000000+00:00", "large":false,
            "member_count":1, "members":[], "channels":[], "threads":[], "presences":[], "voice_states":[], "stage_instances":[], "guild_scheduled_events":[]
        })).unwrap();
        cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(guild))));
        let member: MemberAdd = serde_json::from_value(serde_json::json!({
            "guild_id":"10", "user":{"id":"7","username":"alice","discriminator":"0","avatar":null,"bot":false},
            "roles":[], "pending":false, "joined_at":"2026-10-24T00:00:00.000000+00:00", "deaf":false, "mute":false, "flags":0
        })).unwrap();
        cache.update(&Event::MemberAdd(Box::new(member)));
        let api = Arc::new(TwilightApi::new(
            Arc::new(Client::new("test".into())),
            Id::new(1),
        ));
        let access = |ready, total| CachedAccess {
            cache: InMemoryCacheView(Arc::clone(&cache)),
            api: Arc::clone(&api),
            health: Arc::new(TestReadiness(GatewayStatus { ready, total })),
        };
        assert!(
            access(1, 1)
                .member(Id::new(10), Id::new(7))
                .await
                .unwrap()
                .unwrap()
                .owner
        );
        for (ready, total) in [(0, 0), (0, 1), (1, 2)] {
            assert!(
                access(ready, total)
                    .member(Id::new(10), Id::new(7))
                    .await
                    .is_err()
            );
        }
        let unavailable: GuildCreate =
            serde_json::from_value(serde_json::json!({ "id":"10", "unavailable":true })).unwrap();
        cache.update(&Event::GuildCreate(Box::new(unavailable)));
        assert!(access(1, 1).member(Id::new(10), Id::new(7)).await.is_err());
    }

    #[test]
    fn logs_rejects_two_destinations_at_configuration_time() {
        let registry = Arc::new(ModuleRegistry::new(modules().unwrap()).unwrap());
        let compiler = ConfigCompiler::new(registry).unwrap();
        let document = closure_config::parse_overrides(
            "modules:\n  logs:\n    config:\n      channel: '77'\n      webhook: audit\n",
        )
        .unwrap();
        assert!(compiler.compile(&document).is_err());
    }

    #[test]
    fn automod_add_roles_example_is_valid() {
        let registry = Arc::new(ModuleRegistry::new(modules().unwrap()).unwrap());
        let compiler = ConfigCompiler::new(registry).unwrap();
        let document = parse_overrides("modules: { automod: { config: { rules: { example: { triggers: [{ type: words, words: [spam] }], actions: [{ do: add_roles, roles: ['6'] }] } } } } }").unwrap();
        assert!(compiler.compile(&document).is_ok());
    }

    #[test]
    fn the_shipped_modules_form_a_valid_registry_and_configuration() {
        let registry = Arc::new(ModuleRegistry::new(modules().unwrap()).unwrap());
        let mut catalogs = TranslatorBuilder::new().unwrap();
        catalogs.add_registry(&registry).unwrap();
        ConfigCompiler::new(registry).unwrap();
    }

    #[test]
    fn missing_scripts_keep_the_native_registry_unchanged() {
        let missing =
            std::env::temp_dir().join(format!("closure-no-scripts-{}", std::process::id()));
        assert!(!missing.exists());
        let core = build_core(None, &missing).unwrap();
        let native = ModuleRegistry::new(modules().unwrap()).unwrap();
        let ids = |registry: &ModuleRegistry| {
            registry
                .iter()
                .map(|m| m.manifest().id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&core.registry), ids(&native));
        let mut catalogs = TranslatorBuilder::new().unwrap();
        catalogs.add_registry(&core.registry).unwrap();
        assert_eq!(
            build_commands(&core.registry, &catalogs.build())
                .unwrap()
                .len(),
            21
        );
    }

    #[cfg(feature = "script-lua")]
    #[test]
    fn startup_registers_valid_scripts_and_isolates_invalid_ones() {
        use closure_core::module::{ModuleId, ModuleManifest, ModuleRuntime};
        use serde_json::json;

        let root =
            std::env::temp_dir().join(format!("closure-startup-scripts-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (id, dependency, source, bad_permission) in [
            (
                "a-dependent",
                Some("z-base"),
                "function on_event(e) return {} end",
                false,
            ),
            ("z-base", None, "function on_event(e) return {} end", false),
            ("tags", None, "function on_event(e) return {} end", false),
            ("broken", None, "function (", false),
            (
                "bad-permission",
                None,
                "function on_event(e) return {} end",
                true,
            ),
        ] {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let mut manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "1");
            manifest.runtime = ModuleRuntime::Lua;
            if let Some(dependency) = dependency {
                manifest
                    .dependencies
                    .push(ModuleId::new(dependency).unwrap());
            }
            let mut spec = serde_json::to_value(manifest).unwrap();
            spec["entry"] = json!("main.lua");
            if bad_permission {
                spec["permissions"] =
                    json!([{"key":"other.test", "default_level":50, "description":"test"}]);
            }
            std::fs::write(dir.join("module.yaml"), serde_json::to_vec(&spec).unwrap()).unwrap();
            std::fs::write(dir.join("main.lua"), source).unwrap();
        }
        let core = build_core(None, &root).unwrap();
        let module = |id| core.registry.get(&ModuleId::new(id).unwrap());
        assert!(module("a-dependent").is_some());
        assert!(module("z-base").is_some());
        assert!(module("broken").is_none());
        assert!(module("bad-permission").is_none());
        assert_eq!(
            module("tags").unwrap().manifest().runtime,
            ModuleRuntime::Native
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_bot_configuration_file_is_the_bottom_layer_of_every_server() {
        let write = |name: &str, text: &str| {
            let path =
                std::env::temp_dir().join(format!("closure-test-{}-{name}", std::process::id()));
            std::fs::write(&path, text).unwrap();
            path
        };
        let good = write("good.yaml", "language: ja\nlimits: { concurrency: 12 }\n");
        let core = build_core(Some(&good), Path::new("/nonexistent-closure-scripts-test")).unwrap();
        let defaults = core.store.settings(Id::new(1));
        assert_eq!(defaults.effective.limits.concurrency, 12);
        assert!(defaults.effective.language.0.is_some());

        // What is wrong stops start-up, with the file named.
        let missing = std::env::temp_dir().join("closure-test-no-such-file.yaml");
        let broken = write("broken.yaml", "limits: [");
        let invalid = write("invalid.yaml", "limits: { concurrency: 0 }\n");
        for path in [&missing, &broken, &invalid] {
            let error = build_core(Some(path), Path::new("/nonexistent-closure-scripts-test"))
                .err()
                .expect("start-up is refused");
            assert!(
                format!("{error:#}").contains(&*path.to_string_lossy()),
                "{error:#}"
            );
        }
        for path in [good, broken, invalid] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn a_refused_hand_over_is_retried_and_the_next_guild_still_gets_its_change() {
        let attempts = Arc::new(std::sync::Mutex::new(Vec::<GuildId>::new()));
        let changes = Arc::new(ConfigChanges::default());
        let shutdown = CancellationToken::new();
        let hand_over = {
            let attempts = Arc::clone(&attempts);
            move |guild: GuildId| {
                let mut seen = attempts.lock().unwrap();
                seen.push(guild);
                // Guild 1 is accepted on its third try; guild 2 is refused every time.
                let tries = seen.iter().filter(|attempt| **attempt == guild).count();
                guild == Id::new(1) && tries >= 3
            }
        };
        let task = tokio::spawn(forward_config_changes(
            Arc::clone(&changes),
            hand_over,
            shutdown.clone(),
        ));
        changes.push(Id::new(1));
        changes.push(Id::new(2));

        let expected = 3 + HAND_OVER_ATTEMPTS;
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts.lock().unwrap().len() < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the retries never finished");

        let seen = attempts.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|g| **g == Id::new(1)).count(), 3);
        assert_eq!(
            seen.iter().filter(|g| **g == Id::new(2)).count(),
            HAND_OVER_ATTEMPTS
        );
        shutdown.cancel();
        task.await.unwrap();
    }

    #[test]
    fn every_shipped_command_is_publishable_in_every_language() {
        let registry = ModuleRegistry::new(modules().unwrap()).unwrap();
        let mut catalogs = TranslatorBuilder::new().unwrap();
        catalogs.add_registry(&registry).unwrap();
        let commands = build_commands(&registry, &catalogs.build()).unwrap();
        assert_eq!(commands.len(), 21);
        for command in commands {
            // A name Discord would refuse (uppercase, too long) is dropped with only a log line,
            // so it is looked for here.
            assert!(
                command
                    .name_localizations
                    .as_ref()
                    .is_some_and(|l| l.contains_key("ja")),
                "{} has no Japanese name",
                command.name
            );
            assert!(
                command
                    .description_localizations
                    .is_some_and(|l| l.contains_key("ja")),
                "{}",
                command.name
            );
        }
    }
}
