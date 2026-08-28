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
use tracing::{debug, info, warn};

use crate::config::CredentialStore;
use crate::docker::recreate::{DockerOps, HookStatus};
use crate::docker::spec::ContainerSpec;
use crate::health::{ContainerRuntimeState, HealthProbe};
use crate::registry::ImageRef;
use crate::registry::digest::split_repository;

/// Daemon API version at which a create can attach a container to more than
/// one network in a single call (Docker 25.0). Below it the daemon accepts one
/// endpoint in `NetworkingConfig` and errors on the rest — which for a recreate
/// lands *after* the original container has been stopped and renamed.
const MULTI_NETWORK_MIN_API: (u32, u32) = (1, 44);

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("docker daemon error: {0}")]
    Bollard(#[from] bollard::errors::Error),
    #[error("container inspect produced an incomplete spec: {0}")]
    Spec(crate::docker::spec::SpecError),
    #[error(
        "the daemon speaks Docker API {api_version}, which cannot attach a container to \
         {networks} networks in one create (that needs API 1.44 / Docker 25.0); upgrade \
         the daemon, or detach the extra networks and re-attach them after the update"
    )]
    ApiTooOldForMultiNetwork {
        api_version: String,
        networks: usize,
    },
}

pub struct Docker {
    pub(crate) client: bollard::Docker,
    credentials: Arc<CredentialStore>,
    /// API version agreed with *this* daemon at connect time, as `MAJOR.MINOR`
    /// (bollard's own `client_version()` after negotiation). Every request is
    /// issued at this version, so it is also what the preflight guards read.
    api_version: String,
}

impl Docker {
    /// Connect to the daemon and negotiate the API version.
    ///
    /// The socket is chosen in this order:
    ///
    /// 1. `DOCKER_HOST`, when set — through bollard's scheme-dispatching
    ///    constructor, so `tcp://`, `http://`, `https://` and `ssh://` are
    ///    honoured, not only `unix://`.
    /// 2. The local Docker socket (`/var/run/docker.sock`, or the named pipe on
    ///    Windows).
    /// 3. Podman's sockets (`$XDG_RUNTIME_DIR/podman/podman.sock`,
    ///    `/run/user/$UID/podman/podman.sock`, `/run/podman/podman.sock`), so a
    ///    Podman-only host works with no configuration at all.
    ///
    /// The negotiation is a `GET /version` that downgrades the client to the
    /// daemon's newest supported API. That makes the documented "auto-negotiated"
    /// claim true (freshdock's compiled-in default is newer than what older
    /// daemons accept), and it surfaces an unreachable or wedged daemon here,
    /// at connect time, instead of midway through a recreate.
    pub async fn connect(credentials: Arc<CredentialStore>) -> Result<Self, DockerError> {
        let client = connect_client()?.negotiate_version().await?;
        let api_version = client.client_version().to_string();
        info!(%api_version, "negotiated Docker API version");
        Ok(Self {
            client,
            credentials,
            api_version,
        })
    }

    pub async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        let opts = ListContainersOptions {
            all: false,
            ..Default::default()
        };
        Ok(self.client.list_containers(Some(opts)).await?)
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
        let credentials = self.credentials.get(host).map(|c| DockerCredentials {
            username: c.username.clone(),
            password: Some(c.token.expose().to_string()),
            ..Default::default()
        });
        let opts = CreateImageOptionsBuilder::new()
            .from_image(&image_ref.repository)
            .tag(&image_ref.tag)
            .build();
        let mut stream = self.client.create_image(Some(opts), None, credentials);
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
        self.client
            .stop_container(name, Some(builder.build()))
            .await?;
        Ok(())
    }

    pub async fn start_container(&self, name_or_id: &str) -> Result<(), DockerError> {
        self.client.start_container(name_or_id, None).await?;
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
        let resp = self.client.create_container(Some(opts), body).await?;
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
        self.client.remove_container(name_or_id, Some(opts)).await?;
        Ok(())
    }

    /// Plain `from → to` rename (no archive-naming logic). Used by rollback to
    /// move `<name>-old-<ts>` back to its original name.
    pub async fn rename_container_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
        let opts = RenameContainerOptionsBuilder::new().name(to).build();
        self.client.rename_container(from, opts).await?;
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
                .client
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
                self.client.start_exec(&created.id, None).await?
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
            let mut inspected = self.client.inspect_exec(&created.id).await?;
            for _ in 0..20 {
                if inspected.running != Some(true) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                inspected = self.client.inspect_exec(&created.id).await?;
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
        let resp = self.client.inspect_container(name, None).await?;
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
            crate::selfid::own_container_id_prefix().as_deref(),
        ))
    }

    /// Inspect a container and classify its lifecycle + health into the
    /// daemon-agnostic [`ContainerRuntimeState`] the health gate polls on.
    pub async fn probe_runtime_state(
        &self,
        name_or_id: &str,
    ) -> Result<ContainerRuntimeState, DockerError> {
        let resp = self.client.inspect_container(name_or_id, None).await?;
        Ok(classify_runtime_state(resp.state))
    }
}

