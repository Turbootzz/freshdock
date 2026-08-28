use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Nightly,
    Weekly,
    Monthly,
    Watch,
    Off,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Live => "live",
            Mode::Nightly => "nightly",
            Mode::Weekly => "weekly",
            Mode::Monthly => "monthly",
            Mode::Watch => "watch",
            Mode::Off => "off",
        })
    }
}

impl std::str::FromStr for Mode {
    type Err = LabelError;

    /// Shares the one case-insensitive matcher with label parsing, so a config
    /// `default_mode` and a `freshdock.mode` label can never disagree.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_mode(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    pub mode: Mode,
    pub notify: bool,
    pub schedule: Option<String>,
    /// Remove the superseded image after a healthy update. From the
    /// `freshdock.cleanup` label, falling back to the global default.
    pub cleanup: bool,
    /// Lifecycle hook commands from `freshdock.lifecycle.*` labels (issue #61).
    pub hooks: LifecycleHooks,
    /// Enablement came from `watch_all`, not from any label (issue #79).
    pub auto_enabled: bool,
}

/// A single lifecycle hook: a command exec'd inside the container via
/// `sh -c`, bounded by a timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub command: String,
    /// `None` means unlimited (label value `0`).
    pub timeout: Option<Duration>,
}

/// Hook commands run around an update, watchtower-style
/// (`freshdock.lifecycle.pre-update` / `post-update` and their `-timeout`
/// companions, in seconds).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LifecycleHooks {
    /// Runs in the *old* container after the pull, before the stop. Any
    /// failure (non-zero exit, timeout, exec error) skips the update.
    pub pre_update: Option<Hook>,
    /// Runs in the *new* container once it passes the health gate.
    /// Best-effort: a failure is logged, the update stands.
    pub post_update: Option<Hook>,
}

/// Watchtower defaults its hook timeout to one minute; keeping the same
/// default eases migration.
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// Watchtower's label namespace, read as a fallback so a migrated fleet works
/// without relabelling (issue #63). A `freshdock.*` label always wins over its
/// watchtower counterpart.
const WATCHTOWER_PREFIX: &str = "com.centurylinklabs.watchtower.";

const WT_ENABLE: &str = "com.centurylinklabs.watchtower.enable";
const WT_MONITOR_ONLY: &str = "com.centurylinklabs.watchtower.monitor-only";
const WT_PRE_UPDATE: &str = "com.centurylinklabs.watchtower.lifecycle.pre-update";
const WT_PRE_UPDATE_TIMEOUT: &str = "com.centurylinklabs.watchtower.lifecycle.pre-update-timeout";
const WT_POST_UPDATE: &str = "com.centurylinklabs.watchtower.lifecycle.post-update";
const WT_POST_UPDATE_TIMEOUT: &str = "com.centurylinklabs.watchtower.lifecycle.post-update-timeout";

/// Fleet-wide defaults from `[settings]`, applied when a container omits the
/// matching `freshdock.*` label. A per-container label always overrides these.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyDefaults {
    /// Mode for an enabled container with no `freshdock.mode` (else `watch`).
    pub mode: Option<Mode>,
    /// Default for `freshdock.cleanup`.
    pub cleanup: bool,
    /// Treat a container with no enable label as enabled (issue #79). The
    /// explicit opt-outs still apply.
    pub watch_all: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LabelError {
    #[error("invalid value for label `{key}`: `{value}` (expected true/false)")]
    InvalidBool { key: String, value: String },
    #[error(
        "invalid value for label `freshdock.mode`: `{value}` (expected one of live, nightly, weekly, monthly, watch, off)"
    )]
    InvalidMode { value: String },
    #[error(
        "invalid value for label `{key}`: `{value}` (expected a whole number of seconds, 0 for no timeout)"
    )]
    InvalidTimeout { key: String, value: String },
}

