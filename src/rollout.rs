//! Compose project rollouts (issue #78).
//!
//! Inside a Compose project, one container is the wrong unit of work. An exited
//! one-shot `migrate` service is invisible to a running-only listing, so a new
//! image lands on `web` while the migration stays on the old code, and
//! `depends_on` says an order that per-container updates ignore.
//!
//! A rollout discovers the project's members, picks the ones the moved image
//! applies to, orders them topologically, and runs them through the existing
//! recreate machinery.
//!
//! The label gate still decides, with one narrow bypass: an *unlabelled*
//! service the project waits on with `service_completed_successfully` is
//! re-run anyway, since that condition is the compose file declaring it must
//! complete first. Explicit opt-outs are honoured even then (same shape as the
//! #68 network-dependent repair, via the same
//! [`crate::labels::explicitly_opts_out`]). Unlabelled long-running siblings
//! are not swept in.
//!
//! Dependents are deliberately not stopped up front, unlike the issue's
//! sketch: topological order already puts a one-shot ahead of them, so a failed
//! migration leaves them running on the old image rather than down. That is
//! also what `docker compose up -d` does.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::compose::{self, Dependency, ProjectMember};
use crate::docker::DockerError;
use crate::docker::recreate::{
    Cleanup, DockerOps, OneShotOutcome, recreate_one_shot, recreate_with_health,
};
use crate::health::{Clock, HealthConfig, HealthProbe};
use crate::labels::{self, Mode, Policy, PolicyDefaults};
use crate::registry::ImageRef;
use crate::updater::{HookSkipReason, RecreateOutcome};

/// How long a one-shot may run before the rollout gives up. Generous: a large
/// schema migration is the motivating case.
pub const DEFAULT_ONE_SHOT_TIMEOUT: Duration = Duration::from_secs(600);

/// Timing and cleanup knobs for a rollout.
#[derive(Debug, Clone)]
pub struct RolloutConfig {
    pub health: HealthConfig,
    pub prune_dangling: bool,
    pub one_shot_timeout: Duration,
}

impl Default for RolloutConfig {
    fn default() -> Self {
        Self {
            health: HealthConfig::default(),
            prune_dangling: false,
            one_shot_timeout: DEFAULT_ONE_SHOT_TIMEOUT,
        }
    }
}

/// How a target is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// Recreate, run, and wait for a zero exit. Failure aborts the rollout.
    OneShot,
    /// The ordinary health-gated update.
    Service,
}

/// One container the rollout will update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTarget {
    pub container: String,
    pub service: String,
    pub kind: TargetKind,
    /// The target's own policy, so its cleanup and hook labels still apply. An
    /// unlabelled one-shot carries the disabled default: no hooks, no cleanup.
    pub policy: Policy,
}

/// Why a project member on the updated image was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// An explicit `freshdock.enable=false` / `mode=off`.
    OptedOut,
    /// No enable label, and nothing in the project waits for it to complete.
    NotEnabled,
    /// Labels present but unparseable.
    InvalidLabels(String),
    /// Stopped and not a one-shot. freshdock does not start what you stopped.
    StoppedService,
    /// A one-shot that is currently running, i.e. mid-run.
    OneShotInFlight,
    /// freshdock's own container.
    SelfContainer,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::OptedOut => f.write_str("explicitly opted out of freshdock"),
            SkipReason::NotEnabled => f.write_str(
                "not opted into freshdock, and nothing in the project waits for it to complete",
            ),
            SkipReason::InvalidLabels(e) => write!(f, "invalid freshdock labels: {e}"),
            SkipReason::StoppedService => {
                f.write_str("stopped, and not a one-shot; leaving it stopped")
            }
            SkipReason::OneShotInFlight => f.write_str("one-shot is already running"),
            SkipReason::SelfContainer => f.write_str("this is freshdock's own container"),
        }
    }
}

/// A member on the updated image that the plan excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedMember {
    pub container: String,
    pub reason: SkipReason,
}

/// The ordered work a rollout will do. Produced purely from labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutPlan {
    pub project: String,
    /// Dependencies before dependents.
    pub targets: Vec<PlannedTarget>,
    pub skipped: Vec<SkippedMember>,
    /// Service to its running containers, already filtered for opt-outs and
    /// for freshdock itself.
    restart_candidates: HashMap<String, Vec<String>>,
    graph: HashMap<String, Vec<Dependency>>,
}

impl RolloutPlan {
    /// Containers to restart because they declared
    /// `depends_on.<dep>.restart: true` on a service that was just updated.
    pub fn restarts_for(&self, updated: &HashSet<String>) -> Vec<String> {
        compose::restart_dependents(&self.graph, updated)
            .iter()
            .filter_map(|service| self.restart_candidates.get(service))
            .flatten()
            .cloned()
            .collect()
    }

    /// The services this plan would update if every step succeeded.
    fn all_target_services(&self) -> HashSet<String> {
        self.targets.iter().map(|t| t.service.clone()).collect()
    }

    /// Does this plan add anything over updating `container` on its own? When
    /// it does not, the caller stays on the plain path, so a one-service
    /// project behaves exactly as it did before this feature.
    pub fn adds_nothing_for(&self, container: &str) -> bool {
        self.restarts_for(&self.all_target_services()).is_empty()
            && matches!(self.targets.as_slice(), [only] if only.container == container)
    }

