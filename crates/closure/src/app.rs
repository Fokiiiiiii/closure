use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use closure_api::{ApiState, GatewayStatus, Readiness, ServerLimits, router};
use closure_config::{ConfigCompiler, SettingsStore, parse_overrides};
use closure_core::cases::CaseBackend;
use closure_core::locale::Translator;
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_core::schedule::SchedulerBackend;
use closure_core::settings::SettingsProvider;
use closure_core::storage::StorageBackend;
use closure_db::{Db, DbOptions};
use closure_discord::hierarchy::InMemoryCacheView;
use closure_discord::{
    CommandScope, Dispatcher, DispatcherConfig, GatewayConfig, GatewayHandle, GatewayHealth,
    TwilightApi, build_commands, gateway_intents, joined_guilds, new_cache, register_commands,
    run_jobs, start_gateway,
};
use closure_i18n::TranslatorBuilder;
use closure_mod_diagnostics::DiagnosticsModule;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use secrecy::SecretString;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;
use twilight_model::id::Id;
use twilight_model::id::marker::ApplicationMarker;

use crate::args::Args;
use crate::hooks::{AppHooks, reconcile_guilds};
use crate::sync;

const GATEWAY_STOP_TIMEOUT: Duration = Duration::from_secs(15);
const DISPATCHER_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Every module this build ships. Adding a module means adding it here.
fn modules() -> Result<Vec<Arc<dyn Module>>> {
    Ok(vec![
        Arc::new(DiagnosticsModule::new()?),
        Arc::new(closure_mod_moderation::module()?),
    ])
}

struct GatewayReadiness(Arc<GatewayHealth>);

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

fn build_core(bot_config: Option<&Path>) -> Result<Core> {
    let registry =
        Arc::new(ModuleRegistry::new(modules()?).context("the set of modules is inconsistent")?);
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
    Ok(())
}

/// Starts the HTTP API, or reports that it is disabled when no admin token was given.
async fn start_api(
    args: &Args,
    core: &Core,
    db: &Db,
    health: Arc<GatewayHealth>,
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
        Arc::new(GatewayReadiness(health)),
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
fn start_storage_upkeep(db: Db, shutdown: CancellationToken) -> JoinHandle<()> {
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
) -> Result<bool> {
    let application_id = Id::new(args.application_id.get());
    let http = Arc::new(Client::new(args.discord_token.clone()));
    let cache = new_cache();
    let api = Arc::new(
        TwilightApi::new(Arc::clone(&http), application_id)
            .with_cache(Arc::new(InMemoryCacheView(Arc::clone(&cache)))),
    );
    let dispatcher = started
        .dispatcher
        .insert(
            Dispatcher::start_with_services(
                Arc::clone(&core.registry),
                Arc::clone(&core.store) as Arc<dyn SettingsProvider>,
                api,
                Arc::clone(&core.translator),
                DispatcherConfig::default(),
                Arc::new(db.clone()) as Arc<dyn StorageBackend>,
                Arc::new(db.clone()) as Arc<dyn SchedulerBackend>,
                Arc::new(db.clone()) as Arc<dyn CaseBackend>,
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

    publish_commands(args, &http, application_id, core).await?;

    match joined_guilds(&http).await {
        Ok(guilds) => match reconcile_guilds(db, &guilds).await {
            Ok(left) => tracing::info!(guilds = guilds.len(), left, "reconciled the guild list"),
            Err(error) => tracing::warn!(%error, "could not reconcile the guild list"),
        },
        Err(error) => tracing::warn!(%error, "could not read the guild list from Discord"),
    }

    let hooks = Arc::new(AppHooks::new(db.clone(), Arc::clone(&core.store)));
    let gateway = started.gateway.insert(
        start_gateway(
            &http,
            GatewayConfig {
                token: args.discord_token.clone(),
                intents: gateway_intents(&core.registry),
            },
            cache,
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
        metrics,
        shutdown,
    )
    .await?;

    Ok(tokio::select! {
        () = termination() => false,
        () = gateway.closed() => true,
    })
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

    let core = build_core(args.bot_config.as_deref())?;
    let db = Db::connect(&args.database_url, &DbOptions::default())
        .await
        .context("could not connect to the database")?;
    db.migrate()
        .await
        .context("could not apply the database migrations")?;

    let shutdown = CancellationToken::new();
    let sync_task = sync::start(db.clone(), Arc::clone(&core.store), shutdown.clone())
        .await
        .context("could not load guild configurations")?;

    let upkeep_task = start_storage_upkeep(db.clone(), shutdown.clone());
    let mut started = Started::default();
    let outcome = serve(&args, &core, &db, metrics, &shutdown, &mut started).await;
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
    if let Some(task) = started.jobs_task {
        stop_task(task, "scheduled jobs").await;
    }
    if let Some(dispatcher) = &started.dispatcher {
        dispatcher.shutdown(DISPATCHER_DRAIN_TIMEOUT).await;
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

    #[test]
    fn the_shipped_modules_form_a_valid_registry_and_configuration() {
        let registry = Arc::new(ModuleRegistry::new(modules().unwrap()).unwrap());
        let mut catalogs = TranslatorBuilder::new().unwrap();
        catalogs.add_registry(&registry).unwrap();
        ConfigCompiler::new(registry).unwrap();
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
        let core = build_core(Some(&good)).unwrap();
        let defaults = core.store.settings(Id::new(1));
        assert_eq!(defaults.effective.limits.concurrency, 12);
        assert!(defaults.effective.language.0.is_some());

        // What is wrong stops start-up, with the file named.
        let missing = std::env::temp_dir().join("closure-test-no-such-file.yaml");
        let broken = write("broken.yaml", "limits: [");
        let invalid = write("invalid.yaml", "limits: { concurrency: 0 }\n");
        for path in [&missing, &broken, &invalid] {
            let error = build_core(Some(path)).err().expect("start-up is refused");
            assert!(
                format!("{error:#}").contains(&*path.to_string_lossy()),
                "{error:#}"
            );
        }
        for path in [good, broken, invalid] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn every_shipped_command_is_publishable_in_every_language() {
        let registry = ModuleRegistry::new(modules().unwrap()).unwrap();
        let mut catalogs = TranslatorBuilder::new().unwrap();
        catalogs.add_registry(&registry).unwrap();
        let commands = build_commands(&registry, &catalogs.build()).unwrap();
        assert_eq!(commands.len(), 6);
        for command in commands {
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
