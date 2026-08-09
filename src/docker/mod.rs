pub mod check;
pub mod inspect;
pub mod recreate;
pub mod rename;
pub mod spec;

use std::sync::Arc;

use async_trait::async_trait;
use bollard::auth::DockerCredentials;
use bollard::models::{
    ContainerState, ContainerStateStatusEnum, ContainerSummary, HealthStatusEnum,
};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, ListContainersOptions, PruneImagesOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RenameContainerOptionsBuilder,
    StopContainerOptionsBuilder,
};
use futures::StreamExt;
use tracing::{debug, warn};

use crate::config::CredentialStore;
use crate::docker::recreate::{DockerOps, HookStatus};
use crate::docker::spec::ContainerSpec;
use crate::health::{ContainerRuntimeState, HealthProbe};
use crate::registry::ImageRef;
use crate::registry::digest::split_repository;

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("docker daemon error: {0}")]
    Bollard(#[from] bollard::errors::Error),
    #[error("container inspect produced an incomplete spec: {0}")]
    Spec(crate::docker::spec::SpecError),
}

pub struct Docker(pub(crate) bollard::Docker, Arc<CredentialStore>);

impl Docker {
    pub fn connect(credentials: Arc<CredentialStore>) -> Result<Self, DockerError> {
        Ok(Self(
            bollard::Docker::connect_with_local_defaults()?,
            credentials,
        ))
    }

