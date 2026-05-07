mod docker;
mod errors;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::docker::Docker;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Check => check().await?,
    }
    Ok(())
}

async fn check() -> Result<(), errors::AppError> {
    let docker = Docker::connect()?;
    let containers = docker.list_running().await?;

    for c in &containers {
        let id = c.id.as_deref().unwrap_or("?");
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|s| s.trim_start_matches('/'))
            .unwrap_or("?");
        let image = c.image.as_deref().unwrap_or("?");
        println!("{}\t{}\t{}", &id[..id.len().min(12)], name, image);
    }

    Ok(())
}
