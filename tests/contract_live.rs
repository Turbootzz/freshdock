//! Live contract gate: the daemon-facing promises no fake can prove. Needs a
//! real Docker daemon; without one each test skips, unless
//! `FRESHDOCK_LIVE_REQUIRED` is set, which turns a skip into a failure.
//! `#[ignore]`d; run them with `just live`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::models::{
    ContainerConfig, ContainerCreateBody, ContainerInspectResponse, HealthConfig, HostConfig,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    ListContainersOptions, ListImagesOptions, RemoveContainerOptionsBuilder, RemoveImageOptions,
    RemoveImageOptionsBuilder, TagImageOptionsBuilder,
};
use futures::StreamExt;

/// Everything that does not move a tag runs on `alpine`; its `nc` serves a port.
const IMAGE: &str = "alpine:latest";

/// The cleanup test re-points a tag, so it gets one no other live suite uses.
const MOVING_IMAGE: &str = "redis:alpine";

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Best-effort teardown on its own runtime thread, so a panic unwind still runs it.
struct Cleanup {
    prefix: String,
    /// `(image id, "repo:tag")`: restore a tag this run moved.
    restore: Vec<(String, String)>,
    /// Images to remove, newest first: a committed child goes before its base.
    images: Vec<String>,
    dirs: Vec<PathBuf>,
}

impl Cleanup {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            restore: Vec::new(),
            images: Vec::new(),
            dirs: Vec::new(),
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let prefix = self.prefix.clone();
        let restore = std::mem::take(&mut self.restore);
        let images = std::mem::take(&mut self.images);
        let dirs = std::mem::take(&mut self.dirs);
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
                for (image, tag) in restore {
                    let _ = tag_image_as(&docker, &image, &tag).await;
                }
                for image in images.iter().rev() {
                    if docker
                        .remove_image(image, None::<RemoveImageOptions>, None)
                        .await
                        .is_err()
                    {
                        let opts = RemoveImageOptionsBuilder::default().force(true).build();
                        let _ = docker.remove_image(image, Some(opts), None).await;
                    }
                }
                // After the sweep: a container may still be writing into one.
                for dir in dirs {
                    let _ = std::fs::remove_dir_all(dir);
                }
            });
        })
        .join();
    }
}

