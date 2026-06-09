use async_trait::async_trait;
use tracing::{debug, warn};

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
    /// Remove an image by id/digest. Cleanup passes `force=false` so the daemon
    /// refuses (409) an image still referenced by another container — that
    /// refusal is the guard against deleting a shared base image.
    async fn remove_image(&self, id: &str, force: bool) -> Result<(), DockerError>;
    /// Daemon-wide prune of dangling (untagged) images.
    async fn prune_dangling_images(&self) -> Result<(), DockerError>;
}

/// Post-update image cleanup, off by default (PLAN §5.2 step 8). Both steps are
/// best-effort: a failure is logged and the update still succeeds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cleanup {
    /// Remove the specific image the replaced container was running. Resolved
    /// per-container (the `freshdock.cleanup` label / global default).
    pub remove_replaced: bool,
    /// Additionally run a daemon-wide dangling-image prune. Global-only.
    pub prune_dangling: bool,
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
    /// Local image **ID** the replaced container ran (for cleanup). Captured at
    /// the pre-pull inspect; `None` when the daemon reported no image id.
    pub old_image_id: Option<String>,
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
        old_image_id: spec.image_id,
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
    cleanup: Cleanup,
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
            run_cleanup(ops, cleanup, cycle.old_image_id.as_deref()).await;
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

/// Post-success image cleanup. Runs only after the new container is healthy and
/// the old-container archive has been removed (so the superseded image is no
/// longer referenced by it). Every step is best-effort: a failure — notably a
/// 409 from removing an image still used by another container, which is the
/// desired guard — is logged and swallowed, never failing the completed update.
/// A 409 from `remove_image` means another container still references the image
/// — the intended guard against deleting a shared base, not a real failure. Any
/// other error (network, daemon, not-found) is a genuine cleanup error.
fn is_image_in_use(e: &DockerError) -> bool {
    matches!(
        e,
        DockerError::Bollard(bollard::errors::Error::DockerResponseServerError {
            status_code: 409,
            ..
        })
    )
}

async fn run_cleanup(ops: &impl DockerOps, cleanup: Cleanup, old_image_id: Option<&str>) {
    if cleanup.remove_replaced {
        match old_image_id {
            Some(id) => {
                if let Err(e) = ops.remove_image(id, false).await {
                    // Distinguish the expected "still in use" guard from a real
                    // failure so the log reflects what actually happened.
                    if is_image_in_use(&e) {
                        warn!(image = %id, "superseded image still in use by another container; leaving it in place");
                    } else {
                        warn!(image = %id, error = %e, "failed to remove superseded image; leaving it in place");
                    }
                }
            }
            // No resolved image id (locally-built image, or the daemon omitted
            // it) — nothing safe to target.
            None => debug!(
                "cleanup requested but the replaced image id is unknown; skipping image removal"
            ),
        }
    }
    if cleanup.prune_dangling
        && let Err(e) = ops.prune_dangling_images().await
    {
        warn!(error = %e, "update applied but the dangling-image prune failed; continuing");
    }
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
        /// When set, `remove_image` errors — exercises the best-effort contract
        /// (a cleanup failure must not fail the update).
        image_remove_fails: bool,
        /// When set, `prune_dangling_images` errors — same best-effort contract
        /// for the prune path.
        prune_fails: bool,
        /// When set, `inspect` reports no image id — exercises the safe-skip
        /// path (cleanup requested but nothing safe to target).
        omit_image_id: bool,
    }

    impl RecordingOps {
        fn with_probe(states: &[ContainerRuntimeState]) -> Self {
            Self {
                probe: Mutex::new(states.iter().copied().collect()),
                ..Default::default()
            }
        }

        fn with_failing_image_remove() -> Self {
            Self {
                image_remove_fails: true,
                ..Default::default()
            }
        }

        fn with_failing_prune() -> Self {
            Self {
                prune_fails: true,
                ..Default::default()
            }
        }

        fn without_image_id() -> Self {
            Self {
                omit_image_id: true,
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
                image_id: (!self.omit_image_id).then(|| "sha256:oldimg".to_owned()),
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

        async fn remove_image(&self, id: &str, force: bool) -> Result<(), DockerError> {
            self.record(format!("remove_image:{id}:{force}"));
            if self.image_remove_fails {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "image-remove",
                )));
            }
            Ok(())
        }

        async fn prune_dangling_images(&self) -> Result<(), DockerError> {
            self.record("prune_dangling_images".to_owned());
            if self.prune_fails {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "prune",
                )));
            }
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
                old_image_id: Some("sha256:oldimg".to_owned()),
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
            Cleanup::default(),
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
            "a healthy gate must remove the archive (by name, without force); \
             with cleanup off, no image is touched"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recreate_with_health_removes_old_image_when_cleanup_enabled() {
        use crate::health::TokioClock;

        let ops = RecordingOps::default();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup {
                remove_replaced: true,
                prune_dangling: false,
            },
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
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
                // The replaced image is removed only AFTER the archive container
                // (which referenced it) is gone, by id, without force.
                "remove_image:sha256:oldimg:false".to_owned(),
            ],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recreate_with_health_prunes_dangling_when_enabled() {
        use crate::health::TokioClock;

        let ops = RecordingOps::default();
        recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup {
                remove_replaced: true,
                prune_dangling: true,
            },
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        let calls = ops.into_calls();
        assert_eq!(
            calls.last().map(String::as_str),
            Some("prune_dangling_images"),
            "the dangling prune runs last, after the targeted image removal"
        );
        assert!(calls.contains(&"remove_image:sha256:oldimg:false".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn prune_failure_does_not_fail_the_update() {
        use crate::health::TokioClock;

        // The dangling prune errors, but the update already succeeded — the
        // outcome must still be `Recreated` (best-effort contract, prune path).
        let ops = RecordingOps::with_failing_prune();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup {
                remove_replaced: false,
                prune_dangling: true,
            },
            || 1_700_000_000,
        )
        .await
        .expect("a prune failure must not surface as a recreate error");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_with_no_image_id_skips_image_removal_safely() {
        use crate::health::TokioClock;

        // No resolved image id (e.g. a locally-built image): cleanup is on but
        // there is nothing safe to target, so remove_image must NOT be called —
        // and the update still succeeds.
        let ops = RecordingOps::without_image_id();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup {
                remove_replaced: true,
                prune_dangling: false,
            },
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
        assert!(
            !ops.into_calls()
                .iter()
                .any(|c| c.starts_with("remove_image:")),
            "with no image id, no image removal must be attempted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_failure_does_not_fail_the_update() {
        use crate::health::TokioClock;

        // remove_image errors, but the update already succeeded — the outcome
        // must still be `Recreated` (best-effort cleanup contract).
        let ops = RecordingOps::with_failing_image_remove();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup {
                remove_replaced: true,
                prune_dangling: false,
            },
            || 1_700_000_000,
        )
        .await
        .expect("a cleanup failure must not surface as a recreate error");

        assert!(
            matches!(outcome, RecreateOutcome::Recreated { .. }),
            "a failed image removal must not turn a healthy update into a failure"
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
            Cleanup::default(),
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
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &fast_cfg(),
            &TokioClock,
            Cleanup::default(),
            || 1_700_000_000,
        )
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
