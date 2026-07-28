use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::DockerError;
use super::spec::ContainerSpec;
use crate::health::{Clock, HealthConfig, HealthOutcome, HealthProbe, wait_for_health};
use crate::labels::{Hook, LifecycleHooks};
use crate::registry::ImageRef;
use crate::rollback::{RollbackReason, rollback};
use crate::updater::{HookSkipReason, RecreateOutcome};

/// Verdict of a lifecycle hook exec. `TimedOut` is a normal verdict, not an
/// error — an `Err` from [`DockerOps::exec_hook`] means the exec could not be
/// run at all (no `sh` in the image, daemon error, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStatus {
    Completed { exit_code: i64 },
    TimedOut,
}

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
    /// Run a lifecycle hook command inside a running container via `sh -c`,
    /// bounded by `timeout` (`None` = unlimited).
    async fn exec_hook(
        &self,
        name_or_id: &str,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<HookStatus, DockerError>;
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

/// Outcome of the pure recreate cycle: either it ran to completion, or the
/// pre-update hook refused and the container was left untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed(CycleResult),
    /// The pre-update hook did not succeed; nothing was stopped or replaced.
    Skipped(HookSkipReason),
}

/// Drive one container through the recreate cycle:
/// `inspect → pull → pre-update hook → stop → rename → create → start`.
///
/// The pre-update hook runs after the pull (image already local, so the
/// stopped window stays short) and before the stop (the app is still up to
/// answer the exec). Any hook failure skips the cycle — see
/// [`run_pre_update_hook`].
///
/// This is the **pure cycle** only: health gating, removal of the `-old-`
/// container, the post-update hook, and rollback on failure are layered on
/// top by [`recreate_with_health`].
pub async fn recreate_one(
    ops: &impl DockerOps,
    name: &str,
    hooks: &LifecycleHooks,
    ts_provider: impl Fn() -> i64,
) -> Result<CycleOutcome, DockerError> {
    let spec = ops.inspect(name).await?;
    // Pull uses the `library/`-prefixed parse (registry-correct), but create
    // uses the original `spec.image_ref` so `Config.Image` round-trips
    // byte-identical (issue #25). In Phase 3 the image *ref* is unchanged
    // across the update (only the digest moves, which we don't pin yet), so the
    // rollback event's old/new refs are the same string.
    let image_ref = ImageRef::parse(&spec.image_ref);
    ops.pull(&image_ref).await?;
    if let Some(reason) = run_pre_update_hook(ops, name, hooks.pre_update.as_ref()).await {
        return Ok(CycleOutcome::Skipped(reason));
    }
    ops.stop(
        name,
        spec.config.stop_signal.as_deref(),
        spec.config.stop_timeout,
    )
    .await?;
    let old_name = ops.rename(name, ts_provider()).await?;
    let new_id = ops.create_from_spec(name, &spec, &spec.image_ref).await?;
    ops.start(&new_id).await?;
    Ok(CycleOutcome::Completed(CycleResult {
        old_name,
        new_id,
        old_image_ref: spec.image_ref.clone(),
        new_image_ref: spec.image_ref,
        old_image_id: spec.image_id,
    }))
}

/// Run the pre-update hook in the *old* (still running) container. Returns
/// `Some(reason)` when the update must be skipped. The contract is
/// deliberately stricter than watchtower (which only skips on exit 75):
/// **any** failure — non-zero exit (75 = intentional "not now"), timeout, or
/// an exec that couldn't run — skips the update, because an app whose own
/// pre-hook couldn't confirm readiness must not be taken down.
async fn run_pre_update_hook(
    ops: &impl DockerOps,
    name: &str,
    hook: Option<&Hook>,
) -> Option<HookSkipReason> {
    let hook = hook?;
    info!(container = %name, command = %hook.command, "running pre-update hook");
    match ops.exec_hook(name, &hook.command, hook.timeout).await {
        Ok(HookStatus::Completed { exit_code: 0 }) => None,
        Ok(HookStatus::Completed { exit_code }) => Some(HookSkipReason::NonZeroExit(exit_code)),
        Ok(HookStatus::TimedOut) => Some(HookSkipReason::TimedOut),
        Err(e) => Some(HookSkipReason::ExecFailed(e.to_string())),
    }
}