/// Is a daemon required? In CI's live gate a missing daemon must fail loudly.
fn live_required() -> bool {
    std::env::var("FRESHDOCK_LIVE_REQUIRED")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn skip_or_panic(why: String) -> Option<Docker> {
    assert!(
        !live_required(),
        "FRESHDOCK_LIVE_REQUIRED is set, so this live gate must not be skipped: {why}"
    );
    eprintln!("{why}");
    None
}

async fn connect_or_skip() -> Option<Docker> {
    match Docker::connect_with_local_defaults() {
        Ok(d) => match d.ping().await {
            Ok(_) => Some(d),
            Err(e) => skip_or_panic(format!("skipping live contract test: docker ping: {e}")),
        },
        Err(e) => skip_or_panic(format!(
            "skipping live contract test: cannot connect to docker: {e}"
        )),
    }
}

/// Pull only what is missing: anonymous Hub requests are rate limited.
async fn ensure_image(docker: &Docker, image: &str) -> Result<(), bollard::errors::Error> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
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

async fn tag_image_as(
    docker: &Docker,
    image: &str,
    target: &str,
) -> Result<(), bollard::errors::Error> {
    let (repo, tag) = target.split_once(':').unwrap_or((target, "latest"));
    let opts = TagImageOptionsBuilder::default()
        .repo(repo)
        .tag(tag)
        .build();
    docker.tag_image(image, Some(opts)).await
}

/// Every image id in the store, so teardown can tell what this run added.
async fn image_ids(docker: &Docker) -> HashSet<String> {
    let opts = ListImagesOptions {
        all: true,
        ..Default::default()
    };
    docker
        .list_images(Some(opts))
        .await
        .map(|images| images.into_iter().map(|i| i.id).collect())
        .unwrap_or_default()
}

async fn image_id(docker: &Docker, image: &str) -> String {
    docker
        .inspect_image(image)
        .await
        .unwrap_or_else(|e| panic!("inspect image {image}: {e}"))
        .id
        .expect("image id")
}

/// Run the built binary from a fresh empty working directory: a `freshdock.toml`
/// beside the process is loaded automatically. Both streams come back joined.
fn freshdock(args: &[&str]) -> (Output, String) {
    let cwd = std::env::temp_dir().join(format!("fd-live-cwd-{}", now_nanos()));
    std::fs::create_dir_all(&cwd).expect("create the child's working directory");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_freshdock"));
    cmd.args(args)
        .current_dir(&cwd)
        .env("NO_COLOR", "1")
        .env_remove("FRESHDOCK_CONFIG")
        .env_remove("FRESHDOCK_DEFAULT_MODE")
        .env_remove("FRESHDOCK_CLEANUP")
        .env_remove("FRESHDOCK_PRUNE_DANGLING")
        .env_remove("FRESHDOCK_WATCH_ALL")
        .env_remove("FRESHDOCK_COMPOSE_AWARE")
        .env_remove("FRESHDOCK_ONE_SHOT_TIMEOUT");
    let out = cmd.output().expect("run the freshdock binary");
    let _ = std::fs::remove_dir(&cwd);

    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("--- freshdock {}\n{output}---", args.join(" "));
    (out, output)
}

fn recreate(name: &str) -> String {
    let (out, output) = freshdock(&["recreate", name]);
    assert!(
        out.status.success(),
        "freshdock recreate {name} exited {}:\n{output}",
        out.status
    );
    output
}

/// For outcomes whose exit code is not a contract: refusals, rollbacks, aborts.
fn recreate_unchecked(name: &str) -> String {
    freshdock(&["recreate", name]).1
}

/// A container body on [`IMAGE`]. `stop_timeout` is 1 s: busybox ignores SIGTERM.
fn alpine_body(cmd: &[&str], labels: &[(&str, &str)]) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(IMAGE.to_owned()),
        cmd: Some(cmd.iter().map(|s| (*s).to_owned()).collect()),
        stop_timeout: Some(1),
        labels: Some(
            labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<HashMap<_, _>>(),
        ),
        ..Default::default()
    }
}

/// Passes on its first probe, so the health gate resolves in about a second.
fn fast_healthcheck() -> HealthConfig {
    HealthConfig {
        test: Some(vec!["CMD-SHELL".to_owned(), "true".to_owned()]),
        interval: Some(1_000_000_000),
        timeout: Some(1_000_000_000),
        retries: Some(3),
        start_period: Some(0),
        ..Default::default()
    }
}

async fn spawn(docker: &Docker, name: &str, body: ContainerCreateBody) -> String {
    let opts = CreateContainerOptionsBuilder::new().name(name).build();
    docker
        .create_container(Some(opts), body)
        .await
        .unwrap_or_else(|e| panic!("create {name}: {e}"));
    docker
        .start_container(name, None)
        .await
        .unwrap_or_else(|e| panic!("start {name}: {e}"));
    id_of(docker, name).await
}

async fn inspect(docker: &Docker, name: &str) -> ContainerInspectResponse {
    docker
        .inspect_container(name, None)
        .await
        .unwrap_or_else(|e| panic!("inspect {name}: {e}"))
}

async fn id_of(docker: &Docker, name: &str) -> String {
    inspect(docker, name).await.id.expect("container id")
}

async fn is_running(docker: &Docker, name: &str) -> bool {
    inspect(docker, name)
        .await
        .state
        .and_then(|s| s.running)
        .unwrap_or(false)
}

async fn started_at(docker: &Docker, name: &str) -> String {
    inspect(docker, name)
        .await
        .state
        .and_then(|s| s.started_at)
        .expect("StartedAt")
}

/// The `<name>-old-<ts>` archives left under `prefix`; empty is success.
async fn archives(docker: &Docker, prefix: &str) -> Vec<String> {
    let opts = ListContainersOptions {
        all: true,
        ..Default::default()
    };
    docker
        .list_containers(Some(opts))
        .await
        .expect("list containers")
        .into_iter()
        .filter_map(|c| c.names)
        .flatten()
        .map(|n| n.trim_start_matches('/').to_owned())
        .filter(|n| n.starts_with(prefix) && n.contains("-old-"))
        .collect()
}