/// Build the bollard client for whichever daemon this host exposes, without
/// negotiating yet (see [`Docker::connect`] for the order and the rationale).
///
/// `connect_with_local_defaults` is deliberately *not* the entry point: it only
/// ever opens the local socket and silently ignores a `tcp://`/`ssh://`
/// `DOCKER_HOST`, which would point freshdock at the wrong daemon without a
/// word. Only the socket *family* is logged — a `DOCKER_HOST` can carry
/// credentials (`ssh://user@host`), so its value never reaches the log.
fn connect_client() -> Result<bollard::Docker, bollard::errors::Error> {
    if let Some(host) = std::env::var("DOCKER_HOST").ok().filter(|h| !h.is_empty()) {
        // Scheme-less values ("/var/run/docker.sock") are not a thing bollard
        // accepts, so don't report one that was never there.
        let scheme = host.split_once("://").map_or("(none)", |(s, _)| s);
        info!(%scheme, "connecting to the Docker daemon via DOCKER_HOST");
        return bollard::Docker::connect_with_defaults();
    }
    match bollard::Docker::connect_with_local_defaults() {
        Ok(client) => {
            info!("connected to the local Docker socket");
            Ok(client)
        }
        // No Docker socket at the default location. On a Podman-only host that
        // is the normal state, not a misconfiguration — probe Podman's sockets
        // before giving up so such a host needs no configuration at all.
        #[cfg(unix)]
        Err(bollard::errors::Error::SocketNotFoundError(path)) => {
            info!(
                missing = %path,
                "no Docker socket; probing the Podman socket locations"
            );
            bollard::Docker::connect_with_podman_defaults()
        }
        Err(e) => Err(e),
    }
}

/// Parse a Docker API version (`MAJOR.MINOR`, optionally `v`-prefixed) into a
/// comparable pair. Anything else — including an empty string — is `None`, and
/// callers must treat that as "unknown", never as "old".
fn parse_api_version(raw: &str) -> Option<(u32, u32)> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    let (major, rest) = raw.split_once('.')?;
    // A trailing component (`1.44.0`) is not part of an API version, but it
    // must not make the version unreadable either.
    let minor = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Refuse a recreate the daemon could not complete: re-attaching more than one