    /// Is `container` part of this plan at all?
    pub fn covers(&self, container: &str) -> bool {
        self.targets.iter().any(|t| t.container == container)
    }
}

/// Build the plan for `project` after `image_ref` moved. `members` must include
/// stopped containers: the exited one-shots are the entire point.
pub fn plan(
    project: &str,
    members: &[ProjectMember],
    image_ref: &str,
    defaults: PolicyDefaults,
    own_id_prefix: Option<&str>,
) -> RolloutPlan {
    let graph = compose::graph_of(members);
    let awaited = compose::services_awaited_for_completion(&graph);
    let updated_image = ImageRef::parse(image_ref);

    let mut by_service: HashMap<String, Vec<PlannedTarget>> = HashMap::new();
    let mut skipped = Vec::new();
    let mut restart_candidates: HashMap<String, Vec<String>> = HashMap::new();

    for member in members {
        let Some(info) = compose::parse(&member.labels) else {
            continue;
        };
        let is_self = crate::selfid::is_own_container(own_id_prefix, Some(member.id.as_str()));
        let opted_out = labels::explicitly_opts_out(&member.labels);

        // Independent of the image: a dependent is bumped because its
        // dependency moved, not because it did.
        if member.running && !is_self && !opted_out {
            restart_candidates
                .entry(info.service.clone())
                .or_default()
                .push(member.name.clone());
        }

        if ImageRef::parse(&member.image_ref) != updated_image {
            continue;
        }
        let one_shot = awaited.contains(&info.service);
        let mut skip = |reason| {
            skipped.push(SkippedMember {
                container: member.name.clone(),
                reason,
            })
        };

        if is_self {
            skip(SkipReason::SelfContainer);
            continue;
        }
        if opted_out {
            skip(SkipReason::OptedOut);
            continue;
        }
        let policy = match labels::parse_policy(&member.labels, defaults) {
            Ok(policy) => policy,
            Err(e) => {
                skip(SkipReason::InvalidLabels(e.to_string()));
                continue;
            }
        };
        // The narrow bypass, see the module docs.
        let opted_in = policy.enabled && policy.mode != Mode::Off;
        if !opted_in && !one_shot {
            skip(SkipReason::NotEnabled);
            continue;
        }
        if one_shot {
            // Recreating it now would kill a migration in progress.
            if member.running {
                skip(SkipReason::OneShotInFlight);
                continue;
            }
        } else if !member.running {
            // Recreating it would start it, which is not freshdock's call.
            skip(SkipReason::StoppedService);
            continue;
        }

        by_service
            .entry(info.service.clone())
            .or_default()
            .push(PlannedTarget {
                container: member.name.clone(),
                service: info.service.clone(),
                kind: if one_shot {
                    TargetKind::OneShot
                } else {
                    TargetKind::Service
                },
                policy,
            });
    }

    for containers in restart_candidates.values_mut() {
        containers.sort();
    }
    skipped.sort_by(|a, b| a.container.cmp(&b.container));

    let services: Vec<String> = by_service.keys().cloned().collect();
    let targets = compose::topological_order(&services, &graph)
        .into_iter()
        .filter_map(|service| by_service.remove(&service))
        .flat_map(|mut replicas| {
            // Replicas are unordered between themselves; sort for determinism.
            replicas.sort_by(|a, b| a.container.cmp(&b.container));
            replicas
        })
        .collect();

    RolloutPlan {
        project: project.to_owned(),
        targets,
        skipped,
        restart_candidates,
        graph,
    }
}

/// Why a rollout stopped before finishing its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    OneShotFailed {
        container: String,
        exit_code: i64,
    },
    OneShotTimedOut {
        container: String,
    },
    RolledBack {
        container: String,
    },
    Deferred {
        container: String,
        reason: HookSkipReason,
    },
    StepFailed {
        container: String,
        error: String,
    },
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortReason::OneShotFailed {
                container,
                exit_code,
            } => write!(f, "{container} exited with code {exit_code}"),
            AbortReason::OneShotTimedOut { container } => {
                write!(f, "{container} did not finish within its timeout")
            }
            AbortReason::RolledBack { container } => {
                write!(f, "{container} failed its health gate and was rolled back")
            }
            AbortReason::Deferred { container, reason } => {
                write!(
                    f,
                    "{container} was deferred by its pre-update hook ({reason})"
                )
            }
            AbortReason::StepFailed { container, error } => {
                write!(f, "{container} could not be updated: {error}")
            }
        }
    }
}

/// What one step of a rollout did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolloutStep {
    OneShotCompleted { container: String },
    Updated { container: String, new_id: String },
    Restarted { container: String },
}

/// The result of one rollout, logged and notified as a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutReport {
    pub project: String,
    pub steps: Vec<RolloutStep>,
    /// `Some` when the plan was cut short. Everything before it stands.
    pub aborted: Option<AbortReason>,
    /// Planned targets the rollout did not complete, the failed one included.
    /// All still serving their previous image.
    pub not_completed: Vec<String>,
}

