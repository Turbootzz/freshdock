use async_trait::async_trait;
use bollard::models::ContainerSummary;

use super::{Docker, DockerError};

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
    async fn inspect_image_repo_digests(&self, image: &str) -> Result<Vec<String>, DockerError>;
}

#[async_trait]
impl DockerCheck for Docker {
    async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
        // Explicit `Docker::` path forwards to the inherent method of the same
        // name (inherent impls win path resolution), so this is not recursive.
        Docker::list_running(self).await
    }

    async fn inspect_image_repo_digests(&self, image: &str) -> Result<Vec<String>, DockerError> {
        Docker::inspect_image_repo_digests(self, image).await
    }
}