/// Run `cmd` inside `container`. Failing to exec at all is fatal, not a verdict.
async fn exec_capture(docker: &Docker, container: &str, cmd: &[&str]) -> (String, i64) {
    use bollard::exec::{CreateExecOptions, StartExecResults};

    let created = docker
        .create_exec(
            container,
            CreateExecOptions {
                cmd: Some(cmd.to_vec()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("create exec in {container}: {e}"));

    let mut collected = String::new();
    match docker.start_exec(&created.id, None).await {
        Ok(StartExecResults::Attached { mut output, .. }) => {
            while let Some(chunk) = output.next().await {
                let chunk = chunk.unwrap_or_else(|e| panic!("exec output from {container}: {e}"));
                collected.push_str(&chunk.to_string());
            }
        }
        Ok(StartExecResults::Detached) => panic!("exec in {container} detached unexpectedly"),
        Err(e) => panic!("start exec in {container}: {e}"),
    }

    // The daemon finalises exec state after the stream closes.
    let mut inspected = inspect_exec(docker, container, &created.id).await;
    for _ in 0..20 {
        if inspected.running != Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        inspected = inspect_exec(docker, container, &created.id).await;
    }
    (collected, inspected.exit_code.unwrap_or(-1))
}

async fn inspect_exec(
    docker: &Docker,
    container: &str,
    exec_id: &str,
) -> bollard::models::ExecInspectResponse {
    docker
        .inspect_exec(exec_id)
        .await
        .unwrap_or_else(|e| panic!("inspect exec in {container}: {e}"))
}

/// The busybox listener re-arms between connections, so a single read is a race.
async fn read_from_namespace(docker: &Docker, container: &str) -> String {
    let mut last = String::new();
    for _ in 0..20 {
        let (out, code) = exec_capture(docker, container, &["nc", "127.0.0.1", "8080"]).await;
        if code == 0 && out.contains("fdlive") {
            return out;
        }
        last = format!("nc exited {code}: {out}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last
}

async fn wait_until_exited(docker: &Docker, name: &str) {
    for _ in 0..60 {
        if !is_running(docker, name).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} never exited");
}

/// A host directory the containers bind-mount, so a test can read what they wrote.
fn scratch_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(prefix);
    std::fs::create_dir_all(&dir).expect("create the bind-mount directory");
    dir
}

fn assert_contains(haystack: &str, needle: &str, what: &str) {
    assert!(
        haystack.contains(needle),
        "{what}: expected {needle:?} in output:\n{haystack}"
    );
}

// --- rollback: the safety promise the whole tool rests on ---

/// A replacement that dies on start must leave the *previous* container running
/// under its own id, with no archive left behind.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_crashing_replacement_rolls_back_to_the_previous_container() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-rb-{}", now_nanos());
    let mut cleanup = Cleanup::new(&prefix);
    let dir = scratch_dir(&prefix);
    cleanup.dirs.push(dir.clone());
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let name = format!("{prefix}-app");
    let script = "n=$(cat /fd/n 2>/dev/null || echo 0); n=$((n+1)); echo $n > /fd/n; \
                  if [ \"$n\" = 2 ]; then exit 3; fi; exec sleep 600";
    let mut body = alpine_body(
        &["sh", "-c", script],
        &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
    );
    body.host_config = Some(HostConfig {
        binds: Some(vec![format!("{}:/fd", dir.display())]),
        ..Default::default()
    });
    let before = spawn(&docker, &name, body).await;

    // Unchecked: a rollback's exit code is not the contract this test asserts.
    let output = recreate_unchecked(&name);
    assert_contains(&output, "rolled back", "a crashed update must report it");

    assert_eq!(
        id_of(&docker, &name).await,
        before,
        "rollback must restore the previous container, not a copy of it"
    );
    assert!(
        is_running(&docker, &name).await,
        "the restored container must be running again"
    );
    assert!(
        archives(&docker, &prefix).await.is_empty(),
        "the archive is renamed back on rollback, so none may survive"
    );
}

// --- spec preservation the kitchen-sink round-trip does not cover ---

/// `recreate_roundtrip_live.rs` pins the wide config surface; this pins what it
/// leaves out, plus `Config.Image` coming back as the tag, never a digest.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_recreated_container_keeps_its_restart_policy_and_stop_signal() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-spec-{}", now_nanos());
    let _cleanup = Cleanup::new(&prefix);
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let name = format!("{prefix}-app");
    let mut body = alpine_body(
        &["sleep", "600"],
        &[("freshdock.enable", "true"), ("freshdock.mode", "watch")],
    );
    body.stop_signal = Some("SIGINT".to_owned());
    body.stop_timeout = Some(2);
    body.healthcheck = Some(fast_healthcheck());
    body.host_config = Some(HostConfig {
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::ON_FAILURE),
            maximum_retry_count: Some(3),
        }),
        ..Default::default()
    });
    let before = spawn(&docker, &name, body).await;

    let output = recreate(&name);
    assert_contains(&output, &format!("recreated {name}: healthy"), "update");

    let after = inspect(&docker, &name).await;
    assert_ne!(
        after.id.as_deref(),
        Some(before.as_str()),
        "a healthy update must produce a new container"
    );
    let config = after.config.expect("config");
    assert_eq!(
        config.stop_signal.as_deref(),
        Some("SIGINT"),
        "stop signal drifted"
    );
    assert_eq!(config.stop_timeout, Some(2), "stop timeout drifted");
    assert_eq!(
        config.image.as_deref(),
        Some(IMAGE),
        "Config.Image must round-trip as the tag, never a digest (issue #25)"
    );

    let policy = after
        .host_config
        .and_then(|h| h.restart_policy)
        .expect("restart policy");
    assert_eq!(
        policy.name,
        Some(RestartPolicyNameEnum::ON_FAILURE),
        "restart policy drifted"
    );
    assert_eq!(
        policy.maximum_retry_count,
        Some(3),
        "restart policy retry count drifted"
    );
}