    pub async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        let opts = ListContainersOptions {
            all: false,
            ..Default::default()
        };
        Ok(self.0.list_containers(Some(opts)).await?)
    }

    /// Pull the given image reference from its registry, draining the
    /// progress stream. The orchestrator hands the original `spec.image_ref`
    /// (not a re-rendering) to `create_from_spec` so `Config.Image` round-trips
    /// byte-identical (#25); this only needs the side effect of getting the new
    /// image into the local store.
    ///
    /// Phase 5: when credentials are configured for the image's registry host,
    /// they're passed as the daemon's `X-Registry-Auth` so private images pull
    /// (the registry HEAD check and this pull share one [`CredentialStore`]).
    pub async fn pull_image(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
        let (host, _) = split_repository(&image_ref.repository);
        let credentials = self.1.get(host).map(|c| DockerCredentials {
            username: c.username.clone(),
            password: Some(c.token.expose().to_string()),
            ..Default::default()
        });
        let opts = CreateImageOptionsBuilder::new()
            .from_image(&image_ref.repository)
            .tag(&image_ref.tag)
            .build();
        let mut stream = self.0.create_image(Some(opts), None, credentials);
        while let Some(item) = stream.next().await {
            let info = item?;
            if let Some(status) = info.status {
                debug!(image = %image_ref.repository, %status, "pull progress");
            }
        }
        Ok(())
    }

    pub async fn stop_container(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        let mut builder = StopContainerOptionsBuilder::new();
        if let Some(s) = signal {
            builder = builder.signal(s);
        }
        if let Some(t) = timeout_s {
            // Bollard's StopContainerOptions.t is i32; container stop
            // timeouts realistically fit in that range (Docker rejects
            // anything more than a few hours anyway).
            builder = builder.t(t.try_into().unwrap_or(i32::MAX));
        }
        self.0.stop_container(name, Some(builder.build())).await?;
        Ok(())
    }

    pub async fn start_container(&self, name_or_id: &str) -> Result<(), DockerError> {
        self.0.start_container(name_or_id, None).await?;
        Ok(())
    }

    pub async fn create_container_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        new_image: &str,
    ) -> Result<String, DockerError> {
        let body = spec.to_create_body(new_image);
        let opts = bollard::query_parameters::CreateContainerOptionsBuilder::new()
            .name(name)
            .build();
        let resp = self.0.create_container(Some(opts), body).await?;
        Ok(resp.id)
    }

    /// Remove a container by name or id. `force` issues a SIGKILL + remove for
    /// a still-running container (the rollback path removes the *running* new
    /// instance); `false` is the graceful remove used for the already-stopped
    /// `-old-` archive on a successful update.
    pub async fn remove_container_named(
        &self,
        name_or_id: &str,
        force: bool,
    ) -> Result<(), DockerError> {
        let opts = RemoveContainerOptionsBuilder::new().force(force).build();
        self.0.remove_container(name_or_id, Some(opts)).await?;
        Ok(())
    }

    /// Plain `from → to` rename (no archive-naming logic). Used by rollback to
    /// move `<name>-old-<ts>` back to its original name.
    pub async fn rename_container_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
        let opts = RenameContainerOptionsBuilder::new().name(to).build();
        self.0.rename_container(from, opts).await?;
        Ok(())
    }

    /// Run a lifecycle hook command inside a running container via `sh -c`
    /// (the image must ship a `sh`, as with watchtower). Output is drained at
    /// `debug!`. The timeout bounds the whole exchange with the daemon
    /// (create → drain → exit-code inspect), so a wedged daemon can't hang a
    /// scheduler tick past the hook budget. On timeout the exec itself keeps
    /// running inside the container — Docker has no exec-kill API — but the
    /// verdict is `TimedOut`.
    pub async fn exec_in_container(
        &self,
        name_or_id: &str,
        command: &str,
        timeout: Option<std::time::Duration>,
    ) -> Result<HookStatus, DockerError> {
        use bollard::exec::{CreateExecOptions, StartExecResults};

        let run = async {
            let created = self
                .0
                .create_exec(
                    name_or_id,
                    CreateExecOptions {
                        cmd: Some(vec!["sh", "-c", command]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        ..Default::default()
                    },
                )
                .await?;

            if let StartExecResults::Attached { mut output, .. } =
                self.0.start_exec(&created.id, None).await?
            {
                while let Some(chunk) = output.next().await {
                    debug!(container = %name_or_id, output = %chunk?, "hook output");
                }
            }

            // The daemon finalises exec state asynchronously after the stream
            // closes, so an immediate inspect can still report `running` with
            // no exit code — poll briefly rather than misread a just-finished
            // hook. A code still missing after that is treated as a failure,
            // never a success.
            let mut inspected = self.0.inspect_exec(&created.id).await?;
            for _ in 0..20 {
                if inspected.running != Some(true) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                inspected = self.0.inspect_exec(&created.id).await?;
            }
            Ok::<HookStatus, DockerError>(HookStatus::Completed {
                exit_code: inspected.exit_code.unwrap_or(-1),
            })
        };

        match timeout {
            Some(t) => match tokio::time::timeout(t, run).await {
                Ok(res) => res,
                Err(_) => Ok(HookStatus::TimedOut),
            },
            None => run.await,
        }
    }

    /// Running containers sharing this container's network namespace via
    /// `HostConfig.NetworkMode = container:<ref>`. The owner is inspected first
    /// because the reference is stored exactly as it was given at create time —
    /// a name, a full id, or an id prefix — so matching needs *both* the
    /// container's real name and its full id, not whatever string the caller
    /// happened to address it by (`freshdock recreate <id>` would otherwise
    /// never match a name-based reference).
    pub async fn network_dependents_of(&self, name: &str) -> Result<Vec<String>, DockerError> {
        let resp = self.0.inspect_container(name, None).await?;
        let full_id = resp.id.unwrap_or_default();
        let owner_name = resp
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/'))
            .filter(|n| !n.is_empty())
            .unwrap_or(name);
        Ok(network_dependent_names(
            &self.list_running().await?,
            owner_name,
            &full_id,
            own_hostname().as_deref(),
        ))
    }

    /// Inspect a container and classify its lifecycle + health into the
    /// daemon-agnostic [`ContainerRuntimeState`] the health gate polls on.
    pub async fn probe_runtime_state(
        &self,
        name_or_id: &str,
    ) -> Result<ContainerRuntimeState, DockerError> {
        let resp = self.0.inspect_container(name_or_id, None).await?;
        Ok(classify_runtime_state(resp.state))
    }
}

