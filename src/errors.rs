use crate::docker::DockerError;
use crate::labels::LabelError;
use crate::notify::NotifyError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(transparent)]
    Label(#[from] LabelError),
    #[error(transparent)]
    Notify(#[from] NotifyError),
}
