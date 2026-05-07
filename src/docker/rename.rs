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
