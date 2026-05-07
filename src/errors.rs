use crate::docker::DockerError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Docker(#[from] DockerError),
}