/// Container name from a summary (leading `/` trimmed), falling back to id.
/// Shared by the scheduler's per-tick sweep and the network-dependent scan.
pub(crate) fn container_name(c: &ContainerSummary) -> String {
    c.names
        .as_ref()
        .and_then(|n| n.first())
        .map(|s| s.trim_start_matches('/').to_string())
        .or_else(|| c.id.clone())
        .unwrap_or_else(|| "?".to_string())
}

/// freshdock's own hostname, used to recognise (and never stop) the freshdock
/// container itself. Inside a container the daemon sets the hostname to the
/// container's short id unless overridden; `/etc/hostname` is the reading that
/// survives a `docker exec` environment, with `$HOSTNAME` as the fallback.
fn own_hostname() -> Option<String> {
    let from_file = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        // An empty (whitespace-only) file must fall through to $HOSTNAME, not
        // pin the chain to `Some("")` and disable self-recognition entirely.
        .filter(|h| !h.is_empty());
    from_file
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|h| !h.is_empty())
}

/// Could `hostname` be a container id (or short id)? Docker's short id is 12
/// hex characters; anything shorter or non-hex is an operator-chosen hostname
/// and must never be prefix-matched against container ids.
fn looks_like_container_id(hostname: &str) -> bool {
    hostname.len() >= 12 && hostname.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Names in `running` that share the network namespace of the container
/// identified by `owner_name`/`full_id` (`""` when the id could not be
/// resolved). `self_hostname` is freshdock's own hostname, so it can recognise
/// itself among the dependents. Pure so the matching and exclusion rules stay
/// testable without a daemon.
///
/// Re-attachment repairs a bystander rather than updating it, so it
/// deliberately ignores the `freshdock.enable` policy gate — only the two
/// *explicit* opt-out signals are honoured (see [`explicitly_opts_out`]).
fn network_dependent_names(
    running: &[ContainerSummary],
    owner_name: &str,
    full_id: &str,
    self_hostname: Option<&str>,
) -> Vec<String> {
    running
        .iter()
        .filter_map(|summary| {
            let mode = summary
                .host_config
                .as_ref()
                .and_then(|hc| hc.network_mode.as_deref())?;
            if !network_mode_references(mode, owner_name, full_id) {
                return None;
            }
            let dependent = container_name(summary);
            // A container never depends on itself.
            if dependent == owner_name {
                return None;
            }
            // freshdock joined to the namespace it is updating: stopping
            // ourselves kills the daemon mid-cycle, and an explicit stop
            // defeats `restart: always`.
            if is_self(summary, self_hostname) {
                warn!(
                    container = %dependent,
                    owner = %owner_name,
                    "skipping network-namespace re-attach of freshdock itself; \
                     restart freshdock manually to restore its own networking"
                );
                return None;
            }
            if explicitly_opts_out(summary) {
                warn!(
                    container = %dependent,
                    owner = %owner_name,
                    "network-namespace dependent explicitly opts out of freshdock; \
                     not re-attaching — it keeps a dead network namespace until it \
                     is restarted manually"
                );
                return None;
            }
            Some(dependent)
        })
        .collect()
}

/// Is this summary freshdock's own container? True only when our hostname
/// looks like a container id and the candidate's id starts with it.
fn is_self(summary: &ContainerSummary, self_hostname: Option<&str>) -> bool {
    let Some(hostname) = self_hostname else {
        return false;
    };
    if !looks_like_container_id(hostname) {
        return false;
    }
    summary
        .id
        .as_deref()
        .is_some_and(|id| id.starts_with(hostname))
}

/// Does this container carry an **explicit** freshdock opt-out —
/// `freshdock.enable` set to a false value, or `freshdock.mode=off`? Matching
/// is case-insensitive and whitespace-tolerant, as in [`crate::labels`].
///
/// Absent labels deliberately do *not* opt out: the whole point of the
/// re-attach pass is repairing an unlabelled bystander whose namespace *we*
/// broke.
fn explicitly_opts_out(summary: &ContainerSummary) -> bool {
    let Some(labels) = summary.labels.as_ref() else {
        return false;
    };
    let value = |key: &str| {
        labels
            .get(key)
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_default()
    };
    value("freshdock.enable") == "false" || value("freshdock.mode") == "off"
}

/// The container a `HostConfig.NetworkMode` joins, if any: `container:<ref>`
/// → `Some(<ref>)`, every other mode (`host`, `bridge`, a network name, …)
/// → `None`. The one place the `container:` prefix is parsed, so the dependent
/// *scan* and the reference *rewrite* can never disagree on what counts as a
/// reference.
pub(crate) fn container_reference(mode: &str) -> Option<&str> {
    mode.strip_prefix("container:")
}

/// Does `mode` (a `HostConfig.NetworkMode`) join the network namespace of the
/// container identified by `name`/`full_id`?
///
/// All three shapes are matched — the container name, its full 64-char id, or
/// an id prefix — because what the daemon *stores* depends on its version:
/// modern daemons normalise a name-based reference to the owner's full id at
/// create time, older ones keep whatever was given. Prefixes shorter than 12
/// characters are rejected — Docker's own short-id width — as too weak to
/// attribute the reference to this container.
fn network_mode_references(mode: &str, name: &str, full_id: &str) -> bool {
    let Some(reference) = container_reference(mode) else {
        return false;
    };
    reference == name
        || (!full_id.is_empty()
            && (reference == full_id || (reference.len() >= 12 && full_id.starts_with(reference))))
}

/// Map bollard's `State` into the health gate's projection. `Running` +
/// health status decides the healthcheck vs. grace-period path; anything not
/// running is `Exited`. A missing/`none`/empty health status means no
/// healthcheck was declared.
fn classify_runtime_state(state: Option<ContainerState>) -> ContainerRuntimeState {
    let Some(state) = state else {
        return ContainerRuntimeState::Exited { exit_code: 0 };
    };
    let running = matches!(state.status, Some(ContainerStateStatusEnum::RUNNING))
        || state.running == Some(true);
    if !running {
        return ContainerRuntimeState::Exited {
            exit_code: state.exit_code.unwrap_or(0),
        };
    }
    match state.health.and_then(|h| h.status) {
        Some(HealthStatusEnum::HEALTHY) => ContainerRuntimeState::HealthHealthy,
        Some(HealthStatusEnum::UNHEALTHY) => ContainerRuntimeState::HealthUnhealthy,
        Some(HealthStatusEnum::STARTING) => ContainerRuntimeState::HealthStarting,
        // None / `none` / empty: no healthcheck declared → grace-period path.
        _ => ContainerRuntimeState::RunningNoHealthcheck,
    }
}

/// Production wiring of the `DockerOps` trait. Per-step traces are emitted
/// at `debug!` level — six per recreate would be too chatty at default
/// `info`. The orchestrator's caller (`commands::recreate::run`) emits the
/// single info-level "recreate complete" summary line.
#[async_trait]
impl DockerOps for Docker {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        debug!(container = %name, "inspect");
        self.inspect_container_spec(name).await
    }

    async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
        debug!(repo = %image_ref.repository, tag = %image_ref.tag, "pull");
        self.pull_image(image_ref).await
    }

    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        debug!(container = %name, signal = ?signal, timeout_s = ?timeout_s, "stop");
        self.stop_container(name, signal, timeout_s).await
    }

    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
        debug!(container = %name, ts = ts_unix, "rename");
        self.rename_to_old(name, ts_unix).await
    }

    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError> {
        debug!(container = %name, image = %image, "create");
        self.create_container_from_spec(name, spec, image).await
    }

    async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
        debug!(container = %name_or_id, "start");
        self.start_container(name_or_id).await
    }

    async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError> {
        debug!(container = %name_or_id, force, "remove");
        self.remove_container_named(name_or_id, force).await
    }

    async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
        debug!(from = %from, to = %to, "rename_to");
        self.rename_container_to(from, to).await
    }

    async fn remove_image(&self, id: &str, force: bool) -> Result<(), DockerError> {
        debug!(image = %id, force, "remove_image");
        // `force=false`: the daemon refuses (409) an image still referenced by
        // another container — the caller treats that refusal as a guard, not a
        // failure. `noprune` defaults false, so now-dangling parent layers are
        // also dropped (the intent of "prune the old image").
        let opts = RemoveImageOptionsBuilder::new().force(force).build();
        self.0.remove_image(id, Some(opts), None).await?;
        Ok(())
    }

    async fn prune_dangling_images(&self) -> Result<(), DockerError> {
        debug!("prune_dangling_images");
        let filters = std::collections::HashMap::from([("dangling", vec!["true"])]);
        let opts = PruneImagesOptionsBuilder::new().filters(&filters).build();
        self.0.prune_images(Some(opts)).await?;
        Ok(())
    }

    async fn exec_hook(
        &self,
        name_or_id: &str,
        command: &str,
        timeout: Option<std::time::Duration>,
    ) -> Result<HookStatus, DockerError> {
        debug!(container = %name_or_id, %command, timeout_s = ?timeout.map(|t| t.as_secs()), "exec_hook");
        self.exec_in_container(name_or_id, command, timeout).await
    }

    async fn list_network_dependents(&self, name: &str) -> Result<Vec<String>, DockerError> {
        debug!(container = %name, "list_network_dependents");
        self.network_dependents_of(name).await
    }
}

