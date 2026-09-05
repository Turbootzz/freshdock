//! The scheduler daemon (Phase 4, P4-1/P4-2).
//!
//! A single Tokio loop ticks on `cfg.tick`; on each tick it lists running
//! containers, parses each one's [`Policy`], and processes those whose mode is
//! *due*. `live`/`watch` are due every `cfg.poll_interval`; `nightly`/`weekly`/
//! `monthly` fire on a cron schedule ([`crate::cron`]); `off` is skipped.
//!
//! Containers are processed **sequentially** per tick — that alone guarantees
//! "no overlapping checks per container", keeps the daemon within Docker Hub's
//! anonymous rate budget, and lets the loop stay generic over borrowed traits.
//! `MissedTickBehavior::Skip` drops a tick if the previous one ran long.
//!
//! Schedule state lives in memory only and is recomputed from `list_running`
//! each tick: there is no backfill, so a window missed while the daemon was
//! down simply fires at the next occurrence.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Local, TimeDelta};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::config::ResolvedSettings;
use crate::cron::CronExpr;
use crate::docker::check::DockerCheck;
use crate::docker::container_name;
use crate::docker::recreate::{Cleanup, DockerOps, recreate_with_health};
use crate::docker::rename::is_archive_name;
use crate::errors::AppError;
use crate::health::{Clock, HealthConfig, HealthProbe};
use crate::labels::{self, Mode, Policy};
use crate::notify::{Dispatcher, NotifyEvent};
use crate::probe::{self, ProbeOutcome, ProbeTarget};
use crate::registry::Registry;
use crate::rollout::{self, RolloutConfig, RolloutReport, RolloutStep};
use crate::updater::RecreateOutcome;

/// Tunables for the scheduler loop.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Cadence for `live`/`watch` containers.
    pub poll_interval: Duration,
    /// Loop tick granularity; cron modes are evaluated once per tick.
    pub tick: Duration,
    /// Health-gate timing, forwarded to [`recreate_with_health`].
    pub health: HealthConfig,
    /// Our own container's id prefix, when freshdock runs in a container, so
    /// `watch_all` never auto-targets the daemon itself (issue #79).
    pub own_id_prefix: Option<String>,
}

/// Default cron for a calendar mode when no `freshdock.schedule` override is
/// set. `live`/`watch`/`off` are not cron-driven.
fn default_schedule(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Nightly => Some("0 4 * * *"),
        Mode::Weekly => Some("0 4 * * 0"),
        Mode::Monthly => Some("0 4 1 * *"),
        _ => None,
    }
}

fn is_cron_mode(mode: Mode) -> bool {
    matches!(mode, Mode::Nightly | Mode::Weekly | Mode::Monthly)
}

/// Per-container scheduling state, keyed by container name and rebuilt from the
/// live container list each tick.
struct ContainerState {
    /// Last poll time (`live`/`watch` cadence bookkeeping).
    last_checked: Option<DateTime<Local>>,
    /// Next cron fire time (`nightly`/`weekly`/`monthly`).
    next_fire: Option<DateTime<Local>>,
    /// Parsed effective cron, cached so it's parsed once.
    cron: Option<CronExpr>,
    /// Upstream digest of the last `watch`-mode "update available" notification,
    /// so the same available update isn't re-announced every poll (it would
    /// otherwise notify every `poll_interval` until the user acts).
    last_notified_digest: Option<String>,
}

/// Resolve the effective cron for a policy: explicit `freshdock.schedule`
/// overrides the mode default. Only the calendar modes are cron-driven. A bad
/// expression logs and yields `None` (the container won't be scheduled).
fn cron_for(policy: &Policy, name: &str) -> Option<CronExpr> {
    if !is_cron_mode(policy.mode) {
        return None;
    }
    let expr = policy
        .schedule
        .as_deref()
        .or_else(|| default_schedule(policy.mode))?;
    match CronExpr::parse(expr) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(container = %name, %expr, error = %e, "scheduler: invalid cron schedule; container will not be scheduled");
            None
        }
    }
}

/// Seed fresh state on first sight. Cron containers get their next fire from
/// `now` (no backfill); `live`/`watch` start due immediately.
fn seed_state(policy: &Policy, name: &str, now: DateTime<Local>) -> ContainerState {
    let cron = cron_for(policy, name);
    let next_fire = cron.as_ref().and_then(|c| c.next_after(now));
    ContainerState {
        last_checked: None,
        next_fire,
        cron,
        last_notified_digest: None,
    }
}

/// Is this container due to be checked this tick?
fn due(
    policy: &Policy,
    state: &ContainerState,
    now: DateTime<Local>,
    poll_interval: Duration,
) -> bool {
    match policy.mode {
        Mode::Live | Mode::Watch => match state.last_checked {
            None => true,
            Some(t) => now.signed_duration_since(t) >= to_delta(poll_interval),
        },
        Mode::Nightly | Mode::Weekly | Mode::Monthly => {
            matches!(state.next_fire, Some(nf) if now >= nf)
        }
        Mode::Off => false,
    }
}

fn to_delta(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

/// Run the scheduler until `shutdown` flips to `true` (or its sender drops).
/// Generic over the combined Docker trait surface + a [`Registry`], with an
/// injected wall clock (`now_provider`) so cron evaluation is testable.
#[allow(clippy::too_many_arguments)]
pub async fn run_with<D, R>(
    docker: &D,
    registry: &R,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now_provider: impl Fn() -> DateTime<Local>,
    mut shutdown: watch::Receiver<bool>,
    dispatcher: &Dispatcher,
    settings: ResolvedSettings,
) -> Result<(), AppError>
where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
    R: Registry + Sync,
{
    let mut states: HashMap<String, ContainerState> = HashMap::new();
    let mut warned: HashSet<String> = HashSet::new();
    let mut failed: HashMap<String, String> = HashMap::new();
    let mut ticker = tokio::time::interval(cfg.tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let tick_shutdown = shutdown.clone();

    info!(
        poll_interval_s = cfg.poll_interval.as_secs(),
        tick_s = cfg.tick.as_secs(),
        "scheduler started"
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if *tick_shutdown.borrow() {
                    break;
                }
                run_tick(docker, registry, cfg, clock, &now_provider, &mut states, &mut warned, &mut failed, &tick_shutdown, dispatcher, settings).await;
            }
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    info!("scheduler stopped");
    Ok(())
}

/// One tick: list, parse, and process every due container sequentially. Never
/// propagates an error — a per-tick failure logs and the daemon stays up.
#[allow(clippy::too_many_arguments)]
async fn run_tick<D, R>(
    docker: &D,
    registry: &R,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now_provider: &impl Fn() -> DateTime<Local>,
    states: &mut HashMap<String, ContainerState>,
    // Names already warned about (bad labels, the self skip), so those log
    // once per sighting instead of every tick.
    warned: &mut HashSet<String>,
    // Container name to the upstream digest that failed its health gate.
    failed: &mut HashMap<String, String>,
    shutdown: &watch::Receiver<bool>,
    dispatcher: &Dispatcher,
    settings: ResolvedSettings,
) where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
    R: Registry + Sync,
{
    let containers = match docker.list_running().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "scheduler: list_running failed this tick; daemon stays up");
            return;
        }
    };

    let now = now_provider();
    let empty = HashMap::new();
    let mut live: HashSet<String> = HashSet::new();
    // Containers a compose rollout already updated this tick (issue #78).
    // Their schedule state is left alone: a re-probe next tick is cheap.
    let mut handled: HashSet<String> = HashSet::new();

    for c in &containers {
        // Decline new work once shutdown is signalled; the previous container
        // (if any) already finished, so this is the clean "finish in-flight,
        // stop" point. Return without pruning — the daemon is exiting, and a
        // partial pass would drop unvisited containers' schedule state.
        if *shutdown.borrow() {
            return;
        }

        let name = container_name(c);
        if is_archive_name(&name) {
            continue;
        }
        if handled.contains(&name) {
            debug!(container = %name, "scheduler: already updated by a compose rollout this tick");
            live.insert(name);
            continue;
        }
        let policy = match labels::parse_policy(
            c.labels.as_ref().unwrap_or(&empty),
            settings.policy_defaults(),
        ) {
            Ok(p) => p,
            Err(e) => {
                // First sight only; under watch_all this can be a bystander
                // that will never be fixed, and a per-tick warning would
                // repeat forever.
                if warned.insert(name.clone()) {
                    warn!(container = %name, error = %e, "scheduler: invalid freshdock labels; skipping");
                }
                live.insert(name);
                continue;
            }
        };
        if !policy.enabled || policy.mode == Mode::Off {
            continue;
        }
        // Updating ourselves would kill the update mid-flight. An explicit
        // label is still honoured; only the watch_all sweep is held back.
        if policy.auto_enabled
            && crate::selfid::is_own_container(cfg.own_id_prefix.as_deref(), c.id.as_deref())
        {
            if warned.insert(name.clone()) {
                info!(container = %name, "scheduler: watch_all is skipping freshdock's own container; label it explicitly to include it");
            }
            live.insert(name);
            continue;
        }
        live.insert(name.clone());

        // Diagnostics on first sight only, or a fleet full of ignored
        // watchtower labels would re-warn every tick.
        if !states.contains_key(&name) {
            for note in labels::watchtower_diagnostics(c.labels.as_ref().unwrap_or(&empty)) {
                warn!(container = %name, %note, "scheduler: watchtower label");
            }
        }
        let state = states
            .entry(name.clone())
            .or_insert_with(|| seed_state(&policy, &name, now));
        if !due(&policy, state, now, cfg.poll_interval) {
            continue;
        }

        // Ahead of the bookkeeping: a failed inspect must not cost a cron window.
        let target = match probe::resolve_target(docker, c).await {
            Ok(t) => t,
            Err(e) => {
                warn!(container = %name, error = %e, "scheduler: container inspect failed; skipping this tick");
                // Cron windows are scarce; a poll interval is not.
                if matches!(policy.mode, Mode::Live | Mode::Watch) {
                    state.last_checked = Some(now);
                }
                continue;
            }
        };

        // Advance bookkeeping before the (slow) update so a long recreate can't
        // re-fire on the next tick.
        match policy.mode {
            Mode::Live | Mode::Watch => state.last_checked = Some(now),
            Mode::Nightly | Mode::Weekly | Mode::Monthly => {
                state.next_fire = state.cron.as_ref().and_then(|c| c.next_after(now));
            }
            Mode::Off => {}
        }

        let processed = process_container(
            docker,
            registry,
            cfg,
            clock,
            now,
            &name,
            &policy,
            c.labels.as_ref().unwrap_or(&empty),
            &target,
            dispatcher,
            failed.get(&name).cloned(),
            &mut state.last_notified_digest,
            settings,
        )
        .await;

        match processed.outcome {
            Some(UpdateOutcome::Rejected { digest, containers }) => {
                for c in containers {
                    failed.insert(c, digest.clone());
                }
            }
            Some(UpdateOutcome::Landed { containers }) => {
                for c in containers {
                    failed.remove(&c);
                }
            }
            None => {}
        }
        handled.extend(processed.handled);
    }

    states.retain(|k, _| live.contains(k));
    warned.retain(|k| live.contains(k));
    failed.retain(|k, _| live.contains(k));
}

