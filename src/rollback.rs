//! Rollback on a failed update (Phase 3, P3-2).
//!
//! When the health gate ([`crate::health`]) returns `Timeout` or `Crashed`,
//! [`rollback`] undoes the recreate: stop + force-remove the new container,
//! rename the archived `<name>-old-<ts>` container back to the original name,
//! and start it. It returns a structured [`RollbackEvent`] describing what
//! happened — Phase 6 notifications will consume this; **no notifier is wired
//! in here**.

use tracing::{debug, error, warn};

use crate::docker::DockerError;
use crate::docker::recreate::DockerOps;

/// Why an update was rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReason {
    /// A healthcheck was declared but never reached `healthy` in time.
    HealthTimeout,
    /// The new container exited before becoming healthy.
    Crashed,
}

/// Structured record of a completed rollback. Pure data — a later phase formats
/// it into a notification body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackEvent {
    /// The (restored) container's stable name.
    pub container: String,
    /// Why the new instance was rejected.
    pub reason: RollbackReason,
    /// Image ref the container ran before the update (now restored).
    pub old_image_ref: String,
    /// Image ref the rejected new instance was created from.
    pub new_image_ref: String,
    /// Archive name we renamed back from (e.g. `web-old-1700000000`).
    pub restored_from: String,
}

/// Roll a failed update back to the archived container.
///
/// `images` is `(old_image_ref, new_image_ref)`. `old_name` is the archive
/// name produced by the recreate cycle (`<original>-old-<ts>`) — it is threaded
/// in, not recomputed, so no timestamp-naming logic is duplicated here.
pub async fn rollback(
    ops: &impl DockerOps,
    original_name: &str,
    new_id: &str,
    old_name: &str,
    images: (&str, &str),
    reason: RollbackReason,
) -> Result<RollbackEvent, DockerError> {
    let (old_image_ref, new_image_ref) = images;
    warn!(
        container = %original_name,
        ?reason,
        new = %new_id,
        archived = %old_name,
        "health gate failed — rolling back to the previous container"
    );

    // Restore (remove → rename_to → start) is not atomic: an error mid-way
    // leaves a partial state for the operator to resolve. Retry is intentionally
    // not attempted on a single-host manual tool.

    // 1. Stop the new container (best-effort — it may already be dead), then
    //    force-remove it so the original name is free for the archive.
    if let Err(e) = ops.stop(new_id, None, None).await {
        debug!(new = %new_id, error = %e, "stop of failed container errored; continuing to force-remove");
    }
    ops.remove(new_id, true).await?;

    // 2. Restore the archive: rename it back, then start it.
    ops.rename_to(old_name, original_name).await?;
    ops.start(original_name).await?;

    let event = RollbackEvent {
        container: original_name.to_owned(),
        reason,
        old_image_ref: old_image_ref.to_owned(),
        new_image_ref: new_image_ref.to_owned(),
        restored_from: old_name.to_owned(),
    };
    error!(
        container = %event.container,
        ?reason,
        old_image = %event.old_image_ref,
        new_image = %event.new_image_ref,
        "rolled back: restored previous container after a failed update"
    );
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::spec::ContainerSpec;
    use crate::registry::ImageRef;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records the mutating calls rollback makes, in order. `stop_fails`
    /// simulates a dead/unresponsive new container so the best-effort stop
    /// branch can be exercised.
    #[derive(Default)]
    struct RollbackRecorder {
        calls: Mutex<Vec<String>>,
        stop_fails: bool,
    }

    impl RollbackRecorder {
        fn into_calls(self) -> Vec<String> {
            self.calls.into_inner().unwrap()
        }
    }

    fn probe_err() -> DockerError {
        DockerError::Spec(crate::docker::spec::SpecError::Missing("stop"))
    }

    #[async_trait]
    impl DockerOps for RollbackRecorder {
        async fn inspect(&self, _name: &str) -> Result<ContainerSpec, DockerError> {
            unreachable!("rollback never inspects")
        }
        async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
            unreachable!("rollback never pulls")
        }
        async fn stop(
            &self,
            name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            if self.stop_fails {
                return Err(probe_err());
            }
            Ok(())
        }
        async fn rename(&self, _name: &str, _ts_unix: i64) -> Result<String, DockerError> {
            unreachable!("rollback never creates archive names")
        }
        async fn create_from_spec(
            &self,
            _name: &str,
            _spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            unreachable!("rollback never creates")
        }
        async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start:{name_or_id}"));
            Ok(())
        }
        async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove:{name_or_id}:{force}"));
            Ok(())
        }
        async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rename_to:{from}->{to}"));
            Ok(())
        }
        async fn remove_image(&self, _id: &str, _force: bool) -> Result<(), DockerError> {
            unreachable!(
                "rollback must never remove an image — the restored container still needs it"
            )
        }
        async fn prune_dangling_images(&self) -> Result<(), DockerError> {
            unreachable!("rollback never prunes images")
        }
    }

    #[tokio::test]
    async fn rollback_runs_stop_remove_rename_start_in_order() {
        let ops = RollbackRecorder::default();
        let event = rollback(
            &ops,
            "web",
            "new-id",
            "web-old-1700000000",
            ("nginx:1.27", "nginx:1.28"),
            RollbackReason::Crashed,
        )
        .await
        .unwrap();

        assert_eq!(
            ops.into_calls(),
            vec![
                "stop:new-id".to_owned(),
                "remove:new-id:true".to_owned(),
                "rename_to:web-old-1700000000->web".to_owned(),
                "start:web".to_owned(),
            ],
            "rollback must force-remove the new container before restoring the archive"
        );

        assert_eq!(
            event,
            RollbackEvent {
                container: "web".to_owned(),
                reason: RollbackReason::Crashed,
                old_image_ref: "nginx:1.27".to_owned(),
                new_image_ref: "nginx:1.28".to_owned(),
                restored_from: "web-old-1700000000".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn timeout_reason_is_carried_into_event() {
        let ops = RollbackRecorder::default();
        let event = rollback(
            &ops,
            "db",
            "id2",
            "db-old-42",
            ("pg:16", "pg:17"),
            RollbackReason::HealthTimeout,
        )
        .await
        .unwrap();
        assert_eq!(event.reason, RollbackReason::HealthTimeout);
    }

    #[tokio::test]
    async fn rollback_proceeds_even_if_stop_errors() {
        // A crashed/dead new container can't be stopped cleanly; rollback must
        // still force-remove it and restore the archive.
        let ops = RollbackRecorder {
            stop_fails: true,
            ..Default::default()
        };
        let event = rollback(
            &ops,
            "web",
            "new-id",
            "web-old-7",
            ("nginx:1.27", "nginx:1.28"),
            RollbackReason::Crashed,
        )
        .await
        .expect("a failed stop must not abort the rollback");

        assert_eq!(
            ops.into_calls(),
            vec![
                "stop:new-id".to_owned(),
                "remove:new-id:true".to_owned(),
                "rename_to:web-old-7->web".to_owned(),
                "start:web".to_owned(),
            ],
        );
        assert_eq!(event.restored_from, "web-old-7");
    }
}
