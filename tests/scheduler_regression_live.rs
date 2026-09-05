//! Live regressions: a second container sharing a tag is still updated after the
//! first moved the tag, and a compose one-shot is still re-run when its project
//! rolls out. Needs a real Docker daemon, so `#[ignore]`d; run with:
//!
//! ```bash
//! cargo test --test scheduler_regression_live -- --ignored --test-threads=1
//! ```

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptions,
    RemoveContainerOptionsBuilder, TagImageOptionsBuilder,
};
use futures::StreamExt;

/// The tag both tests move; `busybox` so the other live suites keep theirs.
const TAG: &str = "busybox:latest";
/// A pinned older release, the stale image the tag is pointed at.
const OLD: &str = "busybox:1.36";

/// How long the daemon is left running per test.
const DAEMON_SECS: u64 = 45;

/// The daemon sweeps every enabled container, so two of these would collide.
static SERIAL: Mutex<()> = Mutex::new(());

/// Take the lock, recovering from a poison so one failure cannot cascade.
fn serialise() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Best-effort, prefix-scoped teardown on its own runtime thread, so it works
/// during a panic unwind. Re-pulls [`TAG`] so a failure never leaves it moved.
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
                // freshdock's own pull normally restores the tag already.
                let _ = pull(&docker, TAG).await;
            });
        })
        .join();
    }
}

