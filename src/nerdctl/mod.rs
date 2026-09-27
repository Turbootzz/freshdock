//! The nerdctl backend: containerd containers, driven through the `nerdctl`
//! binary.
//!
//! nerdctl, not containerd, owns a container's networking (CNI), name and
//! port setup, so freshdock asks nerdctl to do every operation rather than
//! talking to containerd directly. Only rootful nerdctl is supported: freshdock
//! must run as root, next to the containerd nerdctl uses.

pub mod auth;
pub mod compare;
pub mod inspect;
pub mod private;
pub mod runner;
pub mod spec;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::config::{CredentialStore, RuntimeSettings};
use crate::docker::DockerError;
use crate::docker::check::{ContainerImage, DockerCheck, LocalImage};
use crate::docker::container::ContainerInfo;
use crate::docker::recreate::{DockerOps, HookStatus};
use crate::docker::spec::{ContainerSpec, SpecBody, SpecError};
use crate::docker::{network_dependent_names, rename::next_available_old_name_async};
use crate::health::{ContainerRuntimeState, HealthProbe};
use crate::registry::ImageRef;
use crate::registry::digest::split_repository;

use self::auth::PullAuth;
use self::inspect::{
    ContainerInspect, ImageInspect, Version, classify_error, error_message, parse_array,
};
use self::private::PrivateDir;
use self::runner::{CmdOutput, CommandRunner, ProcessRunner, RunError};
use self::spec::{CreateInspect, NativeInspect, NerdctlSpec, NetworkConfig};

/// The oldest nerdctl freshdock is tested against.
const MIN_NERDCTL: (u32, u32) = (2, 3);

