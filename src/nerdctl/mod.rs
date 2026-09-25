//! The nerdctl backend: containerd containers, driven through the `nerdctl`
//! binary.
//!
//! nerdctl, not containerd, owns a container's networking (CNI), name and
//! port setup, so freshdock asks nerdctl to do every operation rather than
//! talking to containerd directly. Only rootful nerdctl is supported: freshdock
//! must run as root, next to the containerd nerdctl uses.

pub mod inspect;
pub mod runner;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::config::{CredentialStore, RuntimeSettings};
use crate::docker::DockerError;
use crate::docker::check::{ContainerImage, DockerCheck, LocalImage};
use crate::docker::container::ContainerInfo;
use crate::docker::recreate::{DockerOps, HookStatus};
use crate::docker::spec::{ContainerSpec, SpecError};
use crate::health::{ContainerRuntimeState, HealthProbe};
use crate::registry::ImageRef;

use self::inspect::{
    ContainerInspect, ImageInspect, Version, classify_error, error_message, parse_array,
};
use self::runner::{CommandRunner, ProcessRunner};

/// The oldest nerdctl freshdock is tested against.
const MIN_NERDCTL: (u32, u32) = (2, 3);

/// How long a read-only nerdctl call may take before it counts as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Talks to containerd through the nerdctl binary, in one containerd
/// namespace.
pub struct Nerdctl<R = ProcessRunner> {
    runner: R,
    namespace: String,
    // Used by pulls once updates land (Phase 3).
    #[allow(dead_code)]
    credentials: Arc<CredentialStore>,
}

impl Nerdctl<ProcessRunner> {
    /// Check that nerdctl runs, is new enough and can reach containerd, so a
    /// broken setup fails at startup rather than midway through an update.
    pub async fn connect(
        settings: &RuntimeSettings,
        credentials: Arc<CredentialStore>,
    ) -> Result<Self, DockerError> {
        let nerdctl = Self::with_runner(
            ProcessRunner::new(&settings.nerdctl_path),
            settings.containerd_namespace.clone(),
            credentials,
        );
        warn_unless_root();
        nerdctl.check_version().await?;
        Ok(nerdctl)
    }
}

impl<R: CommandRunner> Nerdctl<R> {
    pub fn with_runner(runner: R, namespace: String, credentials: Arc<CredentialStore>) -> Self {
        Self {
            runner,
            namespace,
            credentials,
        }
    }

    /// Run `nerdctl --namespace <ns> <args>` and return its stdout, or the
    /// classified error when it fails.
    async fn call(&self, args: &[&str]) -> Result<String, DockerError> {
        let mut full = vec!["--namespace".to_owned(), self.namespace.clone()];
        full.extend(args.iter().map(|a| (*a).to_owned()));
        debug!(args = ?args, namespace = %self.namespace, "nerdctl");
        let out = self
            .runner
            .run(&full, &[], CALL_TIMEOUT)
            .await
            .map_err(|e| DockerError::Runtime(e.to_string()))?;
        if out.success() {
            Ok(out.stdout)
        } else {
            let message = error_message(&out.stderr);
            Err(classify_error(if message.is_empty() {
                format!("nerdctl {} exited with {:?}", args.join(" "), out.code)
            } else {
                message
            }))
        }
    }

    async fn check_version(&self) -> Result<(), DockerError> {
        let stdout = self.call(&["version", "--format", "{{json .}}"]).await?;
        let version: Version = serde_json::from_str(&stdout).map_err(|e| {
            DockerError::Runtime(format!("could not parse nerdctl version output: {e}"))
        })?;
        let client = version.client_version().unwrap_or("unknown");
        match parse_version(client) {
            Some(v) if v < MIN_NERDCTL => {
                return Err(DockerError::Runtime(format!(
                    "nerdctl {client} is too old; freshdock needs nerdctl {}.{} or newer",
                    MIN_NERDCTL.0, MIN_NERDCTL.1
                )));
            }
            Some(_) => {}
            None => warn!(
                version = %client,
                "could not read the nerdctl version; assuming it is new enough"
            ),
        }
        let Some(containerd) = version.containerd_version() else {
            return Err(DockerError::Runtime(
                "nerdctl could not reach containerd. freshdock supports rootful nerdctl only: run \
                 it as root, on the host where containerd runs"
                    .to_owned(),
            ));
        };
        info!(
            nerdctl = %client,
            %containerd,
            namespace = %self.namespace,
            "connected to containerd through nerdctl"
        );
        Ok(())
    }

