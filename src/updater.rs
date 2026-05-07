/// Outcome of a recreate attempt. Phase 3 will extend this with `RolledBack`
/// once health gating + rollback land; the variant set is intentionally open
/// so callers can `match` exhaustively without restructuring later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecreateOutcome {
    /// The container was successfully recreated. The old instance has been
    /// renamed to `old_name` and is still on the host (Phase 3 will remove
    /// it on success); the new instance is running with id `new_id`.
    Recreated { old_name: String, new_id: String },
}