// --- network-namespace dependents ---

/// A labelled owner serving a port inside its own network namespace.
async fn spawn_namespace_owner(docker: &Docker, name: &str) -> String {
    let mut body = alpine_body(
        &[
            "sh",
            "-c",
            "while true; do echo fdlive | nc -l -p 8080; done",
        ],
        &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
    );
    body.healthcheck = Some(HealthConfig {
        test: Some(vec![
            "CMD-SHELL".to_owned(),
            "netstat -ltn | grep -q :8080".to_owned(),
        ]),
        interval: Some(1_000_000_000),
        timeout: Some(2_000_000_000),
        retries: Some(5),
        start_period: Some(0),
        ..Default::default()
    });
    let id = spawn(docker, name, body).await;
    for _ in 0..40 {
        if inspect(docker, name)
            .await
            .state
            .and_then(|s| s.health)
            .and_then(|h| h.status)
            .is_some_and(|s| s.to_string() == "healthy")
        {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} never became healthy");
}

fn dependent_body(owner: &str, labels: &[(&str, &str)]) -> ContainerCreateBody {
    let mut body = alpine_body(&["sleep", "600"], labels);
    body.host_config = Some(HostConfig {
        network_mode: Some(format!("container:{owner}")),
        ..Default::default()
    });
    body
}

/// Recreating the owner destroys the namespace its dependents live in. The
/// sidecar must come back attached to the new owner, on its same image id.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_network_namespace_dependent_is_reattached_to_the_new_owner() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-dep-{}", now_nanos());
    let _cleanup = Cleanup::new(&prefix);
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let owner = format!("{prefix}-owner");
    let side = format!("{prefix}-side");
    let old_owner_id = spawn_namespace_owner(&docker, &owner).await;
    spawn(&docker, &side, dependent_body(&owner, &[])).await;

    assert_contains(
        &read_from_namespace(&docker, &side).await,
        "fdlive",
        "the fixture must share the owner's namespace before the update",
    );
    let image_before = inspect(&docker, &side).await.image.expect("sidecar image");

    let output = recreate(&owner);
    assert_contains(&output, &format!("recreated {owner}: healthy"), "update");

    let new_owner_id = id_of(&docker, &owner).await;
    assert_ne!(new_owner_id, old_owner_id, "the owner must be replaced");

    let after = inspect(&docker, &side).await;
    assert_eq!(
        after.host_config.and_then(|h| h.network_mode),
        Some(format!("container:{new_owner_id}")),
        "the dependent must be repointed at the new owner id, not the dead one"
    );
    assert_eq!(
        after.image,
        Some(image_before),
        "a repair re-creates from the image id, so it can never smuggle in an upgrade"
    );
    assert_contains(
        &read_from_namespace(&docker, &side).await,
        "fdlive",
        "the repaired dependent must reach the new owner's namespace",
    );
    assert!(
        archives(&docker, &prefix).await.is_empty(),
        "neither the owner's nor the dependent's archive may survive"
    );
}

