use std::fmt;
use std::net::SocketAddr;

use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;

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
    pub application_id: u64,

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

    /// Publish slash commands to this server only (they appear at once) instead of globally
    /// (which can take up to an hour). Use it while developing.
    #[arg(long, env = "COMMAND_GUILD")]
    pub command_guild: Option<u64>,

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

impl Args {
    /// Installs the global logger. `RUST_LOG` overrides the default level (`info`).
    pub fn init_logging(&self) {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let builder = tracing_subscriber::fmt().with_env_filter(filter);
        match self.log_format {
            LogFormat::Pretty => builder.init(),
            LogFormat::Json => builder.json().init(),
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
        assert_eq!(args.application_id, 42);
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
        ])
        .unwrap();
        assert_eq!(args.api_bind.port(), 9000);
        assert_eq!(args.command_guild, Some(7));
        assert!(args.skip_command_registration);
        assert_eq!(args.log_format, LogFormat::Json);
    }

    #[test]
    fn malformed_values_are_rejected() {
        assert!(parse(&["--api-bind", "not-an-address"]).is_err());
        assert!(parse(&["--command-guild", "abc"]).is_err());
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