impl RolloutReport {
    pub fn updated_containers(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                RolloutStep::Updated { container, .. } => Some(container.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Every container the rollout touched, so a caller sweeping the same
    /// project does not process them twice.
    pub fn touched(&self) -> Vec<&str> {
        self.steps
            .iter()
            .map(|s| match s {
                RolloutStep::OneShotCompleted { container }
                | RolloutStep::Updated { container, .. }
                | RolloutStep::Restarted { container } => container.as_str(),
            })
            .collect()
    }
}

/// Discover the project's members and plan the rollout. A listing error is
/// returned, not swallowed, so the caller can fall back to the plain update.
pub async fn plan_for(
    ops: &impl DockerOps,
    project: &str,
    image_ref: &str,
    defaults: PolicyDefaults,
    own_id_prefix: Option<&str>,
) -> Result<RolloutPlan, DockerError> {
    let members = ops.list_project(project).await?;
    Ok(plan(project, &members, image_ref, defaults, own_id_prefix))
}

/// The rollout for `container`, or `None` when the plain single-container
/// update is the right thing instead: not a compose member, the listing failed,
/// the plan does not cover it, or the plan is just this container.
///
/// The single place that decision is made, so the two callers cannot drift.
/// `container` must be the **inspected** name; a plan is built from a listing,
/// which never names anything by id.
#[allow(clippy::too_many_arguments)]
pub async fn for_container<D>(
    ops: &D,
    container: &str,
    container_labels: &HashMap<String, String>,
    image_ref: &str,
    cfg: &RolloutConfig,
    clock: &impl Clock,
    defaults: PolicyDefaults,
    own_id_prefix: Option<&str>,
    ts_provider: &impl Fn() -> i64,
) -> Option<RolloutReport>
where
    D: DockerOps + HealthProbe,
{
    let info = compose::parse(container_labels)?;
    let plan = match plan_for(ops, &info.project, image_ref, defaults, own_id_prefix).await {
        Ok(plan) => plan,
        Err(e) => {
            warn!(project = %info.project, %container, error = %e, "rollout: could not list the compose project; updating this container on its own");
            return None;
        }
    };
    if !plan.covers(container) {
        warn!(project = %info.project, %container, "rollout: the project plan does not cover this container; updating it on its own");
        return None;
    }
    if plan.adds_nothing_for(container) {
        debug!(project = %info.project, %container, "rollout: nothing else in the project is affected; updating this container on its own");
        return None;
    }
    Some(execute(ops, &plan, cfg, clock, ts_provider).await)
}

/// Run a plan in order, stopping at the first step that fails.
///
/// Past a failed dependency every remaining target depends on it, so updating
/// them would rebuild the failure this exists to prevent. Completed steps
/// stand: there is no safe way to un-apply a migration.
pub async fn execute<D>(
    ops: &D,
    plan: &RolloutPlan,
    cfg: &RolloutConfig,
    clock: &impl Clock,
    ts_provider: &impl Fn() -> i64,
) -> RolloutReport
where
    D: DockerOps + HealthProbe,
{
    let mut report = RolloutReport {
        project: plan.project.clone(),
        steps: Vec::new(),
        aborted: None,
        not_completed: Vec::new(),
    };
    let mut updated_services: HashSet<String> = HashSet::new();

    for skipped in &plan.skipped {
        debug!(project = %plan.project, container = %skipped.container, reason = %skipped.reason, "rollout: skipping project member");
    }
    info!(
        project = %plan.project,
        targets = plan.targets.len(),
        "rollout: starting compose project rollout"
    );

    for target in &plan.targets {
        let cleanup = Cleanup {
            remove_replaced: target.policy.cleanup,
            prune_dangling: cfg.prune_dangling,
        };
        let abort = match target.kind {
            TargetKind::OneShot => {
                info!(project = %plan.project, container = %target.container, service = %target.service, "rollout: re-running one-shot");
                match recreate_one_shot(
                    ops,
                    &target.container,
                    cfg.one_shot_timeout,
                    cfg.health.poll_interval,
                    clock,
                    cleanup,
                    &target.policy.hooks,
                    ts_provider,
                )
                .await
                {
                    Ok(OneShotOutcome::Completed) => {
                        report.steps.push(RolloutStep::OneShotCompleted {
                            container: target.container.clone(),
                        });
                        updated_services.insert(target.service.clone());
                        None
                    }
                    Ok(OneShotOutcome::Failed { exit_code }) => Some(AbortReason::OneShotFailed {
                        container: target.container.clone(),
                        exit_code,
                    }),
                    Ok(OneShotOutcome::TimedOut) => Some(AbortReason::OneShotTimedOut {
                        container: target.container.clone(),
                    }),
                    Ok(OneShotOutcome::SkippedByHook(reason)) => Some(AbortReason::Deferred {
                        container: target.container.clone(),
                        reason,
                    }),
                    Err(e) => Some(AbortReason::StepFailed {
                        container: target.container.clone(),
                        error: e.to_string(),
                    }),
                }
            }
            TargetKind::Service => {
                info!(project = %plan.project, container = %target.container, service = %target.service, "rollout: updating service");
                match recreate_with_health(
                    ops,
                    &target.container,
                    &cfg.health,
                    clock,
                    cleanup,
                    &target.policy.hooks,
                    ts_provider,
                )
                .await
                {
                    // Exhaustive on purpose, as in the `recreate` command.
                    Ok(RecreateOutcome::Recreated { new_id, .. }) => {
                        report.steps.push(RolloutStep::Updated {
                            container: target.container.clone(),
                            new_id,
                        });
                        updated_services.insert(target.service.clone());
                        None
                    }
                    Ok(RecreateOutcome::RolledBack(_)) => Some(AbortReason::RolledBack {
                        container: target.container.clone(),
                    }),
                    Ok(RecreateOutcome::SkippedByHook(reason)) => Some(AbortReason::Deferred {
                        container: target.container.clone(),
                        reason,
                    }),
                    Err(e) => Some(AbortReason::StepFailed {
                        container: target.container.clone(),
                        error: e.to_string(),
                    }),
                }
            }
        };

        if let Some(reason) = abort {
            let touched: HashSet<&str> = report.touched().into_iter().collect();
            report.not_completed = plan
                .targets
                .iter()
                .map(|t| t.container.clone())
                .filter(|c| !touched.contains(c.as_str()))
                .collect();
            warn!(
                project = %plan.project,
                %reason,
                completed = report.steps.len(),
                remaining = report.not_completed.len(),
                "rollout: aborted; the remaining services keep running on their current image"
            );
            report.aborted = Some(reason);
            return report;
        }
    }

    // Only for services that actually changed: a dependent of one that rolled
    // back has nothing new to pick up.
    for container in plan.restarts_for(&updated_services) {
        info!(project = %plan.project, %container, "rollout: restarting dependent (depends_on restart: true)");
        // A restart, not a recreate: no pull, no rollback surface. A failure
        // here is logged and the rollout still stands.
        if let Err(e) = ops.stop(&container, None, None).await {
            warn!(project = %plan.project, %container, error = %e, "rollout: failed to stop dependent for its restart; leaving it as it is");
            continue;
        }
        if let Err(e) = ops.start(&container).await {
            warn!(project = %plan.project, %container, error = %e, "rollout: dependent was stopped for a restart but failed to start again; start it manually");
            continue;
        }
        report.steps.push(RolloutStep::Restarted { container });
    }

    info!(project = %plan.project, steps = report.steps.len(), "rollout: complete");
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{LABEL_DEPENDS_ON, LABEL_ONEOFF, LABEL_PROJECT, LABEL_SERVICE};
    use crate::docker::recreate::HookStatus;
    use crate::docker::spec::ContainerSpec;
    use crate::health::{ContainerRuntimeState, TokioClock};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const PROJECT: &str = "stack";

    /// Builder for a project member. Defaults to a running, unlabelled service
    /// on the image the rollout is about, so each test states only the one
    /// thing it is testing.
    fn member(service: &str, deps: &str) -> ProjectMember {
        ProjectMember {
            name: format!("{PROJECT}-{service}-1"),
            id: format!("id{service}0000000000000000"),
            image_ref: "app:latest".to_owned(),
            labels: HashMap::from([
                (LABEL_PROJECT.to_owned(), PROJECT.to_owned()),
                (LABEL_SERVICE.to_owned(), service.to_owned()),
                (LABEL_DEPENDS_ON.to_owned(), deps.to_owned()),
            ]),
            running: true,
        }
    }

    fn labelled(mut m: ProjectMember, pairs: &[(&str, &str)]) -> ProjectMember {
        for (k, v) in pairs {
            m.labels.insert((*k).to_owned(), (*v).to_owned());
        }
        m
    }

    fn enabled(m: ProjectMember) -> ProjectMember {
        labelled(
            m,
            &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
        )
    }

    fn exited(mut m: ProjectMember) -> ProjectMember {
        m.running = false;
        m
    }

    fn on_image(mut m: ProjectMember, image: &str) -> ProjectMember {
        m.image_ref = image.to_owned();
        m
    }

    /// `web` waits for a `migrate` one-shot and for a healthy `db`, with
    /// `restart: true` on the `db` edge. The shape from the issue.
    fn stack() -> Vec<ProjectMember> {
        vec![
            enabled(member(
                "web",
                "migrate:service_completed_successfully:false,db:service_healthy:true",
            )),
            exited(member("migrate", "")),
            on_image(member("db", ""), "postgres:18"),
        ]
    }

    fn plan_of(members: &[ProjectMember]) -> RolloutPlan {
        plan(
            PROJECT,
            members,
            "app:latest",
            PolicyDefaults::default(),
            None,
        )
    }

    fn target_names(plan: &RolloutPlan) -> Vec<&str> {
        plan.targets.iter().map(|t| t.container.as_str()).collect()
    }

    fn skip_reason<'a>(plan: &'a RolloutPlan, container: &str) -> Option<&'a SkipReason> {
        plan.skipped
            .iter()
            .find(|s| s.container == container)
            .map(|s| &s.reason)
    }