/// The repair bypasses the policy gate, but not an explicit opt-out: that
/// dependent is left alone, and the owner's own update still succeeds.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_dependent_that_opts_out_is_left_alone() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-optout-{}", now_nanos());
    let _cleanup = Cleanup::new(&prefix);
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let owner = format!("{prefix}-owner");
    let side = format!("{prefix}-side");
    spawn_namespace_owner(&docker, &owner).await;
    spawn(
        &docker,
        &side,
        dependent_body(&owner, &[("freshdock.enable", "false")]),
    )
    .await;
    let started_before = started_at(&docker, &side).await;

    let output = recreate(&owner);
    assert_contains(&output, &format!("recreated {owner}: healthy"), "update");

    assert_eq!(
        started_at(&docker, &side).await,
        started_before,
        "an opted-out dependent must not be re-created"
    );
}

// --- compose rollouts ---

/// A two-service project written straight into container labels, so no compose
/// file or binary is needed. `migrate` is the exited one-shot `web` waits on.
async fn spawn_project(docker: &Docker, prefix: &str, dir: &std::path::Path) -> (String, String) {
    let migrate = format!("{prefix}-migrate-1");
    let web = format!("{prefix}-web-1");
    let bind = format!("{}:/d", dir.display());

    let mut migrate_body = alpine_body(
        &[
            "sh",
            "-c",
            "if [ -f /d/fail ]; then echo failed >> /d/log; exit 1; fi; echo ran >> /d/log",
        ],
        &[
            ("com.docker.compose.project", prefix),
            ("com.docker.compose.service", "migrate"),
            ("com.docker.compose.depends_on", ""),
        ],
    );
    migrate_body.host_config = Some(HostConfig {
        binds: Some(vec![bind]),
        ..Default::default()
    });
    spawn(docker, &migrate, migrate_body).await;
    wait_until_exited(docker, &migrate).await;

    let mut web_body = alpine_body(
        &["sleep", "600"],
        &[
            ("com.docker.compose.project", prefix),
            ("com.docker.compose.service", "web"),
            (
                "com.docker.compose.depends_on",
                "migrate:service_completed_successfully:false",
            ),
            ("freshdock.enable", "true"),
            ("freshdock.mode", "live"),
        ],
    );
    web_body.healthcheck = Some(fast_healthcheck());
    spawn(docker, &web, web_body).await;

    (migrate, web)
}

