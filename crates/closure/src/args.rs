use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tracing::Subscriber;
use tracing_subscriber::filter::{FilterExt, LevelFilter, Targets};
use tracing_subscriber::layer::{Filter, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines for a terminal.
    Pretty,
    /// One JSON object per line for log collectors.
    Json,
}

/// Process settings. Every option can also be given as an environment variable, which is the
/// safer way to pass secrets because command lines are visible to other users of the machine.
#[derive(Parser)]
#[command(
    name = "closure",
    version,
    about = "A Discord moderation bot for servers of all sizes"
)]
pub struct Args {
    /// The bot token.
    #[arg(long, env = "DISCORD_TOKEN", hide_env_values = true)]
    pub discord_token: String,

    /// The application (client) id of the bot.
    #[arg(long, env = "DISCORD_APPLICATION_ID")]
    pub application_id: NonZeroU64,

    /// PostgreSQL connection URL, e.g. `postgres://user:password@host/closure`.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// Address of the HTTP API. Keep it on loopback unless TLS terminates in front of it.
    #[arg(long, env = "API_BIND", default_value = "127.0.0.1:8080")]
    pub api_bind: SocketAddr,

    /// Bearer token for the HTTP API (at least 24 characters). Without it the API is not
    /// started and only the bot runs.
    #[arg(long, env = "API_ADMIN_TOKEN", hide_env_values = true)]
    pub api_admin_token: Option<String>,

    /// A configuration document (YAML, the same as a server's) that every server starts from:
    /// the preset, then this, then the server's own document. Read once at start-up.
    #[arg(long, env = "BOT_CONFIG")]
    pub bot_config: Option<PathBuf>,

    /// Publish slash commands to this server only (they appear at once) instead of globally
    /// (which can take up to an hour). Use it while developing.
    #[arg(long, env = "COMMAND_GUILD")]
    pub command_guild: Option<NonZeroU64>,

    /// Do not publish slash commands at start-up.
    #[arg(long)]
    pub skip_command_registration: bool,

    #[arg(long, env = "LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Args")
            .field("application_id", &self.application_id)
            .field("api_bind", &self.api_bind)
            .field("api_enabled", &self.api_admin_token.is_some())
            .field("command_guild", &self.command_guild)
            .field("skip_command_registration", &self.skip_command_registration)
            .field("log_format", &self.log_format)
            .finish_non_exhaustive()
    }
}

/// The filter for log output: `RUST_LOG` (default `info`), and on top of it a hard cap that keeps
/// `twilight_http` and its modules at `warn`.
///
/// Twilight's request spans carry the full request URL, which contains interaction tokens, and any
/// event logged inside such a span prints it. The cap is a separate filter rather than a
/// directive added to `RUST_LOG`, so no `RUST_LOG` value can lift it, however specific or broad.
fn log_filter<S>(rust_log: Option<&str>) -> impl Filter<S> + use<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let requested = rust_log
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let cap = Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("twilight_http", LevelFilter::WARN);
    requested.and(cap)
}

impl Args {
    /// Installs the global logger. `RUST_LOG` overrides the default level (`info`).
    pub fn init_logging(&self) {
        let rust_log = std::env::var("RUST_LOG").ok();
        let registry = tracing_subscriber::registry();
        match self.log_format {
            LogFormat::Pretty => registry
                .with(tracing_subscriber::fmt::layer().with_filter(log_filter(rust_log.as_deref())))
                .init(),
            LogFormat::Json => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_filter(log_filter(rust_log.as_deref())),
                )
                .init(),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut argv = vec![
            "closure",
            "--discord-token",
            "super-secret-token",
            "--application-id",
            "42",
            "--database-url",
            "postgres://user:db-password@localhost/closure",
        ];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    #[test]
    fn defaults_keep_the_api_on_loopback_and_logs_readable() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.api_bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(args.log_format, LogFormat::Pretty);
        assert_eq!(args.application_id.get(), 42);
        assert!(args.api_admin_token.is_none());
        assert!(!args.skip_command_registration);
    }

    #[test]
    fn options_are_parsed() {
        let args = parse(&[
            "--api-bind",
            "0.0.0.0:9000",
            "--api-admin-token",
            "x".repeat(30).as_str(),
            "--command-guild",
            "7",
            "--skip-command-registration",
            "--log-format",
            "json",
            "--bot-config",
            "/etc/closure/bot.yaml",
        ])
        .unwrap();
        assert_eq!(
            args.bot_config,
            Some(PathBuf::from("/etc/closure/bot.yaml"))
        );
        assert_eq!(args.api_bind.port(), 9000);
        assert_eq!(args.command_guild.map(NonZeroU64::get), Some(7));
        assert!(args.skip_command_registration);
        assert_eq!(args.log_format, LogFormat::Json);
    }

    #[test]
    fn malformed_values_are_rejected() {
        assert!(parse(&["--api-bind", "not-an-address"]).is_err());
        assert!(parse(&["--command-guild", "abc"]).is_err());
        // 0 is not a snowflake; accepting it would silently publish commands globally.
        assert!(parse(&["--command-guild", "0"]).is_err());
        assert!(parse(&["--log-format", "xml"]).is_err());
    }

    #[test]
    fn the_token_and_database_url_are_required() {
        let command = Args::command();
        for name in ["discord_token", "application_id", "database_url"] {
            let argument = command
                .get_arguments()
                .find(|a| a.get_id() == name)
                .unwrap();
            assert!(argument.is_required_set(), "{name} must be required");
        }
    }

    /// Runs a request-like span (target `twilight_http`, URL with a token) containing an event of
    /// the application, and returns what the logger wrote.
    fn log_with(rust_log: &str) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(log_filter(Some(rust_log))),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                target: "twilight_http::request",
                "request",
                url = "/interactions/1/SECRET-TOKEN/callback"
            );
            let _entered = span.enter();
            tracing::debug!(target: "twilight_http::client", "sending SECRET-TOKEN");
            tracing::info!(target: "closure::app", "handled");
        });
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn no_rust_log_value_can_bring_twilight_http_logs_back() {
        for rust_log in [
            "trace",
            "twilight_http=trace",
            "twilight_http::request=trace",
            "twilight_http::client=trace,debug",
            "trace,not_twilight_http=warn",
            "[request]=trace",
        ] {
            let output = log_with(rust_log);
            assert!(
                !output.contains("SECRET-TOKEN"),
                "RUST_LOG={rust_log} leaked: {output}"
            );
        }
    }

    #[test]
    fn other_targets_still_follow_rust_log() {
        assert!(log_with("closure=info").contains("handled"));
        assert!(!log_with("closure=warn").contains("handled"));
        // An unusable value falls back to `info` instead of silencing everything.
        assert!(log_with("not a [valid filter").contains("handled"));
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let args = parse(&["--api-admin-token", "admin-secret-value-123456789"]).unwrap();
        let shown = format!("{args:?}");
        for secret in ["super-secret-token", "db-password", "admin-secret-value"] {
            assert!(!shown.contains(secret), "{secret} leaked: {shown}");
        }
    }

    #[test]
    fn secret_environment_values_are_hidden_in_help() {
        let command = Args::command();
        for name in ["discord_token", "database_url", "api_admin_token"] {
            let argument = command
                .get_arguments()
                .find(|a| a.get_id() == name)
                .unwrap();
            assert!(argument.is_hide_env_values_set(), "{name}");
        }
    }
}
