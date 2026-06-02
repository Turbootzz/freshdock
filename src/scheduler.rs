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

use bollard::models::ContainerSummary;
use chrono::{DateTime, Local, TimeDelta};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::cron::CronExpr;
use crate::docker::check::DockerCheck;
use crate::docker::recreate::{DockerOps, recreate_with_health};
use crate::errors::AppError;
use crate::health::{Clock, HealthConfig, HealthProbe};
use crate::labels::{self, Mode, Policy};
use crate::probe::{self, ProbeOutcome};
use crate::registry::Registry;
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

/// Container name from a summary (leading `/` trimmed), falling back to id.
fn container_name(c: &ContainerSummary) -> String {
    c.names
        .as_ref()
        .and_then(|n| n.first())
        .map(|s| s.trim_start_matches('/').to_string())
        .or_else(|| c.id.clone())
        .unwrap_or_else(|| "?".to_string())
}

/// A transient `<name>-old-<ts>` archive from an in-flight recreate. Such
/// containers are stopped (so normally absent from `list_running`); the filter
/// is defensive against a stale archive left running by a crashed cycle.
fn is_archive_name(name: &str) -> bool {
    name.rsplit_once("-old-")
        .is_some_and(|(_, ts)| !ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit()))
}

/// Run the scheduler until `shutdown` flips to `true` (or its sender drops).
/// Generic over the combined Docker trait surface + a [`Registry`], with an
/// injected wall clock (`now_provider`) so cron evaluation is testable.
pub async fn run_with<D, R>(
    docker: &D,
    registry: &R,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now_provider: impl Fn() -> DateTime<Local>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), AppError>