fn log_lines(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("log"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The unlabelled one-shot is re-run exactly once, before the service waiting on it.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_compose_rollout_reruns_the_awaited_one_shot_first() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-roll-{}", now_nanos());
    let mut cleanup = Cleanup::new(&prefix);
    let dir = scratch_dir(&prefix);
    cleanup.dirs.push(dir.clone());
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let (migrate, web) = spawn_project(&docker, &prefix, &dir).await;
    assert_eq!(log_lines(&dir), 1, "the fixture ran the migration once");
    let web_before = id_of(&docker, &web).await;

    let output = recreate(&web);

    assert_eq!(
        log_lines(&dir),
        2,
        "the awaited one-shot must be re-run exactly once more"
    );
    assert_ne!(
        id_of(&docker, &web).await,
        web_before,
        "the service must be updated after its migration"
    );

    let one_shot_step = output
        .find(&format!("{migrate}: re-ran"))
        .unwrap_or_else(|| panic!("no one-shot step in:\n{output}"));
    let service_step = output
        .find(&format!("{web}: updated and healthy"))
        .unwrap_or_else(|| panic!("no service step in:\n{output}"));
    assert!(
        one_shot_step < service_step,
        "depends_on order: the one-shot must be reported before the service:\n{output}"
    );
    assert!(
        archives(&docker, &prefix).await.is_empty(),
        "a completed rollout removes every archive it made"
    );
}

/// A failed migration stops the rollout dead: the service stays on its previous
/// image, and the failed one-shot and its archive are kept for their logs.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_failed_one_shot_aborts_the_rollout_and_keeps_its_evidence() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-abort-{}", now_nanos());
    let mut cleanup = Cleanup::new(&prefix);
    let dir = scratch_dir(&prefix);
    cleanup.dirs.push(dir.clone());
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let (migrate, web) = spawn_project(&docker, &prefix, &dir).await;
    let web_before = id_of(&docker, &web).await;
    std::fs::write(dir.join("fail"), "").expect("arm the failing migration");

    // Unchecked: an aborted rollout's exit code is not this test's contract.
    let output = recreate_unchecked(&web);
    assert_contains(&output, "rollout ABORTED", "a failed one-shot aborts");

    assert_eq!(
        id_of(&docker, &web).await,
        web_before,
        "nothing downstream of a failed migration may be touched"
    );
    let failed = inspect(&docker, &migrate).await.state.expect("state");
    assert_eq!(
        failed.exit_code,
        Some(1),
        "the failed one-shot must be kept as it died"
    );
    let kept = archives(&docker, &prefix).await;
    assert!(
        kept.iter().any(|n| n.starts_with(&migrate)),
        "the one-shot's archive is deliberately kept too, got {kept:?}"
    );
}

// --- lifecycle hooks ---

/// `EX_TEMPFAIL` from the pre-update hook defers: the container is not touched.
#[tokio::test]
#[ignore = "needs-docker"]
async fn a_pre_update_hook_exiting_75_defers_the_update() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-hook-{}", now_nanos());
    let _cleanup = Cleanup::new(&prefix);
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let name = format!("{prefix}-app");
    let before = spawn(
        &docker,
        &name,
        alpine_body(
            &["sleep", "600"],
            &[
                ("freshdock.enable", "true"),
                ("freshdock.mode", "live"),
                ("freshdock.lifecycle.pre-update", "exit 75"),
            ],
        ),
    )
    .await;

    let output = recreate(&name);
    assert_contains(&output, &format!("recreate skipped for {name}"), "deferral");

    assert_eq!(
        id_of(&docker, &name).await,
        before,
        "a deferred update must leave the container exactly as it was"
    );
    assert!(
        archives(&docker, &prefix).await.is_empty(),
        "a deferral happens before the stop, so nothing may be archived"
    );
}

// --- image cleanup ---

