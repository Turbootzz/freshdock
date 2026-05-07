pub mod inspect;

use bollard::models::ContainerSummary;
use bollard::query_parameters::ListContainersOptions;

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("docker daemon error: {0}")]
    Bollard(#[from] bollard::errors::Error),
}

pub struct Docker(bollard::Docker);

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
}