#[async_trait]
impl HealthProbe for Docker {
    async fn probe_state(&self, name_or_id: &str) -> Result<ContainerRuntimeState, DockerError> {
        self.probe_runtime_state(name_or_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::Health;

    fn state(
        status: ContainerStateStatusEnum,
        health: Option<HealthStatusEnum>,
    ) -> Option<ContainerState> {
        Some(ContainerState {
            status: Some(status),
            health: health.map(|s| Health {
                status: Some(s),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[test]
    fn running_with_healthy_check_maps_to_healthy() {
        assert_eq!(
            classify_runtime_state(state(
                ContainerStateStatusEnum::RUNNING,
                Some(HealthStatusEnum::HEALTHY)
            )),
            ContainerRuntimeState::HealthHealthy
        );
    }

    #[test]
    fn running_with_unhealthy_and_starting_map_through() {
        assert_eq!(
            classify_runtime_state(state(
                ContainerStateStatusEnum::RUNNING,
                Some(HealthStatusEnum::UNHEALTHY)
            )),
            ContainerRuntimeState::HealthUnhealthy
        );
        assert_eq!(
            classify_runtime_state(state(
                ContainerStateStatusEnum::RUNNING,
                Some(HealthStatusEnum::STARTING)
            )),
            ContainerRuntimeState::HealthStarting
        );
    }

    #[test]
    fn running_without_healthcheck_maps_to_grace_path() {
        assert_eq!(
            classify_runtime_state(state(ContainerStateStatusEnum::RUNNING, None)),
            ContainerRuntimeState::RunningNoHealthcheck
        );
        assert_eq!(
            classify_runtime_state(state(
                ContainerStateStatusEnum::RUNNING,
                Some(HealthStatusEnum::NONE)
            )),
            ContainerRuntimeState::RunningNoHealthcheck
        );
    }

    #[test]
    fn exited_container_carries_exit_code() {
        let st = Some(ContainerState {
            status: Some(ContainerStateStatusEnum::EXITED),
            running: Some(false),
            exit_code: Some(137),
            ..Default::default()
        });
        assert_eq!(
            classify_runtime_state(st),
            ContainerRuntimeState::Exited { exit_code: 137 }
        );
    }

    #[test]
    fn missing_state_is_treated_as_exited() {
        assert_eq!(
            classify_runtime_state(None),
            ContainerRuntimeState::Exited { exit_code: 0 }
        );
    }

    const FULL_ID: &str = "9f8e7d6c5b4a3210fedcba98765432100123456789abcdef0123456789abcdef";

    #[test]
    fn network_mode_matches_name_full_id_and_long_prefix() {
        assert!(network_mode_references(
            "container:fd-base",
            "fd-base",
            FULL_ID
        ));
        assert!(network_mode_references(
            &format!("container:{FULL_ID}"),
            "fd-base",
            FULL_ID
        ));
        // Docker's own short id is 12 chars — compose and `docker inspect`
        // both surface references at that width.
        assert!(network_mode_references(
            "container:9f8e7d6c5b4a",
            "fd-base",
            FULL_ID
        ));
    }

    #[test]
    fn network_mode_rejects_short_id_prefixes() {
        assert!(
            !network_mode_references("container:9f8e7d6c5b4", "fd-base", FULL_ID),
            "an 11-char prefix is not unique enough to attribute the reference"
        );
        assert!(!network_mode_references(
            "container:9f8e",
            "fd-base",
            FULL_ID
        ));
    }

    #[test]
    fn network_mode_rejects_other_containers_and_non_container_modes() {
        assert!(!network_mode_references(
            "container:other",
            "fd-base",
            FULL_ID
        ));
        for mode in [
            "host",
            "none",
            "bridge",
            "fd-base",
            "container",
            "service:x",
        ] {
            assert!(
                !network_mode_references(mode, "fd-base", FULL_ID),
                "{mode} does not share a network namespace with fd-base"
            );
        }
    }

    #[test]
    fn network_mode_with_unknown_owner_id_still_matches_by_name() {
        // The owner's id could not be resolved: name matching must still work,
        // and an empty id must never prefix-match everything.
        assert!(network_mode_references("container:fd-base", "fd-base", ""));
        assert!(!network_mode_references(
            "container:9f8e7d6c5b4a",
            "fd-base",
            ""
        ));
    }

    fn summary(name: &str, network_mode: Option<&str>) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{name}")]),
            host_config: Some(bollard::models::ContainerSummaryHostConfig {
                network_mode: network_mode.map(str::to_owned),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// [`summary`] plus the container id and labels the exclusion rules read.
    fn summary_with(
        name: &str,
        network_mode: &str,
        id: &str,
        labels: &[(&str, &str)],
    ) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_owned()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..summary(name, Some(network_mode))
        }
    }

    #[test]
    fn dependent_listing_keeps_only_namespace_sharers() {
        let running = [
            summary("fd-base", Some("bridge")),
            summary("fd-peer", Some("container:fd-base")),
            summary("fd-peer-by-id", Some(&format!("container:{FULL_ID}"))),
            summary("fd-unrelated", Some("container:other")),
            summary("fd-host", Some("host")),
            summary("fd-no-hostconfig", None),
        ];
        assert_eq!(
            network_dependent_names(&running, "fd-base", FULL_ID, None),
            vec!["fd-peer".to_owned(), "fd-peer-by-id".to_owned()]
        );
    }

    #[test]
    fn dependent_listing_excludes_the_owner_itself() {
        // A container joined to its own namespace is nonsense the daemon would
        // never produce, but it must never be dragged through a re-attach cycle.
        let running = [
            summary("fd-base", Some("container:fd-base")),
            summary("fd-peer", Some("container:fd-base")),
        ];
        assert_eq!(
            network_dependent_names(&running, "fd-base", FULL_ID, None),
            vec!["fd-peer".to_owned()]
        );
    }

    #[test]
    fn dependent_listing_matches_by_name_when_the_owner_was_addressed_by_id() {
        // `freshdock recreate <id>`: the caller's string is an id, but the
        // owner name handed to this function is the *inspected* one, so a
        // name-based reference still matches (and so does the id-based one).
        let running = [
            summary("fd-peer-by-name", Some("container:fd-base")),
            summary("fd-peer-by-id", Some(&format!("container:{FULL_ID}"))),
        ];
        assert_eq!(
            network_dependent_names(&running, "fd-base", FULL_ID, None),
            vec!["fd-peer-by-name".to_owned(), "fd-peer-by-id".to_owned()],
            "both reference shapes resolve to the same owner"
        );
    }

    #[test]
    fn dependent_listing_keeps_archive_looking_names() {
        // `is_archive_name` is a heuristic: `redis-old-6` is a perfectly normal
        // container name. Repairing a genuinely stale archive is harmless;
        // silently stranding a real sidecar is not — so nothing is excluded on
        // name shape alone.
        let running = [
            summary("redis-old-6", Some("container:fd-base")),
            summary("fd-peer-old-1700000000", Some("container:fd-base")),
        ];
        assert_eq!(
            network_dependent_names(&running, "fd-base", FULL_ID, None),
            vec![
                "redis-old-6".to_owned(),
                "fd-peer-old-1700000000".to_owned()
            ]
        );
    }

    #[test]
    fn dependent_listing_skips_freshdock_itself() {
        // freshdock deployed with `network_mode: container:<vpn>` shows up as a
        // dependent of its own target. Stopping it would kill the daemon
        // mid-cycle, and an explicit stop defeats `restart: always`.
        let self_id = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let running = [
            summary_with("freshdock", "container:fd-vpn", self_id, &[]),
            summary_with("fd-peer", "container:fd-vpn", "0f0f0f0f0f0f0f0f", &[]),
        ];
        // The daemon sets the container's hostname to its short id.
        assert_eq!(
            network_dependent_names(&running, "fd-vpn", FULL_ID, Some("a1b2c3d4e5f6")),
            vec!["fd-peer".to_owned()]
        );
    }

    #[test]
    fn a_non_container_hostname_excludes_nothing() {
        // Host install (or `--hostname my-nas`): the hostname is not a container
        // id, so it must never be matched against container ids.
        let running = [summary_with("fd-peer", "container:fd-vpn", "a1b2c3", &[])];
        for hostname in [None, Some("my-nas"), Some("a1b2c3"), Some("nas-01234567")] {
            assert_eq!(
                network_dependent_names(&running, "fd-vpn", FULL_ID, hostname),
                vec!["fd-peer".to_owned()],
                "hostname {hostname:?} must not exclude anything"
            );
        }
    }

    #[test]
    fn dependent_listing_skips_explicit_opt_outs() {
        let running = [
            summary_with(
                "fd-opted-out",
                "container:fd-base",
                "1",
                &[("freshdock.enable", "False")],
            ),
            summary_with(
                "fd-mode-off",
                "container:fd-base",
                "2",
                &[("freshdock.enable", "true"), ("freshdock.mode", "OFF")],
            ),
            summary_with("fd-unlabelled", "container:fd-base", "3", &[]),
            summary_with(
                "fd-enabled",
                "container:fd-base",
                "4",
                &[("freshdock.enable", "true")],
            ),
        ];
        assert_eq!(
            network_dependent_names(&running, "fd-base", FULL_ID, None),
            vec!["fd-unlabelled".to_owned(), "fd-enabled".to_owned()],
            "only an EXPLICIT opt-out is honoured — an unlabelled bystander is \
             exactly who this repair exists for"
        );
    }

    #[test]
    fn container_reference_extracts_only_container_modes() {
        assert_eq!(container_reference("container:fd-base"), Some("fd-base"));
        assert_eq!(container_reference("container:"), Some(""));
        for mode in [
            "host",
            "none",
            "bridge",
            "service:x",
            "fd-base",
            "container",
        ] {
            assert_eq!(container_reference(mode), None, "{mode}");
        }
    }

    #[test]
    fn container_name_trims_slash_and_falls_back_to_id() {
        assert_eq!(
            container_name(&ContainerSummary {
                names: Some(vec!["/fd-peer".to_owned()]),
                ..Default::default()
            }),
            "fd-peer"
        );
        assert_eq!(
            container_name(&ContainerSummary {
                id: Some("deadbeef".to_owned()),
                ..Default::default()
            }),
            "deadbeef"
        );
    }
}
