pub mod inspect;
pub mod recreate;
pub mod rename;
pub mod spec;

use async_trait::async_trait;
use bollard::models::ContainerSummary;
use bollard::query_parameters::{
    CreateImageOptionsBuilder, ListContainersOptions, StopContainerOptionsBuilder,
};
use futures::StreamExt;
use tracing::{debug, info};

use crate::docker::recreate::DockerOps;
use crate::docker::spec::ContainerSpec;
use crate::registry::ImageRef;

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("docker daemon error: {0}")]
    Bollard(#[from] bollard::errors::Error),
    #[error("container inspect produced an incomplete spec: {0}")]
    Spec(crate::docker::spec::SpecError),
}

pub struct Docker(pub(crate) bollard::Docker);

impl Docker {
    pub fn connect() -> Result<Self, DockerError> {
        Ok(Self(bollard::Docker::connect_with_local_defaults()?))
    }

    pub async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        let opts = ListContainersOptions {
            all: false,
            ..Default::default()
        };
        Ok(self.0.list_containers(Some(opts)).await?)
    }

    /// Pull the given image reference from its registry, draining the
    /// progress stream. Returns the image string the caller should pass to
    /// `create_container` — for Phase 2 this is just `repo:tag` since the
    /// daemon updates the local tag in-place; Phase 5 will replace this
    /// with explicit digest resolution.
    pub async fn pull_image(&self, image_ref: &ImageRef) -> Result<String, DockerError> {
        let opts = CreateImageOptionsBuilder::new()
            .from_image(&image_ref.repository)
            .tag(&image_ref.tag)
            .build();
        let mut stream = self.0.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            let info = item?;
            if let Some(status) = info.status {
                debug!(image = %image_ref.repository, %status, "pull progress");
            }
        }
        Ok(format!("{}:{}", image_ref.repository, image_ref.tag))
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
}

#[async_trait]
impl DockerOps for Docker {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        info!(container = %name, "inspect");
        self.inspect_container_spec(name).await
    }

    async fn pull(&self, image_ref: &ImageRef) -> Result<String, DockerError> {
        info!(repo = %image_ref.repository, tag = %image_ref.tag, "pull");
        self.pull_image(image_ref).await
    }

    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        info!(container = %name, signal = ?signal, timeout_s = ?timeout_s, "stop");
        self.stop_container(name, signal, timeout_s).await
    }

    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
        info!(container = %name, ts = ts_unix, "rename");
        self.rename_to_old(name, ts_unix).await
    }

    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError> {
        info!(container = %name, image = %image, "create");
        self.create_container_from_spec(name, spec, image).await
    }

    async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
        info!(container = %name_or_id, "start");
        self.start_container(name_or_id).await
    }
}