/// What one processed container leaves for the tick to record.
#[derive(Default)]
struct Processed {
    /// Every container an applied update touched, so the tick skips them later.
    handled: Vec<String>,
    outcome: Option<UpdateOutcome>,
}

/// The verdict to remember for the containers whose image was swapped or tried.
enum UpdateOutcome {
    Rejected {
        digest: String,
        containers: Vec<String>,
    },
    Landed {
        containers: Vec<String>,
    },
}

/// Probe one container and act on the verdict: recreate for active modes,
/// report-only for `watch`, skip otherwise. Logs and returns on any failure.
#[allow(clippy::too_many_arguments)]
async fn process_container<D, R>(
    docker: &D,
    registry: &R,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now: DateTime<Local>,
    name: &str,
    policy: &Policy,
    container_labels: &HashMap<String, String>,
    target: &ProbeTarget,
    dispatcher: &Dispatcher,
    last_failed: Option<String>,
    last_notified: &mut Option<String>,
    settings: ResolvedSettings,
) -> Processed
where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
    R: Registry + Sync,
{
    match probe::probe_image(docker, registry, &target.image).await {
        ProbeOutcome::Fetched {
            local,
            latest,
            tag_image_id,
        } => {
            match local.update_available_for(
                &latest,
                target.image_id.as_deref(),
                tag_image_id.as_deref(),
            ) {
                None => {
                    debug!(container = %name, %latest, "scheduler: local digest unknown; not updating");
                    return Processed::default();
                }
                Some(false) => {
                    debug!(container = %name, "scheduler: up to date");
                    return Processed::default();
                }
                Some(true) => {}
            }
            match policy.mode {
                Mode::Watch => {
                    info!(container = %name, %latest, event = "update_available", "scheduler: update available (watch mode — not applied)");
                    // Only notify once per distinct upstream digest, or a watched
                    // update would re-alert every poll until the user acts.
                    if policy.notify && last_notified.as_deref() != Some(latest.as_str()) {
                        dispatcher
                            .dispatch(&NotifyEvent::UpdateAvailable {
                                container: name.to_string(),
                                image: target.image.clone(),
                                latest_digest: latest.clone(),
                            })
                            .await;
                        *last_notified = Some(latest.clone());
                    }
                }
                Mode::Live | Mode::Nightly | Mode::Weekly | Mode::Monthly => {
                    // Retrying the same digest would roll back forever.
                    if last_failed.as_deref() == Some(latest.as_str()) {
                        info!(container = %name, %latest, "scheduler: this digest already failed the health gate; waiting for upstream to move");
                        return Processed::default();
                    }
                    // Meaningful only once the tag carries upstream's digest.
                    let current_image = (local.update_available(&latest) == Some(false))
                        .then_some(tag_image_id.as_deref())
                        .flatten();
                    return apply_update(
                        docker,
                        cfg,
                        clock,
                        now,
                        name,
                        policy,
                        container_labels,
                        target.image.as_str(),
                        &latest,
                        current_image,
                        dispatcher,
                        settings,
                    )
                    .await;
                }
                Mode::Off => {}
            }
        }
        ProbeOutcome::Pinned => {
            debug!(container = %name, "scheduler: image pinned to a digest (no check)");
        }
        ProbeOutcome::AuthRequired => {
            warn!(container = %name, "scheduler: registry requires credentials; set [registry.<name>] creds — not updating");
        }
        ProbeOutcome::CredentialsRejected => {
            warn!(container = %name, "scheduler: configured registry credentials rejected and anonymous denied; check/rotate token — not updating");
        }
        ProbeOutcome::NetworkUnavailable => {
            warn!(container = %name, "scheduler: registry network unavailable; will retry next tick");
        }
        ProbeOutcome::Error(msg) => {
            warn!(container = %name, %msg, "scheduler: digest probe failed; continuing");
        }
    }
    Processed::default()
}

/// Run the health-gated recreate, log its outcome, and (when the container opts
/// in via `policy.notify`) dispatch the matching notification.
#[allow(clippy::too_many_arguments)]
async fn apply_update<D>(
    docker: &D,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now: DateTime<Local>,
    name: &str,
    policy: &Policy,
    container_labels: &HashMap<String, String>,
    image: &str,
    latest: &str,
    tag_image_id: Option<&str>,
    dispatcher: &Dispatcher,
    settings: ResolvedSettings,
) -> Processed
where
    D: DockerOps + HealthProbe + Sync,
{
    let ts = now.timestamp();
    let cleanup = Cleanup {
        remove_replaced: policy.cleanup,
        prune_dangling: settings.prune_dangling,
    };

    // Inside a compose project the unit of work is the project (issue #78).
    // `for_container` returns `None` for anything else.
    if settings.compose_aware
        && let Some(report) = rollout::for_container(
            docker,
            name,
            container_labels,
            &RolloutConfig {
                health: cfg.health.clone(),
                prune_dangling: settings.prune_dangling,
                one_shot_timeout: settings.one_shot_timeout,
                tag_image_id: tag_image_id.map(str::to_owned),
            },
            clock,
            settings.policy_defaults(),
            cfg.own_id_prefix.as_deref(),
            &|| ts,
        )
        .await
    {
        let targets = report.image_targets();
        let outcome = match &report.aborted {
            Some(reason) if reason.rejected_the_image() => Some(UpdateOutcome::Rejected {
                digest: latest.to_owned(),
                containers: targets,
            }),
            Some(_) => None,
            // An in-flight one-shot ran nothing: there is nothing to remember.
            None if report.touched().is_empty() => None,
            None => Some(UpdateOutcome::Landed {
                containers: targets,
            }),
        };
        return Processed {
            handled: report_rollout(&report, policy, image, dispatcher).await,
            outcome,
        };
    }

    let outcome = match recreate_with_health(
        docker,
        name,
        &cfg.health,
        clock,
        cleanup,
        &policy.hooks,
        || ts,
    )
    .await
    {
        Ok(RecreateOutcome::Recreated { old_name, new_id }) => {
            info!(container = %name, archived = %old_name, %new_id, "scheduler: recreated");
            if policy.notify {
                dispatcher
                    .dispatch(&NotifyEvent::UpdateSucceeded {
                        container: name.to_string(),
                        image: image.to_string(),
                        new_id,
                    })
                    .await;
            }
            Some(UpdateOutcome::Landed {
                containers: vec![name.to_owned()],
            })
        }
        Ok(RecreateOutcome::RolledBack(ev)) => {
            warn!(container = %name, reason = ?ev.reason, "scheduler: update unhealthy, rolled back");
            if policy.notify {
                dispatcher
                    .dispatch(&NotifyEvent::UpdateFailed {
                        container: ev.container,
                        reason: ev.reason,
                        old_image_ref: ev.old_image_ref,
                        new_image_ref: ev.new_image_ref,
                        restored_from: ev.restored_from,
                    })
                    .await;
            }
            Some(UpdateOutcome::Rejected {
                digest: latest.to_owned(),
                containers: vec![name.to_owned()],
            })
        }
        Ok(RecreateOutcome::SkippedByHook(reason)) => {
            // Deliberate skip, not a failure: the bookkeeping already advanced,
            // so the next due cycle simply tries again.
            info!(container = %name, %reason, "scheduler: update skipped by pre-update hook; will retry when next due");
            None
        }
        Err(e) => {
            warn!(container = %name, error = %e, "scheduler: recreate failed; daemon continues");
            None
        }
    };
    Processed {
        handled: vec![name.to_string()],
        outcome,
    }
}