/// Is a daemon required? `FRESHDOCK_LIVE_REQUIRED=1` turns a skip into a failure.
fn live_required() -> bool {
    std::env::var("FRESHDOCK_LIVE_REQUIRED")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Note a skip, or fail when [`live_required`] says a skip is not allowed.
/// Every bail-out goes through here, or the CI gate passes having run nothing.
fn skip_or_panic(why: String) {
    assert!(
        !live_required(),
        "FRESHDOCK_LIVE_REQUIRED is set, so this live gate must not be skipped: {why}"
    );
    eprintln!("{why}");
}

async fn connect_or_skip() -> Option<Docker> {
    let why = match Docker::connect_with_local_defaults() {
        Ok(d) => match d.ping().await {
            Ok(_) => return Some(d),
            Err(e) => format!("skipping live regression test: docker ping failed: {e}"),
        },
        Err(e) => format!("skipping live regression test: cannot connect to docker: {e}"),
    };
    skip_or_panic(why);
    None
}

async fn pull(docker: &Docker, image: &str) -> Result<(), bollard::errors::Error> {
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

async fn image_id(docker: &Docker, image: &str) -> String {
    docker
        .inspect_image(image)
        .await
        .unwrap_or_else(|e| panic!("inspect {image}: {e}"))
        .id
        .unwrap_or_else(|| panic!("{image} has no id"))
}

/// Point [`TAG`] at the older image; `None` when the two have converged.
async fn stale_tag(docker: &Docker) -> Option<()> {
    // Pull `latest` first: it restores a tag an earlier run left moved.
    pull(docker, TAG).await.expect("pull busybox:latest");
    pull(docker, OLD).await.expect("pull busybox:1.36");

    let current = image_id(docker, TAG).await;
    let old = image_id(docker, OLD).await;
    if current == old {
        skip_or_panic(format!(
            "{OLD} and {TAG} now resolve to the same image, so there is no stale \
             image for a container to be behind; replace the pinned tag in {} \
             with an older release",
            file!()
        ));
        return None;
    }

    let opts = TagImageOptionsBuilder::new()
        .repo("busybox")
        .tag("latest")
        .build();
    docker
        .tag_image(OLD, Some(opts))
        .await
        .expect("point busybox:latest at the old image");
    Some(())
}

fn label_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// A long-running container on [`TAG`]. Short stop timeout: busybox ignores SIGTERM.
fn sleeper(labels: &[(&str, &str)]) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(TAG.to_owned()),
        cmd: Some(vec!["sleep".to_owned(), "600".to_owned()]),
        stop_timeout: Some(1),
        labels: Some(label_map(labels)),
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
    container_id(docker, name).await
}

async fn container_id(docker: &Docker, name: &str) -> String {
    docker
        .inspect_container(name, None)
        .await
        .unwrap_or_else(|e| panic!("inspect {name}: {e}"))
        .id
        .unwrap_or_else(|| panic!("{name} has no id"))
}

/// Wait until `name` has stopped, and return its exit code.
async fn wait_until_exited(docker: &Docker, name: &str) -> i64 {
    for _ in 0..300 {
        let state = docker
            .inspect_container(name, None)
            .await
            .unwrap_or_else(|e| panic!("inspect {name}: {e}"))
            .state;
        if let Some(state) = state
            && state.running == Some(false)
        {
            return state.exit_code.unwrap_or(-1);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{name} never exited");
}

/// A scratch directory used as the daemon's working directory, so no stray
/// `freshdock.toml` is picked up.
fn workdir(prefix: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(prefix);
    std::fs::create_dir_all(&dir).expect("create the test's scratch directory");
    // The migration container writes here as its own user.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
        .expect("make the scratch directory writable");
    dir
}

fn read_all(mut r: impl Read) -> String {
    let mut s = String::new();
    let _ = r.read_to_string(&mut s);
    s
}

/// Run the real binary as a daemon for `secs`, then kill it and return its log.
/// Debug is on: the "pinned (no check)" verdict a regression produces is debug.
async fn run_daemon(dir: &Path, secs: u64) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_freshdock"))
        .args(["run", "--interval", "5", "--tick", "1"])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info,freshdock::scheduler=debug")
        // The host's settings must not decide what this daemon sweeps.
        .env_remove("FRESHDOCK_CONFIG")
        .env_remove("FRESHDOCK_WATCH_ALL")
        .env_remove("FRESHDOCK_INTERVAL")
        .env_remove("FRESHDOCK_TICK")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn freshdock run");

    // Drain both pipes on their own threads; a full pipe would block the child.
    let out = child.stdout.take().expect("stdout");
    let err = child.stderr.take().expect("stderr");
    let out_thread = std::thread::spawn(move || read_all(out));
    let err_thread = std::thread::spawn(move || read_all(err));

    tokio::time::sleep(Duration::from_secs(secs)).await;
    let _ = child.kill();
    let _ = child.wait();

    let mut log = out_thread.join().unwrap_or_default();
    log.push_str(&err_thread.join().unwrap_or_default());
    log
}

/// The position of the first log line carrying both `marker` and `name`.
/// [`run_daemon`] puts stdout first, so these positions are the daemon's order.
fn line_index(log: &str, marker: &str, name: &str) -> usize {
    log.lines()
        .position(|l| l.contains(marker) && l.contains(name))
        .unwrap_or_else(|| panic!("no {marker:?} line for {name}: {log}"))
}

/// Fail if any of `names` was ever reported as digest-pinned.
fn assert_never_pinned(log: &str, names: &[&str]) {
    for line in log.lines().filter(|l| l.contains("pinned")) {
        for name in names {
            assert!(
                !line.contains(name),
                "{name} was reported as pinned: {line}"
            );
        }
    }
}

/// Two containers on one stale tag: the second must still be updated.
#[tokio::test]
#[ignore = "needs-docker"]
// The serialisation guard is held across awaits; that is the point of it.
#[allow(clippy::await_holding_lock)]
async fn shared_tag_siblings_are_both_updated() {
    let _serial = serialise();
    let Some(docker) = connect_or_skip().await else {
        return;
    };

    let prefix = format!("fd-reg-{}", now_nanos());
    let _cleanup = Cleanup {
        prefix: prefix.clone(),
    };
    if stale_tag(&docker).await.is_none() {
        return;
    }
    let dir = workdir(&prefix);

    let labels = [("freshdock.enable", "true"), ("freshdock.mode", "live")];
    let first = format!("{prefix}-a");
    let second = format!("{prefix}-b");
    let first_before = spawn(&docker, &first, sleeper(&labels)).await;
    let second_before = spawn(&docker, &second, sleeper(&labels)).await;

    let log = run_daemon(&dir, DAEMON_SECS).await;

    let first_after = container_id(&docker, &first).await;
    let second_after = container_id(&docker, &second).await;

    assert_ne!(
        first_before, first_after,
        "the first container on the stale tag must be updated: {log}"
    );
    assert_ne!(
        second_before, second_after,
        "the second container sharing the tag must be updated too: {log}"
    );
    assert_never_pinned(&log, &[&first, &second]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A compose project whose tag has moved on, so every member's listing image is
/// a bare id. The completed one-shot must re-run before the service awaiting it.
#[tokio::test]
#[ignore = "needs-docker"]
#[allow(clippy::await_holding_lock)]
async fn a_compose_one_shot_is_re_run_after_the_tag_moved() {
    let _serial = serialise();
    let Some(docker) = connect_or_skip().await else {
        return;
    };

    let prefix = format!("fd-reg-{}", now_nanos());
    let _cleanup = Cleanup {
        prefix: prefix.clone(),
    };
    if stale_tag(&docker).await.is_none() {
        return;
    }
    let dir = workdir(&prefix);
    let log_file = dir.join("migrations.log");

    // A completed one-shot: unlabelled, exited, and waited on by `web`.
    let migrate = format!("{prefix}-migrate-1");
    let mut body = sleeper(&[
        ("com.docker.compose.project", prefix.as_str()),
        ("com.docker.compose.service", "migrate"),
    ]);
    body.cmd = Some(vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo migrated >> /out/migrations.log".to_owned(),
    ]);
    body.host_config = Some(HostConfig {
        binds: Some(vec![format!("{}:/out", dir.display())]),
        ..Default::default()
    });
    spawn(&docker, &migrate, body).await;
    assert_eq!(
        wait_until_exited(&docker, &migrate).await,
        0,
        "the one-shot must complete before the rollout starts"
    );

    let web = format!("{prefix}-web-1");
    let web_before = spawn(
        &docker,
        &web,
        sleeper(&[
            ("freshdock.enable", "true"),
            ("freshdock.mode", "live"),
            ("com.docker.compose.project", prefix.as_str()),
            ("com.docker.compose.service", "web"),
            (
                "com.docker.compose.depends_on",
                "migrate:service_completed_successfully:false",
            ),
        ]),
    )
    .await;
    // Move the tag forward so both members are behind it, with a bare-id listing.
    pull(&docker, TAG)
        .await
        .expect("move busybox:latest forward");

    let lines_before = migration_lines(&log_file);
    assert_eq!(lines_before, 1, "the one-shot ran once during setup");

    let log = run_daemon(&dir, DAEMON_SECS).await;

    let web_after = container_id(&docker, &web).await;
    let lines_after = migration_lines(&log_file);

    assert_ne!(web_before, web_after, "web must be updated: {log}");
    assert_eq!(
        lines_after,
        lines_before + 1,
        "the one-shot must be re-run exactly once: {log}"
    );
    // The count proves both ran, not that new code waited for the migration.
    assert!(
        line_index(&log, "rollout re-ran one-shot", &migrate)
            < line_index(&log, "rollout updated service", &web),
        "the one-shot must be logged before the service that waits on it: {log}"
    );
    assert_never_pinned(&log, &[&web, &migrate]);

    let _ = std::fs::remove_dir_all(&dir);
}

fn migration_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .count()
}