/// How long a nerdctl call may take before it counts as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A pull downloads the whole image, so it gets far longer.
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A hook with no timeout of its own still must not hang the daemon forever.
const HOOK_FALLBACK_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Talks to containerd through the nerdctl binary, in one containerd
/// namespace.
pub struct Nerdctl<R = ProcessRunner> {
    runner: R,
    namespace: String,
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
        self.call_with(args, &[], CALL_TIMEOUT).await
    }

    /// [`call`](Self::call) with extra environment and its own time limit.
    async fn call_with(
        &self,
        args: &[&str],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<String, DockerError> {
        let out = self
            .run_raw(args, env, timeout)
            .await
            .map_err(|e| DockerError::Runtime(e.to_string()))?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(failure(args, &out))
        }
    }

    async fn run_raw(
        &self,
        args: &[&str],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CmdOutput, RunError> {
        let mut full = vec!["--namespace".to_owned(), self.namespace.clone()];
        full.extend(args.iter().map(|a| (*a).to_owned()));
        debug!(args = ?args, namespace = %self.namespace, "nerdctl");
        self.runner.run(&full, env, timeout).await
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

impl<R: CommandRunner> Nerdctl<R> {
    /// The published ports a stopped container still owns, from nerdctl's
    /// state directory. `None` when it can't be read; inspect's own view (only
    /// populated while running) is used then.
    async fn network_config(&self, compat: &CreateInspect) -> Option<NetworkConfig> {
        let state_dir = compat
            .config
            .as_ref()?
            .labels
            .as_ref()?
            .get("nerdctl/state-dir")?;
        let path = std::path::Path::new(state_dir).join("network-config.json");
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str(&body)
                .inspect_err(|e| warn!(path = %path.display(), error = %e, "unreadable nerdctl network config"))
                .ok(),
            Err(e) => {
                debug!(path = %path.display(), error = %e, "no nerdctl network config");
                None
            }
        }
    }

    /// The image the tag points at now, as a digest-pinned reference
    /// (`docker.io/library/nginx@sha256:…`). That is the handle nerdctl takes
    /// for a specific image: `nerdctl create` and `rmi` both refuse the
    /// config-digest id `image inspect` reports. containerd records a
    /// container's image by reference only, so this is resolved from the tag,
    /// before the pull moves it: the superseded image for cleanup, and the
    /// exact image a network-namespace dependent is repaired onto.
    async fn pinned_image(&self, reference: &str) -> Option<String> {
        if reference.contains('@') {
            return Some(reference.to_owned());
        }
        let img = match self.inspect_image(reference).await {
            Ok(img) => img,
            Err(e) => {
                debug!(image = %reference, error = %e, "could not resolve the image digest");
                return None;
            }
        };
        let digest = crate::probe::local_manifest_digests(reference, &img.repo_digests)
            .into_iter()
            .next()?;
        Some(format!("{}@{digest}", crate::probe::strip_tag(reference)))
    }

    /// `nerdctl create` from a spec, returning the new container's id. The
    /// environment goes through an owner-only env file that exists only for
    /// this call, never through the (world-readable) arguments.
    async fn create(
        &self,
        spec: &NerdctlSpec,
        name: &str,
        image: &str,
    ) -> Result<String, DockerError> {
        let private = match spec.env_file() {
            Some(body) => {
                let dir = PrivateDir::new("env").map_err(|e| {
                    DockerError::Runtime(format!("could not write the environment file: {e}"))
                })?;
                let path = dir.write("env", body.as_bytes()).map_err(|e| {
                    DockerError::Runtime(format!("could not write the environment file: {e}"))
                })?;
                Some((dir, path))
            }
            None => None,
        };
        let env_file = private.as_ref().map(|(_, path)| path.display().to_string());
        let args = spec.create_args(name, image, env_file.as_deref());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let stdout = self.call(&args).await?;
        drop(private);
        stdout
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                DockerError::Runtime("nerdctl create printed no container id".to_owned())
            })
    }

    /// Where the trial container's runtime spec and labels differ from the
    /// original's (see [`compare`]).
    async fn compare_with_trial(
        &self,
        original: &str,
        trial: &str,
    ) -> Result<Vec<String>, DockerError> {
        let a = self.inspect_native_raw(original).await?;
        let b = self.inspect_native_raw(trial).await?;
        let id = |v: &serde_json::Value| {
            v.get("ID")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_owned()
        };
        Ok(compare::differences(&a, &id(&a), &b, &id(&b)))
    }

    async fn inspect_native_raw(&self, name_or_id: &str) -> Result<serde_json::Value, DockerError> {
        let stdout = self.call(&["inspect", "--mode=native", name_or_id]).await?;
        parse_array::<serde_json::Value>("inspect --mode=native", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    async fn inspect_native(&self, name_or_id: &str) -> Result<NativeInspect, DockerError> {
        let stdout = self.call(&["inspect", "--mode=native", name_or_id]).await?;
        parse_array::<NativeInspect>("inspect --mode=native", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    /// Work around `nerdctl rename`: it renames the container but not the
    /// name its post-stop hook releases, which stays the one it was created
    /// with. Removing a renamed archive runs that hook, and it fails once the
    /// replacement holds the original name, failing the `rm` with it.
    ///
    /// When `target` is such an archive, rename the current holder of its
    /// original name to a temporary name, so the hook releases a name nobody
    /// holds. Returns `(name, temporary)` for the caller to rename back.
    async fn step_aside_name_holder(
        &self,
        target: &str,
    ) -> Result<Option<(String, String)>, DockerError> {
        let native = match self.inspect_native(target).await {
            Ok(native) => native,
            // Let `rm` report a missing container in its own words.
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        let (Some(created), Some(current)) = (native.created_name(), native.current_name()) else {
            return Ok(None);
        };
        if created == current {
            return Ok(None);
        }
        let holder = match self.inspect_container(created).await {
            Ok(holder) => holder,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        if holder.id.is_none() || holder.id == native.id {
            return Ok(None);
        }
        let temporary = format!("{created}-freshdock-aside-{}", unique_suffix());
        debug!(archive = %target, holder = %created, %temporary, "stepping the name holder aside to remove a renamed archive");
        self.call(&["rename", created, &temporary]).await?;
        Ok(Some((created.to_owned(), temporary)))
    }
}

/// A suffix for throwaway container names: unique enough on one host.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The update half of the backend. Every step is one nerdctl command, so
/// nerdctl keeps doing the networking, naming and port setup it does for
/// `nerdctl run`.
#[async_trait]
impl<R: CommandRunner> DockerOps for Nerdctl<R> {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        let stdout = self.call(&["inspect", name]).await?;
        let listed = parse_array::<ContainerInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name}")))?;
        let compat = parse_array::<CreateInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .unwrap_or_default();
        let native = self.inspect_native(name).await?;

        let container_name = listed
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/').to_owned())
            .filter(|n| !n.is_empty())
            .ok_or(DockerError::Spec(SpecError::Missing("Name")))?;
        let image_ref = listed
            .image_ref()
            .ok_or(DockerError::Spec(SpecError::Missing("Config.Image")))?
            .to_owned();
        let image_id = self.pinned_image(&image_ref).await;
        let network_config = self.network_config(&compat).await;
        Ok(ContainerSpec {
            name: container_name,
            image_ref,
            image_id,
            body: SpecBody::Nerdctl(NerdctlSpec::from_inspect(compat, native, network_config)),
        })
    }

    /// Refuse, while the container is still untouched, an update that would
    /// have to drop a setting nerdctl can't be told to reproduce, or whose
    /// rebuilt flags nerdctl rejects.
    ///
    /// The flags are tried out with a real `nerdctl create` under a throwaway
    /// name, removed at once. A create failure after the stop would leave the
    /// container down; this catches it while nothing has moved. The trial
    /// container is never started, so it runs nothing. It is created without
    /// the published ports, which `nerdctl create` already reserves and which
    /// the running original still holds.
    async fn preflight_recreate(&self, spec: &ContainerSpec) -> Result<(), DockerError> {
        let nerdctl = spec.nerdctl();
        refuse_unsupported(&spec.name, nerdctl)?;
        let trial = format!("{}-freshdock-check-{}", spec.name, unique_suffix());
        let without_ports = NerdctlSpec {
            ports: Vec::new(),
            ..nerdctl.clone()
        };
        if let Err(e) = self.create(&without_ports, &trial, &spec.image_ref).await {
            return Err(DockerError::Unsupported(format!(
                "nerdctl rejected {}'s rebuilt settings, so it was not updated: {e}",
                spec.name
            )));
        }
        let compared = self.compare_with_trial(&spec.name, &trial).await;
        if let Err(e) = self.call(&["rm", "--force", &trial]).await {
            warn!(container = %trial, error = %e, "could not remove the trial container; remove it by hand");
        }
        let differences = compared?;
        if differences.is_empty() {
            return Ok(());
        }
        Err(DockerError::Unsupported(format!(
            "{} has settings nerdctl does not report, so freshdock can't carry them over and \
             it was not updated. The rebuilt container would differ at: {}",
            spec.name,
            differences.join("; ")
        )))
    }

    async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
        let reference = format!("{}:{}", image_ref.repository, image_ref.tag);
        let (host, _) = split_repository(&image_ref.repository);
        let auth = match self.credentials.get(host) {
            Some(creds) => Some(PullAuth::write(host, creds).map_err(|e| {
                DockerError::Runtime(format!("could not write pull credentials: {e}"))
            })?),
            None => None,
        };
        let env: Vec<(String, String)> = auth
            .iter()
            .map(|a| ("DOCKER_CONFIG".to_owned(), a.dir().display().to_string()))
            .collect();
        self.call_with(&["pull", "--quiet", &reference], &env, PULL_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        let mut args = vec!["stop"];
        if let Some(signal) = signal {
            args.extend(["--signal", signal]);
        }
        let time = timeout_s.map(|t| t.to_string());
        if let Some(time) = &time {
            args.extend(["--time", time]);
        }
        args.push(name);
        // The stop itself may take the whole grace period.
        let grace = Duration::from_secs(timeout_s.unwrap_or(10).max(0) as u64);
        self.call_with(&args, &[], grace + CALL_TIMEOUT).await?;
        Ok(())
    }

    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
        let archive = next_available_old_name_async(name, ts_unix, |candidate| {
            let candidate = candidate.to_owned();
            async move { self.inspect_container(&candidate).await.is_ok() }
        })
        .await;
        self.call(&["rename", name, &archive]).await?;
        Ok(archive)
    }

    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError> {
        let nerdctl = spec.nerdctl();
        // Preflight already refused these for the updated container; a
        // network-namespace dependent's repair has no preflight of its own.
        refuse_unsupported(name, nerdctl)?;
        self.create(nerdctl, name, image).await
    }

    async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
        self.call(&["start", name_or_id]).await?;
        Ok(())
    }

    async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError> {
        let aside = self.step_aside_name_holder(name_or_id).await?;
        let mut args = vec!["rm"];
        if force {
            args.push("--force");
        }
        args.push(name_or_id);
        let removed = self.call(&args).await.map(|_| ());
        if let Some((name, temporary)) = aside
            && let Err(e) = self.call(&["rename", &temporary, &name]).await
        {
            warn!(
                container = %name,
                %temporary,
                error = %e,
                "could not give the container its name back after removing an archive; \
                 rename it by hand"
            );
            removed?;
            return Err(e);
        }
        removed
    }

    async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
        self.call(&["rename", from, to]).await?;
        Ok(())
    }

    /// A superseded image usually has no name left in containerd once the pull
    /// moved its tag, and containerd collects its content with the last
    /// container using it; "no such image" means there is nothing to remove.
    async fn remove_image(&self, id: &str, force: bool) -> Result<(), DockerError> {
        let mut args = vec!["rmi"];
        if force {
            args.push("--force");
        }
        args.push(id);
        match self.call(&args).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => {
                debug!(image = %id, "superseded image already gone");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn prune_dangling_images(&self) -> Result<(), DockerError> {
        self.call(&["image", "prune", "--force"]).await?;
        Ok(())
    }

    /// `sh -c <command>` in the container, like the Docker backend. nerdctl
    /// exits 1 whatever the command's own code, and reports that code as
    /// `exec failed with exit code N`, so it is read back from the message.
    async fn exec_hook(
        &self,
        name_or_id: &str,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<HookStatus, DockerError> {
        let args = ["exec", name_or_id, "sh", "-c", command];
        let out = match self
            .run_raw(&args, &[], timeout.unwrap_or(HOOK_FALLBACK_TIMEOUT))
            .await
        {
            Ok(out) => out,
            Err(RunError::TimedOut { .. }) => return Ok(HookStatus::TimedOut),
            Err(e) => return Err(DockerError::Runtime(e.to_string())),
        };
        if !out.stdout.is_empty() {
            debug!(container = %name_or_id, output = %out.stdout, "hook output");
        }
        if out.success() {
            return Ok(HookStatus::Completed { exit_code: 0 });
        }
        match hook_exit_code(&error_message(&out.stderr)) {
            Some(exit_code) => Ok(HookStatus::Completed { exit_code }),
            None => Err(failure(&args, &out)),
        }
    }

    async fn list_network_dependents(&self, name: &str) -> Result<Vec<String>, DockerError> {
        let owner = self.inspect_container(name).await?;
        let full_id = owner.id.clone().unwrap_or_default();
        let owner_name = owner
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/'))
            .filter(|n| !n.is_empty())
            .unwrap_or(name);
        Ok(network_dependent_names(
            &self.list_running().await?,
            owner_name,
            &full_id,
            crate::selfid::own_container_id_prefix().as_deref(),
        ))
    }
}

fn refuse_unsupported(name: &str, spec: &NerdctlSpec) -> Result<(), DockerError> {
    if spec.unsupported.is_empty() {
        return Ok(());
    }
    Err(DockerError::Unsupported(format!(
        "{name} has settings freshdock can't reproduce with nerdctl, so it was not updated: {}",
        spec.unsupported.join("; ")
    )))
}

/// The error for a failed nerdctl call, from its output.
fn failure(args: &[&str], out: &CmdOutput) -> DockerError {
    let message = error_message(&out.stderr);
    classify_error(if message.is_empty() {
        format!("nerdctl {} exited with {:?}", args.join(" "), out.code)
    } else {
        message
    })
}

/// The command's own exit code from `exec failed with exit code N`.
fn hook_exit_code(message: &str) -> Option<i64> {
    message
        .rsplit_once("exit code ")?
        .1
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()
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

    /// Does `call` match `pattern`, where each `*` stands for any text? Used
    /// for the throwaway container names, which carry a timestamp.
    fn glob_match(pattern: &str, call: &str) -> bool {
        let mut parts = pattern.split('*');
        let first = parts.next().unwrap_or("");
        let Some(mut rest) = call.strip_prefix(first) else {
            return false;
        };
        let mut parts: Vec<&str> = parts.collect();
        let Some(last) = parts.pop() else {
            return rest.is_empty();
        };
        for part in parts {
            match rest.find(part) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        rest.ends_with(last)
    }

    /// Replays scripted output for argument lists (after the namespace prefix,
    /// `*` matching any text) and records every call. An unscripted call
    /// panics.
    #[derive(Default)]
    struct FakeRunner {
        script: Vec<(String, CmdOutput)>,
        calls: Mutex<Vec<String>>,
        /// `config.json` as it was on disk during each call given a
        /// `DOCKER_CONFIG`.
        docker_configs: Mutex<Vec<String>>,
        /// `(path, contents)` of each `--env-file`, read during the call.
        env_files: Mutex<Vec<(String, String)>>,
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
            env: &[(String, String)],
            _timeout: Duration,
        ) -> Result<CmdOutput, RunError> {
            assert_eq!(args[..2], ["--namespace".to_owned(), NS.to_owned()]);
            let call = args[2..].join(" ");
            for (key, dir) in env {
                if key == "DOCKER_CONFIG" {
                    let config = std::fs::read_to_string(format!("{dir}/config.json")).unwrap();
                    self.docker_configs.lock().unwrap().push(config);
                }
            }
            if let Some(i) = args.iter().position(|a| a == "--env-file") {
                let path = args[i + 1].clone();
                let contents = std::fs::read_to_string(&path).unwrap();
                self.env_files.lock().unwrap().push((path, contents));
            }
            self.calls.lock().unwrap().push(call.clone());
            let Some((_, out)) = self.script.iter().find(|(want, _)| glob_match(want, &call))
            else {
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

    const IMAGE: &str = "docker.io/library/nginx:1.27-alpine";
    /// The digest-pinned reference nerdctl takes for this exact image.
    const IMAGE_ID: &str = "docker.io/library/nginx@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";

    fn inspectable(name: &str, fixture_name: &str) -> FakeRunner {
        FakeRunner::default()
            .on(
                &format!("inspect {name}"),
                ok(fixture(&format!("nerdctl_{fixture_name}_inspect.json"))),
            )
            .on(
                &format!("inspect --mode=native {name}"),
                ok(fixture(&format!("nerdctl_{fixture_name}_native.json"))),
            )
            .on(
                &format!("image inspect {IMAGE}"),
                ok(fixture("nerdctl_image_inspect.json")),
            )
    }

    #[test]
    fn glob_match_handles_wildcards() {
        assert!(glob_match("rm a", "rm a"));
        assert!(!glob_match("rm a", "rm ab"));
        assert!(glob_match(
            "rm --force web-check-*",
            "rm --force web-check-1-2"
        ));
        assert!(glob_match(
            "create --name web-check-* --user 1 img",
            "create --name web-check-9 --user 1 img"
        ));
        assert!(!glob_match(
            "create --name web-check-* img",
            "create --name other img"
        ));
    }

    #[tokio::test]
    async fn inspect_builds_a_reproducible_nerdctl_spec() {
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(fixture("nerdctl_full_native.json")),
                ),
        );
        let spec = n.inspect("fd-full").await.unwrap();
        assert_eq!(spec.name, "fd-full");
        assert_eq!(spec.image_ref, IMAGE);
        assert_eq!(spec.image_id.as_deref(), Some(IMAGE_ID));
        assert!(spec.nerdctl().unsupported.is_empty());
        assert_eq!(spec.stop_signal(), Some("SIGQUIT"));
        assert_eq!(spec.stop_timeout(), Some(25));
        n.preflight_recreate(&spec).await.unwrap();
    }

    #[tokio::test]
    async fn preflight_and_create_refuse_unsupported_settings() {
        let n = nerdctl(inspectable("fd-full", "full"));
        let mut spec = n.inspect("fd-full").await.unwrap();
        match &mut spec.body {
            SpecBody::Nerdctl(s) => s.unsupported.push("label nerdctl/ip".to_owned()),
            SpecBody::Docker(_) => unreachable!(),
        }
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("label nerdctl/ip"), "{err}");
        let err = n
            .create_from_spec("fd-full", &spec, IMAGE)
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("create")),
            "nothing is created"
        );
    }

    #[tokio::test]
    async fn preflight_refuses_when_nerdctl_rejects_the_trial_create() {
        let n = nerdctl(inspectable("fd-full", "full").on(
            "create --name fd-full-freshdock-check-* *",
            fail("time=\"t\" level=fatal msg=\"failed to parse volume options\"\n"),
        ));
        let spec = n.inspect("fd-full").await.unwrap();
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(
            err.to_string().contains("failed to parse volume options"),
            "{err}"
        );
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("rm")),
            "a failed create leaves nothing to remove"
        );
    }

    #[tokio::test]
    async fn create_passes_the_environment_in_a_private_file_only() {
        let mut compat: Vec<serde_json::Value> =
            serde_json::from_str(&fixture("nerdctl_plain_inspect.json")).unwrap();
        compat[0]["Config"]["Env"] = serde_json::json!(["PATH=/bin", "DB_PASSWORD=hunter2-secret"]);
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect fd-plain",
                    ok(serde_json::to_string(&compat).unwrap()),
                )
                .on(
                    "inspect --mode=native fd-plain",
                    ok(fixture("nerdctl_plain_native.json")),
                )
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(fixture("nerdctl_image_inspect.json")),
                )
                .on("create --name fd-plain --env-file * *", ok("newid\n")),
        );
        let spec = n.inspect("fd-plain").await.unwrap();
        assert_eq!(
            n.create_from_spec("fd-plain", &spec, IMAGE).await.unwrap(),
            "newid"
        );
        let calls = n.runner.calls();
        assert!(
            !calls.iter().any(|c| c.contains("hunter2-secret")),
            "no call carries the value: {calls:?}"
        );
        let files = n.runner.env_files.lock().unwrap().clone();
        assert_eq!(files.len(), 1);
        let (path, contents) = &files[0];
        assert!(
            contents.contains("DB_PASSWORD=hunter2-secret\n"),
            "{contents}"
        );
        assert!(
            !std::path::Path::new(path).exists(),
            "the env file is removed after the create"
        );
    }

    #[tokio::test]
    async fn preflight_refuses_when_the_trial_differs_from_the_original() {
        let mut trial: Vec<serde_json::Value> =
            serde_json::from_str(&fixture("nerdctl_full_native.json")).unwrap();
        trial[0]["Spec"]["process"]["noNewPrivileges"] = serde_json::Value::Bool(true);
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(serde_json::to_string(&trial).unwrap()),
                ),
        );
        let spec = n.inspect("fd-full").await.unwrap();
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("noNewPrivileges"), "{err}");
        assert!(
            n.runner
                .calls()
                .iter()
                .any(|c| c.starts_with("rm --force fd-full-freshdock-check-")),
            "the trial is removed either way"
        );
    }

    fn native_json(id: &str, created: &str, current: &str) -> String {
        format!(
            r#"[{{"ID":"{id}","Labels":{{"nerdctl/name":"{current}"}},"Spec":{{"annotations":{{"nerdctl/name":"{created}"}}}}}}]"#
        )
    }

    fn inspect_json(id: &str, name: &str) -> String {
        format!(r#"[{{"Id":"{id}","Name":"{name}"}}]"#)
    }

    #[tokio::test]
    async fn removing_a_renamed_archive_steps_the_name_holder_aside() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web-old-1",
                    ok(native_json("old", "web", "web-old-1")),
                )
                .on("inspect web", ok(inspect_json("new", "web")))
                .on("rename web web-freshdock-aside-*", ok(""))
                .on("rm web-old-1", ok(""))
                .on("rename web-freshdock-aside-* web", ok("")),
        );
        n.remove("web-old-1", false).await.unwrap();
        let calls = n.runner.calls();
        let order: Vec<&str> = calls
            .iter()
            .map(|c| c.split(' ').take(2).collect::<Vec<_>>().join(" "))
            .map(|c| match c.as_str() {
                "rename web" => "aside",
                "rm web-old-1" => "rm",
                c if c.starts_with("rename web-freshdock-aside") => "back",
                _ => "inspect",
            })
            .collect();
        assert_eq!(order, ["inspect", "inspect", "aside", "rm", "back"]);
    }

    #[tokio::test]
    async fn the_name_holder_gets_its_name_back_even_when_rm_fails() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web-old-1",
                    ok(native_json("old", "web", "web-old-1")),
                )
                .on("inspect web", ok(inspect_json("new", "web")))
                .on("rename web web-freshdock-aside-*", ok(""))
                .on(
                    "rm web-old-1",
                    fail("time=\"t\" level=fatal msg=\"boom\"\n"),
                )
                .on("rename web-freshdock-aside-* web", ok("")),
        );
        assert!(n.remove("web-old-1", false).await.is_err());
        assert!(
            n.runner
                .calls()
                .last()
                .unwrap()
                .starts_with("rename web-freshdock-aside-")
        );
    }

    #[tokio::test]
    async fn removing_a_container_under_its_own_name_is_a_plain_rm() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web",
                    ok(native_json("new", "web", "web")),
                )
                .on("rm --force web", ok("")),
        );
        n.remove("web", true).await.unwrap();
        assert_eq!(n.runner.calls().len(), 2);
    }

    #[tokio::test]
    async fn create_passes_the_rebuilt_flags_and_returns_the_id() {
        let n = nerdctl(inspectable("fd-cmd", "cmd").on(
            "image inspect docker.io/library/alpine:3.20",
            fail(NOT_FOUND),
        ));
        let spec = n.inspect("fd-cmd").await.unwrap();
        assert_eq!(spec.image_id, None, "an unresolvable tag leaves no id");
        let expected = spec
            .nerdctl()
            .create_args("fd-cmd", "alpine:3.20", Some("*"))
            .join(" ");
        let n = nerdctl(FakeRunner::default().on(&expected, ok("abc123\n")));
        assert_eq!(
            n.create_from_spec("fd-cmd", &spec, "alpine:3.20")
                .await
                .unwrap(),
            "abc123"
        );
    }

    #[tokio::test]
    async fn pull_uses_freshdock_credentials_through_a_temporary_docker_config() {
        let config = crate::config::Config::from_toml(
            "[registry.ghcr]\nusername = \"me\"\ntoken = \"s3cret\"\n",
        )
        .unwrap();
        let store = crate::config::build_store(config, std::iter::empty());
        let runner = FakeRunner::default()
            .on("pull --quiet ghcr.io/me/app:1", ok(""))
            .on("pull --quiet library/nginx:alpine", ok(""));
        let n = Nerdctl::with_runner(runner, NS.to_owned(), Arc::new(store));
        n.pull(&ImageRef::parse("ghcr.io/me/app:1")).await.unwrap();
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        let configs = n.runner.docker_configs.lock().unwrap().clone();
        assert_eq!(
            configs.len(),
            1,
            "only the registry with credentials gets a config"
        );
        assert!(configs[0].contains("ghcr.io"), "{}", configs[0]);
    }

    #[tokio::test]
    async fn stop_passes_signal_and_grace_period() {
        let n = nerdctl(
            FakeRunner::default()
                .on("stop --signal SIGQUIT --time 25 web", ok(""))
                .on("stop web", ok("")),
        );
        n.stop("web", Some("SIGQUIT"), Some(25)).await.unwrap();
        n.stop("web", None, None).await.unwrap();
    }

    #[tokio::test]
    async fn rename_picks_a_free_archive_name() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect web-old-1700000000",
                    ok(fixture("nerdctl_plain_inspect.json")),
                )
                .on("inspect web-old-1700000000-1", fail(NOT_FOUND))
                .on("rename web web-old-1700000000-1", ok("")),
        );
        assert_eq!(
            n.rename("web", 1_700_000_000).await.unwrap(),
            "web-old-1700000000-1"
        );
    }

    #[tokio::test]
    async fn hook_exit_codes_are_read_from_nerdctls_message() {
        let n = nerdctl(
            FakeRunner::default()
                .on("exec web sh -c true", ok(""))
                .on(
                    "exec web sh -c exit 75",
                    fail("time=\"t\" level=fatal msg=\"exec failed with exit code 75\"\n"),
                )
                .on("exec gone sh -c true", fail(NOT_FOUND)),
        );
        assert_eq!(
            n.exec_hook("web", "true", None).await.unwrap(),
            HookStatus::Completed { exit_code: 0 }
        );
        assert_eq!(
            n.exec_hook("web", "exit 75", None).await.unwrap(),
            HookStatus::Completed { exit_code: 75 }
        );
        assert!(
            n.exec_hook("gone", "true", None)
                .await
                .unwrap_err()
                .is_not_found()
        );
    }

    #[test]
    fn hook_exit_code_parses_the_code() {
        assert_eq!(hook_exit_code("exec failed with exit code 75"), Some(75));
        assert_eq!(hook_exit_code("exec failed with exit code 1"), Some(1));
        assert_eq!(hook_exit_code("no such container: x"), None);
    }

    #[tokio::test]
    async fn network_dependents_are_found_by_the_owners_full_id() {
        let runner = FakeRunner::default()
            .on(
                "inspect fd-plain",
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n{NETDEP_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID} {NETDEP_ID}"),
                ok(joined(
                    &fixture("nerdctl_plain_inspect.json"),
                    &fixture("nerdctl_netdep_inspect.json"),
                )),
            );
        let n = nerdctl(runner);
        assert_eq!(
            n.list_network_dependents("fd-plain").await.unwrap(),
            ["fd-netdep"]
        );
    }

    #[tokio::test]
    async fn image_cleanup_maps_in_use_to_a_conflict() {
        let n = nerdctl(FakeRunner::default().on(
            &format!("rmi {IMAGE_ID}"),
            fail(
                "time=\"t\" level=fatal msg=\"1 errors:\\nconflict: unable to delete x (cannot be forced) - image is being used by running container y\"\n",
            ),
        ));
        assert!(
            n.remove_image(IMAGE_ID, false)
                .await
                .unwrap_err()
                .is_conflict()
        );
    }

    #[tokio::test]
    async fn removing_an_image_that_is_already_gone_succeeds() {
        let n = nerdctl(FakeRunner::default().on(&format!("rmi {IMAGE_ID}"), fail(NOT_FOUND)));
        n.remove_image(IMAGE_ID, false).await.unwrap();
    }

    #[tokio::test]
    async fn the_trial_create_leaves_out_published_ports() {
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(fixture("nerdctl_full_native.json")),
                ),
        );
        let mut spec = n.inspect("fd-full").await.unwrap();
        match &mut spec.body {
            SpecBody::Nerdctl(s) => s.ports = vec!["0.0.0.0:18080:80/tcp".to_owned()],
            SpecBody::Docker(_) => unreachable!(),
        }
        n.preflight_recreate(&spec).await.unwrap();
        let calls = n.runner.calls();
        let trial = calls.iter().find(|c| c.starts_with("create")).unwrap();
        assert!(!trial.contains("--publish"), "{trial}");
        assert!(
            spec.nerdctl()
                .create_args("fd-full", IMAGE, None)
                .contains(&"--publish".to_owned()),
            "the real create keeps them"
        );
    }

    #[test]
    fn versions_parse_major_minor() {
        assert_eq!(parse_version("v2.3.4"), Some((2, 3)));
        assert_eq!(parse_version("2.10.0-rc.1"), Some((2, 10)));
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
    }
}
