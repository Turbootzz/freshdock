use super::DockerError;

/// Maximum number of suffix attempts when renaming an old container collides.
/// Beyond this we surface an error rather than spin.
const MAX_COLLISION_ATTEMPTS: u32 = 64;

/// Format a container's archive name from its original name and a Unix
/// timestamp. The convention is `<original>-old-<ts>`. Phase 3 rollback
/// reuses this exact helper to find the archived container.
pub fn old_name_for(original: &str, ts_unix: i64) -> String {
    format!("{original}-old-{ts_unix}")
}

/// Does `name` look like an archive produced by [`old_name_for`] —
/// `<name>-old-<ts>`, or the `<name>-old-<ts>-<n>` form
/// [`next_available_old_name`] falls back to on a collision? Archives are
/// stopped (so normally absent from `list_running`); callers use this
/// defensively against a stale archive left running by a crashed cycle.
///
/// This is a *heuristic on a name*, not proof: a user container legitimately
/// called `redis-old-6` matches. Only use it where a false positive is
/// harmless (the scheduler simply declines to update such a container).
pub fn is_archive_name(name: &str) -> bool {
    let Some((_, tail)) = name.rsplit_once("-old-") else {
        return false;
    };
    match tail.split_once('-') {
        Some((ts, seq)) => is_all_digits(ts) && is_all_digits(seq),
        None => is_all_digits(tail),
    }
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Resolve a non-colliding `<original>-old-<ts>` archive name. If the base
/// name is already taken, append `-1`, `-2`, … up to
/// [`MAX_COLLISION_ATTEMPTS`]. The `exists` callback abstracts the daemon
/// lookup so this stays pure-function and unit-testable.
pub fn next_available_old_name(
    original: &str,
    ts_unix: i64,
    exists: impl Fn(&str) -> bool,
) -> String {
    let base = old_name_for(original, ts_unix);
    if !exists(&base) {
        return base;
    }
    for n in 1..=MAX_COLLISION_ATTEMPTS {
        let candidate = format!("{base}-{n}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    // Extremely unlikely (would mean 64 archived copies for the same
    // second). Returning the last attempted name surfaces the collision
    // upstream when the rename API call rejects it, rather than silently
    // overwriting.
    format!("{base}-{MAX_COLLISION_ATTEMPTS}")
}

impl super::Docker {
    /// Rename a running container to its `<name>-old-<ts>` archive form,
    /// avoiding collisions via [`next_available_old_name`]. Returns the new
    /// name so the caller can pass it to a later removal/rollback step.
    ///
    /// **TOCTOU caveat.** There is an inherent race window between the
    /// per-candidate "does this name exist?" probe and the eventual
    /// rename API call: another caller (or a separate freshdock instance)
    /// could create a container with the chosen name in between. In that
    /// case the rename returns a daemon error rather than silently
    /// overwriting; we propagate it as `DockerError::Bollard`. In a
    /// single-host homelab the practical risk is negligible — if you do
    /// hit it, retry the recreate. Phase 3 may add a typed
    /// `RenameConflict` variant + automatic retry once we understand how
    /// bollard surfaces the daemon's 409.
    pub async fn rename_to_old(&self, original: &str, ts_unix: i64) -> Result<String, DockerError> {
        let new_name = {
            let docker = self.0.clone();
            next_available_old_name_async(original, ts_unix, |candidate| {
                let docker = docker.clone();
                let candidate = candidate.to_owned();
                async move { docker.inspect_container(&candidate, None).await.is_ok() }
            })
            .await
        };
        let opts = bollard::query_parameters::RenameContainerOptionsBuilder::new()
            .name(&new_name)
            .build();
        self.0.rename_container(original, opts).await?;
        Ok(new_name)
    }
}

/// Async sibling of [`next_available_old_name`] for the real-daemon case
/// where the existence check is itself an async API call.
async fn next_available_old_name_async<F, Fut>(original: &str, ts_unix: i64, exists: F) -> String
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let base = old_name_for(original, ts_unix);
    if !exists(&base).await {
        return base;
    }
    for n in 1..=MAX_COLLISION_ATTEMPTS {
        let candidate = format!("{base}-{n}");
        if !exists(&candidate).await {
            return candidate;
        }
    }
    format!("{base}-{MAX_COLLISION_ATTEMPTS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names_are_detected() {
        assert!(is_archive_name("web-old-1700000000"));
        assert!(is_archive_name(&old_name_for("web", 1_700_000_000)));
        assert!(!is_archive_name("web"));
        assert!(!is_archive_name("web-old-")); // no timestamp
        assert!(!is_archive_name("my-old-laptop")); // non-numeric suffix
    }

    #[test]
    fn collision_suffixed_archive_names_are_detected() {
        // The `-<n>` form `next_available_old_name` produces on a collision is
        // just as much an archive as the base name.
        assert!(is_archive_name("web-old-1700000000-1"));
        assert!(is_archive_name("web-old-1700000000-64"));
        assert!(!is_archive_name("web-old-1700000000-1-2")); // not a shape we emit
        assert!(!is_archive_name("web-old-1700000000-x"));
        assert!(!is_archive_name("web-old-1700000000-")); // dangling separator
    }

    #[test]
    fn every_name_next_available_can_return_is_recognised_as_an_archive() {
        // Round-trip guard: the producer and the recogniser must not drift.
        let taken = [
            "web-old-1700000000".to_owned(),
            "web-old-1700000000-1".to_owned(),
        ];
        let name = next_available_old_name("web", 1_700_000_000, |c| taken.iter().any(|t| t == c));
        assert_eq!(name, "web-old-1700000000-2");
        assert!(is_archive_name(&name));
    }
}