    // ── planning ───────────────────────────────────────────────────────────

    #[test]
    fn the_one_shot_runs_before_the_service_that_waits_on_it() {
        let plan = plan_of(&stack());
        assert_eq!(target_names(&plan), vec!["stack-migrate-1", "stack-web-1"]);
        assert_eq!(plan.targets[0].kind, TargetKind::OneShot);
        assert_eq!(plan.targets[1].kind, TargetKind::Service);
    }

    #[test]
    fn an_unlabelled_one_shot_is_swept_in_because_the_project_waits_for_it() {
        // The whole point of #78: nobody labels the migrate service, and
        // skipping it is what puts new code against an old schema.
        let plan = plan_of(&stack());
        assert!(target_names(&plan).contains(&"stack-migrate-1"));
    }

    #[test]
    fn an_unlabelled_long_running_sibling_is_left_alone() {
        // Sharing an image with a labelled container is not consent.
        let mut members = stack();
        members.push(member("worker", ""));
        let plan = plan_of(&members);
        assert!(!target_names(&plan).contains(&"stack-worker-1"));
        assert_eq!(
            skip_reason(&plan, "stack-worker-1"),
            Some(&SkipReason::NotEnabled)
        );
    }

    #[test]
    fn watch_all_makes_an_unlabelled_sibling_a_target() {
        let mut members = stack();
        members.push(member("worker", ""));
        let plan = plan(
            PROJECT,
            &members,
            "app:latest",
            PolicyDefaults {
                watch_all: true,
                ..Default::default()
            },
            None,
        );
        assert!(target_names(&plan).contains(&"stack-worker-1"));
    }

