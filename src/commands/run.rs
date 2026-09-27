//! `freshdock run` — the scheduler daemon entry point (Phase 4).
//!
//! Wires the configured runtime (the Docker daemon, or containerd through
//! nerdctl) + OCI registry into [`scheduler::run_with`], installs
//! SIGINT/SIGTERM handlers that flip a shutdown flag, and bounds the
//! post-signal drain by `--stop-timeout`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{
    CredentialStore, NotificationConfig, ResolvedSettings, Runtime, RuntimeSettings,
};
use crate::docker::Docker;
use crate::docker::check::DockerCheck;
use crate::docker::recreate::DockerOps;
use crate::errors::AppError;
use crate::health::{HealthConfig, HealthProbe, TokioClock};
use crate::nerdctl::Nerdctl;
use crate::notify::Dispatcher;
use crate::registry::digest::OciRegistry;
use crate::scheduler::{self, SchedulerConfig};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    interval: u64,
    tick: u64,
    stop_timeout: u64,
    credentials: Arc<CredentialStore>,
    notifications: NotificationConfig,
    settings: ResolvedSettings,
    runtime: RuntimeSettings,
) -> Result<(), AppError> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "freshdock daemon starting"
    );
    // Built before the runtime connect so a missing CA store fails the start.
    let client = crate::http::client()?;
    let timing = Timing {
        interval,
        tick,
        stop_timeout,
    };
    match runtime.runtime {
        Runtime::Docker => {
            let docker = Docker::connect(credentials.clone()).await?;
            serve(
                &docker,
                client,
                credentials,
                notifications,
                settings,
                timing,
            )
            .await
        }
        Runtime::Nerdctl => {
            let nerdctl = Nerdctl::connect(&runtime, credentials.clone()).await?;
            nerdctl.sweep_or_warn().await;
            serve(
                &nerdctl,
                client,
                credentials,
                notifications,
                settings,
                timing,
            )
            .await
        }
    }
}

/// The `run` flags, in seconds.
struct Timing {
    interval: u64,
    tick: u64,
    stop_timeout: u64,
}

/// Everything after the runtime connect: the same daemon loop for every runtime.
async fn serve<D>(
    docker: &D,
    client: reqwest::Client,
    credentials: Arc<CredentialStore>,
    notifications: NotificationConfig,
    settings: ResolvedSettings,
    timing: Timing,
) -> Result<(), AppError>
where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
{
    let registry = OciRegistry::with_client(client.clone(), credentials);
    // Build the dispatcher once from config, sharing one HTTP client with the
    // backends. A target that fails to build is skipped with a WARN.
    let dispatcher = Dispatcher::from_config(notifications, client);
    dispatcher.log_configured();

    // `tokio::time::interval` panics on a zero period, and a poll interval below
    // the tick can never be honoured (due is evaluated once per tick), so clamp:
    // tick ≥ 1s and interval ≥ max(tick, 1s).
    let tick_secs = timing.tick.max(1);
    let interval_secs = timing.interval.max(1).max(tick_secs);
    let cfg = SchedulerConfig {
        poll_interval: Duration::from_secs(interval_secs),
        tick: Duration::from_secs(tick_secs),
        health: HealthConfig::default(),
        own_id_prefix: crate::selfid::own_container_id_prefix(),
    };
    if settings.watch_all {
        match cfg.own_id_prefix.as_deref() {
            Some(prefix) => info!(%prefix, "watch_all: self-guard active"),
            None => info!(
                "watch_all: could not identify our own container (custom hostname, or not \
                 running in one); if freshdock runs in a container, label it freshdock.mode=off"
            ),
        }
    }

    let (tx, rx) = watch::channel(false);
    let mut deadline_rx = rx.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutdown signal received; finishing in-flight work then exiting");
        let _ = tx.send(true);
    });

    // The loop finishes the in-flight container before returning; cap that
    // drain so a hung recreate can't block shutdown indefinitely.
    let stop_timeout = Duration::from_secs(timing.stop_timeout);
    tokio::select! {
        res = scheduler::run_with(docker, &registry, &cfg, &TokioClock, Local::now, rx, &dispatcher, settings) => res,
        _ = async {
            let _ = deadline_rx.wait_for(|v| *v).await;
            tokio::time::sleep(stop_timeout).await;
        } => {
            warn!(timeout_s = stop_timeout.as_secs(), "shutdown drain exceeded stop-timeout; forcing exit");
            Ok(())
        }
    }
}

/// Resolve on the first SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
