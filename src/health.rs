//! Health gating (Phase 3, P3-1).
//!
//! After a recreated container is started, [`wait_for_health`] blocks until one
//! of three terminal [`HealthOutcome`]s is reached:
//!
//! - **`Healthy`** — a declared healthcheck reported `healthy`, *or* (no
//!   healthcheck) the container stayed running for the whole grace period.
//! - **`Timeout`** — a healthcheck was declared but never reached `healthy`
//!   within `health_timeout`.
//! - **`Crashed`** — the container exited before becoming healthy / before the
//!   grace period elapsed.
//!
//! Acting on the outcome (removal of the archived container on success,
//! rollback on failure) is **out of scope** here — that is the rollback module
//! (P3-2). This module only classifies.

use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::docker::DockerError;

/// Terminal verdict of the health gate. Stable enum — the rollback path
/// (P3-2) matches on it exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthOutcome {
    Healthy,
    Timeout,
    Crashed,
}

/// Daemon-agnostic projection of a container's lifecycle + health, derived
/// from `State.Running` / `State.Status` / `State.Health.Status`. Keeping the
/// poll loop branching on this (rather than a raw `ContainerInspectResponse`)
/// is what lets [`wait_for_health`] be unit-tested with a scripted fake that
/// never touches a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntimeState {
    /// Running; healthcheck declared but still `starting`.
    HealthStarting,
    /// Running; healthcheck reports `healthy`.
    HealthHealthy,
    /// Running; healthcheck reports `unhealthy`.
    HealthUnhealthy,
    /// Running; no healthcheck declared (`none`/empty) — grace-period path.
    RunningNoHealthcheck,
    /// No longer running (exited/dead/removing). Carries the exit code so
    /// tracing and the rollback event can report *why*.
    Exited { exit_code: i64 },
}

/// Tunable durations for the health gate. Phase 3 constructs this via
/// [`Default`]; sourcing the values from `freshdock.toml`/labels is Phase 4/5
/// config work — the struct is ready for it.
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// Max time to wait for `healthy` when a healthcheck exists.
    pub health_timeout: Duration,
    /// How long a container with no healthcheck must stay running to be
    /// considered successfully updated.
    pub grace_period: Duration,
    /// Delay between inspect polls.
    pub poll_interval: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_secs(120),
            grace_period: Duration::from_secs(10),
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// The single daemon read [`wait_for_health`] needs. Split from `DockerOps`
/// so the five health scenarios can be exercised with a one-method fake.
#[async_trait]
pub trait HealthProbe {
    async fn probe_state(&self, name_or_id: &str) -> Result<ContainerRuntimeState, DockerError>;
}

/// Injected time source, so tests run under `#[tokio::test(start_paused)]`
/// without burning real wall-clock seconds. Production uses [`TokioClock`].
#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    async fn sleep(&self, dur: Duration);
}

/// Real clock backed by `tokio::time` — under a paused test runtime its
/// `Instant` and `sleep` both advance in virtual time on auto-advance.
pub struct TokioClock;

#[async_trait]
impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Poll `ops.probe_state(id)` on `cfg.poll_interval` until a terminal
/// [`HealthOutcome`]. `Healthy`/`Exited` resolve immediately; the
/// healthcheck path is bounded by `health_timeout`, the no-healthcheck path
/// by `grace_period`. Transitions are traced (not every poll) to stay quiet.
///
/// Transient probe errors are tolerated — the loop logs and keeps polling
/// within the budget. Persistent failure past `health_timeout` yields
/// `Timeout` (the safe verdict the caller rolls back on), so this always
/// returns an outcome.
pub async fn wait_for_health(
    ops: &impl HealthProbe,
    id: &str,
    cfg: &HealthConfig,
    clock: &impl Clock,
) -> HealthOutcome {
    let start = clock.now();
    let mut prev: Option<ContainerRuntimeState> = None;
    // Deadline used when probes error: matches whichever path we've observed so
    // a no-healthcheck container doesn't retry for the (longer) health_timeout.
    let mut error_deadline = cfg.health_timeout;

    loop {
        match ops.probe_state(id).await {
            Ok(state) => {
                if prev != Some(state) {
                    debug!(container = %id, ?state, "health: state transition");
                    prev = Some(state);
                }

                match state {
                    ContainerRuntimeState::HealthHealthy => {
                        info!(container = %id, "health: healthy");
                        return HealthOutcome::Healthy;
                    }
                    ContainerRuntimeState::Exited { exit_code } => {
                        info!(container = %id, exit_code, "health: container exited before healthy");
                        return HealthOutcome::Crashed;
                    }
                    ContainerRuntimeState::HealthStarting
                    | ContainerRuntimeState::HealthUnhealthy => {
                        error_deadline = cfg.health_timeout;
                        if clock.now().duration_since(start) >= cfg.health_timeout {
                            info!(container = %id, "health: timed out waiting for healthy");
                            return HealthOutcome::Timeout;
                        }
                    }
                    ContainerRuntimeState::RunningNoHealthcheck => {
                        error_deadline = cfg.grace_period;
                        if clock.now().duration_since(start) >= cfg.grace_period {
                            info!(container = %id, "health: grace period elapsed, still running");
                            return HealthOutcome::Healthy;
                        }
                    }
                }
            }
            Err(e) => {
                // Tolerate transient blips; give up with Timeout if persistent.
                warn!(container = %id, error = %e, "health: probe failed, will retry");
                if clock.now().duration_since(start) >= error_deadline {
                    warn!(container = %id, "health: probe failed persistently, treating as timeout");
                    return HealthOutcome::Timeout;
                }
            }
        }

        clock.sleep(cfg.poll_interval).await;
    }
}

