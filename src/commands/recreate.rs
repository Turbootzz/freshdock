use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::config::{CredentialStore, ResolvedSettings};
use crate::docker::Docker;
use crate::docker::recreate::{Cleanup, DockerOps, recreate_with_health};
use crate::errors::AppError;
use crate::health::{Clock, HealthConfig, HealthProbe, TokioClock};
use crate::labels::{self, Mode, PolicyDefaults};
use crate::updater::RecreateOutcome;

/// Recreate a single container by name, health-gated: inspect → pull → stop →
/// rename → create → start → wait-for-healthy → (remove old | rollback).
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
///
/// Thin entry: wires the live daemon + default health timing, delegates to the
/// testable [`run_with`].
pub async fn run(
    name: String,
    credentials: Arc<CredentialStore>,
    settings: ResolvedSettings,
) -> Result<(), AppError> {
    let docker = Docker::connect(credentials)?;
    run_with(
        &docker,
        &name,
        &HealthConfig::default(),
        &TokioClock,
        settings.policy_defaults(),
        settings.prune_dangling,
        current_unix_timestamp,
    )
    .await
}

/// Testable core of `recreate`: parameterised over the daemon ops, health
/// timing, clock, and timestamp source so unit tests can exercise the policy
/// gate without a live socket. `defaults`/`prune_dangling` come from
/// `[settings]`; the per-container `freshdock.cleanup` label still wins.
#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    docker: &(impl DockerOps + HealthProbe),
    name: &str,
    health: &HealthConfig,
    clock: &impl Clock,
    defaults: PolicyDefaults,
    prune_dangling: bool,
    ts_provider: impl Fn() -> i64,
) -> Result<(), AppError> {
    let spec = docker.inspect(name).await?;

    let empty: HashMap<String, String> = HashMap::new();
    let policy = labels::parse_policy(spec.config.labels.as_ref().unwrap_or(&empty), defaults)?;
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

    let cleanup = Cleanup {
        remove_replaced: policy.cleanup,
        prune_dangling,
    };
    let outcome = recreate_with_health(docker, name, health, clock, cleanup, ts_provider).await?;
    // Exhaustive match (no wildcard) on purpose: a new `RecreateOutcome`
    // variant forces this site to decide what to print.
    match outcome {
        RecreateOutcome::Recreated { old_name, new_id } => {
            info!(
                container = %name,
                archived_as = %old_name,
                new_id = %new_id,
                "recreate complete — new container healthy, old container removed"
            );
            println!(
                "recreated {name}: healthy — removed old container {old_name}, new id {new_id}"
            );
        }
        RecreateOutcome::RolledBack(event) => {
            warn!(
                container = %name,
                reason = ?event.reason,
                restored_from = %event.restored_from,
                "recreate rolled back — previous container restored"
            );
            println!(
                "recreate failed for {name}: new image was unhealthy ({:?}); rolled back to the previous container (restored from {})",
                event.reason, event.restored_from
            );
        }
    }
    Ok(())
}

fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;
    use crate::docker::spec::ContainerSpec;
    use crate::health::{ContainerRuntimeState, TokioClock};
    use crate::registry::ImageRef;
    use async_trait::async_trait;
    use bollard::models::ContainerConfig;

    /// Fake whose only real method is `inspect` (returns a configurable label
    /// set). Every other op panics — so any test where the policy gate *fails*
    /// to short-circuit will blow up loudly, proving "zero DockerOps calls past
    /// the initial inspect".
    struct GateOps {
        labels: Option<HashMap<String, String>>,
    }

    impl GateOps {
        fn with_labels(pairs: &[(&str, &str)]) -> Self {
            Self {
                labels: Some(
                    pairs
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                ),
            }
        }
        fn without_labels() -> Self {
            Self { labels: None }
        }
    }

    #[async_trait]
    impl DockerOps for GateOps {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: "nginx:alpine".to_owned(),
                image_id: None,
                config: ContainerConfig {
                    labels: self.labels.clone(),
                    ..Default::default()
                },
                host_config: None,
                network_endpoints: None,
            })
        }
        async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
            panic!("policy gate must refuse before pull");
        }
        async fn stop(
            &self,
            _name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            panic!("policy gate must refuse before stop");
        }
        async fn rename(&self, _name: &str, _ts_unix: i64) -> Result<String, DockerError> {
            panic!("policy gate must refuse before rename");
        }
        async fn create_from_spec(
            &self,
            _name: &str,
            _spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            panic!("policy gate must refuse before create");
        }
        async fn start(&self, _name_or_id: &str) -> Result<(), DockerError> {
            panic!("policy gate must refuse before start");
        }
        async fn remove(&self, _name_or_id: &str, _force: bool) -> Result<(), DockerError> {
            panic!("policy gate must refuse before remove");
        }
        async fn rename_to(&self, _from: &str, _to: &str) -> Result<(), DockerError> {
            panic!("policy gate must refuse before rename_to");
        }
        async fn remove_image(&self, _id: &str, _force: bool) -> Result<(), DockerError> {
            panic!("policy gate must refuse before remove_image");
        }
        async fn prune_dangling_images(&self) -> Result<(), DockerError> {
            panic!("policy gate must refuse before prune_dangling_images");
        }
    }

    #[async_trait]
    impl HealthProbe for GateOps {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            panic!("policy gate must refuse before any health probe");
        }
    }

    async fn assert_refused(ops: GateOps) {
        run_with(
            &ops,
            "c",
            &HealthConfig::default(),
            &TokioClock,
            PolicyDefaults::default(),
            false,
            || 0,
        )
        .await
        .expect("a refused recreate is a graceful no-op, not an error");
    }

    #[tokio::test]
    async fn no_freshdock_labels_is_refused() {
        assert_refused(GateOps::without_labels()).await;
    }

    #[tokio::test]
    async fn enable_false_is_refused() {
        assert_refused(GateOps::with_labels(&[("freshdock.enable", "false")])).await;
    }

    #[tokio::test]
    async fn mode_off_is_refused() {
        assert_refused(GateOps::with_labels(&[
            ("freshdock.enable", "true"),
            ("freshdock.mode", "off"),
        ]))
        .await;
    }
}
