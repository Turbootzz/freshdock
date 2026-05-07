use async_trait::async_trait;

use super::DockerError;
use super::spec::ContainerSpec;
use crate::registry::ImageRef;
use crate::updater::RecreateOutcome;

/// Daemon operations the recreate orchestrator depends on. Abstracted as a
/// trait so unit tests can substitute a recording fake without spinning up
/// a real Docker socket.
#[async_trait]
pub trait DockerOps {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError>;
    async fn pull(&self, image_ref: &ImageRef) -> Result<String, DockerError>;
    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError>;
    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError>;
    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError>;
    async fn start(&self, name_or_id: &str) -> Result<(), DockerError>;
}

/// Drive one container through the recreate cycle:
/// `inspect → pull → stop → rename → create → start`.
///
/// Health gating, removal of the `-old-` container, and rollback on failure
/// are explicitly **out of scope** for Phase 2 (per [docs/PLAN.md] §7) — they
/// land in Phase 3.
pub async fn recreate_one(
    ops: &impl DockerOps,
    name: &str,
    ts_provider: impl Fn() -> i64,
) -> Result<RecreateOutcome, DockerError> {
    let spec = ops.inspect(name).await?;
    let image_ref = ImageRef::parse(&spec.image_ref);
    let new_image = ops.pull(&image_ref).await?;
    ops.stop(
        name,
        spec.config.stop_signal.as_deref(),
        spec.config.stop_timeout,
    )
    .await?;
    let old_name = ops.rename(name, ts_provider()).await?;
    let new_id = ops.create_from_spec(name, &spec, &new_image).await?;
    ops.start(&new_id).await?;
    Ok(RecreateOutcome::Recreated { old_name, new_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Recording fake that captures the sequence of `DockerOps` calls so the
    /// test can assert the orchestrator visits each step in the right order.
    #[derive(Default)]
    struct RecordingOps {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingOps {
        fn record(&self, label: &str) {
            self.calls.lock().unwrap().push(label.to_owned());
        }

        fn into_calls(self) -> Vec<String> {
            self.calls.into_inner().unwrap()
        }
    }

    #[async_trait]
    impl DockerOps for RecordingOps {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            self.record("inspect");
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: "nginx:alpine".to_owned(),
                config: bollard::models::ContainerConfig::default(),
                host_config: None,
                network_endpoints: None,
            })
        }

        async fn pull(&self, _image_ref: &ImageRef) -> Result<String, DockerError> {
            self.record("pull");
            Ok("nginx@sha256:beef".to_owned())
        }

        async fn stop(
            &self,
            _name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            self.record("stop");
            Ok(())
        }

        async fn rename(&self, _name: &str, _ts_unix: i64) -> Result<String, DockerError> {
            self.record("rename");
            Ok("fd-smoke-old-1700000000".to_owned())
        }

        async fn create_from_spec(
            &self,
            _name: &str,
            _spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            self.record("create");
            Ok("new-id".to_owned())
        }

        async fn start(&self, _name_or_id: &str) -> Result<(), DockerError> {
            self.record("start");
            Ok(())
        }
    }

    #[tokio::test]
    async fn recreate_one_visits_steps_in_canonical_order() {
        let ops = RecordingOps::default();
        let outcome = recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            outcome,
            RecreateOutcome::Recreated {
                old_name: "fd-smoke-old-1700000000".to_owned(),
                new_id: "new-id".to_owned(),
            }
        );
        assert_eq!(
            ops.into_calls(),
            vec![
                "inspect".to_owned(),
                "pull".to_owned(),
                "stop".to_owned(),
                "rename".to_owned(),
                "create".to_owned(),
                "start".to_owned(),
            ],
            "the orchestrator must drive operations in this exact order — \
             reordering breaks the safety contract (e.g. starting before \
             rename would race the old container)"
        );
    }
}
