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
    CreateImageOptionsBuilder, ListContainersOptions, RemoveContainerOptionsBuilder,
    RenameContainerOptionsBuilder, StopContainerOptionsBuilder,
};
use futures::StreamExt;
use tracing::debug;

use crate::config::CredentialStore;
use crate::docker::recreate::DockerOps;
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
}