    #[test]
    fn an_explicit_opt_out_wins_even_for_a_one_shot() {
        // The bypass covers *absent* labels only; a stated "no" is still a no.
        for opt_out in [
            ("freshdock.enable", "false"),
            ("com.centurylinklabs.watchtower.enable", "false"),
            ("freshdock.mode", "off"),
        ] {
            let members = vec![
                enabled(member(
                    "web",
                    "migrate:service_completed_successfully:false",
                )),
                labelled(exited(member("migrate", "")), &[opt_out]),
            ];
            let plan = plan_of(&members);
            assert_eq!(
                target_names(&plan),
                vec!["stack-web-1"],
                "{opt_out:?} must keep the one-shot out of the rollout"
            );
            assert_eq!(
                skip_reason(&plan, "stack-migrate-1"),
                Some(&SkipReason::OptedOut)
            );
        }
    }

    #[test]
    fn a_stopped_service_that_is_not_a_one_shot_is_not_started() {
        // Recreating it would start it, and the operator stopping it is a
        // decision freshdock does not get to overrule.
        let members = vec![
            enabled(member("web", "")),
            exited(enabled(member("worker", ""))),
        ];
        let plan = plan_of(&members);
        assert_eq!(target_names(&plan), vec!["stack-web-1"]);
        assert_eq!(
            skip_reason(&plan, "stack-worker-1"),
            Some(&SkipReason::StoppedService)
        );
    }

    #[test]
    fn a_one_shot_that_is_still_running_is_not_stomped() {
        let members = vec![
            enabled(member(
                "web",
                "migrate:service_completed_successfully:false",
            )),
            member("migrate", ""),
        ];
        let plan = plan_of(&members);
        assert_eq!(target_names(&plan), vec!["stack-web-1"]);
        assert_eq!(
            skip_reason(&plan, "stack-migrate-1"),
            Some(&SkipReason::OneShotInFlight)
        );
    }

    #[test]
    fn a_compose_run_oneoff_is_never_a_target() {
        let mut members = stack();
        members.push(labelled(
            enabled(member("web", "")),
            &[(LABEL_ONEOFF, "True")],
        ));
        let plan = plan_of(&members);
        assert_eq!(plan.targets.len(), 2, "only migrate and web");
    }

    #[test]
    fn only_members_on_the_moved_image_are_targets() {
        // `db` runs postgres:18 and is untouched by an app:latest push.
        let plan = plan_of(&stack());
        assert!(!target_names(&plan).contains(&"stack-db-1"));
        assert!(
            skip_reason(&plan, "stack-db-1").is_none(),
            "not even reported"
        );
    }

    #[test]
    fn image_refs_are_matched_after_normalisation() {
        // `app` and `app:latest` are the same image; `library/` prefixing must
        // not make a sibling look like a different one.
        let members = vec![
            enabled(member("web", "")),
            on_image(enabled(member("worker", "")), "app"),
        ];
        let plan = plan_of(&members);
        assert_eq!(target_names(&plan), vec!["stack-web-1", "stack-worker-1"]);
    }

    #[test]
    fn freshdocks_own_container_is_never_rolled_out() {
        let mut me = enabled(member("freshdock", ""));
        me.id = "abcdef0123456789abcdef".to_owned();
        let plan = plan(
            PROJECT,
            &[enabled(member("web", "")), me],
            "app:latest",
            PolicyDefaults::default(),
            Some("abcdef012345"),
        );
        assert_eq!(target_names(&plan), vec!["stack-web-1"]);
        assert_eq!(
            skip_reason(&plan, "stack-freshdock-1"),
            Some(&SkipReason::SelfContainer)
        );
    }

    #[test]
    fn unparseable_labels_skip_the_member_rather_than_the_project() {
        let members = vec![
            enabled(member("web", "")),
            labelled(member("worker", ""), &[("freshdock.enable", "yes-please")]),
        ];
        let plan = plan_of(&members);
        assert_eq!(target_names(&plan), vec!["stack-web-1"]);
        assert!(matches!(
            skip_reason(&plan, "stack-worker-1"),
            Some(SkipReason::InvalidLabels(_))
        ));
    }

    #[test]
    fn only_a_restart_true_edge_produces_a_restart() {
        let members = vec![
            enabled(member("db", "")),
            member("web", "db:service_healthy:true"),
            member("worker", "db:service_healthy:false"),
        ];
        let plan = plan(
            PROJECT,
            &members,
            "app:latest",
            PolicyDefaults::default(),
            None,
        );
        assert_eq!(
            plan.restarts_for(&HashSet::from(["db".to_owned()])),
            vec!["stack-web-1"]
        );
    }

