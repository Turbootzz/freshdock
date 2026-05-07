use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::docker::Docker;
use crate::docker::recreate::recreate_one;
use crate::errors::AppError;
use crate::labels::{self, Mode};
use crate::updater::RecreateOutcome;

/// Recreate a single container by name: inspect → pull → stop → rename →
/// create → start. Health gating, removal of the archived `-old-` instance,
/// and rollback on failure are Phase 3 work and explicitly *not* run here.
pub async fn run(name: String) -> Result<(), AppError> {
    let docker = Docker::connect()?;
    let spec = docker.inspect_container_spec(&name).await?;

    let policy = labels::parse_policy(
        spec.config.labels.as_ref().unwrap_or(&Default::default()),
        None,
    )?;
    if !policy.enabled || policy.mode == Mode::Off {
        warn!(
            container = %name,
            mode = %policy.mode,
            enabled = policy.enabled,
            "refusing to recreate a container that did not opt in to freshdock"
        );
        return Ok(());
    }

    let outcome = recreate_one(&docker, &name, current_unix_timestamp).await?;
    let RecreateOutcome::Recreated { old_name, new_id } = outcome;
    info!(
        container = %name,
        archived_as = %old_name,
        new_id = %new_id,
        "recreate complete — old container is preserved (Phase 3 will remove it after health gating)"
    );
    println!("recreated {name}: archived old container as {old_name}, new id {new_id}");
    Ok(())
}

fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
