//! Live nerdctl gate: the update cycle against a **real** containerd, driven
//! through the nerdctl CLI exactly as `FRESHDOCK_RUNTIME=nerdctl` does.
//!
//! Opt-in twice over. The tests are `#[ignore]`d, and they only run when
//! `FRESHDOCK_NERDCTL_LIVE=1`; otherwise they skip, even under
//! `FRESHDOCK_LIVE_REQUIRED`, so the Docker live gate (`just live`) is not
//! turned red by a host without nerdctl. With the variable set, an unreachable
//! nerdctl is a failure, not a skip.
//!
//! Needs rootful nerdctl (2.3+), so run the test binary as root:
//!
//! ```bash
//! bin=$(cargo test --locked --test nerdctl_live --no-run --message-format=json \
//!   | jq -r 'select(.executable != null) | .executable')
//! sudo FRESHDOCK_NERDCTL_LIVE=1 "$bin" --ignored --test-threads=1
//! ```
//!
//! Everything happens in the `freshdock-live` containerd namespace, under
//! per-test name prefixes, and is removed afterwards, even on a failed
//! assertion.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use freshdock::config::{CredentialStore, Runtime, RuntimeSettings};
use freshdock::docker::check::DockerCheck;
use freshdock::docker::recreate::{Cleanup, DockerOps, recreate_with_health};
use freshdock::health::{HealthConfig, TokioClock};
use freshdock::labels::LifecycleHooks;
use freshdock::nerdctl::{Nerdctl, compare};
use freshdock::updater::RecreateOutcome;
use serde_json::Value;

const NS: &str = "freshdock-live";
const ALPINE: &str = "docker.io/library/alpine:3.20";
const NGINX: &str = "docker.io/library/nginx:1.27-alpine";