    #[test]
    fn a_restart_dependent_that_opts_out_is_not_restarted() {
        let members = vec![
            enabled(member("db", "")),
            labelled(
                member("web", "db:service_healthy:true"),
                &[("freshdock.enable", "false")],
            ),
        ];
        let plan = plan(
            PROJECT,
            &members,
            "app:latest",
            PolicyDefaults::default(),
            None,
        );
        assert!(
            plan.restarts_for(&HashSet::from(["db".to_owned()]))
                .is_empty()
        );
    }

    #[test]
    fn a_stopped_dependent_is_not_started_by_a_restart_edge() {
        let members = vec![
            enabled(member("db", "")),
            exited(member("web", "db:service_healthy:true")),
        ];
        let plan = plan(
            PROJECT,
            &members,
            "app:latest",
            PolicyDefaults::default(),
            None,
        );
        assert!(
            plan.restarts_for(&HashSet::from(["db".to_owned()]))
                .is_empty()
        );
    }

    #[test]
    fn a_service_that_did_not_change_does_not_restart_its_dependents() {
        let members = vec![
            enabled(member("db", "")),
            member("web", "db:service_healthy:true"),
        ];
        let plan = plan(
            PROJECT,
            &members,
            "app:latest",
            PolicyDefaults::default(),
            None,
        );
        assert!(plan.restarts_for(&HashSet::new()).is_empty());
    }

    #[test]
    fn a_lone_service_adds_nothing_over_the_plain_update() {
        // The common case: one labelled container in a one-service project must
        // keep behaving exactly as it did before this feature existed.
        let plan = plan_of(&[enabled(member("web", ""))]);
        assert!(plan.covers("stack-web-1"));
        assert!(plan.adds_nothing_for("stack-web-1"));
    }

    #[test]
    fn a_project_with_a_one_shot_is_not_nothing() {
        let plan = plan_of(&stack());
        assert!(!plan.adds_nothing_for("stack-web-1"));
    }

    #[test]
    fn a_plan_that_excludes_the_triggering_container_does_not_cover_it() {
        let plan = plan_of(&[member("web", "")]);
        assert!(!plan.covers("stack-web-1"));
    }

    // ── execution ──────────────────────────────────────────────────────────

    /// A whole compose project behind the `DockerOps`/`HealthProbe` traits.
    ///
    /// Records every call so the *order* a rollout visits containers in is
    /// assertable, which is the property this feature exists to provide.
    /// `create_from_spec` hands back `new-<name>`, so the health/exit probes
    /// can be scripted per container by name.
    #[derive(Default)]
    struct StackOps {
        members: Vec<ProjectMember>,
        calls: Mutex<Vec<String>>,
        probe: Mutex<HashMap<String, VecDeque<ContainerRuntimeState>>>,
        create_fails_for: Option<String>,
        stop_fails_for: Option<String>,
        list_project_fails: bool,
    }

    impl StackOps {
        fn new(members: Vec<ProjectMember>) -> Self {
            Self {
                members,
                ..Default::default()
            }
        }

        /// Script a container's post-start states. Unscripted containers report
        /// healthy, so only the interesting ones need saying.
        fn probing(self, container: &str, states: &[ContainerRuntimeState]) -> Self {
            self.probe
                .lock()
                .unwrap()
                .insert(container.to_owned(), states.iter().copied().collect());
            self
        }

        /// The canonical happy stack: the migration exits zero.
        fn with_successful_migration(self) -> Self {
            self.probing(
                "stack-migrate-1",
                &[ContainerRuntimeState::Exited { exit_code: 0 }],
            )
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Just the container-visiting calls, so an assertion reads as the
        /// rollout sequence rather than the whole recreate cycle.
        fn visits(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.starts_with("create:") || c.starts_with("restart-stop:"))
                .collect()
        }

        fn member(&self, name: &str) -> Option<&ProjectMember> {
            self.members.iter().find(|m| m.name == name)
        }
    }

    #[async_trait]
    impl DockerOps for StackOps {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            self.record(format!("inspect:{name}"));
            let member = self
                .member(name)
                .unwrap_or_else(|| panic!("inspect of an unknown container {name}"));
            Ok(ContainerSpec {
                name: member.name.clone(),
                image_ref: member.image_ref.clone(),
                image_id: Some("sha256:old".to_owned()),
                config: bollard::models::ContainerConfig {
                    labels: Some(member.labels.clone()),
                    ..Default::default()
                },
                host_config: None,
                network_endpoints: None,
            })
        }

