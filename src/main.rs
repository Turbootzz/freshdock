use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use freshdock::commands;
use freshdock::config::{Config, ENV_VAR_HELP};

#[derive(Parser)]
#[command(name = "freshdock", version, about, after_long_help = ENV_VAR_HELP)]
struct Cli {
    /// Disable ANSI colors. The NO_COLOR env var (any non-empty value) does
    /// the same; it's checked by hand because the convention is
    /// presence-based, not value-parsed like clap's env attribute.
    #[arg(long, global = true)]
    no_color: bool,
    /// Path to the freshdock.toml credentials file. Defaults to
    /// ./freshdock.toml if present; the FRESHDOCK_CONFIG env var sets it too
    /// (this flag wins).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List opted-in containers and report which have updates available.
    Check,
    /// Recreate a single container in place: inspect → pull → stop → rename →
    /// create → start, then health-gate the new container and roll back to the
    /// previous one if it fails.
    Recreate {
        /// Name (or ID) of the running container to recreate.
        name: String,
    },
    /// Run the scheduler daemon: poll opted-in containers on their per-mode
    /// cadence and recreate (or, for watch, report) when a new digest is
    /// available. Runs until SIGINT/SIGTERM.
    Run {
        /// Poll cadence for live/watch containers, in seconds.
        #[arg(long, env = "FRESHDOCK_INTERVAL", default_value_t = 300)]
        interval: u64,
        /// Scheduler loop tick granularity, in seconds. Cron modes are
        /// evaluated once per tick, so this bounds how late a calendar fire is.
        #[arg(long, env = "FRESHDOCK_TICK", default_value_t = 60)]
        tick: u64,
        /// Max seconds to drain in-flight work after a shutdown signal.
        #[arg(long, env = "FRESHDOCK_STOP_TIMEOUT", default_value_t = 30)]
        stop_timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let no_color = cli.no_color || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(!no_color)
        .init();

    // Resolve credentials once (flag > FRESHDOCK_CONFIG env > default file) and
    // share the store across the registry HEAD flow and the daemon pull.
    let config_path = cli
        .config
        .clone()
        .or_else(|| std::env::var_os("FRESHDOCK_CONFIG").map(PathBuf::from));
    let config = Config::load(config_path.as_deref())?;
    let credentials = config.credentials;
    let settings = config.settings;

    match cli.cmd {
        Cmd::Check => commands::check::run(no_color, credentials, settings).await?,
        Cmd::Recreate { name } => commands::recreate::run(name, credentials, settings).await?,
        Cmd::Run {
            interval,
            tick,
            stop_timeout,
        } => {
            commands::run::run(
                interval,
                tick,
                stop_timeout,
                credentials,
                config.notifications,
                settings,
            )
            .await?
        }
    }
    Ok(())
}
