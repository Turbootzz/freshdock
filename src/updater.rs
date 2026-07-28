use std::fmt;

use crate::rollback::RollbackEvent;

/// Outcome of a recreate attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecreateOutcome {
    /// The new container passed the health gate. `old_name` is the archive the
    /// old instance was renamed to before being removed on success; the new
    /// instance is running with id `new_id`.
    Recreated { old_name: String, new_id: String },
    /// The new container failed the health gate and the previous container was
    /// restored. Carries the structured [`RollbackEvent`].
    RolledBack(RollbackEvent),
    /// The pre-update lifecycle hook did not succeed, so the update was
    /// skipped before touching the container (issue #61). Not an error: the
    /// container keeps running on its old image until a later cycle.
    SkippedByHook(HookSkipReason),
}

/// Why a pre-update hook caused the update to be skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSkipReason {
    /// The hook exited non-zero. `75` (`EX_TEMPFAIL`, watchtower-compatible)
    /// is the conventional "not now, retry later" signal.
    NonZeroExit(i64),
    /// The hook ran past its `freshdock.lifecycle.pre-update-timeout`.
    TimedOut,
    /// The exec could not be run at all (e.g. no `sh` in the image).
    ExecFailed(String),
}

impl fmt::Display for HookSkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookSkipReason::NonZeroExit(75) => {
                write!(f, "hook requested a deferral (exit code 75, EX_TEMPFAIL)")
            }
            HookSkipReason::NonZeroExit(code) => write!(f, "hook exited with code {code}"),
            HookSkipReason::TimedOut => f.write_str("hook timed out"),
            HookSkipReason::ExecFailed(e) => write!(f, "hook could not be executed: {e}"),
        }
    }
}
