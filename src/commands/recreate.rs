use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::config::{CredentialStore, ResolvedSettings};
use crate::docker::Docker;
use crate::docker::recreate::{Cleanup, DockerOps, recreate_with_health};
use crate::errors::AppError;
use crate::health::{Clock, HealthConfig, HealthProbe, TokioClock};
use crate::labels::{self, Mode};
use crate::rollout::{self, RolloutConfig, RolloutReport, RolloutStep};
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
/// Under `watch_all` (issue #79) an unlabelled container counts as opted in,
/// so the gate passes; the explicit opt-outs above are still refused.
///
/// Thin entry: wires the live daemon + default health timing, delegates to the
/// testable [`run_with`].
pub async fn run(
    name: String,
    credentials: Arc<CredentialStore>,
    settings: ResolvedSettings,
) -> Result<(), AppError> {
    let docker = Docker::connect(credentials).await?;
    run_with(
        &docker,
        &name,
        &HealthConfig::default(),
        &TokioClock,
        settings,
        current_unix_timestamp,
    )
    .await
}

/// Testable core of `recreate`: parameterised over the daemon ops, health
/// timing, clock, and timestamp source so unit tests can exercise the policy
/// gate without a live socket. `settings` carries the `[settings]` defaults;
/// the per-container `freshdock.cleanup` label still wins.
pub async fn run_with(
    docker: &(impl DockerOps + HealthProbe),
    name: &str,
    health: &HealthConfig,
    clock: &impl Clock,
    settings: ResolvedSettings,
    ts_provider: impl Fn() -> i64,
) -> Result<(), AppError> {
    let spec = docker.inspect(name).await?;

    let empty: HashMap<String, String> = HashMap::new();
    let policy = labels::parse_policy(
        spec.config.labels.as_ref().unwrap_or(&empty),
        settings.policy_defaults(),
    )?;
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

    for note in labels::watchtower_diagnostics(spec.config.labels.as_ref().unwrap_or(&empty)) {
        warn!(container = %name, %note, "watchtower label");
    }

    // A compose member is rolled out with the rest of its project (#78); the
    // operator named one container, but the stack is the unit of work.
    if settings.compose_aware
        && let Some(report) = rollout::for_container(
            docker,
            &spec.name,
            spec.config.labels.as_ref().unwrap_or(&empty),
            &RolloutConfig {
                health: health.clone(),
                prune_dangling: settings.prune_dangling,
                one_shot_timeout: settings.one_shot_timeout,
            },
            clock,
            settings.policy_defaults(),
            crate::selfid::own_container_id_prefix().as_deref(),
            &ts_provider,
        )
        .await
    {
        print_rollout(&report);
        return Ok(());
    }

    let cleanup = Cleanup {
        remove_replaced: policy.cleanup,
        prune_dangling: settings.prune_dangling,
    };
    let outcome = recreate_with_health(
        docker,
        name,
        health,
        clock,
        cleanup,
        &policy.hooks,
        ts_provider,
    )
    .await?;
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
        RecreateOutcome::SkippedByHook(reason) => {
            info!(
                container = %name,
                %reason,
                "recreate skipped by the pre-update hook — container left unchanged"
            );
            println!("recreate skipped for {name}: {reason} — container left unchanged");
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

/// Report a rollout on stdout, one line per step plus the verdict, in the
/// order it happened.
fn print_rollout(report: &RolloutReport) {
    println!("rollout of compose project {}:", report.project);
    for step in &report.steps {
        match step {
            RolloutStep::OneShotCompleted { container } => {
                println!("  {container}: re-ran to a successful exit")
            }
            RolloutStep::Updated {
                container, new_id, ..
            } => {
                println!("  {container}: updated and healthy (new id {new_id})")
            }
            RolloutStep::Restarted { container } => {
                println!("  {container}: restarted (depends_on restart: true)")
            }
        }
    }
    match &report.aborted {
        None if report.steps.is_empty() => {
            println!("rollout did nothing:");
            for skipped in &report.skipped {
                println!("  {}: {}", skipped.container, skipped.reason);
            }
        }
        None => println!("rollout complete: {} step(s)", report.steps.len()),
        Some(reason) => println!(
            "rollout ABORTED: {reason}. The services after this point were not touched \
             and are still running their previous image."
        ),
    }
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
    /// set). `pull` returns an error (so a gate-pass surfaces as `Err`); every
    /// op past it panics, so any test where the policy gate *fails* to
    /// short-circuit blows up loudly.
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
            // Always an error, never a panic: a refusal returns Ok before
            // reaching pull, so run_with's Ok/Err splits gate-refused from
            // gate-passed on its own.
            Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                "pull refused by the test fake",
            )))
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
        async fn exec_hook(
            &self,
            _name_or_id: &str,
            _command: &str,
            _timeout: Option<std::time::Duration>,
        ) -> Result<crate::docker::recreate::HookStatus, DockerError> {
            panic!("policy gate must refuse before exec_hook");
        }
        async fn list_network_dependents(&self, _name: &str) -> Result<Vec<String>, DockerError> {
            panic!("policy gate must refuse before list_network_dependents");
        }
    }

    #[async_trait]
    impl HealthProbe for GateOps {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            panic!("policy gate must refuse before any health probe");
        }
    }

    async fn assert_refused_with(ops: GateOps, settings: ResolvedSettings) {
        run_with(
            &ops,
            "c",
            &HealthConfig::default(),
            &TokioClock,
            settings,
            || 0,
        )
        .await
        .expect("a refused recreate is a graceful no-op, not an error");
    }

    async fn assert_refused(ops: GateOps) {
        assert_refused_with(ops, ResolvedSettings::default()).await;
    }

    /// Settings with the opt-out mode switched on.
    fn watch_all() -> ResolvedSettings {
        ResolvedSettings {
            watch_all: true,
            ..Default::default()
        }
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

    // --- watch_all opt-out mode (issue #79) ---

    #[tokio::test]
    async fn unlabelled_with_watch_all_passes_the_gate() {
        // The pull error can only come from past the gate: reaching it is the
        // assertion.
        let result = run_with(
            &GateOps::without_labels(),
            "c",
            &HealthConfig::default(),
            &TokioClock,
            watch_all(),
            || 0,
        )
        .await;
        assert!(
            result.is_err(),
            "watch_all must let an unlabelled container through the gate"
        );
    }

    #[tokio::test]
    async fn enable_false_with_watch_all_is_refused() {
        assert_refused_with(
            GateOps::with_labels(&[("freshdock.enable", "false")]),
            watch_all(),
        )
        .await;
    }

    #[tokio::test]
    async fn mode_off_with_watch_all_is_refused() {
        assert_refused_with(
            GateOps::with_labels(&[("freshdock.mode", "off")]),
            watch_all(),
        )
        .await;
    }
}