fn enabled() -> bool {
    std::env::var("FRESHDOCK_NERDCTL_LIVE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn nerdctl_bin() -> String {
    std::env::var("FRESHDOCK_NERDCTL_PATH").unwrap_or_else(|_| "nerdctl".to_owned())
}

/// Run nerdctl in the test namespace; `Err` carries its stderr.
fn nerdctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new(nerdctl_bin())
        .args(["--namespace", NS])
        .args(args)
        .output()
        .map_err(|e| format!("could not run nerdctl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn must(args: &[&str]) -> String {
    nerdctl(args).unwrap_or_else(|e| panic!("nerdctl {}: {e}", args.join(" ")))
}

fn inspect(name: &str) -> Option<Value> {
    let out = nerdctl(&["inspect", name]).ok()?;
    serde_json::from_str::<Vec<Value>>(&out)
        .ok()?
        .into_iter()
        .next()
}

fn native(name: &str) -> Value {
    let out = must(&["inspect", "--mode=native", name]);
    serde_json::from_str::<Vec<Value>>(&out).unwrap().remove(0)
}

fn id_of(name: &str) -> Option<String> {
    inspect(name)?["Id"].as_str().map(str::to_owned)
}

fn running(name: &str) -> bool {
    inspect(name).is_some_and(|c| c["State"]["Running"] == Value::Bool(true))
}

/// `portMappings` from the container's `network-config.json`.
fn port_mappings(name: &str) -> Value {
    let c = inspect(name).expect("container exists");
    let dir = c["Config"]["Labels"]["nerdctl/state-dir"]
        .as_str()
        .expect("state dir");
    let body = std::fs::read_to_string(format!("{dir}/network-config.json")).unwrap_or_default();
    serde_json::from_str::<Value>(&body).map_or(Value::Null, |v| v["portMappings"].clone())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn prefix(test: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("fdl-{test}-{nanos}")
}

/// Stops and removes every container whose name starts with the prefix, and
/// the listed networks. Stopping first matters: `rm -f` on a container with a
/// restart policy can race containerd's restart monitor and orphan a task
/// that keeps its ports.
struct Teardown {
    prefix: String,
    networks: Vec<String>,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        let names = nerdctl(&["ps", "-a", "--format", "{{.Names}}"]).unwrap_or_default();
        for name in names.lines().filter(|n| n.starts_with(&self.prefix)) {
            let _ = nerdctl(&["stop", "--time", "1", name]);
            if nerdctl(&["rm", name]).is_err() {
                let _ = nerdctl(&["rm", "--force", name]);
            }
        }
        for net in &self.networks {
            let _ = nerdctl(&["network", "rm", net]);
        }
    }
}

/// The backend under test, or `None` to skip.
async fn backend() -> Option<Nerdctl> {
    if !enabled() {
        eprintln!("skipping: set FRESHDOCK_NERDCTL_LIVE=1 to run the nerdctl live gate");
        return None;
    }
    let settings = RuntimeSettings {
        runtime: Runtime::Nerdctl,
        nerdctl_path: nerdctl_bin(),
        containerd_namespace: NS.to_owned(),
    };
    let n = Nerdctl::connect(&settings, Arc::new(CredentialStore::default()))
        .await
        .unwrap_or_else(|e| panic!("FRESHDOCK_NERDCTL_LIVE is set but nerdctl is unusable: {e}"));
    must(&["pull", "--quiet", ALPINE]);
    must(&["pull", "--quiet", NGINX]);
    Some(n)
}

fn health() -> HealthConfig {
    HealthConfig {
        health_timeout: Duration::from_secs(60),
        grace_period: Duration::from_secs(5),
        poll_interval: Duration::from_millis(500),
    }
}

const NO_CLEANUP: Cleanup = Cleanup {
    remove_replaced: false,
    prune_dangling: false,
};

async fn recreate(
    n: &Nerdctl,
    name: &str,
) -> Result<RecreateOutcome, freshdock::docker::DockerError> {
    recreate_with_health(
        n,
        name,
        &health(),
        &TokioClock,
        NO_CLEANUP,
        &LifecycleHooks::default(),
        now_unix,
    )
    .await
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn a_richly_configured_container_round_trips() {
    let Some(n) = backend().await else { return };
    let p = prefix("rt");
    let net = format!("{p}-net");
    let vol = format!("{p}-vol");
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![net.clone()],
    };
    must(&["network", "create", &net]);
    let name = format!("{p}-web");
    let volume = format!("{vol}:/data");
    #[rustfmt::skip]
    let args = [
        "run", "-d", "--name", &name,
        "--label", "freshdock.enable=true", "--label", "app=demo",
        "--network", &net,
        "-p", "127.0.0.1:18190:80", "-p", "[::1]:18191:81",
        "-v", &volume, "-v", "/etc/hostname:/host-hostname:ro", "--tmpfs", "/scratch",
        "-e", "FOO=bar", "-e", "SPACED=a b c", "--workdir", "/usr/share/nginx",
        "--restart", "unless-stopped",
        "--health-cmd", "wget -qO- http://127.0.0.1/ >/dev/null || exit 1",
        "--health-interval", "2s",
        "--hostname", "rt-host", "--add-host", "example.test:10.0.0.9", "--dns", "9.9.9.9",
        "--cap-add", "NET_ADMIN", "--memory", "256m", "--cpus", "0.5", "--pids-limit", "300",
        "--stop-signal", "SIGQUIT", "--stop-timeout", "20", "--log-opt", "max-size=5m",
        "--security-opt", "no-new-privileges", "--init", "--shm-size", "96m",
        NGINX,
    ];
    must(&args);
    let before_id = id_of(&name).unwrap();
    let before = native(&name);
    let before_ports = port_mappings(&name);

    let outcome = recreate(&n, &name).await.expect("recreate");
    assert!(
        matches!(outcome, RecreateOutcome::Recreated { .. }),
        "{outcome:?}"
    );

    let after_id = id_of(&name).expect("the container exists under its name");
    assert_ne!(after_id, before_id, "a new container replaced the old one");
    assert!(running(&name));
    let differences = compare::differences(&before, &before_id, &native(&name), &after_id);
    assert!(differences.is_empty(), "settings changed: {differences:?}");
    assert_eq!(
        port_mappings(&name),
        before_ports,
        "published ports carried over"
    );
    let names = must(&["ps", "-a", "--format", "{{.Names}}"]);
    assert!(
        !names
            .lines()
            .any(|l| l.starts_with(&format!("{name}-old-")) || l.contains("-freshdock-")),
        "no archive or throwaway container left: {names}"
    );
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn a_crashing_replacement_is_rolled_back() {
    let Some(n) = backend().await else { return };
    let p = prefix("rb");
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![],
    };
    let name = format!("{p}-app");
    // Stays up only while /tmp/ok exists in its own writable layer, which a
    // replacement never has.
    must(&[
        "run",
        "-d",
        "--name",
        &name,
        "--label",
        "freshdock.enable=true",
        ALPINE,
        "sh",
        "-c",
        "sleep 2; while [ -f /tmp/ok ]; do sleep 1; done; exit 3",
    ]);
    must(&["exec", &name, "touch", "/tmp/ok"]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before_id = id_of(&name).unwrap();

    let outcome = recreate(&n, &name).await.expect("recreate");
    assert!(
        matches!(outcome, RecreateOutcome::RolledBack(_)),
        "{outcome:?}"
    );
    assert_eq!(
        id_of(&name).as_deref(),
        Some(before_id.as_str()),
        "the original is back"
    );
    assert!(running(&name));
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn a_network_namespace_dependent_follows_its_owner() {
    let Some(n) = backend().await else { return };
    let p = prefix("ns");
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![],
    };
    let owner = format!("{p}-vpn");
    let dependent = format!("{p}-app");
    must(&[
        "run",
        "-d",
        "--name",
        &owner,
        "--label",
        "freshdock.enable=true",
        ALPINE,
        "sleep",
        "100000",
    ]);
    must(&[
        "run",
        "-d",
        "--name",
        &dependent,
        "--network",
        &format!("container:{owner}"),
        ALPINE,
        "sleep",
        "100000",
    ]);
    let dependent_before = id_of(&dependent).unwrap();

    let outcome = recreate(&n, &owner).await.expect("recreate");
    assert!(
        matches!(outcome, RecreateOutcome::Recreated { .. }),
        "{outcome:?}"
    );
    let owner_after = id_of(&owner).unwrap();
    let dep = inspect(&dependent).expect("dependent exists");
    assert_ne!(
        id_of(&dependent).unwrap(),
        dependent_before,
        "the dependent was re-created"
    );
    assert_eq!(
        dep["HostConfig"]["NetworkMode"],
        format!("container:{owner_after}")
    );
    assert!(running(&dependent));
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn a_setting_that_cannot_be_reproduced_is_refused_before_the_stop() {
    let Some(n) = backend().await else { return };
    let p = prefix("ip");
    let net = format!("{p}-net");
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![net.clone()],
    };
    must(&["network", "create", "--subnet", "10.199.77.0/24", &net]);
    let name = format!("{p}-app");
    must(&[
        "run",
        "-d",
        "--name",
        &name,
        "--label",
        "freshdock.enable=true",
        "--network",
        &net,
        "--ip",
        "10.199.77.9",
        ALPINE,
        "sleep",
        "100000",
    ]);
    let before_id = id_of(&name).unwrap();

    let spec = n.inspect(&name).await.expect("inspect");
    let err = n
        .preflight_recreate(&spec)
        .await
        .expect_err("a static IP is refused");
    assert!(err.to_string().contains("nerdctl/ip"), "{err}");
    let err = recreate(&n, &name)
        .await
        .expect_err("the cycle stops at preflight");
    assert!(err.to_string().contains("not updated"), "{err}");
    assert_eq!(
        id_of(&name).as_deref(),
        Some(before_id.as_str()),
        "untouched"
    );
    assert!(running(&name));
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn the_sweep_repairs_a_dead_freshdocks_leftovers_but_not_a_running_ones() {
    let Some(_) = backend().await else { return };
    let p = prefix("sw");
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![],
    };
    // Left by a freshdock that died (pid 999999999 can't exist): a service
    // stepped aside between the renames, and a never-started trial container.
    let service = format!("{p}-svc");
    let aside = format!("{service}-freshdock-aside-999999999-2-0");
    must(&["run", "-d", "--name", &aside, ALPINE, "sleep", "100000"]);
    let service_id = id_of(&aside).unwrap();
    let trial = format!("{p}-db-freshdock-check-999999999-2-1");
    must(&["create", "--name", &trial, ALPINE, "true"]);
    // The same, but made by a process that is still running (this test): a
    // daemon in the middle of an update.
    let live = std::process::id();
    let busy_aside = format!("{p}-busy-freshdock-aside-{live}-2-0");
    must(&[
        "run",
        "-d",
        "--name",
        &busy_aside,
        ALPINE,
        "sleep",
        "100000",
    ]);
    let busy_trial = format!("{p}-busy-freshdock-check-{live}-2-1");
    must(&["create", "--name", &busy_trial, ALPINE, "true"]);

    // Connecting alone (what `check` does) changes nothing.
    let n = backend().await.unwrap();
    assert!(
        inspect(&aside).is_some() && inspect(&trial).is_some(),
        "connect must not sweep"
    );

    n.sweep().await.expect("sweep");

    assert_eq!(
        id_of(&service).as_deref(),
        Some(service_id.as_str()),
        "the service has its name back"
    );
    assert!(running(&service));
    assert!(inspect(&aside).is_none());
    assert!(
        inspect(&trial).is_none(),
        "the dead process's trial is gone"
    );
    assert!(
        inspect(&busy_aside).is_some(),
        "a running freshdock's step-aside is left alone"
    );
    assert!(
        inspect(&busy_trial).is_some(),
        "a running freshdock's trial is left alone"
    );
}

/// A second namespace to publish from. Namespaces have separate image stores,
/// so the registry's tag can move while the test namespace's stays put, as
/// when upstream publishes a new image.
const STAGING_NS: &str = "freshdock-live-staging";

fn staging(args: &[&str]) -> Result<String, String> {
    let out = Command::new(nerdctl_bin())
        .args(["--namespace", STAGING_NS])
        .args(args)
        .output()
        .map_err(|e| format!("could not run nerdctl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Publish `source` as `tag` from the staging namespace, retrying while a
/// freshly started registry comes up.
fn publish(source: &str, tag: &str) {
    for args in [vec!["pull", "--quiet", source], vec!["tag", source, tag]] {
        staging(&args).unwrap_or_else(|e| panic!("staging nerdctl {}: {e}", args.join(" ")));
    }
    let mut last = String::new();
    for _ in 0..20 {
        match staging(&["push", tag]) {
            Ok(_) => return,
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("push {tag}: {last}");
}

fn image_id(reference: &str) -> String {
    let out = must(&["image", "inspect", reference]);
    serde_json::from_str::<Vec<Value>>(&out).unwrap()[0]["Id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
#[ignore = "needs rootful nerdctl; set FRESHDOCK_NERDCTL_LIVE=1"]
async fn siblings_on_a_tag_that_moves_are_both_updated() {
    let Some(n) = backend().await else { return };
    let p = prefix("tag");
    let tag = "127.0.0.1:15000/freshdock-live/shared:v";
    // Declared first so it drops last: the images can only go once the
    // containers using them have.
    let _images = ImageTeardown(tag);
    let _teardown = Teardown {
        prefix: p.clone(),
        networks: vec![],
    };
    must(&[
        "run",
        "-d",
        "--name",
        &format!("{p}-registry"),
        "-p",
        "127.0.0.1:15000:5000",
        "docker.io/library/registry:2",
    ]);
    publish(ALPINE, tag);
    must(&["pull", "--quiet", tag]);
    let old = image_id(tag);

    let (a, b) = (format!("{p}-a"), format!("{p}-b"));
    for c in [&a, &b] {
        must(&[
            "run",
            "-d",
            "--name",
            c,
            "--label",
            "freshdock.enable=true",
            tag,
            "sleep",
            "100000",
        ]);
    }
    // freshdock lists them before anything is pulled, as a scheduler pass does.
    let image_of = |listed: &[freshdock::docker::container::ContainerInfo], name: &str| {
        listed
            .iter()
            .find(|c| {
                c.names
                    .as_ref()
                    .is_some_and(|n| n.iter().any(|x| x == name))
            })
            .and_then(|c| c.image_id.clone())
    };
    let first = n.list_running().await.unwrap();
    assert_eq!(image_of(&first, &b).as_deref(), Some(old.as_str()));

    // Upstream publishes a new image under the same tag; updating A pulls it.
    publish("docker.io/library/busybox:1.36", tag);
    let outcome = recreate(&n, &a).await.expect("recreate a");
    assert!(
        matches!(outcome, RecreateOutcome::Recreated { .. }),
        "{outcome:?}"
    );
    let new = image_id(tag);
    assert_ne!(new, old, "A's pull moved the tag");

    // B still runs the old image, so it is behind its tag...
    let listed = n.list_running().await.unwrap();
    assert_eq!(
        image_of(&listed, &b).as_deref(),
        Some(old.as_str()),
        "B is seen behind its tag"
    );
    assert_eq!(image_of(&listed, &a).as_deref(), Some(new.as_str()));
    // ...and its update goes through: the trial is built from the image B
    // runs, not the new one, so nothing looks like a lost setting.
    let outcome = recreate(&n, &b).await.expect("recreate b is not refused");
    assert!(
        matches!(outcome, RecreateOutcome::Recreated { .. }),
        "{outcome:?}"
    );
    let listed = n.list_running().await.unwrap();
    assert_eq!(
        image_of(&listed, &b).as_deref(),
        Some(new.as_str()),
        "B is on the new image"
    );
}

/// Removes the test registry's images from both namespaces.
struct ImageTeardown(&'static str);

impl Drop for ImageTeardown {
    fn drop(&mut self) {
        let repo = self.0.rsplit_once(':').map_or(self.0, |(r, _)| r);
        for run in [nerdctl as fn(&[&str]) -> Result<String, String>, staging] {
            let out = run(&["images", "--format", "{{.Repository}}:{{.Tag}}@{{.Digest}}"])
                .unwrap_or_default();
            for line in out.lines().filter(|l| l.starts_with(repo)) {
                let (name, digest) = line.split_once('@').unwrap_or((line, ""));
                let target = if name.ends_with(":<none>") {
                    format!("{repo}@{digest}")
                } else {
                    name.to_owned()
                };
                let _ = run(&["rmi", "--force", &target]);
            }
        }
    }
}