/// Point [`MOVING_IMAGE`] at an image committed from the real one, so nothing
/// else references it. Teardown is registered before the tag moves.
async fn seed_superseded_image(docker: &Docker, prefix: &str, cleanup: &mut Cleanup) -> String {
    match docker
        .inspect_image(MOVING_IMAGE)
        .await
        .ok()
        .and_then(|i| i.id)
    {
        Some(prior) => cleanup.restore.push((prior, MOVING_IMAGE.to_owned())),
        None => {
            ensure_image(docker, MOVING_IMAGE)
                .await
                .expect("pull the moving image");
            let pulled = image_id(docker, MOVING_IMAGE).await;
            cleanup.images.push(pulled);
        }
    }

    let seed = format!("{prefix}-seed");
    let opts = CreateContainerOptionsBuilder::new().name(&seed).build();
    docker
        .create_container(
            Some(opts),
            ContainerCreateBody {
                image: Some(MOVING_IMAGE.to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("create the seed container");
    let committed = docker
        .commit_container(
            CommitContainerOptionsBuilder::default()
                .container(&seed)
                .repo(prefix)
                .tag("superseded")
                .build(),
            ContainerConfig::default(),
        )
        .await
        .expect("commit the seed container")
        .id;
    cleanup.images.push(committed.clone());
    let ropts = RemoveContainerOptionsBuilder::new().force(true).build();
    docker
        .remove_container(&seed, Some(ropts))
        .await
        .expect("remove the seed container");

    tag_image_as(docker, &committed, MOVING_IMAGE)
        .await
        .expect("re-point the moving tag");
    // Drop the private tag: a still-referenced image is refused with a 409.
    docker
        .remove_image(
            &format!("{prefix}:superseded"),
            None::<RemoveImageOptions>,
            None,
        )
        .await
        .expect("untag the committed image");
    committed
}

fn cleanup_body(cleanup_label: Option<&str>) -> ContainerCreateBody {
    let mut labels = vec![
        ("freshdock.enable".to_owned(), "true".to_owned()),
        ("freshdock.mode".to_owned(), "watch".to_owned()),
    ];
    if let Some(value) = cleanup_label {
        labels.push(("freshdock.cleanup".to_owned(), value.to_owned()));
    }
    ContainerCreateBody {
        image: Some(MOVING_IMAGE.to_owned()),
        stop_timeout: Some(2),
        labels: Some(labels.into_iter().collect()),
        healthcheck: Some(fast_healthcheck()),
        ..Default::default()
    }
}

/// The superseded image is removed after a healthy update only with
/// `freshdock.cleanup=true`. Asserted on image ids: the tag moves either way.
#[tokio::test]
#[ignore = "needs-docker"]
async fn image_cleanup_removes_the_superseded_image_only_when_opted_in() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-clean-{}", now_nanos());
    let mut cleanup = Cleanup::new(&prefix);
    let host_images = image_ids(&docker).await;
    let superseded = seed_superseded_image(&docker, &prefix, &mut cleanup).await;

    let kept = format!("{prefix}-kept");
    spawn(&docker, &kept, cleanup_body(None)).await;
    let output = recreate(&kept);
    assert_contains(&output, &format!("recreated {kept}: healthy"), "update");
    assert!(
        docker.inspect_image(&superseded).await.is_ok(),
        "without the cleanup label the superseded image must be kept"
    );

    // Remove what the update's pull added, unless the host already had it.
    let pulled = image_id(&docker, MOVING_IMAGE).await;
    if !host_images.contains(&pulled) {
        cleanup.images.push(pulled);
    }

    let removed = format!("{prefix}-removed");
    tag_image_as(&docker, &superseded, MOVING_IMAGE)
        .await
        .expect("re-point the moving tag");
    spawn(&docker, &removed, cleanup_body(Some("true"))).await;
    let output = recreate(&removed);
    assert_contains(&output, &format!("recreated {removed}: healthy"), "update");
    assert!(
        docker.inspect_image(&superseded).await.is_err(),
        "freshdock.cleanup=true must remove the image the replaced container ran"
    );
}

// --- the policy gate ---

/// An unlabelled container is refused, and refused *before* the pull. It runs on
/// a local-only tag, so a pull would fail loudly: no pull error is the evidence.
#[tokio::test]
#[ignore = "needs-docker"]
async fn an_unlabelled_container_is_refused_and_left_untouched() {
    let Some(docker) = connect_or_skip().await else {
        return;
    };
    let prefix = format!("fd-live-plain-{}", now_nanos());
    let mut cleanup = Cleanup::new(&prefix);
    ensure_image(&docker, IMAGE)
        .await
        .expect("alpine available");

    let local_tag = format!("{prefix}:unpullable");
    tag_image_as(&docker, IMAGE, &local_tag)
        .await
        .expect("create the local-only tag");
    cleanup.images.push(local_tag.clone());

    let name = format!("{prefix}-app");
    let mut body = alpine_body(&["sleep", "600"], &[]);
    body.image = Some(local_tag);
    let before = spawn(&docker, &name, body).await;

    let output = recreate_unchecked(&name);
    assert_contains(
        &output,
        "refusing to recreate",
        "the run must reach the policy gate and say it refused",
    );
    assert!(
        !output.contains("pull access denied"),
        "the gate must refuse before the pull:\n{output}"
    );
    assert_eq!(
        id_of(&docker, &name).await,
        before,
        "a refused container must not be recreated"
    );
    assert!(
        archives(&docker, &prefix).await.is_empty(),
        "a refused container must not be archived either:\n{output}"
    );
}
