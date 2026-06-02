//! Live "weird config" recreate round-trip — the PLAN §6.3 quality gate (P3-3).
//!
//! Creates a deliberately rich container, recreates it through the in-process
//! [`recreate_one`] entry point against the **real** Docker daemon, then asserts
//! the inspected config round-trips byte-identical except for the container id
//! and image digest. Per PLAN §6.3: if this passes the tool is safe to ship; if
//! it fails the tool is dangerous. A failure here is a **release blocker**.
//!
//! Run it explicitly (it is `#[ignore]`d so default CI without a daemon stays
//! green):
//!
//! ```bash
//! cargo test --test recreate_roundtrip_live -- --ignored
//! ```
//!
//! It uses raw `bollard` 0.21 (already a dependency) rather than
//! `testcontainers`: the latter's 0.27.x line pins bollard 0.20, which would
//! duplicate bollard and fail `cargo deny` (see Cargo.toml). Cleanup is
//! best-effort and prefix-scoped, so a panic mid-test never leaks containers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, EndpointIpamConfig, EndpointSettings, HealthConfig, HostConfig,
    HostConfigLogConfig, Ipam, IpamConfig, NetworkCreateRequest, NetworkingConfig,
    ResourcesUlimits, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptions,
    ListNetworksOptions, RemoveContainerOptionsBuilder,
};
use futures::StreamExt;

use freshdock::config::CredentialStore;
use freshdock::docker::recreate::recreate_one;

const IMAGE: &str = "nginx:alpine";
const ALIAS: &str = "fd-nginx-alias";

// Each test gets its own /24 so the two #[ignore] cases can run concurrently
// without "Pool overlaps" network-create conflicts.
fn subnet_for(octet: u8) -> String {
    format!("172.31.{octet}.0/24")
}
fn static_ip_for(octet: u8) -> String {
    format!("172.31.{octet}.42")
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Best-effort, prefix-scoped teardown. Runs on its own OS thread + runtime so
/// it works even while unwinding from a failed assertion (sync `Drop` cannot
/// await, and we must not block the test's own runtime).
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
                if let Ok(nets) = docker.list_networks(None::<ListNetworksOptions>).await {
                    for n in nets {
                        if n.name
                            .as_deref()
                            .is_some_and(|name| name.starts_with(&prefix))
                        {
                            let _ = docker.remove_network(&n.name.unwrap()).await;
                        }
                    }
                }
            });
        })
        .join();
    }
}

