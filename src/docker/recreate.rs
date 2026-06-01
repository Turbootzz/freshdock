use async_trait::async_trait;
use tracing::warn;

use super::DockerError;
use super::spec::ContainerSpec;
use crate::health::{Clock, HealthConfig, HealthOutcome, HealthProbe, wait_for_health};
use crate::registry::ImageRef;
use crate::rollback::{RollbackReason, rollback};
use crate::updater::RecreateOutcome;

/// Daemon operations the recreate orchestrator depends on. Abstracted as a
/// trait so unit tests can substitute a recording fake without spinning up
/// a real Docker socket.
#[async_trait]
pub trait DockerOps {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError>;
    async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError>;
    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError>;
    /// Rename a live container to its `<name>-old-<ts>` archive form (computes
    /// a collision-free archive name). Returns the chosen archive name.
    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError>;
    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError>;
    async fn start(&self, name_or_id: &str) -> Result<(), DockerError>;
    /// Remove a container by name or id. `force` SIGKILLs a still-running
    /// container first — rollback removes the *running* new instance.
    async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError>;
    /// Generic `from → to` rename with no archive-naming logic. Rollback uses
    /// it to move `<name>-old-<ts>` back to the original name; distinct from
    /// [`rename`](DockerOps::rename), which *creates* the archive name.
    async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError>;
}

/// Raw result of the recreate cycle — richer than [`RecreateOutcome`] because
/// the rollback path needs both image refs to build its event. The high-level
/// command outcome is derived from this in [`recreate_with_health`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleResult {
    /// Archive name the old container was renamed to (`<name>-old-<ts>`).
    pub old_name: String,
    /// Id of the freshly started replacement container.
    pub new_id: String,
    /// Image ref the container ran *before* the update (for rollback events).
    pub old_image_ref: String,
    /// Image ref the replacement was created from.
    pub new_image_ref: String,
}

/// Drive one container through the recreate cycle:
/// `inspect → pull → stop → rename → create → start`.
///
/// This is the **pure cycle** only: health gating, removal of the `-old-`
/// container, and rollback on failure are layered on top by
/// [`recreate_with_health`].
pub async fn recreate_one(
    ops: &impl DockerOps,
    name: &str,
    ts_provider: impl Fn() -> i64,
) -> Result<CycleResult, DockerError> {
    let spec = ops.inspect(name).await?;
    // Pull uses the `library/`-prefixed parse (registry-correct), but create
    // uses the original `spec.image_ref` so `Config.Image` round-trips
    // byte-identical (issue #25). In Phase 3 the image *ref* is unchanged
    // across the update (only the digest moves, which we don't pin yet), so the
    // rollback event's old/new refs are the same string.
    let image_ref = ImageRef::parse(&spec.image_ref);
    ops.pull(&image_ref).await?;
    ops.stop(
        name,
        spec.config.stop_signal.as_deref(),
        spec.config.stop_timeout,
    )
    .await?;
    let old_name = ops.rename(name, ts_provider()).await?;
    let new_id = ops.create_from_spec(name, &spec, &spec.image_ref).await?;
    ops.start(&new_id).await?;
    Ok(CycleResult {
        old_name,
        new_id,
        old_image_ref: spec.image_ref.clone(),
        new_image_ref: spec.image_ref,
    })
}