/// Best-effort post-update hook in the *new* container. By this point the
/// update has already succeeded, so any failure is logged and swallowed —
/// mirroring the cleanup contract.
async fn run_post_update_hook(ops: &impl DockerOps, name: &str, new_id: &str, hook: Option<&Hook>) {
    let Some(hook) = hook else { return };
    info!(container = %name, command = %hook.command, "running post-update hook");
    match ops.exec_hook(new_id, &hook.command, hook.timeout).await {
        Ok(HookStatus::Completed { exit_code: 0 }) => {}
        Ok(HookStatus::Completed { exit_code }) => {
            warn!(container = %name, exit_code, "post-update hook exited non-zero; the update stands");
        }
        Ok(HookStatus::TimedOut) => {
            warn!(container = %name, "post-update hook timed out; the update stands");
        }
        Err(e) => {
            warn!(container = %name, error = %e, "post-update hook could not be executed; the update stands");
        }
    }
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
    hooks: &LifecycleHooks,
    ts_provider: impl Fn() -> i64,
) -> Result<RecreateOutcome, DockerError> {
    let cycle = match recreate_one(ops, name, hooks, ts_provider).await? {
        CycleOutcome::Completed(cycle) => cycle,
        CycleOutcome::Skipped(reason) => {
            // Exit 75 is the hook *asking* to defer — expected traffic, not a
            // problem; everything else is worth a warning.
            if reason == HookSkipReason::NonZeroExit(75) {
                info!(container = %name, %reason, "update deferred by pre-update hook");
            } else {
                warn!(container = %name, %reason, "update skipped by pre-update hook");
            }
            return Ok(RecreateOutcome::SkippedByHook(reason));
        }
    };

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
            // App maintenance first, image housekeeping last (so the dangling
            // prune stays the final step of a successful update).
            run_post_update_hook(ops, name, &cycle.new_id, hooks.post_update.as_ref()).await;
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
        /// Verdict `exec_hook` returns (`None` = exited 0).
        hook_status: Option<HookStatus>,
        /// When set, `exec_hook` errors — the exec-transport-failure path.
        exec_fails: bool,
    }

    impl RecordingOps {
        fn with_probe(states: &[ContainerRuntimeState]) -> Self {
            Self {
                probe: Mutex::new(states.iter().copied().collect()),
                ..Default::default()
            }
        }

        fn with_hook_status(status: HookStatus) -> Self {
            Self {
                hook_status: Some(status),
                ..Default::default()
            }
        }

        fn with_failing_exec() -> Self {
            Self {
                exec_fails: true,
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

        async fn exec_hook(
            &self,
            name_or_id: &str,
            command: &str,
            _timeout: Option<Duration>,
        ) -> Result<HookStatus, DockerError> {
            self.record(format!("exec_hook:{name_or_id}:{command}"));
            if self.exec_fails {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "exec",
                )));
            }
            Ok(self
                .hook_status
                .unwrap_or(HookStatus::Completed { exit_code: 0 }))
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
        let cycle = recreate_one(&ops, "fd-smoke", &LifecycleHooks::default(), || {
            1_700_000_000
        })
        .await
        .expect("recording fake never errors");

        assert_eq!(
            cycle,
            CycleOutcome::Completed(CycleResult {
                old_name: "fd-smoke-old-1700000000".to_owned(),
                new_id: "new-id".to_owned(),
                old_image_ref: "nginx:alpine".to_owned(),
                new_image_ref: "nginx:alpine".to_owned(),
                old_image_id: Some("sha256:oldimg".to_owned()),
            })
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
            || 1_700_000_000,
        )
        .await
        .expect("a cleanup failure must not surface as a recreate error");

        assert!(
            matches!(outcome, RecreateOutcome::Recreated { .. }),
            "a failed image removal must not turn a healthy update into a failure"
        );
    }

    fn hooks(pre: Option<&str>, post: Option<&str>) -> LifecycleHooks {
        let mk = |c: &str| Hook {
            command: c.to_owned(),
            timeout: Some(std::time::Duration::from_secs(60)),
        };
        LifecycleHooks {
            pre_update: pre.map(mk),
            post_update: post.map(mk),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pre_update_hook_runs_between_pull_and_stop() {
        use crate::health::TokioClock;

        let ops = RecordingOps::default();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(Some("/app/drain.sh"), None),
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
        assert_eq!(
            &ops.into_calls()[..4],
            &[
                "inspect".to_owned(),
                "pull".to_owned(),
                "exec_hook:fd-smoke:/app/drain.sh".to_owned(),
                "stop:fd-smoke".to_owned(),
            ],
            "the pre-update hook must run in the OLD container, after the pull \
             (image ready) and before the stop (app still up)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pre_update_hook_nonzero_exit_skips_update() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_hook_status(HookStatus::Completed { exit_code: 1 });
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(Some("/app/drain.sh"), None),
            || 1_700_000_000,
        )
        .await
        .expect("a refused hook is a graceful skip, not an error");

        assert_eq!(
            outcome,
            RecreateOutcome::SkippedByHook(HookSkipReason::NonZeroExit(1))
        );
        assert_eq!(
            ops.into_calls(),
            vec![
                "inspect".to_owned(),
                "pull".to_owned(),
                "exec_hook:fd-smoke:/app/drain.sh".to_owned(),
            ],
            "after a refused pre-update hook the container must be left untouched"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pre_update_hook_timeout_skips_update() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_hook_status(HookStatus::TimedOut);
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(Some("/app/drain.sh"), None),
            || 1_700_000_000,
        )
        .await
        .expect("a timed-out hook is a graceful skip, not an error");

        assert_eq!(
            outcome,
            RecreateOutcome::SkippedByHook(HookSkipReason::TimedOut)
        );
        assert!(
            !ops.into_calls().iter().any(|c| c.starts_with("stop:")),
            "a timed-out pre-update hook must not stop the container"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pre_update_hook_exec_error_skips_update() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_failing_exec();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(Some("/app/drain.sh"), None),
            || 1_700_000_000,
        )
        .await
        .expect("an exec transport error skips the update rather than failing it");

        assert!(matches!(
            outcome,
            RecreateOutcome::SkippedByHook(HookSkipReason::ExecFailed(_))
        ));
        assert!(!ops.into_calls().iter().any(|c| c.starts_with("stop:")));
    }

    #[tokio::test(start_paused = true)]
    async fn post_update_hook_runs_in_new_container_after_archive_removal() {
        use crate::health::TokioClock;

        let ops = RecordingOps::default();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(None, Some("php artisan cache:clear")),
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
        assert_eq!(
            ops.into_calls().last().map(String::as_str),
            Some("exec_hook:new-id:php artisan cache:clear"),
            "the post-update hook must run in the NEW container, after the \
             health gate and archive removal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn post_update_hook_failure_does_not_fail_update() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_hook_status(HookStatus::Completed { exit_code: 2 });
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(None, Some("php artisan cache:clear")),
            || 1_700_000_000,
        )
        .await
        .expect("a failed post-update hook must not surface as an error");

        assert!(
            matches!(outcome, RecreateOutcome::Recreated { .. }),
            "post-update hooks are best-effort — the update already succeeded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn post_update_exec_error_does_not_fail_update() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_failing_exec();
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(None, Some("php artisan cache:clear")),
            || 1_700_000_000,
        )
        .await
        .expect("a post-update exec error must not surface as an error");

        assert!(matches!(outcome, RecreateOutcome::Recreated { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn rollback_skips_post_update_hook() {
        use crate::health::TokioClock;

        let ops = RecordingOps::with_probe(&[ContainerRuntimeState::Exited { exit_code: 1 }]);
        let outcome = recreate_with_health(
            &ops,
            "fd-smoke",
            &HealthConfig::default(),
            &TokioClock,
            Cleanup::default(),
            &hooks(None, Some("php artisan cache:clear")),
            || 1_700_000_000,
        )
        .await
        .expect("recording fake never errors");

        assert!(matches!(outcome, RecreateOutcome::RolledBack(_)));
        assert!(
            !ops.into_calls()
                .iter()
                .any(|c| c.starts_with("exec_hook:new-id")),
            "the update did not happen — the post-update hook must not run"
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
            &LifecycleHooks::default(),
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
            &LifecycleHooks::default(),
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
