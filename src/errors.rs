use crate::docker::DockerError;
use crate::labels::LabelError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(transparent)]
    Label(#[from] LabelError),
}