pub fn parse_policy(
    labels: &HashMap<String, String>,
    defaults: PolicyDefaults,
) -> Result<Policy, LabelError> {
    // Either enable label states intent; only their absence falls back to the
    // fleet-wide `watch_all`.
    let explicit = match labels.get("freshdock.enable") {
        Some(v) => Some(parse_bool("freshdock.enable", v)?),
        None => match labels.get(WT_ENABLE) {
            Some(v) => Some(parse_bool(WT_ENABLE, v)?),
            None => None,
        },
    };
    let enabled = explicit.unwrap_or(defaults.watch_all);
    let auto_enabled = explicit.is_none() && enabled;

    if !enabled {
        return Ok(Policy {
            enabled: false,
            mode: Mode::Off,
            notify: false,
            schedule: None,
            cleanup: false,
            hooks: LifecycleHooks::default(),
            auto_enabled: false,
        });
    }

    let monitor_only = match labels.get(WT_MONITOR_ONLY) {
        Some(v) => parse_bool(WT_MONITOR_ONLY, v)?,
        None => false,
    };
    let mode = match labels.get("freshdock.mode") {
        Some(v) => parse_mode(v)?,
        // monitor-only is explicit per-container intent, so it also beats the
        // fleet-wide `[settings] default_mode`.
        None if monitor_only => Mode::Watch,
        None => defaults.mode.unwrap_or(Mode::Watch),
    };

    let notify = match labels.get("freshdock.notify") {
        None => false,
        Some(v) => parse_bool("freshdock.notify", v)?,
    };

    let schedule = labels.get("freshdock.schedule").cloned();

    let cleanup = match labels.get("freshdock.cleanup") {
        None => defaults.cleanup,
        Some(v) => parse_bool("freshdock.cleanup", v)?,
    };

    let hooks = LifecycleHooks {
        pre_update: parse_hook(
            labels,
            "freshdock.lifecycle.pre-update",
            "freshdock.lifecycle.pre-update-timeout",
            WT_PRE_UPDATE,
            WT_PRE_UPDATE_TIMEOUT,
        )?,
        post_update: parse_hook(
            labels,
            "freshdock.lifecycle.post-update",
            "freshdock.lifecycle.post-update-timeout",
            WT_POST_UPDATE,
            WT_POST_UPDATE_TIMEOUT,
        )?,
    };

    Ok(Policy {
        enabled,
        mode,
        notify,
        schedule,
        cleanup,
        hooks,
        auto_enabled,
    })
}

/// The timeout labels are validated even when no command is set — a dangling
/// timeout is almost certainly a typo'd hook setup, and silence would hide it.
/// The freshdock labels win over their watchtower fallbacks independently, so
/// a mid-migration mix (watchtower command + freshdock timeout) behaves
/// predictably.
fn parse_hook(
    labels: &HashMap<String, String>,
    command_key: &str,
    timeout_key: &str,
    wt_command_key: &str,
    wt_timeout_key: &str,
) -> Result<Option<Hook>, LabelError> {
    let timeout = match labels.get(timeout_key) {
        Some(v) => parse_timeout(timeout_key, v, 1)?,
        // Watchtower counts MINUTES, not seconds.
        None => match labels.get(wt_timeout_key) {
            Some(v) => parse_timeout(wt_timeout_key, v, 60)?,
            None => Some(DEFAULT_HOOK_TIMEOUT),
        },
    };
    let command = labels
        .get(command_key)
        .or_else(|| labels.get(wt_command_key));
    Ok(command.map(|c| Hook {
        command: c.clone(),
        timeout,
    }))
}

/// Parse a hook timeout label: a whole number of `unit_secs`-sized units
/// (`1` = seconds, `60` = watchtower's minutes), `0` = no timeout.
fn parse_timeout(key: &str, value: &str, unit_secs: u64) -> Result<Option<Duration>, LabelError> {
    match value.trim().parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(n) => Ok(Some(Duration::from_secs(n.saturating_mul(unit_secs)))),
        Err(_) => Err(LabelError::InvalidTimeout {
            key: key.to_string(),
            value: value.to_string(),
        }),
    }
}

