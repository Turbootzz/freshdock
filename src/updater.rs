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
}
