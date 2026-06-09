use std::collections::HashMap;
use std::fmt;

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
}

/// Fleet-wide defaults from `[settings]`, applied when a container omits the
/// matching `freshdock.*` label. A per-container label always overrides these.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyDefaults {
    /// Mode for an enabled container with no `freshdock.mode` (else `watch`).
    pub mode: Option<Mode>,
    /// Default for `freshdock.cleanup`.
    pub cleanup: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LabelError {
    #[error("invalid value for label `{key}`: `{value}` (expected true/false)")]
    InvalidBool { key: String, value: String },
    #[error(
        "invalid value for label `freshdock.mode`: `{value}` (expected one of live, nightly, weekly, monthly, watch, off)"
    )]
    InvalidMode { value: String },
}

pub fn parse_policy(
    labels: &HashMap<String, String>,
    defaults: PolicyDefaults,
) -> Result<Policy, LabelError> {
    let enabled = match labels.get("freshdock.enable") {
        None => false,
        Some(v) => parse_bool("freshdock.enable", v)?,
    };

    if !enabled {
        return Ok(Policy {
            enabled: false,
            mode: Mode::Off,
            notify: false,
            schedule: None,
            cleanup: false,
        });
    }

    let mode = match labels.get("freshdock.mode") {
        Some(v) => parse_mode(v)?,
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

    Ok(Policy {
        enabled,
        mode,
        notify,
        schedule,
        cleanup,
    })
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