/// The full Phase-3 update: run the recreate cycle, then health-gate the new
/// container. On success the archived `-old-` container is removed; on
/// `Timeout`/`Crashed` the update is rolled back to it. Returns the
/// command-facing [`RecreateOutcome`].
pub async fn recreate_with_health(
    ops: &(impl DockerOps + HealthProbe),
    name: &str,
    cfg: &HealthConfig,
    clock: &impl Clock,
    ts_provider: impl Fn() -> i64,
) -> Result<RecreateOutcome, DockerError> {
    let cycle = recreate_one(ops, name, ts_provider).await?;

    // `wait_for_health` always returns a verdict (it tolerates transient probe
    // errors), so a blip can't strand a half-recreated container; a persistent
    // failure becomes `Timeout` → rollback.
    let reason = match wait_for_health(ops, &cycle.new_id, cfg, clock).await {
        HealthOutcome::Healthy => {
            // The healthy new container is the source of truth. Removing the
            // (already-stopped) archive is best-effort: failing it must not
            // report the whole update as failed.
            if let Err(e) = ops.remove(&cycle.old_name, false).await {
                warn!(archive = %cycle.old_name, error = %e, "new container healthy but failed to remove archived old container; remove it manually");
            }
            return Ok(RecreateOutcome::Recreated {
                old_name: cycle.old_name,
                new_id: cycle.new_id,
            });
        }
        HealthOutcome::Timeout => RollbackReason::HealthTimeout,
        HealthOutcome::Crashed => RollbackReason::Crashed,
    };

    let event = rollback(
        ops,
        name,
        &cycle.new_id,
        &cycle.old_name,
        (&cycle.old_image_ref, &cycle.new_image_ref),
        reason,
    )
    .await?;
    Ok(RecreateOutcome::RolledBack(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::ContainerRuntimeState;
    use std::sync::Mutex;

    use std::collections::VecDeque;

    /// Recording fake that captures the sequence of `DockerOps` calls (with the
    /// target name + force flag, so the success-vs-rollback removal contract is
    /// checkable) and the image handed to `create_from_spec` (issue-#25). The
    /// health probe replays a scripted sequence; an empty script is healthy.
    #[derive(Default)]
    struct RecordingOps {
        calls: Mutex<Vec<String>>,
        created_image: Mutex<Option<String>>,
        probe: Mutex<VecDeque<ContainerRuntimeState>>,
    }

    impl RecordingOps {
        fn with_probe(states: &[ContainerRuntimeState]) -> Self {
            Self {
                probe: Mutex::new(states.iter().copied().collect()),
                ..Default::default()
            }
        }

        fn record(&self, label: String) {
            self.calls.lock().unwrap().push(label);
        }

        fn created_image(&self) -> Option<String> {
            self.created_image.lock().unwrap().clone()
        }

        fn into_calls(self) -> Vec<String> {
            self.calls.into_inner().unwrap()
        }
    }

    #[async_trait]
    impl DockerOps for RecordingOps {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            self.record("inspect".to_owned());
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: "nginx:alpine".to_owned(),
                config: bollard::models::ContainerConfig::default(),
                host_config: None,
                network_endpoints: None,
            })
        }

        async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
            self.record("pull".to_owned());
            Ok(())
        }

        async fn stop(
            &self,
            name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            self.record(format!("stop:{name}"));
            Ok(())
        }

        async fn rename(&self, _name: &str, _ts_unix: i64) -> Result<String, DockerError> {
            self.record("rename".to_owned());
            Ok("fd-smoke-old-1700000000".to_owned())
        }

        async fn create_from_spec(
            &self,
            _name: &str,
            _spec: &ContainerSpec,
            image: &str,
        ) -> Result<String, DockerError> {
            self.record("create".to_owned());
            *self.created_image.lock().unwrap() = Some(image.to_owned());
            Ok("new-id".to_owned())
        }

        async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
            self.record(format!("start:{name_or_id}"));
            Ok(())
        }

        async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError> {
            self.record(format!("remove:{name_or_id}:{force}"));
            Ok(())
        }

        async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
            self.record(format!("rename_to:{from}->{to}"));
            Ok(())
        }
    }

    #[async_trait]
    impl HealthProbe for RecordingOps {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            self.record("probe_state".to_owned());
            let mut q = self.probe.lock().unwrap();
            let next = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front()
                    .copied()
                    .unwrap_or(ContainerRuntimeState::HealthHealthy)
            };
            Ok(next)
        }
    }

    #[tokio::test]
    async fn recreate_one_visits_steps_in_canonical_order() {
        let ops = RecordingOps::default();
        let cycle = recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            cycle,
            CycleResult {
                old_name: "fd-smoke-old-1700000000".to_owned(),
                new_id: "new-id".to_owned(),
                old_image_ref: "nginx:alpine".to_owned(),
                new_image_ref: "nginx:alpine".to_owned(),
            }
        );
        assert_eq!(
            ops.created_image().as_deref(),
            Some("nginx:alpine"),
            "the new container must be created from the original image ref, not \
             the `library/`-prefixed pull return (issue #25)"
        );
        assert_eq!(
            ops.into_calls(),
            vec![
                "inspect".to_owned(),
                "pull".to_owned(),
                "stop:fd-smoke".to_owned(),
                "rename".to_owned(),
                "create".to_owned(),
                "start:new-id".to_owned(),
            ],
            "the orchestrator must drive operations in this exact order — \
             reordering breaks the safety contract (e.g. starting before \
             rename would race the old container)"
        );
    }

    fn fast_cfg() -> HealthConfig {
        // Zero budgets so the unhealthy/timeout path resolves on the first poll.
        HealthConfig {
            health_timeout: std::time::Duration::ZERO,
            grace_period: std::time::Duration::ZERO,
            poll_interval: std::time::Duration::from_millis(1),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recreate_with_health_removes_archive_when_healthy() {
        use crate::health::TokioClock;

        let ops = RecordingOps::default();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            || 1_700_000_000,
        )
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
                "stop:fd-smoke".to_owned(),
                "rename".to_owned(),
                "create".to_owned(),
                "start:new-id".to_owned(),
                "probe_state".to_owned(),
                "remove:fd-smoke-old-1700000000:false".to_owned(),
            ],
            "a healthy gate must remove the archive (by name, without force)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recreate_with_health_rolls_back_when_crashed() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_probe(&[ContainerRuntimeState::Exited { exit_code: 1 }]);
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert_eq!(
            outcome,
            RecreateOutcome::RolledBack(crate::rollback::RollbackEvent {
                container: "fd-smoke".to_owned(),
                reason: RollbackReason::Crashed,
                old_image_ref: "nginx:alpine".to_owned(),
                new_image_ref: "nginx:alpine".to_owned(),
                restored_from: "fd-smoke-old-1700000000".to_owned(),
            })
        );
        assert_eq!(
            ops.into_calls(),
            vec![
                "inspect".to_owned(),
                "pull".to_owned(),
                "stop:fd-smoke".to_owned(),
                "rename".to_owned(),
                "create".to_owned(),
                "start:new-id".to_owned(),
                "probe_state".to_owned(),
                // rollback: force-remove the new container, restore the archive.
                "stop:new-id".to_owned(),
                "remove:new-id:true".to_owned(),
                "rename_to:fd-smoke-old-1700000000->fd-smoke".to_owned(),
                "start:fd-smoke".to_owned(),
            ],
            "a crashed gate must roll back, not remove the archive"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recreate_with_health_rolls_back_on_timeout() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_probe(&[ContainerRuntimeState::HealthUnhealthy]);
        let outcome =
            recreate_with_health(&ops, "fd-smoke", &fast_cfg(), &TokioClock, || 1_700_000_000)
                .await
                .expect("recording fake never errors");

        match outcome {
            RecreateOutcome::RolledBack(event) => {
                assert_eq!(event.reason, RollbackReason::HealthTimeout);
                assert_eq!(event.restored_from, "fd-smoke-old-1700000000");
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }
        let calls = ops.into_calls();
        assert!(
            calls.contains(&"remove:new-id:true".to_owned()),
            "the new container must be force-removed on timeout"
        );
        assert!(
            !calls.contains(&"remove:fd-smoke-old-1700000000:false".to_owned()),
            "the archive must be restored, never removed, on timeout"
        );
    }
}
