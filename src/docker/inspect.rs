use super::DockerError;
use super::check::LocalImage;
use super::spec::{ContainerSpec, SpecError};

impl super::Docker {
    /// Inspect a running container by name or ID and return a [`ContainerSpec`]
    /// suitable for the recreate cycle.
    pub async fn inspect_container_spec(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        let resp = self.client.inspect_container(name, None).await?;
        Ok(ContainerSpec::from_inspect(resp)?)
    }

    /// One local image: its id and the `repo@sha256:...` digests it carries.
    /// Empty for a locally built image, and under containerd once its tag moved.
    pub async fn inspect_local_image(&self, image: &str) -> Result<LocalImage, DockerError> {
        let resp = self.client.inspect_image(image).await?;
        Ok(LocalImage {
            id: resp.id,
            repo_digests: resp.repo_digests.unwrap_or_default(),
        })
    }
}

impl From<SpecError> for DockerError {
    fn from(value: SpecError) -> Self {
        DockerError::Spec(value)
    }
}