/// Notes about `com.centurylinklabs.watchtower.*` labels freshdock ignores
/// (unsupported features) or overrides (a `freshdock.*` counterpart with a
/// different effect). Best-effort and pure — invalid values are reported by
/// [`parse_policy`], not here — so callers decide where and how often to log.
pub fn watchtower_diagnostics(labels: &HashMap<String, String>) -> Vec<String> {
    let mut notes = Vec::new();
    for (key, value) in labels {
        let Some(suffix) = key.strip_prefix(WATCHTOWER_PREFIX) else {
            continue;
        };
        match suffix {
            "enable" => {
                if let Some(fd) = labels.get("freshdock.enable")
                    && parse_bool("freshdock.enable", fd).ok() != parse_bool(key, value).ok()
                {
                    notes.push(format!(
                        "`{key}={value}` conflicts with `freshdock.enable={fd}`; the freshdock label wins"
                    ));
                }
            }
            "monitor-only" => {
                if let Some(mode) = labels.get("freshdock.mode")
                    && parse_bool(key, value).unwrap_or(false)
                    && parse_mode(mode).ok() != Some(Mode::Watch)
                {
                    notes.push(format!(
                        "`{key}=true` conflicts with `freshdock.mode={mode}`; the freshdock label wins"
                    ));
                }
            }
            "lifecycle.pre-update" | "lifecycle.post-update" => {
                let fd_key = format!("freshdock.{suffix}");
                if let Some(fd) = labels.get(&fd_key)
                    && fd != value
                {
                    notes.push(format!("`{key}` is overridden by `{fd_key}`"));
                }
            }
            "lifecycle.pre-update-timeout" | "lifecycle.post-update-timeout" => {
                let fd_key = format!("freshdock.{suffix}");
                if let Some(fd) = labels.get(&fd_key)
                    && parse_timeout(&fd_key, fd, 1).ok() != parse_timeout(key, value, 60).ok()
                {
                    notes.push(format!(
                        "`{key}` (minutes) is overridden by `{fd_key}` (seconds)"
                    ));
                }
            }
            _ => notes.push(format!(
                "unsupported watchtower label `{key}` ignored (no freshdock equivalent)"
            )),
        }
    }
    // HashMap iteration order is random; keep log output stable.
    notes.sort();
    notes
}

fn parse_bool(key: &str, value: &str) -> Result<bool, LabelError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(LabelError::InvalidBool {
            key: key.to_string(),
            value: value.to_string(),
        }),
    }
}

