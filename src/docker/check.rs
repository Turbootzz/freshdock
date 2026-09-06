use async_trait::async_trait;
use bollard::models::ContainerSummary;

use super::{Docker, DockerError};

/// What the daemon knows about one local image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalImage {
    /// The image id (`sha256:<64 hex>`), when the daemon reported one.
    pub id: Option<String>,
    /// `RepoDigests` as reported, `repo@sha256:<hex>` entries.
    pub repo_digests: Vec<String>,
}

/// The identity of a running container: its reference and the image it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerImage {
    /// `Config.Image`, the reference the container was created from.
    pub reference: String,
    /// The id of the image the container runs.
    pub image_id: Option<String>,
}

/// Daemon **read** operations the `check` command depends on. Abstracted as a
/// trait — analogous to [`DockerOps`](super::recreate::DockerOps) for the
/// recreate cycle — so `commands::check::run_with` can be unit-tested with a
/// recording fake instead of a live Docker socket. The mutating recreate
/// cycle lives on `DockerOps`; `check` is read-only, so it gets its own
/// narrow trait rather than overloading `DockerOps` with methods it never
/// uses.
#[async_trait]
pub trait DockerCheck {
    async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError>;
    /// Inspect a local image by reference or id. An unknown image is an error.
    async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError>;
    /// Inspect a container for its `Config.Image` and the id of the image it runs.
    async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError>;
}

#[async_trait]
impl DockerCheck for Docker {
    async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        // Explicit `Docker::` path forwards to the inherent method of the same
        // name (inherent impls win path resolution), so this is not recursive.
        Docker::list_running(self).await
    }

    async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError> {
        Docker::inspect_local_image(self, image).await
    }

    async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError> {
        let spec = self.inspect_container_spec(id).await?;
        Ok(ContainerImage {
            reference: spec.image_ref,
            image_id: spec.image_id,
        })
    }
}