/// Log a finished rollout and notify on it. Success stays per-container, the
/// notification operators already have rules for, and each one honours its own
/// `freshdock.notify`. An abort is per project, so it follows the policy of the
/// container that triggered the rollout.
async fn report_rollout(
    report: &RolloutReport,
    policy: &Policy,
    image: &str,
    dispatcher: &Dispatcher,
) -> Vec<String> {
    let claimed = report.claimed();
    for step in &report.steps {
        match step {
            RolloutStep::OneShotCompleted { container } => {
                info!(project = %report.project, %container, "scheduler: rollout re-ran one-shot")
            }
            RolloutStep::Updated {
                container,
                new_id,
                notify,
            } => {
                info!(project = %report.project, %container, %new_id, "scheduler: rollout updated service");
                // This container's own `freshdock.notify`, not the trigger's: a
                // sibling that opted out of notifications must not be announced
                // just because the container that started the rollout opted in.
                if *notify {
                    dispatcher
                        .dispatch(&NotifyEvent::UpdateSucceeded {
                            container: container.clone(),
                            image: image.to_string(),
                            new_id: new_id.clone(),
                        })
                        .await;
                }
            }
            RolloutStep::Restarted { container } => {
                info!(project = %report.project, %container, "scheduler: rollout restarted dependent")
            }
        }
    }
    if let Some(reason) = &report.aborted {
        warn!(project = %report.project, %reason, "scheduler: rollout aborted");
        if policy.notify {
            dispatcher
                .dispatch(&NotifyEvent::RolloutAborted {
                    project: report.project.clone(),
                    reason: reason.to_string(),
                    // `touched`, not `updated_containers`: a migration that
                    // completed before the abort has already moved the schema.
                    completed: report.touched().into_iter().map(str::to_string).collect(),
                    remaining: report.not_completed.clone(),
                })
                .await;
        }
    }
    claimed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;
    use crate::docker::check::{ContainerImage, LocalImage};
    use crate::docker::spec::ContainerSpec;
    use crate::health::{ContainerRuntimeState, TokioClock};
    use crate::registry::{Digest, ImageRef, RegistryError};
    use async_trait::async_trait;
    use bollard::models::{ContainerConfig, ContainerSummary};
    use chrono::TimeZone;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIG_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIG_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIG_C: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn cfg() -> SchedulerConfig {
        SchedulerConfig {
            poll_interval: Duration::from_secs(300),
            tick: Duration::from_secs(60),
            health: HealthConfig::default(),
            own_id_prefix: None,
        }
    }

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 2, 12, 0, 0).unwrap()
    }

    fn policy(mode: Mode, schedule: Option<&str>) -> Policy {
        Policy {
            enabled: true,
            mode,
            notify: false,
            schedule: schedule.map(str::to_owned),
            cleanup: false,
            hooks: crate::labels::LifecycleHooks::default(),
            auto_enabled: false,
        }
    }

    // --- pure due / seeding logic ---

    #[test]
    fn live_is_due_on_first_sight_then_after_the_interval() {
        let p = policy(Mode::Live, None);
        let st = seed_state(&p, "c", now());
        assert!(
            due(&p, &st, now(), cfg().poll_interval),
            "first sight is due"
        );

        let st = ContainerState {
            last_checked: Some(now()),
            ..seed_state(&p, "c", now())
        };
        // 4 minutes later: not yet (interval is 5).
        let t4 = now() + TimeDelta::minutes(4);
        assert!(!due(&p, &st, t4, cfg().poll_interval));
        // 5 minutes later: due again.
        let t5 = now() + TimeDelta::minutes(5);
        assert!(due(&p, &st, t5, cfg().poll_interval));
    }

    #[test]
    fn off_mode_is_never_due() {
        let p = policy(Mode::Off, None);
        let st = seed_state(&p, "c", now());
        assert!(!due(
            &p,
            &st,
            now() + TimeDelta::days(365),
            cfg().poll_interval
        ));
    }

    #[test]
    fn cron_mode_is_not_due_until_the_window_and_does_not_backfill() {
        let p = policy(Mode::Nightly, None); // default 0 4 * * *
        let st = seed_state(&p, "c", now()); // seeded at 12:00 → next fire tomorrow 04:00
        assert!(
            !due(&p, &st, now(), cfg().poll_interval),
            "no immediate backfill"
        );
        // Just before the window.
        let before = Local.with_ymd_and_hms(2026, 6, 3, 3, 59, 0).unwrap();
        assert!(!due(&p, &st, before, cfg().poll_interval));
        // At the window.
        let at = Local.with_ymd_and_hms(2026, 6, 3, 4, 0, 0).unwrap();
        assert!(due(&p, &st, at, cfg().poll_interval));
    }

    #[test]
    fn schedule_override_beats_the_mode_default() {
        let p = policy(Mode::Nightly, Some("30 2 * * *"));
        let st = seed_state(&p, "c", now());
        // default 04:00 is not the fire time; 02:30 next day is.
        let expected = Local.with_ymd_and_hms(2026, 6, 3, 2, 30, 0).unwrap();
        assert_eq!(st.next_fire, Some(expected));
    }

    #[test]
    fn invalid_schedule_leaves_a_cron_container_unscheduled() {
        let p = policy(Mode::Weekly, Some("not a cron"));
        let st = seed_state(&p, "c", now());
        assert!(st.cron.is_none());
        assert!(st.next_fire.is_none());
        assert!(!due(
            &p,
            &st,
            now() + TimeDelta::days(30),
            cfg().poll_interval
        ));
    }

    // --- run_tick behaviour with a recording fake ---

    fn summary(name: &str, image: &str, labels: &[(&str, &str)]) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{name}")]),
            id: Some(format!("id-{name}")),
            image: Some(image.to_owned()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// Fake that satisfies the whole Docker surface the scheduler needs. It
    /// serves a fixed container list + local digest, reports healthy, and
    /// counts `create_from_spec` calls so "did a recreate happen?" is checkable.
    struct FakeNode {
        containers: Mutex<Vec<ContainerSummary>>,
        /// Every digest the local image is recorded under — a list so the
        /// republished-index case (#74) is expressible.
        local_digests: Vec<String>,
        /// Image override keyed by reference or id, so a moved tag can disagree.
        images: Mutex<HashMap<String, LocalImage>>,
        /// What a pull does to the local store: the tag then resolves here.
        on_pull: Vec<(String, LocalImage)>,
        /// Container id to `Config.Image`, when it differs from the listing.
        config_images: HashMap<String, String>,
        failing: Mutex<HashSet<String>>,
        image_inspects: AtomicUsize,
        container_inspects: AtomicUsize,
        hook_calls: AtomicUsize,
        list_fails: bool,
        creates: AtomicUsize,
        /// State the (recreated) container reports to the health gate. Default
        /// healthy; `unhealthy()` makes it crash so the gate rolls back.
        health_state: ContainerRuntimeState,
        /// Verdict `exec_hook` returns; default exit 0.
        hook_status: crate::docker::recreate::HookStatus,
        /// What `list_project` reports (#78). Empty means "not a compose
        /// project", which is what every pre-#78 test wants.
        project_members: Vec<crate::compose::ProjectMember>,
    }

    impl FakeNode {
        fn new(containers: Vec<ContainerSummary>, local_digest: &str) -> Self {
            Self::with_digests(containers, &[local_digest])
        }
        /// The image recorded under several manifest digests, as a republished
        /// multi-arch index leaves it (#74).
        fn with_digests(containers: Vec<ContainerSummary>, local_digests: &[&str]) -> Self {
            Self {
                containers: Mutex::new(containers),
                local_digests: local_digests.iter().map(|d| (*d).to_owned()).collect(),
                images: Mutex::new(HashMap::new()),
                on_pull: Vec::new(),
                config_images: HashMap::new(),
                failing: Mutex::new(HashSet::new()),
                image_inspects: AtomicUsize::new(0),
                container_inspects: AtomicUsize::new(0),
                hook_calls: AtomicUsize::new(0),
                list_fails: false,
                creates: AtomicUsize::new(0),
                health_state: ContainerRuntimeState::HealthHealthy,
                hook_status: crate::docker::recreate::HookStatus::Completed { exit_code: 0 },
                project_members: Vec::new(),
            }
        }
        /// What the daemon reports for one image reference or id.
        fn with_image(self, key: &str, image: LocalImage) -> Self {
            self.images.lock().unwrap().insert(key.to_owned(), image);
            self
        }
        /// After pulling `image`, the local store resolves it to `after`.
        fn on_pull(mut self, image: &str, after: LocalImage) -> Self {
            self.on_pull.push((image.to_owned(), after));
            self
        }
        /// `Config.Image` for one container, when it differs from the listing.
        fn with_config_image(mut self, id: &str, reference: &str) -> Self {
            self.config_images
                .insert(id.to_owned(), reference.to_owned());
            self
        }
        /// Make this container's inspect fail until [`Self::heal`].
        fn failing_container(self, id: &str) -> Self {
            self.failing.lock().unwrap().insert(id.to_owned());
            self
        }
        /// Let every container inspect succeed again.
        fn heal(&self) {
            self.failing.lock().unwrap().clear();
        }
        fn image_inspects(&self) -> usize {
            self.image_inspects.load(Ordering::SeqCst)
        }
        fn container_inspects(&self) -> usize {
            self.container_inspects.load(Ordering::SeqCst)
        }
        fn hook_calls(&self) -> usize {
            self.hook_calls.load(Ordering::SeqCst)
        }
        /// Serve a compose project from `list_project`.
        fn in_project(mut self, members: Vec<crate::compose::ProjectMember>) -> Self {
            self.project_members = members;
            self
        }
        fn failing() -> Self {
            Self {
                list_fails: true,
                ..Self::new(vec![], DIG_A)
            }
        }
        /// Make the recreated container crash so the health gate rolls back.
        fn unhealthy(mut self) -> Self {
            self.health_state = ContainerRuntimeState::Exited { exit_code: 1 };
            self
        }
        /// Make the pre-update hook veto the update (non-zero exit).
        fn hook_refuses(mut self) -> Self {
            self.hook_status = crate::docker::recreate::HookStatus::Completed { exit_code: 75 };
            self
        }
        fn creates(&self) -> usize {
            self.creates.load(Ordering::SeqCst)
        }
    }

    fn err() -> DockerError {
        DockerError::Spec(crate::docker::spec::SpecError::Missing("test"))
    }

    #[async_trait]
    impl DockerCheck for FakeNode {
        async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
            if self.list_fails {
                Err(err())
            } else {
                Ok(self.containers.lock().unwrap().clone())
            }
        }
        async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError> {
            self.image_inspects.fetch_add(1, Ordering::SeqCst);
            if let Some(configured) = self.images.lock().unwrap().get(image) {
                return Ok(configured.clone());
            }
            // Report the local digest under the image's repo so the probe's
            // RepoDigests match succeeds.
            let repo = image.split(':').next().unwrap_or(image);
            Ok(LocalImage {
                id: None,
                repo_digests: self
                    .local_digests
                    .iter()
                    .map(|d| format!("{repo}@{d}"))
                    .collect(),
            })
        }
        async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError> {
            self.container_inspects.fetch_add(1, Ordering::SeqCst);
            if self.failing.lock().unwrap().contains(id) {
                return Err(err());
            }
            let listed = self
                .containers
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.id.as_deref() == Some(id))
                .cloned()
                .ok_or_else(err)?;
            let reference = self
                .config_images
                .get(id)
                .cloned()
                .or(listed.image)
                .ok_or_else(err)?;
            Ok(ContainerImage {
                reference,
                image_id: listed.image_id,
            })
        }
    }

    #[async_trait]
    impl DockerOps for FakeNode {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: "alpine:3.19".to_owned(),
                image_id: Some("sha256:oldimg".to_owned()),
                config: ContainerConfig::default(),
                host_config: None,
                network_endpoints: None,
            })
        }
        async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
            for (image, after) in &self.on_pull {
                if ImageRef::parse(image) == *image_ref {
                    self.images
                        .lock()
                        .unwrap()
                        .insert(image.clone(), after.clone());
                }
            }
            Ok(())
        }
        async fn stop(
            &self,
            _name: &str,
            _signal: Option<&str>,
            _timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            Ok(())
        }
        async fn rename(&self, _name: &str, ts_unix: i64) -> Result<String, DockerError> {
            Ok(format!("c-old-{ts_unix}"))
        }
        async fn create_from_spec(
            &self,
            name: &str,
            spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            // Model the update: the container now runs what the tag resolves to.
            let current = self
                .images
                .lock()
                .unwrap()
                .get(&spec.image_ref)
                .and_then(|i| i.id.clone());
            if let Some(id) = current {
                for c in self.containers.lock().unwrap().iter_mut() {
                    if container_name(c) == name {
                        c.image_id = Some(id.clone());
                    }
                }
            }
            Ok("new-id".to_owned())
        }
        async fn start(&self, _name_or_id: &str) -> Result<(), DockerError> {
            Ok(())
        }
        async fn remove(&self, _name_or_id: &str, _force: bool) -> Result<(), DockerError> {
            Ok(())
        }
        async fn rename_to(&self, _from: &str, _to: &str) -> Result<(), DockerError> {
            Ok(())
        }
        async fn remove_image(&self, _id: &str, _force: bool) -> Result<(), DockerError> {
            Ok(())
        }
        async fn prune_dangling_images(&self) -> Result<(), DockerError> {
            Ok(())
        }
        async fn exec_hook(
            &self,
            _name_or_id: &str,
            _command: &str,
            _timeout: Option<std::time::Duration>,
        ) -> Result<crate::docker::recreate::HookStatus, DockerError> {
            self.hook_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.hook_status)
        }
        async fn list_network_dependents(&self, _name: &str) -> Result<Vec<String>, DockerError> {
            Ok(vec![])
        }
        async fn list_project(
            &self,
            _project: &str,
        ) -> Result<Vec<crate::compose::ProjectMember>, DockerError> {
            Ok(self.project_members.clone())
        }
    }

    #[async_trait]
    impl HealthProbe for FakeNode {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            Ok(self.health_state)
        }
    }

    struct FakeRegistry {
        digest: Mutex<String>,
        network_down: bool,
        auth_required: bool,
        calls: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(digest: &str) -> Self {
            Self {
                digest: Mutex::new(digest.to_owned()),
                network_down: false,
                auth_required: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn offline() -> Self {
            Self {
                digest: Mutex::new(DIG_B.to_owned()),
                network_down: true,
                auth_required: false,
                calls: AtomicUsize::new(0),
            }
        }
        /// Publish a different digest upstream, between ticks.
        fn set_digest(&self, digest: &str) {
            *self.digest.lock().unwrap() = digest.to_owned();
        }
        fn auth_required() -> Self {
            Self {
                digest: Mutex::new(DIG_B.to_owned()),
                network_down: false,
                auth_required: true,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Registry for FakeRegistry {
        async fn fetch_digest(&self, _image: &ImageRef) -> Result<Digest, RegistryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.network_down {
                Err(RegistryError::NetworkUnavailable("test".into()))
            } else if self.auth_required {
                Err(RegistryError::Auth("no credentials".into()))
            } else {
                Ok(Digest(self.digest.lock().unwrap().clone()))
            }
        }
    }

    /// Drive a single `run_tick` with a fresh (not-shutting-down) state map.
    async fn one_tick(node: &FakeNode, reg: &FakeRegistry) -> HashMap<String, ContainerState> {
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();
        run_tick(
            node,
            reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        states
    }

    /// Like [`one_tick`] but with a caller-supplied dispatcher, for asserting
    /// which notifications a tick produces.
    async fn one_tick_with(node: &FakeNode, reg: &FakeRegistry, dispatcher: &Dispatcher) {
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();
        run_tick(
            node,
            reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            dispatcher,
            ResolvedSettings::default(),
        )
        .await;
    }

    /// Everything a tick carries forward.
    #[derive(Default)]
    struct TickState {
        states: HashMap<String, ContainerState>,
        warned: HashSet<String>,
        failed: HashMap<String, String>,
    }

    /// Drive one tick with a caller-supplied config, settings, and carried state.
    async fn one_tick_cfg(
        node: &FakeNode,
        reg: &FakeRegistry,
        cfg: &SchedulerConfig,
        settings: ResolvedSettings,
        st: &mut TickState,
    ) {
        let (_tx, rx) = watch::channel(false);
        run_tick(
            node,
            reg,
            cfg,
            &TokioClock,
            &now,
            &mut st.states,
            &mut st.warned,
            &mut st.failed,
            &rx,
            &Dispatcher::noop(),
            settings,
        )
        .await;
    }

    #[tokio::test]
    async fn live_container_with_new_digest_is_recreated() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B); // upstream differs → update
        one_tick(&node, &reg).await;
        assert_eq!(
            node.creates(),
            1,
            "a changed digest must trigger a recreate"
        );
    }

    #[tokio::test]
    async fn live_container_up_to_date_is_not_recreated() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_A); // same digest
        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0, "matching digests must not recreate");
    }

    #[tokio::test]
    async fn live_container_is_not_recreated_when_the_index_was_republished() {
        // Issue #74: the local image is still recorded under an older index
        // digest, but upstream's current digest is among its RepoDigests — the
        // platform manifest never changed, so there is nothing to apply.
        let node = FakeNode::with_digests(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            &[DIG_A, DIG_B],
        );
        let reg = FakeRegistry::new(DIG_B);
        one_tick(&node, &reg).await;
        assert_eq!(
            node.creates(),
            0,
            "the upstream digest is already present locally — recreating would loop forever"
        );
    }

    #[tokio::test]
    async fn locally_built_container_with_no_repo_digests_is_not_recreated() {
        // Nothing to compare against; recreating would pull an unrelated
        // registry image over a local build.
        let node = FakeNode::with_digests(
            vec![summary(
                "app",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            &[],
        );
        let reg = FakeRegistry::new(DIG_B);
        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0, "an unknown local digest must not update");
    }

    #[tokio::test]
    async fn watch_container_never_recreates_even_with_a_new_digest() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "watch")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B); // upstream differs
        one_tick(&node, &reg).await;
        assert_eq!(
            node.creates(),
            0,
            "watch mode reports updates but must never pull or recreate"
        );
    }

    #[tokio::test]
    async fn registry_requiring_auth_is_probed_but_never_recreates() {
        // Phase 5: a non-Docker-Hub image is now probed. With no credentials the
        // registry reports AuthRequired, which must not trigger a recreate (and
        // must not loop into a failing pull).
        let node = FakeNode::new(
            vec![summary(
                "priv",
                "ghcr.io/owner/repo:v1",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::auth_required();
        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0, "auth-required must not recreate");
        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            1,
            "the image is probed now"
        );
    }

    #[tokio::test]
    async fn pinned_image_is_skipped_without_io() {
        let node = FakeNode::new(
            vec![summary(
                "pinned",
                "alpine@sha256:abcabc",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0);
        assert_eq!(reg.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn network_unavailable_does_not_recreate_and_keeps_running() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::offline();
        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0);
    }

    #[tokio::test]
    async fn list_running_failure_is_swallowed() {
        let node = FakeNode::failing();
        let reg = FakeRegistry::new(DIG_B);
        // Must not panic; just returns having done nothing.
        let states = one_tick(&node, &reg).await;
        assert!(states.is_empty());
        assert_eq!(node.creates(), 0);
    }

    #[tokio::test]
    async fn disabled_and_off_containers_are_ignored() {
        let node = FakeNode::new(
            vec![
                summary("off", "alpine:3.19", &[("freshdock.enable", "false")]),
                summary(
                    "ignored",
                    "redis:7",
                    &[("freshdock.enable", "true"), ("freshdock.mode", "off")],
                ),
            ],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        let states = one_tick(&node, &reg).await;
        assert!(states.is_empty(), "neither container should be scheduled");
        assert_eq!(node.creates(), 0);
    }

    #[tokio::test]
    async fn archive_containers_in_the_list_are_ignored() {
        let node = FakeNode::new(
            vec![summary(
                "web-old-1700000000",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        let states = one_tick(&node, &reg).await;
        assert!(states.is_empty(), "archives must be filtered out");
        assert_eq!(node.creates(), 0);
    }

    #[tokio::test]
    async fn vanished_containers_are_pruned_from_state() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "watch")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_A);
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();

        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert!(states.contains_key("web"));

        // Container disappears; next tick prunes it.
        node.containers.lock().unwrap().clear();
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert!(
            states.is_empty(),
            "pruned after vanishing from list_running"
        );
    }

    #[tokio::test]
    async fn shutdown_flag_declines_new_work() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        let (_tx, rx) = watch::channel(true); // already shutting down
        let mut states = HashMap::new();
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert_eq!(
            node.creates(),
            0,
            "no work starts once shutdown is signalled"
        );
    }

    #[tokio::test]
    async fn run_with_exits_promptly_when_shutdown_is_already_set() {
        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        let (_tx, rx) = watch::channel(true);
        run_with(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            now,
            rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await
        .unwrap();
        assert_eq!(node.creates(), 0, "a pre-set shutdown processes nothing");
    }

    #[tokio::test]
    async fn cron_container_fires_at_its_window_then_advances_without_refiring() {
        let node = FakeNode::new(
            vec![summary(
                "nightly",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "nightly")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B); // upstream differs → would recreate
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();

        // An injectable wall clock so we can step across the 04:00 window.
        let clock = std::cell::Cell::new(Local.with_ymd_and_hms(2026, 6, 2, 3, 59, 0).unwrap());
        let now_fn = || clock.get();

        // 03:59 seeds the container (default `0 4 * * *`) → not yet due.
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now_fn,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert_eq!(node.creates(), 0, "not due before the window");

        // 04:00 → due → recreate, and next_fire advances to tomorrow.
        clock.set(Local.with_ymd_and_hms(2026, 6, 2, 4, 0, 0).unwrap());
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now_fn,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert_eq!(node.creates(), 1, "fires at the window");

        // 04:01 → next_fire is tomorrow now, so it must not re-fire.
        clock.set(Local.with_ymd_and_hms(2026, 6, 2, 4, 1, 0).unwrap());
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now_fn,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            ResolvedSettings::default(),
        )
        .await;
        assert_eq!(node.creates(), 1, "does not re-fire after firing");
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_breaks_promptly_on_a_mid_park_shutdown_signal() {
        // No containers → ticks are cheap; a long tick proves the loop wakes on
        // the signal itself, not by waiting out the interval.
        let node = FakeNode::new(vec![], DIG_A);
        let reg = FakeRegistry::new(DIG_A);
        let (tx, rx) = watch::channel(false);
        let big_cfg = SchedulerConfig {
            poll_interval: Duration::from_secs(3600),
            tick: Duration::from_secs(3600),
            health: HealthConfig::default(),
            own_id_prefix: None,
        };

        let handle = tokio::spawn(async move {
            run_with(
                &node,
                &reg,
                &big_cfg,
                &TokioClock,
                now,
                rx,
                &Dispatcher::noop(),
                ResolvedSettings::default(),
            )
            .await
        });

        // Let the first immediate tick run and the loop park on `select!`.
        tokio::time::sleep(Duration::from_millis(1)).await;
        tx.send(true).unwrap();

        // Must return well within the 3600 s tick interval.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run_with returns promptly after the signal")
            .expect("scheduler task joins")
            .expect("run_with ok");
    }

    // --- watch_all opt-out mode + self guard (issue #79) ---

    const SELF_ID: &str = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";
    const SELF_PREFIX: &str = "abc123def456";

    /// Opt-out mode with an active default mode, so "was it processed?" shows
    /// up as a recreate.
    fn watch_all_settings() -> ResolvedSettings {
        ResolvedSettings {
            watch_all: true,
            default_mode: Some(Mode::Live),
            ..Default::default()
        }
    }

    // --- compose project rollouts (issue #78) ---

    const COMPOSE_IMAGE: &str = "app:latest";

    /// A running, live-mode compose member of project `stack`.
    fn compose_summary(service: &str, deps: &str) -> ContainerSummary {
        compose_summary_with(service, deps, &[])
    }

    /// As [`compose_summary`], plus any extra freshdock labels.
    fn compose_summary_with(service: &str, deps: &str, extra: &[(&str, &str)]) -> ContainerSummary {
        let mut labels = vec![
            ("freshdock.enable", "true"),
            ("freshdock.mode", "live"),
            (crate::compose::LABEL_PROJECT, "stack"),
            (crate::compose::LABEL_SERVICE, service),
            (crate::compose::LABEL_DEPENDS_ON, deps),
        ];
        labels.extend_from_slice(extra);
        summary(&format!("stack-{service}-1"), COMPOSE_IMAGE, &labels)
    }

    /// The same container as `list_project` reports it.
    fn compose_member(summary: &ContainerSummary) -> crate::compose::ProjectMember {
        crate::compose::ProjectMember {
            name: container_name(summary),
            id: summary.id.clone().unwrap_or_default(),
            image_ref: COMPOSE_IMAGE.to_owned(),
            image_id: Some("sha256:app".to_owned()),
            labels: summary.labels.clone().unwrap_or_default(),
            running: true,
        }
    }

    fn compose_node() -> (FakeNode, Vec<ContainerSummary>) {
        let running = vec![compose_summary("web", ""), compose_summary("worker", "")];
        let members = running.iter().map(compose_member).collect();
        (
            FakeNode::new(running.clone(), DIG_A).in_project(members),
            running,
        )
    }

    async fn tick_with_settings(node: &FakeNode, reg: &FakeRegistry, settings: ResolvedSettings) {
        let (_tx, rx) = watch::channel(false);
        run_tick(
            node,
            reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut HashMap::new(),
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &Dispatcher::noop(),
            settings,
        )
        .await;
    }

    #[tokio::test]
    async fn a_rollout_updates_the_whole_project_from_one_probe() {
        let (node, _) = compose_node();
        let reg = FakeRegistry::new(DIG_B);
        tick_with_settings(&node, &reg, ResolvedSettings::default()).await;

        assert_eq!(node.creates(), 2, "both project members are updated");
        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            1,
            "the second member was updated by the rollout, so the tick must not \
             probe or update it a second time"
        );
    }

    #[tokio::test]
    async fn compose_aware_off_keeps_the_old_per_container_behaviour() {
        let (node, _) = compose_node();
        let reg = FakeRegistry::new(DIG_B);
        tick_with_settings(
            &node,
            &reg,
            ResolvedSettings {
                compose_aware: false,
                ..Default::default()
            },
        )
        .await;

        assert_eq!(node.creates(), 2);
        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            2,
            "with rollouts off, each container is probed and updated on its own"
        );
    }

    #[tokio::test]
    async fn an_aborted_rollout_is_not_re_triggered_by_the_members_it_never_reached() {
        // Without this, every remaining member re-enters the rollout later in
        // the same tick and re-runs the step that already failed, once each.
        let (node, _) = compose_node();
        let node = FakeNode {
            health_state: ContainerRuntimeState::Exited { exit_code: 1 },
            ..node
        };
        let reg = FakeRegistry::new(DIG_B);
        tick_with_settings(&node, &reg, ResolvedSettings::default()).await;

        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            1,
            "the aborted rollout owns every member it planned, reached or not"
        );
    }

    #[tokio::test]
    async fn a_sibling_that_opted_out_of_notifications_is_not_announced() {
        // Both are updated by one rollout, but only `web` asked to be told
        // about it. The trigger's policy must not speak for the other.
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(body_partial_json(
                json!({"event": "succeeded", "container": "stack-web-1"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(wm_method("POST"))
            .and(body_partial_json(
                json!({"event": "succeeded", "container": "stack-worker-1"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let running = vec![
            compose_summary_with("web", "", &[("freshdock.notify", "true")]),
            compose_summary_with("worker", "", &[("freshdock.notify", "false")]),
        ];
        let members = running.iter().map(compose_member).collect();
        let node = FakeNode::new(running, DIG_A).in_project(members);
        let reg = FakeRegistry::new(DIG_B);

        let (_tx, rx) = watch::channel(false);
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now,
            &mut HashMap::new(),
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &webhook_dispatcher(server.uri()),
            ResolvedSettings::default(),
        )
        .await;

        assert_eq!(node.creates(), 2, "both are still updated");
    }

    #[tokio::test]
    async fn a_container_that_is_not_a_compose_member_is_untouched_by_the_feature() {
        // The regression guard for every non-compose fleet: one probe, one
        // update, exactly as before #78.
        let node = FakeNode::new(
            vec![summary(
                "solo",
                "alpine:3.19",
                &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        tick_with_settings(&node, &reg, ResolvedSettings::default()).await;

        assert_eq!(node.creates(), 1);
        assert_eq!(reg.calls.load(Ordering::SeqCst), 1);
    }

    /// Give a summary a fixed container id, for the self-guard tests.
    fn with_id(summary: ContainerSummary, id: &str) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_owned()),
            ..summary
        }
    }

    /// Give a summary the id of the image it actually runs.
    fn with_image_id(summary: ContainerSummary, image_id: &str) -> ContainerSummary {
        ContainerSummary {
            image_id: Some(image_id.to_owned()),
            ..summary
        }
    }

    /// Config whose own-id prefix matches [`SELF_ID`].
    fn self_cfg() -> SchedulerConfig {
        SchedulerConfig {
            own_id_prefix: Some(SELF_PREFIX.to_owned()),
            ..cfg()
        }
    }

    #[tokio::test]
    async fn watch_all_processes_an_unlabelled_container() {
        let node = FakeNode::new(vec![summary("web", "alpine:3.19", &[])], DIG_A);
        let reg = FakeRegistry::new(DIG_B);
        one_tick_cfg(
            &node,
            &reg,
            &cfg(),
            watch_all_settings(),
            &mut TickState::default(),
        )
        .await;
        assert_eq!(
            node.creates(),
            1,
            "an unlabelled container is a target under watch_all"
        );
    }

    #[tokio::test]
    async fn watch_all_skips_freshdocks_own_container() {
        let node = FakeNode::new(
            vec![with_id(summary("freshdock", "alpine:3.19", &[]), SELF_ID)],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        one_tick_cfg(
            &node,
            &reg,
            &self_cfg(),
            watch_all_settings(),
            &mut TickState::default(),
        )
        .await;
        assert_eq!(node.creates(), 0, "the daemon must not auto-update itself");
        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            0,
            "and must not even probe itself"
        );
    }

    #[tokio::test]
    async fn explicitly_enabled_own_container_is_still_updated() {
        let node = FakeNode::new(
            vec![with_id(
                summary(
                    "freshdock",
                    "alpine:3.19",
                    &[("freshdock.enable", "true"), ("freshdock.mode", "live")],
                ),
                SELF_ID,
            )],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        one_tick_cfg(
            &node,
            &reg,
            &self_cfg(),
            watch_all_settings(),
            &mut TickState::default(),
        )
        .await;
        assert_eq!(
            node.creates(),
            1,
            "an explicit opt-in label overrides the self guard"
        );
    }

    /// A second dispatcher, registered once and never dropped, that discards
    /// everything. It keeps tracing-core off its single-dispatcher fast path,
    /// where a parallel test holding no subscriber can blank the capture below.
    /// Fuller note on the twin in [`crate::notify`]'s tests.
    static KEEPALIVE: std::sync::LazyLock<tracing::Dispatch> = std::sync::LazyLock::new(|| {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_ansi(false)
                .finish(),
        )
    });

    #[tokio::test]
    async fn own_container_skip_logs_once_across_ticks() {
        use std::io::Write;
        use std::sync::Arc;
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        std::sync::LazyLock::force(&KEEPALIVE);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_ansi(false)
            .finish();

        let node = FakeNode::new(
            vec![with_id(summary("freshdock", "alpine:3.19", &[]), SELF_ID)],
            DIG_A,
        );
        let reg = FakeRegistry::new(DIG_B);
        let cfg = self_cfg();
        let mut st = TickState::default();
        async {
            one_tick_cfg(&node, &reg, &cfg, watch_all_settings(), &mut st).await;
            one_tick_cfg(&node, &reg, &cfg, watch_all_settings(), &mut st).await;
        }
        .with_subscriber(subscriber)
        .await;

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out.matches("skipping freshdock's own container").count(),
            1,
            "the skip must log on first sight only, not every tick: {out}"
        );
    }

    // --- end-to-end: scheduler outcome → real dispatcher → mock HTTP target ---
    //
    // These drive the real `run_tick` → updater → health gate → rollback path
    // with a real `Dispatcher` (one webhook target) pointed at a wiremock
    // server, so the notification a given outcome produces is asserted on the
    // wire. The only fake is the Docker trait surface (`FakeNode`).

    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method as wm_method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A dispatcher with one webhook target (subscribed to all triggers) aimed
    /// at `uri`.
    fn webhook_dispatcher(uri: String) -> Dispatcher {
        use crate::config::{NotificationConfig, NotificationTarget, Secret};
        let mut targets = std::collections::HashMap::new();
        targets.insert(
            "hook".to_string(),
            NotificationTarget::Webhook {
                url: Secret::new(uri),
                triggers: None,
            },
        );
        Dispatcher::from_config(NotificationConfig { targets }, crate::http::client())
    }

    fn notifying_container(mode: &str, notify: bool) -> Vec<ContainerSummary> {
        vec![summary(
            "web",
            "alpine:3.19",
            &[
                ("freshdock.enable", "true"),
                ("freshdock.mode", mode),
                ("freshdock.notify", if notify { "true" } else { "false" }),
            ],
        )]
    }

    #[tokio::test]
    async fn watch_update_available_notifies_when_opted_in() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(body_partial_json(
                json!({"event": "available", "container": "web"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let node = FakeNode::new(notifying_container("watch", true), DIG_A);
        let reg = FakeRegistry::new(DIG_B); // upstream differs → update available
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
        assert_eq!(node.creates(), 0, "watch never recreates");
        // .expect(1) verified on server drop.
    }

    #[tokio::test]
    async fn watch_up_to_date_sends_no_available_notification() {
        // notify=true but the upstream digest matches local → no update → the
        // "available" notification must NOT fire (guards against alert spam).
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let node = FakeNode::new(notifying_container("watch", true), DIG_A);
        let reg = FakeRegistry::new(DIG_A); // same digest → up to date
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
    }

    #[tokio::test]
    async fn watch_available_notifies_once_until_the_digest_changes() {
        // Two polls of the same available update must produce only one
        // notification (no re-alert every poll_interval).
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let node = FakeNode::new(notifying_container("watch", true), DIG_A);
        let reg = FakeRegistry::new(DIG_B); // update available, unchanged across polls
        let dispatcher = webhook_dispatcher(server.uri());
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();

        let clock = std::cell::Cell::new(Local.with_ymd_and_hms(2026, 6, 2, 12, 0, 0).unwrap());
        let now_fn = || clock.get();

        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now_fn,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &dispatcher,
            ResolvedSettings::default(),
        )
        .await;
        // 10 min later → past the 5 min poll interval, so watch is due again;
        // same digest → must NOT re-notify.
        clock.set(Local.with_ymd_and_hms(2026, 6, 2, 12, 10, 0).unwrap());
        run_tick(
            &node,
            &reg,
            &cfg(),
            &TokioClock,
            &now_fn,
            &mut states,
            &mut HashSet::new(),
            &mut HashMap::new(),
            &rx,
            &dispatcher,
            ResolvedSettings::default(),
        )
        .await;
    }

    #[tokio::test]
    async fn no_notification_when_notify_is_false() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0) // notify=false must suppress the dispatch entirely
            .mount(&server)
            .await;

        let node = FakeNode::new(notifying_container("watch", false), DIG_A);
        let reg = FakeRegistry::new(DIG_B);
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
    }

    #[tokio::test]
    async fn skipped_by_hook_sends_no_notification_and_does_not_recreate() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0) // a hook skip is neither "succeeded" nor "failed"
            .mount(&server)
            .await;

        let node = FakeNode::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[
                    ("freshdock.enable", "true"),
                    ("freshdock.mode", "live"),
                    ("freshdock.notify", "true"),
                    ("freshdock.lifecycle.pre-update", "/app/drain.sh"),
                ],
            )],
            DIG_A,
        )
        .hook_refuses();
        let reg = FakeRegistry::new(DIG_B); // update available, but the hook vetoes
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
        assert_eq!(
            node.creates(),
            0,
            "a refused pre-update hook must not recreate"
        );
    }

    #[tokio::test]
    async fn live_success_notifies_updated() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(body_partial_json(
                json!({"event": "succeeded", "container": "web"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let node = FakeNode::new(notifying_container("live", true), DIG_A); // healthy by default
        let reg = FakeRegistry::new(DIG_B);
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
        assert_eq!(node.creates(), 1, "live recreates on a changed digest");
    }

    #[tokio::test]
    async fn live_rollback_notifies_failed() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(body_partial_json(
                json!({"event": "failed", "container": "web"}),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        // The recreated container crashes → real health gate rolls back → the
        // real RollbackEvent flows into an UpdateFailed notification.
        let node = FakeNode::new(notifying_container("live", true), DIG_A).unhealthy();
        let reg = FakeRegistry::new(DIG_B);
        one_tick_with(&node, &reg, &webhook_dispatcher(server.uri())).await;
    }
    // --- siblings on a shared tag ---

    const OLD_ID: &str = "sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";
    const NEW_ID: &str = "sha256:0e7f2f0e2e8b4a4b8c3d1a5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192";

    /// Labels for an enabled `live` container.
    fn live_labels() -> [(&'static str, &'static str); 2] {
        [("freshdock.enable", "true"), ("freshdock.mode", "live")]
    }

    /// A local image with an id and full `repo@sha256:` entries.
    fn image(id: &str, digests: &[&str]) -> LocalImage {
        LocalImage {
            id: Some(id.to_owned()),
            repo_digests: digests.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    #[tokio::test]
    async fn sibling_on_a_shared_tag_is_updated_after_the_tag_moved() {
        // Updating the first moves the tag; the second is then behind it.
        let live = &live_labels();
        let node = FakeNode::new(
            vec![
                with_image_id(with_id(summary("a", "alpine:3.19", live), "c1"), OLD_ID),
                with_image_id(with_id(summary("b", "alpine:3.19", live), "c2"), OLD_ID),
            ],
            DIG_A,
        )
        .with_image("alpine:3.19", image(OLD_ID, &[&format!("alpine@{DIG_A}")]))
        .on_pull("alpine:3.19", image(NEW_ID, &[&format!("alpine@{DIG_B}")]));
        let reg = FakeRegistry::new(DIG_B);

        one_tick(&node, &reg).await;
        assert_eq!(
            node.creates(),
            2,
            "both containers run the old image and must be recreated"
        );
    }

    #[tokio::test]
    async fn sibling_behind_the_tag_is_updated_when_its_image_has_no_digests() {
        let live = &live_labels();
        let node = FakeNode::new(
            vec![with_image_id(
                with_id(summary("b", "alpine:3.19", live), "c2"),
                OLD_ID,
            )],
            DIG_A,
        )
        .with_image("alpine:3.19", image(NEW_ID, &[&format!("alpine@{DIG_B}")]))
        .with_image(OLD_ID, LocalImage::default());
        let reg = FakeRegistry::new(DIG_B);

        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 1);
    }

    #[tokio::test]
    async fn an_id_rewritten_summary_is_probed_through_config_image() {
        let live = &live_labels();
        let node = FakeNode::new(
            vec![with_image_id(
                with_id(summary("a", OLD_ID, live), "c1"),
                OLD_ID,
            )],
            DIG_A,
        )
        .with_image("alpine:3.19", image(NEW_ID, &[&format!("alpine@{DIG_A}")]))
        .with_config_image("c1", "alpine:3.19");
        let reg = FakeRegistry::new(DIG_B);

        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 1);
    }

    #[tokio::test]
    async fn a_container_created_by_image_id_stays_pinned() {
        // `Config.Image` is itself an id: the container really is pinned.
        let live = &live_labels();
        let node = FakeNode::new(
            vec![with_image_id(
                with_id(summary("a", OLD_ID, live), "c1"),
                OLD_ID,
            )],
            DIG_A,
        )
        .with_config_image("c1", OLD_ID);
        let reg = FakeRegistry::new(DIG_B);

        one_tick(&node, &reg).await;
        assert_eq!(node.creates(), 0);
        assert_eq!(
            node.image_inspects(),
            0,
            "a pinned reference must not touch the daemon"
        );
    }
    #[tokio::test]
    async fn a_failed_digest_is_not_retried_until_upstream_moves() {
        let live = &live_labels();
        let node = FakeNode::new(
            vec![with_id(summary("web", "alpine:3.19", live), "c1")],
            DIG_A,
        )
        .unhealthy();
        let reg = FakeRegistry::new(DIG_B);
        let every_tick = SchedulerConfig {
            poll_interval: Duration::from_secs(0),
            ..cfg()
        };
        let mut st = TickState::default();

        one_tick_cfg(
            &node,
            &reg,
            &every_tick,
            ResolvedSettings::default(),
            &mut st,
        )
        .await;
        assert_eq!(node.creates(), 1, "the first attempt runs and rolls back");

        one_tick_cfg(
            &node,
            &reg,
            &every_tick,
            ResolvedSettings::default(),
            &mut st,
        )
        .await;
        assert_eq!(node.creates(), 1, "the same digest must not be retried");

        reg.set_digest(DIG_C);
        one_tick_cfg(
            &node,
            &reg,
            &every_tick,
            ResolvedSettings::default(),
            &mut st,
        )
        .await;
        assert_eq!(node.creates(), 2, "a new upstream digest is tried again");
    }

    /// Poll interval zero, so two ticks in a row are a retry.
    fn every_tick() -> SchedulerConfig {
        SchedulerConfig {
            poll_interval: Duration::from_secs(0),
            ..cfg()
        }
    }

    /// A two-service project with a pre-update hook, so the rollout can defer.
    fn deferring_project() -> FakeNode {
        let hook = &[("freshdock.lifecycle.pre-update", "/app/drain.sh")];
        let running = vec![
            compose_summary_with("web", "", hook),
            compose_summary_with("worker", "", hook),
        ];
        let members = running.iter().map(compose_member).collect();
        FakeNode::new(running, DIG_A)
            .in_project(members)
            .hook_refuses()
    }

    #[tokio::test]
    async fn a_deferred_rollout_is_retried_on_the_next_tick() {
        // Exit 75 is "not now", not a rejected digest.
        let node = deferring_project();
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();

        for _ in 0..3 {
            one_tick_cfg(
                &node,
                &reg,
                &every_tick(),
                ResolvedSettings::default(),
                &mut st,
            )
            .await;
        }
        assert_eq!(
            node.hook_calls(),
            3,
            "every tick the project is due asks the hook again"
        );
        assert_eq!(node.creates(), 0, "and the hook refuses every time");
    }

    #[tokio::test]
    async fn a_rolled_back_rollout_is_not_retried_until_upstream_moves() {
        let (node, _) = compose_node();
        let node = FakeNode {
            health_state: ContainerRuntimeState::Exited { exit_code: 1 },
            ..node
        };
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();
        let settings = ResolvedSettings::default();

        one_tick_cfg(&node, &reg, &every_tick(), settings, &mut st).await;
        assert_eq!(node.creates(), 1, "the first service rolls back");

        one_tick_cfg(&node, &reg, &every_tick(), settings, &mut st).await;
        assert_eq!(node.creates(), 1, "the same digest must not be retried");

        reg.set_digest(DIG_C);
        one_tick_cfg(&node, &reg, &every_tick(), settings, &mut st).await;
        assert_eq!(node.creates(), 2, "a new upstream digest is tried again");
    }

    #[tokio::test]
    async fn a_sibling_claimed_before_it_was_seeded_remembers_the_rejected_digest() {
        // The rollout claims `worker` before the tick ever seeds it.
        let (node, _) = compose_node();
        let node = FakeNode {
            health_state: ContainerRuntimeState::Exited { exit_code: 1 },
            ..node
        };
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();

        one_tick_cfg(
            &node,
            &reg,
            &every_tick(),
            ResolvedSettings::default(),
            &mut st,
        )
        .await;
        assert!(
            !st.states.contains_key("stack-worker-1"),
            "the claimed sibling is never seeded, which is the whole problem"
        );

        one_tick_cfg(
            &node,
            &reg,
            &every_tick(),
            ResolvedSettings::default(),
            &mut st,
        )
        .await;
        assert_eq!(
            node.creates(),
            1,
            "the sibling must not re-run the rollout that just rolled back"
        );
    }

    #[tokio::test]
    async fn a_restarted_dependent_keeps_its_own_rejected_digest() {
        // `web` is bumped because `db` moved, not because its own image did.
        let db = compose_summary("db", "");
        let web = ContainerSummary {
            image: Some("other:latest".to_owned()),
            ..compose_summary_with("web", "db:service_healthy:true", &[])
        };
        let mut web_member = compose_member(&web);
        web_member.image_ref = "other:latest".to_owned();
        web_member.image_id = Some("sha256:other".to_owned());
        let node = FakeNode::new(vec![db.clone(), web], DIG_A)
            .in_project(vec![compose_member(&db), web_member]);
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();
        st.failed.insert("stack-web-1".to_owned(), DIG_B.to_owned());

        one_tick_cfg(
            &node,
            &reg,
            &every_tick(),
            ResolvedSettings::default(),
            &mut st,
        )
        .await;

        assert_eq!(node.creates(), 1, "only db runs the image that moved");
        assert_eq!(
            st.failed.get("stack-web-1").map(String::as_str),
            Some(DIG_B),
            "the restarted dependent keeps the digest that rolled it back"
        );
    }

    #[tokio::test]
    async fn an_inspect_failure_holds_a_live_container_for_the_poll_interval() {
        let live = &live_labels();
        let node = FakeNode::new(vec![with_id(summary("web", OLD_ID, live), "c1")], DIG_A)
            .with_config_image("c1", "alpine:3.19")
            .failing_container("c1");
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();

        for _ in 0..2 {
            one_tick_cfg(&node, &reg, &cfg(), ResolvedSettings::default(), &mut st).await;
        }
        assert_eq!(
            node.container_inspects(),
            1,
            "two ticks inside one interval are one attempt"
        );
    }

    #[tokio::test]
    async fn a_healthy_update_is_not_re_applied_on_the_next_tick() {
        // The behind-the-tag rule has to settle rather than fire forever.
        let live = &live_labels();
        let node = FakeNode::new(
            vec![with_image_id(
                with_id(summary("web", "alpine:3.19", live), "c1"),
                OLD_ID,
            )],
            DIG_A,
        )
        .with_image("alpine:3.19", image(OLD_ID, &[&format!("alpine@{DIG_A}")]))
        .on_pull("alpine:3.19", image(NEW_ID, &[&format!("alpine@{DIG_B}")]));
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();

        for _ in 0..2 {
            one_tick_cfg(
                &node,
                &reg,
                &every_tick(),
                ResolvedSettings::default(),
                &mut st,
            )
            .await;
        }
        assert_eq!(
            node.creates(),
            1,
            "the update landed; there is nothing left"
        );
    }

    #[tokio::test]
    async fn a_digest_update_still_rolls_out_members_on_the_tag_image() {
        // The tag itself is behind upstream, so no member is current.
        let (node, _) = compose_node();
        let node = node.with_image(
            COMPOSE_IMAGE,
            image("sha256:app", &[&format!("app@{DIG_A}")]),
        );
        let reg = FakeRegistry::new(DIG_B);
        tick_with_settings(&node, &reg, ResolvedSettings::default()).await;

        assert_eq!(node.creates(), 2, "both members are still updated");
        assert_eq!(
            reg.calls.load(Ordering::SeqCst),
            1,
            "by one rollout, from one probe"
        );
    }

    #[tokio::test]
    async fn an_inspect_failure_skips_the_tick_without_consuming_a_cron_window() {
        let nightly = &[("freshdock.enable", "true"), ("freshdock.mode", "nightly")];
        let node = FakeNode::new(vec![with_id(summary("web", OLD_ID, nightly), "c1")], DIG_A)
            .with_config_image("c1", "alpine:3.19")
            .failing_container("c1");
        let reg = FakeRegistry::new(DIG_B);
        let mut st = TickState::default();
        // Seed at midnight so the 04:00 window is open at the fixed `now` of 12:00.
        st.states.insert(
            "web".to_owned(),
            seed_state(
                &policy(Mode::Nightly, None),
                "web",
                Local.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap(),
            ),
        );
        let fire = st.states["web"].next_fire;

        one_tick_cfg(&node, &reg, &cfg(), ResolvedSettings::default(), &mut st).await;
        assert_eq!(node.creates(), 0, "nothing is updated on a failed inspect");
        assert_eq!(
            st.states["web"].next_fire, fire,
            "the cron window must not be consumed by a failed inspect"
        );

        node.heal();
        one_tick_cfg(&node, &reg, &cfg(), ResolvedSettings::default(), &mut st).await;
        assert_eq!(node.creates(), 1, "the window still fires once it can");
    }
}
