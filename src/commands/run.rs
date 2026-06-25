//! `freshdock run` — the scheduler daemon entry point (Phase 4).
//!
//! Wires the live Docker daemon + OCI registry into [`scheduler::run_with`],
//! installs SIGINT/SIGTERM handlers that flip a shutdown flag, and bounds the
//! post-signal drain by `--stop-timeout`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{CredentialStore, NotificationConfig, ResolvedSettings};
use crate::docker::Docker;
use crate::errors::AppError;
use crate::health::{HealthConfig, TokioClock};
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
) -> Result<(), AppError> {
    let docker = Docker::connect(credentials.clone())?;
    let registry = OciRegistry::new(credentials);
    // Build the dispatcher once from config, sharing one HTTP client with the
    // backends. A misconfigured target is skipped with a WARN (resilient): a
    // notification typo must never stop the daemon from updating containers.
    let dispatcher = Dispatcher::from_config(notifications, crate::http::client());
    dispatcher.log_configured();

    // `tokio::time::interval` panics on a zero period, and a poll interval below
    // the tick can never be honoured (due is evaluated once per tick), so clamp:
    // tick ≥ 1s and interval ≥ max(tick, 1s).
    let tick_secs = tick.max(1);
    let interval_secs = interval.max(1).max(tick_secs);
    let cfg = SchedulerConfig {
        poll_interval: Duration::from_secs(interval_secs),
        tick: Duration::from_secs(tick_secs),
        health: HealthConfig::default(),
    };

    let (tx, rx) = watch::channel(false);
    let mut deadline_rx = rx.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("shutdown signal received; finishing in-flight work then exiting");
        let _ = tx.send(true);
    });

    // The loop finishes the in-flight container before returning; cap that
    // drain so a hung recreate can't block shutdown indefinitely.
    let stop_timeout = Duration::from_secs(stop_timeout);
    tokio::select! {
        res = scheduler::run_with(&docker, &registry, &cfg, &TokioClock, Local::now, rx, &dispatcher, settings) => res,
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