fn parse_mode(value: &str) -> Result<Mode, LabelError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "live" => Ok(Mode::Live),
        "nightly" => Ok(Mode::Nightly),
        "weekly" => Ok(Mode::Weekly),
        "monthly" => Ok(Mode::Monthly),
        "watch" => Ok(Mode::Watch),
        "off" => Ok(Mode::Off),
        _ => Err(LabelError::InvalidMode {
            value: value.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn missing_enable_means_disabled() {
        let p = parse_policy(&labels(&[]), PolicyDefaults::default()).unwrap();
        assert!(!p.enabled);
        assert_eq!(p.mode, Mode::Off);
        assert!(!p.notify);
        assert!(p.schedule.is_none());
        assert!(!p.cleanup);
    }

    #[test]
    fn enable_false_means_disabled() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "false")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(!p.enabled);
    }

    #[test]
    fn enabled_with_no_mode_and_no_global_default_falls_back_to_watch() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.enabled);
        assert_eq!(p.mode, Mode::Watch);
    }

    #[test]
    fn enabled_with_no_mode_uses_global_default() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true")]),
            PolicyDefaults {
                mode: Some(Mode::Nightly),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Nightly);
    }

    #[test]
    fn explicit_mode_label_overrides_global_default() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.mode", "watch")]),
            PolicyDefaults {
                mode: Some(Mode::Live),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Watch, "the label must win over the global");
    }

    #[test]
    fn each_mode_parses() {
        for (raw, expected) in [
            ("live", Mode::Live),
            ("nightly", Mode::Nightly),
            ("weekly", Mode::Weekly),
            ("monthly", Mode::Monthly),
            ("watch", Mode::Watch),
            ("off", Mode::Off),
        ] {
            let p = parse_policy(
                &labels(&[("freshdock.enable", "true"), ("freshdock.mode", raw)]),
                PolicyDefaults::default(),
            )
            .unwrap();
            assert_eq!(p.mode, expected, "mode={raw}");
        }
    }

    #[test]
    fn mode_from_str_round_trips_each_variant_case_insensitively() {
        for (raw, expected) in [
            ("live", Mode::Live),
            ("NIGHTLY", Mode::Nightly),
            ("Weekly", Mode::Weekly),
            ("  monthly\t", Mode::Monthly),
            ("watch", Mode::Watch),
            ("off", Mode::Off),
        ] {
            assert_eq!(raw.parse::<Mode>().unwrap(), expected, "from_str({raw})");
        }
        assert!("hourly".parse::<Mode>().is_err());
    }

    #[test]
    fn mode_parsing_is_case_insensitive() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.mode", "Nightly")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Nightly);
    }

    #[test]
    fn invalid_mode_returns_typed_error() {
        let err = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.mode", "hourly")]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(matches!(err, LabelError::InvalidMode { ref value } if value == "hourly"));
    }

    #[test]
    fn invalid_enable_returns_typed_error() {
        let err = parse_policy(
            &labels(&[("freshdock.enable", "yes")]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LabelError::InvalidBool { ref key, ref value } if key == "freshdock.enable" && value == "yes")
        );
    }

    #[test]
    fn notify_true_sets_notify() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.notify", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.notify);
    }

    #[test]
    fn invalid_notify_returns_typed_error() {
        let err = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.notify", "sure")]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LabelError::InvalidBool { ref key, .. } if key == "freshdock.notify")
        );
    }

    #[test]
    fn cleanup_label_overrides_global_default() {
        // Label false beats global true.
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.cleanup", "false")]),
            PolicyDefaults {
                cleanup: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !p.cleanup,
            "freshdock.cleanup=false must override global true"
        );

        // Label true beats global false.
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.cleanup", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.cleanup);
    }

    #[test]
    fn cleanup_absent_inherits_global_default() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true")]),
            PolicyDefaults {
                cleanup: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(p.cleanup, "no label → global default applies");
    }

    #[test]
    fn invalid_cleanup_returns_typed_error() {
        let err = parse_policy(
            &labels(&[("freshdock.enable", "true"), ("freshdock.cleanup", "maybe")]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LabelError::InvalidBool { ref key, .. } if key == "freshdock.cleanup")
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", " true "),
                ("freshdock.mode", "  Nightly\t"),
                ("freshdock.notify", "\nfalse "),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.enabled);
        assert_eq!(p.mode, Mode::Nightly);
        assert!(!p.notify);
    }

    #[test]
    fn display_for_mode_is_canonical_lowercase() {
        assert_eq!(Mode::Live.to_string(), "live");
        assert_eq!(Mode::Nightly.to_string(), "nightly");
        assert_eq!(Mode::Weekly.to_string(), "weekly");
        assert_eq!(Mode::Monthly.to_string(), "monthly");
        assert_eq!(Mode::Watch.to_string(), "watch");
        assert_eq!(Mode::Off.to_string(), "off");
    }

    #[test]
    fn lifecycle_hooks_absent_by_default() {
        let p = parse_policy(
            &labels(&[("freshdock.enable", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.hooks, LifecycleHooks::default());
        assert!(p.hooks.pre_update.is_none());
        assert!(p.hooks.post_update.is_none());
    }

    #[test]
    fn pre_update_hook_parses_with_default_timeout() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.pre-update", "/app/flush.sh"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        let hook = p.hooks.pre_update.expect("pre-update hook must parse");
        assert_eq!(hook.command, "/app/flush.sh");
        assert_eq!(hook.timeout, Some(Duration::from_secs(60)));
        assert!(p.hooks.post_update.is_none());
    }

    #[test]
    fn post_update_hook_parses_with_explicit_timeout() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.post-update", "php artisan cache:clear"),
                ("freshdock.lifecycle.post-update-timeout", "120"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        let hook = p.hooks.post_update.expect("post-update hook must parse");
        assert_eq!(hook.command, "php artisan cache:clear");
        assert_eq!(hook.timeout, Some(Duration::from_secs(120)));
        assert!(p.hooks.pre_update.is_none());
    }

    #[test]
    fn hook_timeout_zero_disables_the_timeout() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.pre-update", "/app/drain.sh"),
                ("freshdock.lifecycle.pre-update-timeout", "0"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.hooks.pre_update.unwrap().timeout, None);
    }

    #[test]
    fn invalid_hook_timeout_returns_typed_error() {
        let err = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.pre-update", "/app/drain.sh"),
                ("freshdock.lifecycle.pre-update-timeout", "soon"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LabelError::InvalidTimeout { ref key, ref value }
                if key == "freshdock.lifecycle.pre-update-timeout" && value == "soon"
        ));
    }

    #[test]
    fn hook_timeout_label_is_validated_even_without_a_command() {
        // A dangling timeout label is almost certainly a typo'd setup — surface
        // it instead of silently ignoring it.
        let err = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.post-update-timeout", "brief"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LabelError::InvalidTimeout { ref key, .. } if key == "freshdock.lifecycle.post-update-timeout")
        );
    }

    #[test]
    fn hook_timeout_tolerates_surrounding_whitespace() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.pre-update", "/app/drain.sh"),
                ("freshdock.lifecycle.pre-update-timeout", " 30 "),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(
            p.hooks.pre_update.unwrap().timeout,
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn disabled_container_has_no_hooks() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "false"),
                ("freshdock.lifecycle.pre-update", "/app/drain.sh"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.hooks, LifecycleHooks::default());
    }

    // --- watchtower label translation (issue #63) ---

    #[test]
    fn watchtower_enable_true_alone_opts_in_with_default_mode() {
        let p = parse_policy(
            &labels(&[("com.centurylinklabs.watchtower.enable", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.enabled);
        assert_eq!(p.mode, Mode::Watch, "opted in, but on OUR safe default");
    }

    #[test]
    fn watchtower_enable_false_stays_disabled() {
        let p = parse_policy(
            &labels(&[("com.centurylinklabs.watchtower.enable", "false")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(!p.enabled);
    }

    #[test]
    fn freshdock_enable_wins_over_watchtower_enable() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "false"),
                ("com.centurylinklabs.watchtower.enable", "true"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(!p.enabled, "freshdock.enable=false must win");

        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("com.centurylinklabs.watchtower.enable", "false"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.enabled, "freshdock.enable=true must win");
    }

    #[test]
    fn watchtower_monitor_only_maps_to_watch() {
        let p = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                ("com.centurylinklabs.watchtower.monitor-only", "true"),
            ]),
            // monitor-only is explicit user intent, so it must beat the
            // fleet-wide default mode too.
            PolicyDefaults {
                mode: Some(Mode::Live),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Watch);
    }

    #[test]
    fn watchtower_monitor_only_false_is_inert() {
        let p = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                ("com.centurylinklabs.watchtower.monitor-only", "false"),
            ]),
            PolicyDefaults {
                mode: Some(Mode::Live),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            p.mode,
            Mode::Live,
            "monitor-only=false falls through to defaults"
        );
    }

    #[test]
    fn freshdock_mode_wins_over_monitor_only() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.mode", "live"),
                ("com.centurylinklabs.watchtower.monitor-only", "true"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Live);
    }

    #[test]
    fn watchtower_lifecycle_hooks_map_with_minute_timeouts() {
        let p = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update",
                    "/app/drain.sh",
                ),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update-timeout",
                    "5",
                ),
                (
                    "com.centurylinklabs.watchtower.lifecycle.post-update",
                    "cache-clear",
                ),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        let pre = p.hooks.pre_update.expect("pre-update must map");
        assert_eq!(pre.command, "/app/drain.sh");
        assert_eq!(
            pre.timeout,
            Some(Duration::from_secs(300)),
            "watchtower timeouts are MINUTES"
        );
        let post = p.hooks.post_update.expect("post-update must map");
        assert_eq!(post.command, "cache-clear");
        assert_eq!(
            post.timeout,
            Some(Duration::from_secs(60)),
            "watchtower's default hook budget is 1 minute — same as ours"
        );
    }

    #[test]
    fn watchtower_timeout_zero_disables_the_timeout() {
        let p = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                (
                    "com.centurylinklabs.watchtower.lifecycle.post-update",
                    "cache-clear",
                ),
                (
                    "com.centurylinklabs.watchtower.lifecycle.post-update-timeout",
                    "0",
                ),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.hooks.post_update.unwrap().timeout, None);
    }

    #[test]
    fn freshdock_lifecycle_labels_win_over_watchtower() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.lifecycle.pre-update", "/fd.sh"),
                ("freshdock.lifecycle.pre-update-timeout", "30"),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update",
                    "/wt.sh",
                ),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update-timeout",
                    "5",
                ),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        let pre = p.hooks.pre_update.unwrap();
        assert_eq!(pre.command, "/fd.sh");
        assert_eq!(
            pre.timeout,
            Some(Duration::from_secs(30)),
            "seconds, not minutes"
        );
    }

    #[test]
    fn freshdock_timeout_applies_to_a_watchtower_command() {
        // Mixed setup mid-migration: command still on the watchtower label, a
        // freshdock timeout override added. freshdock label wins per rule.
        let p = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update",
                    "/wt.sh",
                ),
                ("freshdock.lifecycle.pre-update-timeout", "90"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(
            p.hooks.pre_update.unwrap().timeout,
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn invalid_watchtower_enable_returns_typed_error() {
        let err = parse_policy(
            &labels(&[("com.centurylinklabs.watchtower.enable", "yes")]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LabelError::InvalidBool { ref key, .. } if key == "com.centurylinklabs.watchtower.enable"
        ));
    }

    #[test]
    fn invalid_watchtower_timeout_returns_typed_error() {
        let err = parse_policy(
            &labels(&[
                ("com.centurylinklabs.watchtower.enable", "true"),
                (
                    "com.centurylinklabs.watchtower.lifecycle.pre-update-timeout",
                    "soon",
                ),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LabelError::InvalidTimeout { ref key, .. }
                if key == "com.centurylinklabs.watchtower.lifecycle.pre-update-timeout"
        ));
    }

    #[test]
    fn diagnostics_flag_unsupported_watchtower_labels() {
        let notes = watchtower_diagnostics(&labels(&[
            ("com.centurylinklabs.watchtower.enable", "true"),
            ("com.centurylinklabs.watchtower.no-pull", "true"),
            ("com.centurylinklabs.watchtower.depends-on", "db"),
            ("com.centurylinklabs.watchtower.lifecycle.pre-check", "x"),
            ("freshdock.mode", "live"),
        ]));
        let joined = notes.join("\n");
        assert!(joined.contains("no-pull"), "{notes:?}");
        assert!(joined.contains("depends-on"), "{notes:?}");
        assert!(joined.contains("lifecycle.pre-check"), "{notes:?}");
        assert!(
            !joined.contains("watchtower.enable"),
            "translated labels are not 'unsupported': {notes:?}"
        );
    }

    #[test]
    fn diagnostics_flag_an_enable_conflict() {
        let notes = watchtower_diagnostics(&labels(&[
            ("freshdock.enable", "true"),
            ("com.centurylinklabs.watchtower.enable", "false"),
        ]));
        assert!(
            notes.iter().any(|n| n.contains("enable")),
            "conflicting enable labels must be flagged: {notes:?}"
        );
    }

    #[test]
    fn diagnostics_stay_quiet_when_labels_agree_or_are_absent() {
        assert!(
            watchtower_diagnostics(&labels(&[
                ("freshdock.enable", "true"),
                ("com.centurylinklabs.watchtower.enable", "true"),
            ]))
            .is_empty(),
            "agreeing labels are not a conflict"
        );
        assert!(watchtower_diagnostics(&labels(&[("freshdock.enable", "true")])).is_empty());
    }

    // --- watch_all opt-out mode (issue #79) ---

    fn watch_all() -> PolicyDefaults {
        PolicyDefaults {
            watch_all: true,
            ..Default::default()
        }
    }

    #[test]
    fn watch_all_enables_unlabelled_container() {
        let p = parse_policy(&labels(&[]), watch_all()).unwrap();
        assert!(
            p.enabled,
            "an absent enable label means enabled under watch_all"
        );
        assert_eq!(p.mode, Mode::Watch, "watch_all alone stays report-only");
        assert!(
            p.auto_enabled,
            "enablement came from watch_all, not a label"
        );
    }

    #[test]
    fn watch_all_respects_default_mode() {
        let p = parse_policy(
            &labels(&[]),
            PolicyDefaults {
                watch_all: true,
                mode: Some(Mode::Nightly),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.mode, Mode::Nightly);
        assert!(p.auto_enabled);
    }

    #[test]
    fn watch_all_enable_false_opts_out() {
        let p = parse_policy(&labels(&[("freshdock.enable", "false")]), watch_all()).unwrap();
        assert!(!p.enabled);
        assert!(!p.auto_enabled);
    }

    #[test]
    fn watch_all_mode_off_opts_out_via_mode() {
        // No enable label, so watch_all enables it; `mode=off` is what the
        // downstream gates refuse.
        let p = parse_policy(&labels(&[("freshdock.mode", "off")]), watch_all()).unwrap();
        assert!(p.enabled);
        assert_eq!(p.mode, Mode::Off);
        assert!(p.auto_enabled);
    }

    #[test]
    fn watch_all_watchtower_enable_false_opts_out() {
        let p = parse_policy(
            &labels(&[("com.centurylinklabs.watchtower.enable", "false")]),
            watch_all(),
        )
        .unwrap();
        assert!(!p.enabled, "a watchtower exclusion label still excludes");
        assert!(!p.auto_enabled);
    }

    #[test]
    fn explicit_enable_true_is_not_auto_enabled() {
        for pairs in [
            &[("freshdock.enable", "true")][..],
            &[("com.centurylinklabs.watchtower.enable", "true")][..],
        ] {
            let p = parse_policy(&labels(pairs), watch_all()).unwrap();
            assert!(p.enabled);
            assert!(
                !p.auto_enabled,
                "an explicit label opted in, not watch_all: {pairs:?}"
            );
        }
    }

    #[test]
    fn watch_all_off_preserves_current_behavior() {
        let p = parse_policy(&labels(&[]), PolicyDefaults::default()).unwrap();
        assert!(!p.enabled);
        assert!(!p.auto_enabled);

        let p = parse_policy(
            &labels(&[("freshdock.enable", "true")]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert!(p.enabled);
        assert!(!p.auto_enabled);
    }

    #[test]
    fn schedule_is_captured_as_string() {
        let p = parse_policy(
            &labels(&[
                ("freshdock.enable", "true"),
                ("freshdock.mode", "nightly"),
                ("freshdock.schedule", "0 4 * * *"),
            ]),
            PolicyDefaults::default(),
        )
        .unwrap();
        assert_eq!(p.schedule.as_deref(), Some("0 4 * * *"));
    }
}
