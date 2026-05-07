use std::collections::HashMap;
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
///
/// ## Policy gate
///
/// This is a *manual* admin tool, not the automatic update loop, so the
/// `freshdock.mode` knob (live / nightly / weekly / monthly / watch) is
/// **deliberately not** enforced here — those modes describe how the
/// scheduler treats the container, not whether the operator can ever
/// touch it. A `mode=watch` container is a perfectly valid target for
/// `freshdock recreate`: the operator has explicitly typed the command.
///
/// What we *do* refuse is the two opt-out signals from PLAN §4 ("honest
/// defaults"): containers without `freshdock.enable=true`, and containers
/// with `freshdock.mode=off`. Those are the user saying "this container
/// is not a freshdock target at all" and we respect that even on a
/// manual invocation.
pub async fn run(name: String) -> Result<(), AppError> {
    let docker = Docker::connect()?;
    let spec = docker.inspect_container_spec(&name).await?;

    let empty: HashMap<String, String> = HashMap::new();
    let policy = labels::parse_policy(spec.config.labels.as_ref().unwrap_or(&empty), None)?;
    if !policy.enabled || policy.mode == Mode::Off {
        warn!(
            container = %name,
            mode = %policy.mode,
            enabled = policy.enabled,
            "refusing to recreate: container is not opted into freshdock \
             (set freshdock.enable=true and a non-off mode to allow even \
             manual recreate)"
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