    async fn inspect_container(&self, name_or_id: &str) -> Result<ContainerInspect, DockerError> {
        let stdout = self.call(&["inspect", name_or_id]).await?;
        parse_array::<ContainerInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    /// Inspect several containers in one call. If any has gone away in the
    /// meantime nerdctl fails the whole call, so fall back to one call per
    /// container and skip the ones that are gone.
    async fn inspect_containers(&self, ids: &[&str]) -> Result<Vec<ContainerInspect>, DockerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["inspect"];
        args.extend_from_slice(ids);
        match self.call(&args).await {
            Ok(stdout) => parse_array("inspect", &stdout),
            Err(e) if e.is_not_found() => {
                let mut found = Vec::with_capacity(ids.len());
                for id in ids {
                    match self.inspect_container(id).await {
                        Ok(c) => found.push(c),
                        Err(e) if e.is_not_found() => {
                            debug!(container = %id, "container disappeared while listing")
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(found)
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl<R: CommandRunner> DockerCheck for Nerdctl<R> {
    async fn list_running(&self) -> Result<Vec<ContainerInfo>, DockerError> {
        let stdout = self.call(&["ps", "--no-trunc", "-q"]).await?;
        let ids: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        Ok(self
            .inspect_containers(&ids)
            .await?
            .iter()
            .map(ContainerInspect::to_info)
            .collect())
    }

    async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError> {
        let stdout = self.call(&["image", "inspect", image]).await?;
        parse_array::<ImageInspect>("image inspect", &stdout)?
            .into_iter()
            .next()
            .map(LocalImage::from)
            .ok_or_else(|| DockerError::NotFound(format!("no such image: {image}")))
    }

    /// `image_id` is `None`: containerd records a container's image by
    /// reference only (see [`ContainerInspect::to_info`]).
    async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError> {
        let container = self.inspect_container(id).await?;
        let reference = container
            .image_ref()
            .ok_or(DockerError::Spec(SpecError::Missing("Config.Image")))?
            .to_owned();
        Ok(ContainerImage {
            reference,
            image_id: None,
        })
    }
}

#[async_trait]
impl<R: CommandRunner> HealthProbe for Nerdctl<R> {
    async fn probe_state(&self, name_or_id: &str) -> Result<ContainerRuntimeState, DockerError> {
        Ok(self.inspect_container(name_or_id).await?.runtime_state())
    }
}

/// The update half of the backend. Until it lands (Phase 3), every operation
/// refuses before anything is touched: the recreate cycle's first step is
/// `inspect`, so an update fails without stopping the container.
#[async_trait]
impl<R: CommandRunner> DockerOps for Nerdctl<R> {
    async fn inspect(&self, _name: &str) -> Result<ContainerSpec, DockerError> {
        Err(not_yet())
    }
    async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn stop(
        &self,
        _name: &str,
        _signal: Option<&str>,
        _timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn rename(&self, _name: &str, _ts_unix: i64) -> Result<String, DockerError> {
        Err(not_yet())
    }
    async fn create_from_spec(
        &self,
        _name: &str,
        _spec: &ContainerSpec,
        _image: &str,
    ) -> Result<String, DockerError> {
        Err(not_yet())
    }
    async fn start(&self, _name_or_id: &str) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn remove(&self, _name_or_id: &str, _force: bool) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn rename_to(&self, _from: &str, _to: &str) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn remove_image(&self, _id: &str, _force: bool) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn prune_dangling_images(&self) -> Result<(), DockerError> {
        Err(not_yet())
    }
    async fn exec_hook(
        &self,
        _name_or_id: &str,
        _command: &str,
        _timeout: Option<Duration>,
    ) -> Result<HookStatus, DockerError> {
        Err(not_yet())
    }
    async fn list_network_dependents(&self, _name: &str) -> Result<Vec<String>, DockerError> {
        Err(not_yet())
    }
}

fn not_yet() -> DockerError {
    DockerError::Unsupported(
        "updating containers with the nerdctl runtime is not implemented yet; use watch mode"
            .to_owned(),
    )
}

/// `v2.3.4` → `(2, 3)`.
fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Rootful nerdctl needs root; say so up front rather than leaving the user to
/// decode a socket permission error.
fn warn_unless_root() {
    #[cfg(target_os = "linux")]
    {
        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
            });
        if let Some(uid) = uid
            && uid != "0"
        {
            warn!(
                %uid,
                "freshdock is not running as root; rootful nerdctl needs root, and rootless \
                 nerdctl is not supported yet"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::runner::{CmdOutput, RunError};
    use super::*;
    use crate::docker::container::ListedState;

    const NS: &str = "freshdock-test";
    const PLAIN_ID: &str = "b196d3ba3a8a5e7d4b54a070f45d7522286de1aad1c1a875a478627a2aad79e8";
    const NETDEP_ID: &str = "ccaf33aa4167e0c0dc05b622bc3237a6ddddae1a48ade08964e4fe3b17ce4706";
    const NOT_FOUND: &str = "time=\"t\" level=fatal msg=\"1 errors: [no such object gone]\"\n";
    const VERSION_ARGS: &str = "version --format {{json .}}";

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn ok(stdout: impl Into<String>) -> CmdOutput {
        CmdOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn fail(stderr: &str) -> CmdOutput {
        CmdOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    /// Two one-element inspect arrays joined into one two-element array.
    fn joined(a: &str, b: &str) -> String {
        let mut all: Vec<serde_json::Value> = serde_json::from_str(a).unwrap();
        all.extend(serde_json::from_str::<Vec<serde_json::Value>>(b).unwrap());
        serde_json::to_string(&all).unwrap()
    }

    /// Replays scripted output for exact argument lists (after the namespace
    /// prefix) and records every call. An unscripted call panics.
    #[derive(Default)]
    struct FakeRunner {
        script: Vec<(String, CmdOutput)>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn on(mut self, args: &str, out: CmdOutput) -> Self {
            self.script.push((args.to_owned(), out));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            args: &[String],
            _env: &[(String, String)],
            _timeout: Duration,
        ) -> Result<CmdOutput, RunError> {
            assert_eq!(args[..2], ["--namespace".to_owned(), NS.to_owned()]);
            let call = args[2..].join(" ");
            self.calls.lock().unwrap().push(call.clone());
            let Some((_, out)) = self.script.iter().find(|(want, _)| *want == call) else {
                panic!("unexpected nerdctl call: {call}");
            };
            Ok(out.clone())
        }
    }

    fn nerdctl(runner: FakeRunner) -> Nerdctl<FakeRunner> {
        Nerdctl::with_runner(runner, NS.to_owned(), Arc::new(CredentialStore::default()))
    }

    #[tokio::test]
    async fn version_check_accepts_a_supported_nerdctl() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v2.3.4"},"Server":{"Components":[{"Name":"containerd","Version":"v2.3.2"}]}}"#),
        ));
        n.check_version().await.unwrap();
    }

    #[tokio::test]
    async fn version_check_rejects_an_old_nerdctl() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v1.7.6"},"Server":{"Components":[{"Name":"containerd","Version":"v1.7.0"}]}}"#),
        ));
        let err = n.check_version().await.unwrap_err().to_string();
        assert!(err.contains("too old"), "{err}");
    }

    #[tokio::test]
    async fn version_check_explains_an_unreachable_containerd() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v2.3.4"},"Server":null}"#),
        ));
        let err = n.check_version().await.unwrap_err().to_string();
        assert!(err.contains("could not reach containerd"), "{err}");
        assert!(err.contains("root"), "{err}");
    }

    #[tokio::test]
    async fn list_running_inspects_every_listed_container_in_one_call() {
        let runner = FakeRunner::default()
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n{NETDEP_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID} {NETDEP_ID}"),
                ok(joined(
                    &fixture("nerdctl_plain_inspect.json"),
                    &fixture("nerdctl_netdep_inspect.json"),
                )),
            );
        let n = nerdctl(runner);
        let listed = n.list_running().await.unwrap();
        let names: Vec<_> = listed.iter().map(crate::docker::container_name).collect();
        assert_eq!(names, ["fd-plain", "fd-netdep"]);
        assert!(listed.iter().all(|c| c.state == ListedState::Running));
        assert_eq!(n.runner.calls().len(), 2);
    }

    #[tokio::test]
    async fn list_running_skips_a_container_that_disappeared() {
        let runner = FakeRunner::default()
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\ngone\n")))
            .on(&format!("inspect {PLAIN_ID} gone"), fail(NOT_FOUND))
            .on(
                &format!("inspect {PLAIN_ID}"),
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on("inspect gone", fail(NOT_FOUND));
        let listed = nerdctl(runner).list_running().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_deref(), Some(PLAIN_ID));
    }

    #[tokio::test]
    async fn list_running_with_no_containers_makes_no_inspect_call() {
        let n = nerdctl(FakeRunner::default().on("ps --no-trunc -q", ok("")));
        assert!(n.list_running().await.unwrap().is_empty());
        assert_eq!(n.runner.calls(), ["ps --no-trunc -q"]);
    }

    #[tokio::test]
    async fn a_listing_failure_other_than_not_found_propagates() {
        let runner = FakeRunner::default()
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID}"),
                fail("time=\"t\" level=fatal msg=\"failed to dial containerd\"\n"),
            );
        let err = nerdctl(runner).list_running().await.unwrap_err();
        assert!(matches!(err, DockerError::Runtime(_)), "{err}");
    }

    #[tokio::test]
    async fn inspect_image_reads_repo_digests() {
        let n = nerdctl(FakeRunner::default().on(
            "image inspect nginx:1.27-alpine",
            ok(fixture("nerdctl_image_inspect.json")),
        ));
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert_eq!(img.repo_digests.len(), 1);
    }

    #[tokio::test]
    async fn inspect_image_of_a_missing_image_is_not_found() {
        let n = nerdctl(FakeRunner::default().on("image inspect nope:1", fail(NOT_FOUND)));
        assert!(n.inspect_image("nope:1").await.unwrap_err().is_not_found());
    }

    #[tokio::test]
    async fn container_image_reports_the_reference_and_no_id() {
        let n = nerdctl(FakeRunner::default().on(
            "inspect fd-plain",
            ok(fixture("nerdctl_plain_inspect.json")),
        ));
        assert_eq!(
            n.container_image("fd-plain").await.unwrap(),
            ContainerImage {
                reference: "docker.io/library/nginx:1.27-alpine".to_owned(),
                image_id: None,
            }
        );
    }

    #[tokio::test]
    async fn probe_state_of_a_running_container_without_healthcheck() {
        let n = nerdctl(FakeRunner::default().on(
            "inspect fd-plain",
            ok(fixture("nerdctl_plain_inspect.json")),
        ));
        assert_eq!(
            n.probe_state("fd-plain").await.unwrap(),
            ContainerRuntimeState::RunningNoHealthcheck
        );
    }

    #[tokio::test]
    async fn updates_refuse_before_touching_anything() {
        let n = nerdctl(FakeRunner::default());
        let err = n.inspect("fd-plain").await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(n.runner.calls().is_empty());
    }

    #[test]
    fn versions_parse_major_minor() {
        assert_eq!(parse_version("v2.3.4"), Some((2, 3)));
        assert_eq!(parse_version("2.10.0-rc.1"), Some((2, 10)));
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
    }
}