        async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
            self.record("pull".to_owned());
            Ok(())
        }

        async fn stop(
            &self,
            name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            // The restart pass stops a container that is not mid-recreate; tag
            // it so the two are distinguishable in an assertion.
            let mid_cycle = self.calls().iter().any(|c| c == &format!("inspect:{name}"));
            self.record(if mid_cycle {
                format!("stop:{name}")
            } else {
                format!("restart-stop:{name}")
            });
            if self.stop_fails_for.as_deref() == Some(name) {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "stop",
                )));
            }
            Ok(())
        }

        async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
            self.record(format!("rename:{name}"));
            Ok(crate::docker::rename::old_name_for(name, ts_unix))
        }

        async fn create_from_spec(
            &self,
            name: &str,
            _spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            self.record(format!("create:{name}"));
            if self.create_fails_for.as_deref() == Some(name) {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "create",
                )));
            }
            Ok(format!("new-{name}"))
        }

        async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
            self.record(format!("start:{name_or_id}"));
            Ok(())
        }

        async fn remove(&self, name_or_id: &str, _force: bool) -> Result<(), DockerError> {
            self.record(format!("remove:{name_or_id}"));
            Ok(())
        }

        async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
            self.record(format!("rename_to:{from}->{to}"));
            Ok(())
        }

        async fn remove_image(&self, id: &str, _force: bool) -> Result<(), DockerError> {
            self.record(format!("remove_image:{id}"));
            Ok(())
        }

        async fn prune_dangling_images(&self) -> Result<(), DockerError> {
            self.record("prune".to_owned());
            Ok(())
        }

        async fn exec_hook(
            &self,
            name_or_id: &str,
            _command: &str,
            _timeout: Option<Duration>,
        ) -> Result<HookStatus, DockerError> {
            self.record(format!("exec:{name_or_id}"));
            Ok(HookStatus::Completed { exit_code: 0 })
        }

        async fn list_network_dependents(&self, _name: &str) -> Result<Vec<String>, DockerError> {
            Ok(Vec::new())
        }

        async fn list_project(&self, project: &str) -> Result<Vec<ProjectMember>, DockerError> {
            self.record(format!("list_project:{project}"));
            if self.list_project_fails {
                return Err(DockerError::Spec(crate::docker::spec::SpecError::Missing(
                    "list_project",
                )));
            }
            Ok(self.members.clone())
        }
    }

    #[async_trait]
    impl HealthProbe for StackOps {
        async fn probe_state(&self, id: &str) -> Result<ContainerRuntimeState, DockerError> {
            let name = id.strip_prefix("new-").unwrap_or(id);
            let mut scripts = self.probe.lock().unwrap();
            let Some(states) = scripts.get_mut(name) else {
                return Ok(ContainerRuntimeState::HealthHealthy);
            };
            Ok(if states.len() > 1 {
                states.pop_front().expect("non-empty")
            } else {
                *states.front().expect("script must have a state")
            })
        }
    }

    fn fast_cfg() -> RolloutConfig {
        RolloutConfig {
            health: HealthConfig {
                health_timeout: Duration::from_secs(5),
                grace_period: Duration::from_secs(1),
                poll_interval: Duration::from_millis(50),
            },
            prune_dangling: false,
            one_shot_timeout: Duration::from_secs(30),
        }
    }

    async fn run(ops: &StackOps, plan: &RolloutPlan) -> RolloutReport {
        execute(ops, plan, &fast_cfg(), &TokioClock, &|| 42).await
    }

    #[tokio::test(start_paused = true)]
    async fn the_migration_is_recreated_and_run_before_the_service() {
        let ops = StackOps::new(stack()).with_successful_migration();
        let report = run(&ops, &plan_of(&stack())).await;

        assert_eq!(
            ops.visits(),
            vec!["create:stack-migrate-1", "create:stack-web-1"],
            "the one-shot has to be re-run before the code that depends on it"
        );
        assert!(report.aborted.is_none());
        assert_eq!(
            report.steps,
            vec![
                RolloutStep::OneShotCompleted {
                    container: "stack-migrate-1".to_owned()
                },
                RolloutStep::Updated {
                    container: "stack-web-1".to_owned(),
                    new_id: "new-stack-web-1".to_owned(),
                },
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_migration_aborts_before_the_service_is_touched() {
        // The GlitchTip case: if the migration fails, the new application code
        // must never come up against the old schema.
        let ops = StackOps::new(stack()).probing(
            "stack-migrate-1",
            &[ContainerRuntimeState::Exited { exit_code: 1 }],
        );
        let report = run(&ops, &plan_of(&stack())).await;

        assert_eq!(ops.visits(), vec!["create:stack-migrate-1"]);
        assert_eq!(
            report.aborted,
            Some(AbortReason::OneShotFailed {
                container: "stack-migrate-1".to_owned(),
                exit_code: 1,
            })
        );
        assert_eq!(
            report.not_completed,
            vec!["stack-migrate-1", "stack-web-1"],
            "the failed migration counts as not completed too"
        );
        assert!(report.updated_containers().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_migration_that_never_finishes_aborts_the_rollout() {
        let ops = StackOps::new(stack()).probing(
            "stack-migrate-1",
            &[ContainerRuntimeState::RunningNoHealthcheck],
        );
        let report = run(&ops, &plan_of(&stack())).await;

        assert_eq!(
            report.aborted,
            Some(AbortReason::OneShotTimedOut {
                container: "stack-migrate-1".to_owned()
            })
        );
        assert_eq!(ops.visits(), vec!["create:stack-migrate-1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_one_shot_keeps_its_container_and_archive_for_inspection() {
        // Its logs are the only thing left that says why the migration failed,
        // so neither a rollback nor a cleanup may remove them.
        let ops = StackOps::new(stack()).probing(
            "stack-migrate-1",
            &[ContainerRuntimeState::Exited { exit_code: 1 }],
        );
        run(&ops, &plan_of(&stack())).await;

        let archive = crate::docker::rename::old_name_for("stack-migrate-1", 42);
        assert!(
            !ops.calls()
                .iter()
                .any(|c| c == &format!("remove:{archive}")),
            "the archive must survive a failed migration"
        );
        assert!(
            !ops.calls().iter().any(|c| c.starts_with("rename_to:")),
            "a failed migration is not rolled back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_service_that_rolls_back_stops_the_rest_of_the_project() {
        let members = vec![
            enabled(member("api", "")),
            enabled(member("web", "api:service_healthy:false")),
        ];
        let ops = StackOps::new(members.clone()).probing(
            "stack-api-1",
            &[ContainerRuntimeState::Exited { exit_code: 1 }],
        );
        let report = run(&ops, &plan_of(&members)).await;

        assert_eq!(
            report.aborted,
            Some(AbortReason::RolledBack {
                container: "stack-api-1".to_owned()
            })
        );
        assert!(
            !ops.visits().contains(&"create:stack-web-1".to_owned()),
            "web depends on api; a rolled-back api must not be followed by a web update"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_true_dependent_is_bumped_after_every_target_lands() {
        let members = vec![
            enabled(member("db", "")),
            on_image(member("web", "db:service_healthy:true"), "web:latest"),
        ];
        let ops = StackOps::new(members.clone());
        let report = run(&ops, &plan_of(&members)).await;

        assert_eq!(
            ops.visits(),
            vec!["create:stack-db-1", "restart-stop:stack-web-1"],
            "the dependent is restarted, not recreated, and only after db landed"
        );
        assert!(ops.calls().iter().any(|c| c == "start:stack-web-1"));
        assert_eq!(
            report.steps.last(),
            Some(&RolloutStep::Restarted {
                container: "stack-web-1".to_owned()
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_aborted_rollout_restarts_nothing() {
        let members = vec![
            enabled(member("db", "migrate:service_completed_successfully:false")),
            exited(member("migrate", "")),
            on_image(member("web", "db:service_healthy:true"), "web:latest"),
        ];
        let ops = StackOps::new(members.clone()).probing(
            "stack-migrate-1",
            &[ContainerRuntimeState::Exited { exit_code: 1 }],
        );
        let report = run(&ops, &plan_of(&members)).await;

        assert!(report.aborted.is_some());
        assert!(
            !ops.calls().iter().any(|c| c.starts_with("restart-stop:")),
            "nothing changed, so there is nothing for a dependent to pick up"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dependent_that_cannot_be_restarted_does_not_fail_the_rollout() {
        let members = vec![
            enabled(member("db", "")),
            on_image(member("web", "db:service_healthy:true"), "web:latest"),
        ];
        let mut ops = StackOps::new(members.clone());
        ops.stop_fails_for = Some("stack-web-1".to_owned());
        let report = run(&ops, &plan_of(&members)).await;

        assert!(report.aborted.is_none(), "the update itself still stands");
        assert!(!report.steps.contains(&RolloutStep::Restarted {
            container: "stack-web-1".to_owned()
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_errors_aborts_and_names_the_container() {
        let mut ops = StackOps::new(stack());
        ops.create_fails_for = Some("stack-migrate-1".to_owned());
        let report = run(&ops, &plan_of(&stack())).await;

        assert!(matches!(
            report.aborted,
            Some(AbortReason::StepFailed { ref container, .. }) if container == "stack-migrate-1"
        ));
        assert_eq!(report.not_completed, vec!["stack-migrate-1", "stack-web-1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn touched_lists_every_container_the_rollout_visited() {
        let ops = StackOps::new(stack()).with_successful_migration();
        let report = run(&ops, &plan_of(&stack())).await;
        assert_eq!(report.touched(), vec!["stack-migrate-1", "stack-web-1"]);
    }

    // ── the entry point both callers share ─────────────────────────────────

    async fn rollout_for(ops: &StackOps, container: &str) -> Option<RolloutReport> {
        let labels = ops.member(container).expect("member").labels.clone();
        for_container(
            ops,
            container,
            &labels,
            "app:latest",
            &fast_cfg(),
            &TokioClock,
            PolicyDefaults::default(),
            None,
            &|| 42,
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_container_outside_a_compose_project_is_not_a_rollout() {
        let mut plain = enabled(member("web", ""));
        plain.labels.remove(LABEL_PROJECT);
        let ops = StackOps::new(vec![plain]);
        assert!(rollout_for(&ops, "stack-web-1").await.is_none());
        assert!(
            ops.calls().is_empty(),
            "a non-member must not even list the project"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_service_project_falls_back_to_the_plain_update() {
        let ops = StackOps::new(vec![enabled(member("web", ""))]);
        assert!(rollout_for(&ops, "stack-web-1").await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_project_listing_degrades_to_the_plain_update() {
        // A rollout is an improvement on the plain update, so losing it must
        // never cost the update itself.
        let mut ops = StackOps::new(stack());
        ops.list_project_fails = true;
        assert!(rollout_for(&ops, "stack-web-1").await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_project_with_a_one_shot_does_run_as_a_rollout() {
        let ops = StackOps::new(stack()).with_successful_migration();
        let report = rollout_for(&ops, "stack-web-1").await.expect("a rollout");
        assert_eq!(report.touched(), vec!["stack-migrate-1", "stack-web-1"]);
    }
}