where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
    R: Registry + Sync,
{
    let mut states: HashMap<String, ContainerState> = HashMap::new();
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
                run_tick(docker, registry, cfg, clock, &now_provider, &mut states, &tick_shutdown).await;
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
    shutdown: &watch::Receiver<bool>,
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
        let policy = match labels::parse_policy(c.labels.as_ref().unwrap_or(&empty), None) {
            Ok(p) => p,
            Err(e) => {
                warn!(container = %name, error = %e, "scheduler: invalid freshdock labels; skipping");
                continue;
            }
        };
        if !policy.enabled || policy.mode == Mode::Off {
            continue;
        }
        live.insert(name.clone());

        let state = states
            .entry(name.clone())
            .or_insert_with(|| seed_state(&policy, &name, now));
        if !due(&policy, state, now, cfg.poll_interval) {
            continue;
        }

        // Advance bookkeeping before the (slow) update so a long recreate can't
        // re-fire on the next tick.
        match policy.mode {
            Mode::Live | Mode::Watch => state.last_checked = Some(now),
            Mode::Nightly | Mode::Weekly | Mode::Monthly => {
                state.next_fire = state.cron.as_ref().and_then(|c| c.next_after(now));
            }
            Mode::Off => {}
        }

        let image = c.image.as_deref().unwrap_or_default();
        process_container(docker, registry, cfg, clock, now, &name, &policy, image).await;
    }

    states.retain(|k, _| live.contains(k));
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
    image: &str,
) where
    D: DockerCheck + DockerOps + HealthProbe + Sync,
    R: Registry + Sync,
{
    match probe::probe_image(docker, registry, image).await {
        ProbeOutcome::Fetched { local, latest } => {
            let Some(local) = local.as_deref() else {
                debug!(container = %name, %latest, "scheduler: local digest unknown; not updating");
                return;
            };
            if local == latest {
                debug!(container = %name, "scheduler: up to date");
                return;
            }
            match policy.mode {
                Mode::Watch => {
                    info!(container = %name, %latest, event = "update_available", "scheduler: update available (watch mode — not applied)");
                }
                Mode::Live | Mode::Nightly | Mode::Weekly | Mode::Monthly => {
                    apply_update(docker, cfg, clock, now, name).await;
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
        ProbeOutcome::NetworkUnavailable => {
            warn!(container = %name, "scheduler: registry network unavailable; will retry next tick");
        }
        ProbeOutcome::Error(msg) => {
            warn!(container = %name, %msg, "scheduler: digest probe failed; continuing");
        }
    }
}

/// Run the health-gated recreate and log its outcome.
async fn apply_update<D>(
    docker: &D,
    cfg: &SchedulerConfig,
    clock: &impl Clock,
    now: DateTime<Local>,
    name: &str,
) where
    D: DockerOps + HealthProbe + Sync,
{
    let ts = now.timestamp();
    match recreate_with_health(docker, name, &cfg.health, clock, || ts).await {
        Ok(RecreateOutcome::Recreated { old_name, new_id }) => {
            info!(container = %name, archived = %old_name, %new_id, "scheduler: recreated");
        }
        Ok(RecreateOutcome::RolledBack(ev)) => {
            warn!(container = %name, reason = ?ev.reason, "scheduler: update unhealthy, rolled back");
        }
        Err(e) => {
            warn!(container = %name, error = %e, "scheduler: recreate failed; daemon continues");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;
    use crate::docker::spec::ContainerSpec;
    use crate::health::{ContainerRuntimeState, TokioClock};
    use crate::registry::{Digest, ImageRef, RegistryError};
    use async_trait::async_trait;
    use bollard::models::ContainerConfig;
    use chrono::TimeZone;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIG_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIG_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn cfg() -> SchedulerConfig {
        SchedulerConfig {
            poll_interval: Duration::from_secs(300),
            tick: Duration::from_secs(60),
            health: HealthConfig::default(),
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

    #[test]
    fn archive_names_are_detected() {
        assert!(is_archive_name("web-old-1700000000"));
        assert!(!is_archive_name("web"));
        assert!(!is_archive_name("web-old-")); // no timestamp
        assert!(!is_archive_name("my-old-laptop")); // non-numeric suffix
    }

    // --- run_tick behaviour with a recording fake ---

    fn summary(name: &str, image: &str, labels: &[(&str, &str)]) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{name}")]),
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
        local_digest: String,
        list_fails: bool,
        creates: AtomicUsize,
    }

    impl FakeNode {
        fn new(containers: Vec<ContainerSummary>, local_digest: &str) -> Self {
            Self {
                containers: Mutex::new(containers),
                local_digest: local_digest.to_owned(),
                list_fails: false,
                creates: AtomicUsize::new(0),
            }
        }
        fn failing() -> Self {
            Self {
                containers: Mutex::new(vec![]),
                local_digest: DIG_A.to_owned(),
                list_fails: true,
                creates: AtomicUsize::new(0),
            }
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
        async fn inspect_image_repo_digests(
            &self,
            image: &str,
        ) -> Result<Vec<String>, DockerError> {
            // Report the local digest under the image's repo so the probe's
            // RepoDigests match succeeds.
            let repo = image.split(':').next().unwrap_or(image);
            Ok(vec![format!("{repo}@{}", self.local_digest)])
        }
    }

    #[async_trait]
    impl DockerOps for FakeNode {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: "alpine:3.19".to_owned(),
                config: ContainerConfig::default(),
                host_config: None,
                network_endpoints: None,
            })
        }
        async fn pull(&self, _image_ref: &ImageRef) -> Result<(), DockerError> {
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
            _name: &str,
            _spec: &ContainerSpec,
            _image: &str,
        ) -> Result<String, DockerError> {
            self.creates.fetch_add(1, Ordering::SeqCst);
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
    }

    #[async_trait]
    impl HealthProbe for FakeNode {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            Ok(ContainerRuntimeState::HealthHealthy)
        }
    }

    struct FakeRegistry {
        digest: String,
        network_down: bool,
        auth_required: bool,
        calls: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(digest: &str) -> Self {
            Self {
                digest: digest.to_owned(),
                network_down: false,
                auth_required: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn offline() -> Self {
            Self {
                digest: DIG_B.to_owned(),
                network_down: true,
                auth_required: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn auth_required() -> Self {
            Self {
                digest: DIG_B.to_owned(),
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
                Ok(Digest(self.digest.clone()))
            }
        }
    }

    /// Drive a single `run_tick` with a fresh (not-shutting-down) state map.
    async fn one_tick(node: &FakeNode, reg: &FakeRegistry) -> HashMap<String, ContainerState> {
        let (_tx, rx) = watch::channel(false);
        let mut states = HashMap::new();
        run_tick(node, reg, &cfg(), &TokioClock, &now, &mut states, &rx).await;
        states
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

        run_tick(&node, &reg, &cfg(), &TokioClock, &now, &mut states, &rx).await;
        assert!(states.contains_key("web"));

        // Container disappears; next tick prunes it.
        node.containers.lock().unwrap().clear();
        run_tick(&node, &reg, &cfg(), &TokioClock, &now, &mut states, &rx).await;
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
        run_tick(&node, &reg, &cfg(), &TokioClock, &now, &mut states, &rx).await;
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
        run_with(&node, &reg, &cfg(), &TokioClock, now, rx)
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
        run_tick(&node, &reg, &cfg(), &TokioClock, &now_fn, &mut states, &rx).await;
        assert_eq!(node.creates(), 0, "not due before the window");

        // 04:00 → due → recreate, and next_fire advances to tomorrow.
        clock.set(Local.with_ymd_and_hms(2026, 6, 2, 4, 0, 0).unwrap());
        run_tick(&node, &reg, &cfg(), &TokioClock, &now_fn, &mut states, &rx).await;
        assert_eq!(node.creates(), 1, "fires at the window");

        // 04:01 → next_fire is tomorrow now, so it must not re-fire.
        clock.set(Local.with_ymd_and_hms(2026, 6, 2, 4, 1, 0).unwrap());
        run_tick(&node, &reg, &cfg(), &TokioClock, &now_fn, &mut states, &rx).await;
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
        };

        let handle =
            tokio::spawn(
                async move { run_with(&node, &reg, &big_cfg, &TokioClock, now, rx).await },
            );

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
}