/// Terminal verdict of the one-shot gate ([`wait_for_exit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// The container reached a stopped state. `0` is what
    /// `service_completed_successfully` is asking about.
    Exited { exit_code: i64 },
    /// Still running when the budget ran out.
    TimedOut,
}

/// Poll until `id` stops running, returning its exit code (issue #78).
///
/// The compose counterpart of [`wait_for_health`], which reads any exit as a
/// crash and so is the wrong question for a one-shot. Reuses [`HealthProbe`]
/// rather than adding a daemon method. A persistent probe failure resolves as
/// [`ExitOutcome::TimedOut`], the verdict that aborts the rollout, which is the
/// safe direction when the daemon has stopped answering.
pub async fn wait_for_exit(
    ops: &impl HealthProbe,
    id: &str,
    timeout: Duration,
    poll_interval: Duration,
    clock: &impl Clock,
) -> ExitOutcome {
    let start = clock.now();
    loop {
        match ops.probe_state(id).await {
            Ok(ContainerRuntimeState::Exited { exit_code }) => {
                info!(container = %id, exit_code, "one-shot: finished");
                return ExitOutcome::Exited { exit_code };
            }
            Ok(state) => debug!(container = %id, ?state, "one-shot: still running"),
            Err(e) => warn!(container = %id, error = %e, "one-shot: probe failed, will retry"),
        }
        if clock.now().duration_since(start) >= timeout {
            warn!(container = %id, "one-shot: did not finish within its timeout");
            return ExitOutcome::TimedOut;
        }
        clock.sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Fake that replays a scripted sequence of states, one per poll. Once the
    /// queue is down to its last element that element repeats forever — so
    /// "unhealthy until timeout" or "running through the grace period" needs
    /// just a single entry.
    struct ScriptedProbe {
        states: Mutex<VecDeque<ContainerRuntimeState>>,
    }

    impl ScriptedProbe {
        fn new(states: &[ContainerRuntimeState]) -> Self {
            Self {
                states: Mutex::new(states.iter().copied().collect()),
            }
        }
    }

    #[async_trait]
    impl HealthProbe for ScriptedProbe {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            let mut q = self.states.lock().unwrap();
            let next = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                *q.front().expect("script must have at least one state")
            };
            Ok(next)
        }
    }

    fn test_cfg() -> HealthConfig {
        HealthConfig {
            health_timeout: Duration::from_secs(5),
            grace_period: Duration::from_secs(3),
            poll_interval: Duration::from_secs(1),
        }
    }

    use ContainerRuntimeState::*;

    #[tokio::test(start_paused = true)]
    async fn healthcheck_becomes_healthy_within_timeout() {
        let ops = ScriptedProbe::new(&[HealthStarting, HealthStarting, HealthHealthy]);
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn healthcheck_stays_unhealthy_until_timeout() {
        let ops = ScriptedProbe::new(&[HealthUnhealthy]);
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn no_healthcheck_running_through_grace_is_healthy() {
        let ops = ScriptedProbe::new(&[RunningNoHealthcheck]);
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn exit_during_grace_period_is_crashed() {
        let ops = ScriptedProbe::new(&[
            RunningNoHealthcheck,
            RunningNoHealthcheck,
            Exited { exit_code: 1 },
        ]);
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Crashed);
    }

    #[tokio::test(start_paused = true)]
    async fn exit_while_starting_is_crashed() {
        let ops = ScriptedProbe::new(&[HealthStarting, Exited { exit_code: 137 }]);
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Crashed);
    }

    /// Errors on the first `fail` probes, then returns `then` forever.
    struct FlakyProbe {
        remaining_failures: Mutex<u32>,
        then: ContainerRuntimeState,
    }

    fn probe_error() -> DockerError {
        DockerError::Spec(crate::docker::spec::SpecError::Missing("probe"))
    }

    #[async_trait]
    impl HealthProbe for FlakyProbe {
        async fn probe_state(&self, _id: &str) -> Result<ContainerRuntimeState, DockerError> {
            let mut left = self.remaining_failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                Err(probe_error())
            } else {
                Ok(self.then)
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_probe_errors_are_tolerated_then_resolve() {
        // Two failed polls, then the container reports healthy: a momentary
        // inspect blip must not abort the update.
        let ops = FlakyProbe {
            remaining_failures: Mutex::new(2),
            then: HealthHealthy,
        };
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_probe_errors_time_out() {
        // Probe never succeeds → safe "couldn't confirm" verdict, which the
        // caller routes to rollback rather than leaving a half-recreated state.
        let ops = FlakyProbe {
            remaining_failures: Mutex::new(u32::MAX),
            then: HealthHealthy,
        };
        let outcome = wait_for_health(&ops, "c", &test_cfg(), &TokioClock).await;
        assert_eq!(outcome, HealthOutcome::Timeout);
    }

    // ── one-shot gate (#78) ────────────────────────────────────────────────

    const ONE_SHOT_TIMEOUT: Duration = Duration::from_secs(30);
    const ONE_SHOT_POLL: Duration = Duration::from_secs(1);

    async fn wait_one_shot(ops: &impl HealthProbe) -> ExitOutcome {
        wait_for_exit(ops, "c", ONE_SHOT_TIMEOUT, ONE_SHOT_POLL, &TokioClock).await
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_shot_that_finishes_reports_its_exit_code() {
        let ops = ScriptedProbe::new(&[RunningNoHealthcheck, Exited { exit_code: 0 }]);
        assert_eq!(
            wait_one_shot(&ops).await,
            ExitOutcome::Exited { exit_code: 0 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_one_shot_reports_its_non_zero_code() {
        // The whole point of the gate: a migration that fails has to be
        // distinguishable from one that succeeded, not just "it stopped".
        let ops = ScriptedProbe::new(&[RunningNoHealthcheck, Exited { exit_code: 1 }]);
        assert_eq!(
            wait_one_shot(&ops).await,
            ExitOutcome::Exited { exit_code: 1 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_shot_that_never_finishes_times_out() {
        let ops = ScriptedProbe::new(&[RunningNoHealthcheck]);
        assert_eq!(wait_one_shot(&ops).await, ExitOutcome::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_container_is_not_mistaken_for_a_finished_one_shot() {
        // A long-running service wired up as a one-shot by a bad label must
        // time out, never report a success it did not have.
        let ops = ScriptedProbe::new(&[HealthHealthy]);
        assert_eq!(wait_one_shot(&ops).await, ExitOutcome::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_shot_already_exited_resolves_on_the_first_poll() {
        let ops = ScriptedProbe::new(&[Exited { exit_code: 0 }]);
        assert_eq!(
            wait_one_shot(&ops).await,
            ExitOutcome::Exited { exit_code: 0 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transient_probe_errors_do_not_fail_the_one_shot_gate() {
        let ops = FlakyProbe {
            remaining_failures: Mutex::new(2),
            then: Exited { exit_code: 0 },
        };
        assert_eq!(
            wait_one_shot(&ops).await,
            ExitOutcome::Exited { exit_code: 0 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_probe_errors_abort_the_one_shot_gate() {
        // Unknown outcome must not be read as success: a rollout that cannot
        // confirm the migration ran has to stop.
        let ops = FlakyProbe {
            remaining_failures: Mutex::new(u32::MAX),
            then: Exited { exit_code: 0 },
        };
        assert_eq!(wait_one_shot(&ops).await, ExitOutcome::TimedOut);
    }
}