/// network in a single create needs API [`MULTI_NETWORK_MIN_API`], and
/// `ContainerSpec::to_create_body` replays *every* endpoint the container had.
/// On an older daemon that create fails after the original has already been
/// stopped and renamed, so the check has to happen before anything moves.
///
/// An unparseable version is treated as new enough: a daemon reporting a string
/// we don't understand is not evidence that it is old, and refusing on it would
/// brick working setups over cosmetics.
fn multi_network_guard(api_version: &str, networks: usize) -> Result<(), DockerError> {
    if networks <= 1 {
        return Ok(());
    }
    match parse_api_version(api_version) {
        Some(version) if version < MULTI_NETWORK_MIN_API => {
            Err(DockerError::ApiTooOldForMultiNetwork {
                api_version: api_version.to_owned(),
                networks,
            })
        }
        Some(_) => Ok(()),
        None => {
            debug!(
                %api_version,
                "could not parse the daemon's API version; assuming it is new \
                 enough for a multi-network create"
            );
            Ok(())
        }
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

/// Names in `running` that share the network namespace of the container
/// identified by `owner_name`/`full_id` (`""` when the id could not be
/// resolved). `self_hostname` is freshdock's own hostname, so it can recognise
/// itself among the dependents. Pure so the matching and exclusion rules stay
/// testable without a daemon.
///
/// Re-attachment repairs a bystander rather than updating it, so it
/// deliberately ignores the `freshdock.enable` policy gate — only the
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
            if crate::selfid::is_own_container(self_hostname, summary.id.as_deref()) {
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

/// Does this container carry an **explicit** freshdock opt-out —
/// `freshdock.enable` (or its watchtower spelling) set to a false value, or
/// `freshdock.mode=off`? Matching is case-insensitive and
/// whitespace-tolerant, as in [`crate::labels`].
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
    value("freshdock.enable") == "false"
        || value("com.centurylinklabs.watchtower.enable") == "false"
        || value("freshdock.mode") == "off"
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

    /// The one check the daemon itself makes unavoidable: a container attached
    /// to several networks can only be re-created in one call from API 1.44
    /// (Docker 25.0) onwards.
    async fn preflight_recreate(&self, spec: &ContainerSpec) -> Result<(), DockerError> {
        let networks = spec.network_endpoints.as_ref().map_or(0, |e| e.len());
        debug!(container = %spec.name, networks, api_version = %self.api_version, "preflight");
        multi_network_guard(&self.api_version, networks)
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
        self.client.remove_image(id, Some(opts), None).await?;
        Ok(())
    }

    async fn prune_dangling_images(&self) -> Result<(), DockerError> {
        debug!("prune_dangling_images");
        let filters = std::collections::HashMap::from([("dangling", vec!["true"])]);
        let opts = PruneImagesOptionsBuilder::new().filters(&filters).build();
        self.client.prune_images(Some(opts)).await?;
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

    // --- multi-network create preflight (API 1.44 / Docker 25.0 floor) ---

    #[test]
    fn api_versions_are_parsed_as_major_minor() {
        assert_eq!(parse_api_version("1.43"), Some((1, 43)));
        assert_eq!(parse_api_version("1.44"), Some((1, 44)));
        assert_eq!(parse_api_version(" 1.55 "), Some((1, 55)));
        // Docker writes the API version both bare and `v`-prefixed.
        assert_eq!(parse_api_version("v1.47"), Some((1, 47)));
        // A trailing patch component is not part of the API version, but must
        // not make an otherwise-fine version unreadable.
        assert_eq!(parse_api_version("1.44.0"), Some((1, 44)));
    }

    #[test]
    fn unreadable_api_versions_parse_to_none() {
        for raw in ["", "   ", "latest", "1", "1.", ".44", "x.y", "1.x"] {
            assert_eq!(parse_api_version(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn multi_network_guard_refuses_below_api_1_44() {
        let err = multi_network_guard("1.43", 2)
            .expect_err("a 2-network create is not expressible on API 1.43");
        assert!(matches!(
            err,
            DockerError::ApiTooOldForMultiNetwork {
                ref api_version,
                networks: 2,
            } if api_version == "1.43"
        ));
        // The message has to tell the operator what to do about it.
        let msg = err.to_string();
        assert!(msg.contains("1.44"), "{msg}");
        assert!(msg.contains("1.43"), "{msg}");
    }

    #[test]
    fn multi_network_guard_allows_api_1_44_and_newer() {
        assert!(multi_network_guard("1.44", 2).is_ok());
        assert!(multi_network_guard("1.55", 5).is_ok());
        assert!(multi_network_guard("2.0", 3).is_ok());
    }

    #[test]
    fn multi_network_guard_ignores_single_network_containers() {
        // The overwhelming majority of containers: one endpoint (or none) is
        // expressible on every API version freshdock has ever talked to.
        assert!(multi_network_guard("1.24", 1).is_ok());
        assert!(multi_network_guard("1.24", 0).is_ok());
    }

    #[test]
    fn an_unreadable_api_version_never_bricks_a_recreate() {
        // A daemon reporting something we cannot parse is not evidence that it
        // is old — refusing on it would break working setups over a string.
        for raw in ["", "latest", "banana", "1.x"] {
            assert!(
                multi_network_guard(raw, 3).is_ok(),
                "{raw:?} must be treated as new enough"
            );
        }
    }

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
            summary_with(
                "fd-wt-out",
                "container:fd-base",
                "3",
                &[("com.centurylinklabs.watchtower.enable", "false")],
            ),
            summary_with("fd-unlabelled", "container:fd-base", "4", &[]),
            summary_with(
                "fd-enabled",
                "container:fd-base",
                "5",
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
