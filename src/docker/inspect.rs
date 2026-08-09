use super::DockerError;
use super::spec::{ContainerSpec, SpecError};

impl super::Docker {
    /// Inspect a running container by name or ID and return a [`ContainerSpec`]
    /// suitable for the recreate cycle.
    pub async fn inspect_container_spec(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        let resp = self.client.inspect_container(name, None).await?;
        Ok(ContainerSpec::from_inspect(resp)?)
    }

    /// Returns the locally-cached manifest digests (`repo@sha256:...`) for an
    /// image reference, as Docker reports them via `docker image inspect`.
    /// Returns an empty vec when the image has no recorded RepoDigests (e.g.
    /// images built locally and never pulled from a registry).
    pub async fn inspect_image_repo_digests(
        &self,
        image: &str,
    ) -> Result<Vec<String>, DockerError> {
        let resp = self.client.inspect_image(image).await?;
        Ok(resp.repo_digests.unwrap_or_default())
    }
}

impl From<SpecError> for DockerError {
    fn from(value: SpecError) -> Self {
        DockerError::Spec(value)
    }
}
