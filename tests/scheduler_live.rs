//! Live scheduler smoke test (Phase 4). Drives [`scheduler::run_with`] for one
//! tick against the **real** Docker daemon and asserts it does the right thing
//! for an up-to-date `live` container and a `watch` container: neither is
//! recreated (their container ids stay stable).
//!
//! This deliberately covers the *no-churn* path, which is deterministic and
//! CI-stable: a freshly-pulled image already matches its upstream digest, so a
//! correct scheduler must leave it alone, and `watch` must never recreate
//! regardless. The *recreate-on-change* path is covered by the scheduler unit
//! tests and by `recreate_roundtrip_live.rs` (forcing a genuinely newer Hub
//! digest in CI is flaky). Runs offline too — an unreachable registry also
//! results in no recreate.
//!
//! `#[ignore]`d (needs Docker); run with:
//!
//! ```bash
//! cargo test --test scheduler_live -- --ignored
//! ```

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptions,
    RemoveContainerOptionsBuilder,
};
use chrono::Local;
use futures::StreamExt;
use tokio::sync::watch;

use std::sync::Arc;

use freshdock::config::CredentialStore;
use freshdock::health::{HealthConfig, TokioClock};
use freshdock::registry::digest::OciRegistry;
use freshdock::scheduler::{self, SchedulerConfig};

const IMAGE: &str = "alpine:latest";

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Best-effort, prefix-scoped teardown on its own runtime thread (works during
/// a panic unwind).
struct Cleanup {
    prefix: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let prefix = self.prefix.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let Ok(docker) = Docker::connect_with_local_defaults() else {
                    return;
                };
                let opts = ListContainersOptions {
                    all: true,
                    ..Default::default()
                };
                if let Ok(list) = docker.list_containers(Some(opts)).await {
                    for c in list {
                        let matches = c.names.as_ref().is_some_and(|ns| {
                            ns.iter()
                                .any(|n| n.trim_start_matches('/').starts_with(&prefix))
                        });
                        if matches && let Some(id) = c.id {
                            let ropts = RemoveContainerOptionsBuilder::new().force(true).build();
                            let _ = docker.remove_container(&id, Some(ropts)).await;
                        }
                    }
                }
            });
        })
        .join();
    }
}

async fn connect_or_skip() -> Option<Docker> {
    match Docker::connect_with_local_defaults() {
        Ok(d) => match d.ping().await {
            Ok(_) => Some(d),
            Err(e) => {
                eprintln!("skipping live scheduler test: docker ping failed: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("skipping live scheduler test: cannot connect to docker: {e}");
            None
        }
    }
}

async fn ensure_image(docker: &Docker, image: &str) -> Result<(), bollard::errors::Error> {
    let (repo, tag) = image.split_once(':').unwrap_or((image, "latest"));
    let opts = CreateImageOptionsBuilder::new()
        .from_image(repo)
        .tag(tag)
        .build();
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}

fn body(mode: &str) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(IMAGE.to_owned()),
        cmd: Some(vec!["sleep".to_owned(), "3600".to_owned()]),
        labels: Some(HashMap::from([
            ("freshdock.enable".to_owned(), "true".to_owned()),
            ("freshdock.mode".to_owned(), mode.to_owned()),
        ])),
        ..Default::default()
    }
}

async fn spawn(docker: &Docker, name: &str, mode: &str) -> String {
    let opts = CreateContainerOptionsBuilder::new().name(name).build();
    docker
        .create_container(Some(opts), body(mode))
        .await
        .expect("create container");
    docker.start_container(name, None).await.expect("start");
    docker
        .inspect_container(name, None)
        .await
        .expect("inspect")
        .id
        .expect("id")
}

#[tokio::test]
#[ignore = "needs-docker"]
async fn up_to_date_live_and_watch_containers_are_not_recreated() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };

    let prefix = format!("fd-sched-{}", now_nanos());
    let _cleanup = Cleanup {
        prefix: prefix.clone(),
    };

    ensure_image(&docker, IMAGE).await.expect("pull alpine");
    let live_name = format!("{prefix}-live");
    let watch_name = format!("{prefix}-watch");
    let live_id = spawn(&docker, &live_name, "live").await;
    let watch_id = spawn(&docker, &watch_name, "watch").await;

    // Run the scheduler for one immediate tick, then signal shutdown.
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let credentials = Arc::new(CredentialStore::default());
        let fd = freshdock::docker::Docker::connect(credentials.clone())
            .expect("freshdock docker connect");
        let registry = OciRegistry::new(credentials);
        let cfg = SchedulerConfig {
            poll_interval: Duration::from_secs(1),
            tick: Duration::from_secs(1),
            health: HealthConfig::default(),
        };
        scheduler::run_with(&fd, &registry, &cfg, &TokioClock, Local::now, rx)
            .await
            .expect("scheduler run");
    });

    // Give the first tick time to list, probe Docker Hub, and decide.
    tokio::time::sleep(Duration::from_secs(4)).await;
    tx.send(true).expect("signal shutdown");
    handle.await.expect("scheduler task joins");

    // Both containers are freshly pulled (up to date) — and watch never
    // recreates anyway — so their ids must be unchanged.
    let live_after = docker
        .inspect_container(&live_name, None)
        .await
        .expect("live still exists")
        .id
        .expect("live id");
    let watch_after = docker
        .inspect_container(&watch_name, None)
        .await
        .expect("watch still exists")
        .id
        .expect("watch id");

    assert_eq!(
        live_id, live_after,
        "an up-to-date live container must not be recreated"
    );
    assert_eq!(
        watch_id, watch_after,
        "a watch container must never be recreated"
    );
}