/// Connect + ping; on any failure print a skip notice and return `None` so a
/// developer running `--ignored` without a daemon gets a graceful no-op rather
/// than a failure (mirrors the network-unavailable skip convention).
async fn connect_or_skip() -> Option<Docker> {
    match Docker::connect_with_local_defaults() {
        Ok(d) => match d.ping().await {
            Ok(_) => Some(d),
            Err(e) => {
                eprintln!("skipping live round-trip: docker ping failed: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("skipping live round-trip: cannot connect to docker: {e}");
            None
        }
    }
}

/// Pull `repo:tag` so the create call has the image locally (CI runners start
/// with an empty image cache).
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

/// The "kitchen-sink" container body: every dimension the recreate cycle must
/// preserve.
fn weird_body(network: &str, static_ip: &str) -> ContainerCreateBody {
    let labels = HashMap::from([
        ("freshdock.enable".to_owned(), "true".to_owned()),
        ("freshdock.mode".to_owned(), "watch".to_owned()),
        ("com.example.team".to_owned(), "infra".to_owned()),
    ]);
    let log_config = HashMap::from([
        ("max-size".to_owned(), "10m".to_owned()),
        ("max-file".to_owned(), "3".to_owned()),
    ]);
    let sysctls = HashMap::from([("net.ipv4.ip_forward".to_owned(), "1".to_owned())]);
    let tmpfs = HashMap::from([("/run/fd".to_owned(), "rw,size=16m".to_owned())]);
    let endpoints = HashMap::from([(
        network.to_owned(),
        EndpointSettings {
            aliases: Some(vec![ALIAS.to_owned()]),
            ipam_config: Some(EndpointIpamConfig {
                ipv4_address: Some(static_ip.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )]);

    ContainerCreateBody {
        image: Some(IMAGE.to_owned()),
        env: Some(vec!["FD_TEST=hello".to_owned(), "ANSWER=42".to_owned()]),
        cmd: Some(vec![
            "nginx".to_owned(),
            "-g".to_owned(),
            "daemon off;".to_owned(),
        ]),
        working_dir: Some("/usr/share/nginx/html".to_owned()),
        user: Some("root".to_owned()),
        stop_signal: Some("SIGQUIT".to_owned()),
        stop_timeout: Some(15),
        labels: Some(labels),
        healthcheck: Some(HealthConfig {
            test: Some(vec!["CMD-SHELL".to_owned(), "true".to_owned()]),
            interval: Some(1_000_000_000),
            timeout: Some(1_000_000_000),
            retries: Some(2),
            start_period: Some(0),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            network_mode: Some(network.to_owned()),
            cap_add: Some(vec!["NET_ADMIN".to_owned()]),
            cap_drop: Some(vec!["SYS_ADMIN".to_owned()]),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".to_owned()),
                config: Some(log_config),
            }),
            sysctls: Some(sysctls),
            ulimits: Some(vec![ResourcesUlimits {
                name: Some("nofile".to_owned()),
                soft: Some(1024),
                hard: Some(4096),
            }]),
            tmpfs: Some(tmpfs),
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints),
        }),
        ..Default::default()
    }
}

/// Pull the image, create the custom network on the `octet`'s /24, then
/// create + start the kitchen-sink container. Returns `(network, container,
/// static_ip)`.
async fn spawn_weird(docker: &Docker, prefix: &str, octet: u8) -> (String, String, String) {
    let network = format!("{prefix}-net");
    let name = format!("{prefix}-web");
    let static_ip = static_ip_for(octet);

    ensure_image(docker, IMAGE)
        .await
        .expect("pull nginx:alpine");
    docker
        .create_network(NetworkCreateRequest {
            name: network.clone(),
            driver: Some("bridge".to_owned()),
            ipam: Some(Ipam {
                config: Some(vec![IpamConfig {
                    subnet: Some(subnet_for(octet)),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("create custom network");

    let opts = CreateContainerOptionsBuilder::new().name(&name).build();
    docker
        .create_container(Some(opts), weird_body(&network, &static_ip))
        .await
        .expect("create kitchen-sink container");
    docker
        .start_container(&name, None)
        .await
        .expect("start kitchen-sink container");

    (network, name, static_ip)
}

/// The alias may surface under `Aliases` or `DNSNames` depending on engine
/// version (Docker has been migrating endpoint aliases toward `DNSNames`).
fn endpoint_has_alias(ep: &EndpointSettings) -> bool {
    let has = |f: &Option<Vec<String>>| f.as_ref().is_some_and(|v| v.iter().any(|x| x == ALIAS));
    has(&ep.aliases) || has(&ep.dns_names)
}

#[tokio::test]
#[ignore = "needs-docker"]
async fn weird_config_recreate_roundtrip_is_byte_identical() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };

    let prefix = format!("fd-rt-{}", now_nanos());
    let _cleanup = Cleanup {
        prefix: prefix.clone(),
    };
    let (network, name, static_ip) = spawn_weird(&docker, &prefix, 77).await;

    // Snapshot the original config, then recreate through the real cycle.
    let before = docker
        .inspect_container(&name, None)
        .await
        .expect("inspect before");

    let fd = freshdock::docker::Docker::connect(Arc::new(CredentialStore::default()))
        .expect("freshdock docker connect");
    let cycle = recreate_one(&fd, &name, now_unix)
        .await
        .expect("recreate_one against live daemon");

    let after = docker
        .inspect_container(&name, None)
        .await
        .expect("inspect after");

    // --- identity: a genuinely new container, same image ref (issue #25) ---
    assert_ne!(
        before.id, after.id,
        "recreate must produce a new container id"
    );
    assert_eq!(
        after.id.as_deref(),
        Some(cycle.new_id.as_str()),
        "the running container must be the new instance returned by the cycle"
    );

    let bc = before.config.as_ref().expect("before config");
    let ac = after.config.as_ref().expect("after config");

    assert_eq!(
        bc.image, ac.image,
        "Config.Image must round-trip (nginx:alpine, not library/nginx:alpine) — issue #25"
    );

    // --- Config dimensions ---
    assert_eq!(bc.env, ac.env, "env drifted");
    assert_eq!(bc.cmd, ac.cmd, "cmd drifted");
    assert_eq!(bc.working_dir, ac.working_dir, "working_dir drifted");
    assert_eq!(bc.user, ac.user, "user drifted");
    assert_eq!(bc.stop_signal, ac.stop_signal, "stop_signal drifted");
    assert_eq!(bc.stop_timeout, ac.stop_timeout, "stop_timeout drifted");
    assert_eq!(
        bc.labels, ac.labels,
        "labels drifted (freshdock + user labels)"
    );

    let bh = bc.healthcheck.as_ref().expect("before healthcheck");
    let ah = ac.healthcheck.as_ref().expect("after healthcheck");
    assert_eq!(bh.test, ah.test, "healthcheck test drifted");
    assert_eq!(bh.interval, ah.interval, "healthcheck interval drifted");
    assert_eq!(bh.timeout, ah.timeout, "healthcheck timeout drifted");
    assert_eq!(bh.retries, ah.retries, "healthcheck retries drifted");
    assert_eq!(
        bh.start_period, ah.start_period,
        "healthcheck start_period drifted"
    );

    // --- HostConfig dimensions ---
    let bhc = before.host_config.as_ref().expect("before host_config");
    let ahc = after.host_config.as_ref().expect("after host_config");
    assert_eq!(bhc.cap_add, ahc.cap_add, "cap_add drifted");
    assert_eq!(bhc.cap_drop, ahc.cap_drop, "cap_drop drifted");
    assert_eq!(
        bhc.restart_policy.as_ref().and_then(|r| r.name),
        ahc.restart_policy.as_ref().and_then(|r| r.name),
        "restart_policy drifted"
    );
    assert_eq!(bhc.log_config, ahc.log_config, "log_config drifted");
    assert_eq!(bhc.sysctls, ahc.sysctls, "sysctls drifted");
    assert_eq!(bhc.ulimits, ahc.ulimits, "ulimits drifted");
    assert_eq!(bhc.tmpfs, ahc.tmpfs, "tmpfs drifted");

    // --- Network endpoint: alias + static IP on the custom network ---
    let before_net = before
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
        .and_then(|m| m.get(&network))
        .expect("before network endpoint");
    let after_net = after
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
        .and_then(|m| m.get(&network))
        .expect("after network endpoint — recreate must re-attach the custom network");

    assert!(
        endpoint_has_alias(before_net),
        "fixture should set the network alias"
    );
    assert!(
        endpoint_has_alias(after_net),
        "network alias drifted — recreate dropped the user-defined alias"
    );
    assert_eq!(
        before_net
            .ipam_config
            .as_ref()
            .and_then(|i| i.ipv4_address.as_deref()),
        after_net
            .ipam_config
            .as_ref()
            .and_then(|i| i.ipv4_address.as_deref()),
        "static IPv4 drifted"
    );
    assert_eq!(
        after_net
            .ipam_config
            .as_ref()
            .and_then(|i| i.ipv4_address.as_deref()),
        Some(static_ip.as_str()),
        "static IPv4 must be preserved exactly"
    );
}

/// Live coverage of the Phase-3 gated path: the kitchen-sink container has a
/// passing healthcheck, so `recreate_with_health` must report `Recreated` and
/// remove the `-old-` archive once the new container is healthy.
#[tokio::test]
#[ignore = "needs-docker"]
async fn recreate_with_health_removes_archive_on_live_success() {
    use freshdock::docker::recreate::recreate_with_health;
    use freshdock::health::{HealthConfig, TokioClock};
    use freshdock::updater::RecreateOutcome;

    let Some(docker) = connect_or_skip().await else {
        return;
    };

    let prefix = format!("fd-rt-{}", now_nanos());
    let _cleanup = Cleanup {
        prefix: prefix.clone(),
    };
    let (_network, name, _static_ip) = spawn_weird(&docker, &prefix, 78).await;

    let fd = freshdock::docker::Docker::connect(Arc::new(CredentialStore::default()))
        .expect("freshdock docker connect");
    let outcome = recreate_with_health(&fd, &name, &HealthConfig::default(), &TokioClock, now_unix)
        .await
        .expect("recreate_with_health against live daemon");

    let old_name = match outcome {
        RecreateOutcome::Recreated { old_name, .. } => old_name,
        RecreateOutcome::RolledBack(e) => panic!("healthy container must not roll back: {e:?}"),
    };

    // The new container exists under its original name and is running...
    let new = docker
        .inspect_container(&name, None)
        .await
        .expect("the recreated container must exist");
    assert_eq!(
        new.state.and_then(|s| s.running),
        Some(true),
        "the recreated container must be running"
    );
    // ...and the archive was removed on success — specifically a 404, not some
    // unrelated transport/daemon error masquerading as "gone".
    match docker.inspect_container(&old_name, None).await {
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {}
        Ok(_) => panic!("the -old- archive must be removed after a healthy gate"),
        Err(e) => panic!("expected a 404 for the removed archive, got: {e}"),
    }
}
