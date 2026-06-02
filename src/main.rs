use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use freshdock::commands;

#[derive(Parser)]
#[command(name = "freshdock", version, about)]
struct Cli {
    #[arg(long, global = true)]
    no_color: bool,
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
        #[arg(long, default_value_t = 300)]
        interval: u64,
        /// Scheduler loop tick granularity, in seconds. Cron modes are
        /// evaluated once per tick, so this bounds how late a calendar fire is.
        #[arg(long, default_value_t = 60)]
        tick: u64,
        /// Max seconds to drain in-flight work after a shutdown signal.
        #[arg(long, default_value_t = 30)]
        stop_timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(!cli.no_color)
        .init();

    match cli.cmd {
        Cmd::Check => commands::check::run(cli.no_color).await?,
        Cmd::Recreate { name } => commands::recreate::run(name).await?,
        Cmd::Run {
            interval,
            tick,
            stop_timeout,
        } => commands::run::run(interval, tick, stop_timeout).await?,
    }
    Ok(())
}
