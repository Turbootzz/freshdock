//! Registry credential loading (Phase 5, P5-2).
//!
//! Credentials come from two layers, environment **always winning** over file
//! (PLAN §5.6): a `freshdock.toml` `[registry.<name>]` table and
//! `FRESHDOCK_REGISTRY_<NAME>_USERNAME` / `_TOKEN` env vars. Both are keyed by a
//! friendly registry name (`dockerhub`, `ghcr`, `quay`, `lscr`) or a literal
//! host; [`canonicalize_host`] folds those onto one canonical host so the file
//! entry, the env override, and the image's registry host all resolve together.
//!
//! Secrets are wrapped in [`Secret`], whose `Debug` redacts the value and which
//! has no `Display` — so a token can never reach a log line, even at trace.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tracing::warn;

use crate::labels::{Mode, PolicyDefaults};

/// A credential value (password / personal access token) that must never appear
/// in logs. `Debug` prints `Secret("[REDACTED]")`; there is deliberately no
/// `Display` impl, so `tracing`'s `%field` can't stringify it and `?field` goes
/// through the redacting `Debug`. Call [`Secret::expose`] only at the point of
/// building an auth header.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw secret. Only call where the value is consumed (e.g. an
    /// `Authorization` header) — never to log or format it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(\"[REDACTED]\")")
    }
}

/// One registry's credentials. `username` is optional (GHCR accepts any
/// username with a PAT; Docker Hub requires the real account name).
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryCredentials {
    #[serde(default)]
    pub username: Option<String>,
    pub token: Secret,
}

/// The parsed `freshdock.toml`: registry credentials (Phase 5), notification
/// targets (Phase 6), and fleet-wide `[settings]` defaults.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub registry: HashMap<String, RegistryCredentials>,
    #[serde(default)]
    pub notifications: HashMap<String, NotificationTarget>,
    #[serde(default)]
    pub settings: Settings,
}

/// The `[settings]` table: fleet-wide defaults a container can override with a
/// `freshdock.*` label. Kept as raw strings here; [`Config::load`] validates
/// them into [`ResolvedSettings`] so a bad value warns instead of aborting.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Settings {
    /// Mode for an enabled container with no `freshdock.mode` label. An
    /// unrecognised value is warned about and ignored (falls back to `watch`).
    #[serde(default)]
    pub default_mode: Option<String>,
    /// Default for the per-container `freshdock.cleanup` toggle: remove the
    /// superseded image after a healthy update. Off by default (PLAN §5.2).
    #[serde(default)]
    pub cleanup: bool,
    /// Additionally run a daemon-wide dangling-image prune after a successful
    /// update. Daemon-wide, so global-only (no per-container override).
    #[serde(default)]
    pub prune_dangling: bool,
}

/// Validated [`Settings`], ready for the commands. `Copy` so it threads cheaply
/// through the scheduler chain alongside the other borrowed config.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolvedSettings {
    pub default_mode: Option<Mode>,
    pub cleanup: bool,
    pub prune_dangling: bool,
}

impl ResolvedSettings {
    /// The label-parsing defaults this implies (mode + cleanup). `prune_dangling`
    /// is not a label concept, so it is not part of [`PolicyDefaults`].
    pub fn policy_defaults(&self) -> PolicyDefaults {
        PolicyDefaults {
            mode: self.default_mode,
            cleanup: self.cleanup,
        }
    }
}

/// Validate the raw `[settings]` table. An invalid `default_mode` is a warning,
/// not a hard error — mirrors the resilient env-overlay handling so one typo
/// can't stop the daemon from starting.
fn resolve_settings(settings: Settings) -> ResolvedSettings {
    let default_mode = match settings.default_mode.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<Mode>() {
            Ok(mode) => Some(mode),
            Err(_) => {
                warn!(
                    value = %raw,
                    "ignoring invalid [settings] default_mode (expected one of \
                     live, nightly, weekly, monthly, watch, off); falling back to watch"
                );
                None
            }
        },
    };
    ResolvedSettings {
        default_mode,
        cleanup: settings.cleanup,
        prune_dangling: settings.prune_dangling,
    }
}

/// One `[notifications.<name>]` table. The `type` field selects the backend; an
/// unknown value is a clean parse error. `triggers` filters which lifecycle
/// events reach this target — omitted means all of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NotificationTarget {
    Webhook {
        // `Secret`: a webhook URL can embed a token (Discord's always does), and
        // wrapping it keeps the derived `Debug` from ever leaking one.
        url: Secret,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
    Discord {
        webhook_url: Secret,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
    Telegram {
        bot_token: Secret,
        chat_id: String,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
    Smtp {
        host: String,
        #[serde(default = "default_smtp_port")]
        port: u16,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<Secret>,
        from: String,
        to: Vec<String>,
        #[serde(default = "default_true")]
        starttls: bool,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
}

/// SMTP submission port — STARTTLS on 587 is the modern default.
fn default_smtp_port() -> u16 {
    587
}

fn default_true() -> bool {
    true
}

/// Notification targets after the secret env-overlay, ready for the dispatcher.
#[derive(Debug, Default)]
pub struct NotificationConfig {
    pub targets: HashMap<String, NotificationTarget>,
}

/// Everything loaded from `freshdock.toml` + environment, shared across the run.
/// Credentials are `Arc` (shared with the registry and daemon pull); the
/// notification config is consumed once when the daemon builds its dispatcher.
pub struct LoadedConfig {
    pub credentials: Arc<CredentialStore>,
    pub notifications: NotificationConfig,
    pub settings: ResolvedSettings,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing config file {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

/// Credentials indexed by canonical registry host, ready for O(1) lookup by the
/// registry HEAD flow and the daemon pull. Built once and shared (`Arc`).
#[derive(Debug, Default)]
pub struct CredentialStore {
    by_host: HashMap<String, RegistryCredentials>,
}

impl CredentialStore {
    /// Credentials for an image's registry host (any alias or host form).
    pub fn get(&self, host: &str) -> Option<&RegistryCredentials> {
        self.by_host.get(&canonicalize_host(host))
    }

    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
    }
}

impl Config {
    /// Parse a `freshdock.toml` body. Pure (no I/O) so tests don't touch disk.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// Load the config: read `path` (or the default `./freshdock.toml` when
    /// `path` is `None`), then overlay `FRESHDOCK_REGISTRY_*` /
    /// `FRESHDOCK_NOTIFY_*` env vars on top.
    ///
    /// An *explicit* path that doesn't exist is an error; a missing *default*
    /// file is not (it just yields env-only / empty config).
    pub fn load(path: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
        let mut config = match path {
            Some(p) => Self::read_file(p)?,
            None => {
                let default = Path::new(DEFAULT_CONFIG_FILE);
                // `try_exists` surfaces a permission/IO error instead of
                // silently treating it as "absent" and yielding empty config.
                match default.try_exists() {
                    Ok(true) => Self::read_file(default)?,
                    Ok(false) => Self::default(),
                    Err(source) => {
                        return Err(ConfigError::Read {
                            path: default.display().to_string(),
                            source,
                        });
                    }
                }
            }
        };
        // Take notifications + settings out before `build_store` consumes the
        // rest; both env overlays read the same process env (`vars()` is cheap).
        let notifications =
            build_notifications(std::mem::take(&mut config.notifications), std::env::vars());
        let settings = resolve_settings(std::mem::take(&mut config.settings));
        let credentials = Arc::new(build_store(config, std::env::vars()));
        Ok(LoadedConfig {
            credentials,
            notifications,
            settings,
        })
    }

    fn read_file(path: &Path) -> Result<Self, ConfigError> {
        let body = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&body).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}

/// Default config search path, relative to the working directory.
pub const DEFAULT_CONFIG_FILE: &str = "freshdock.toml";

/// Documentation of the recognised credential env vars, surfaced in `--help`.
pub const ENV_VAR_HELP: &str = "Registry credentials may also be supplied via environment, which \
overrides the config file:\n  FRESHDOCK_REGISTRY_<NAME>_USERNAME   e.g. FRESHDOCK_REGISTRY_GHCR_USERNAME\n  \
FRESHDOCK_REGISTRY_<NAME>_TOKEN      e.g. FRESHDOCK_REGISTRY_GHCR_TOKEN\n<NAME> is dockerhub, ghcr, quay, \
lscr, or a registry host.\nNotification secrets may be overridden the same way (<NAME> is the \
[notifications.<NAME>] table name, upper-cased with '-' as '_'):\n  FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN    (telegram)\n  \
FRESHDOCK_NOTIFY_<NAME>_PASSWORD     (smtp)\nUse plain alphanumeric target names so two can't map to the \
same variable (e.g. `ops-mail` and `ops_mail` collide).\nFRESHDOCK_CONFIG sets the config file path.";

/// Fold a config key / image host onto its canonical registry host so a
/// `[registry.dockerhub]` table, a `FRESHDOCK_REGISTRY_DOCKERHUB_TOKEN`, and an
/// image's `registry-1.docker.io` host all collapse to the same lookup key.
pub(crate) fn canonicalize_host(key: &str) -> String {
    match key.trim().to_ascii_lowercase().as_str() {
        "dockerhub" | "docker" | "docker.io" | "registry-1.docker.io" | "index.docker.io" => {
            "docker.io".to_string()
        }
        "ghcr" | "ghcr.io" => "ghcr.io".to_string(),
        "quay" | "quay.io" => "quay.io".to_string(),
        "lscr" | "lscr.io" => "lscr.io".to_string(),
        other => other.to_string(),
    }
}

/// Merge a parsed [`Config`] with `FRESHDOCK_REGISTRY_*` env vars into a
/// canonical-host [`CredentialStore`]. Env wins, **per field** — a lone
/// `FRESHDOCK_REGISTRY_GHCR_TOKEN` overrides the file token while keeping the
/// file's username. Injecting `env_vars` keeps this pure and testable (pass
/// `std::iter::empty()` for a file-only store).
pub fn build_store<I>(config: Config, env_vars: I) -> CredentialStore
where
    I: Iterator<Item = (String, String)>,
{
    let mut by_host: HashMap<String, RegistryCredentials> = HashMap::new();
    for (key, creds) in config.registry {
        by_host.insert(canonicalize_host(&key), creds);
    }

    // Collect partial env credentials first (a registry may set only a token,
    // only a username, or both across two separate vars).
    let mut env: HashMap<String, (Option<String>, Option<Secret>)> = HashMap::new();
    for (key, value) in env_vars {
        let Some(rest) = key.strip_prefix("FRESHDOCK_REGISTRY_") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix("_USERNAME") {
            env.entry(canonicalize_host(&name.to_ascii_lowercase()))
                .or_default()
                .0 = Some(value);
        } else if let Some(name) = rest.strip_suffix("_TOKEN") {
            env.entry(canonicalize_host(&name.to_ascii_lowercase()))
                .or_default()
                .1 = Some(Secret::new(value));
        }
    }

    for (host, (username, token)) in env {
        match by_host.get_mut(&host) {
            Some(existing) => {
                if username.is_some() {
                    existing.username = username;
                }
                if let Some(token) = token {
                    existing.token = token;
                }
            }
            None => match token {
                Some(token) => {
                    by_host.insert(host, RegistryCredentials { username, token });
                }
                // A username with no token (file or env) can't authenticate.
                None => warn!(
                    registry = %host,
                    "ignoring FRESHDOCK_REGISTRY_*_USERNAME with no matching token"
                ),
            },
        }
    }

    CredentialStore { by_host }
}

/// Normalize a target name to its env-var form (upper-case, `-` → `_`), so
/// `[notifications.ops-mail]` is overridden by `FRESHDOCK_NOTIFY_OPS_MAIL_*`.
fn notify_env_name(key: &str) -> String {
    key.to_ascii_uppercase().replace('-', "_")
}

/// Overlay `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN` / `_PASSWORD` env vars onto the
/// declared targets, so a Telegram token or SMTP password can stay out of the
/// file. Env only *overrides a secret on an already-declared target* — it never
/// creates a target (KISS). Injecting `env_vars` keeps this pure and testable.
pub fn build_notifications<I>(
    mut targets: HashMap<String, NotificationTarget>,
    env_vars: I,
) -> NotificationConfig
where
    I: Iterator<Item = (String, String)>,
{
    // Map each target's env-name back to its real key for O(1) lookup.
    let index: HashMap<String, String> = targets
        .keys()
        .map(|k| (notify_env_name(k), k.clone()))
        .collect();

    for (key, value) in env_vars {
        let Some(rest) = key.strip_prefix("FRESHDOCK_NOTIFY_") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix("_BOT_TOKEN") {
            match index.get(name).and_then(|k| targets.get_mut(k)) {
                Some(NotificationTarget::Telegram { bot_token, .. }) => {
                    *bot_token = Secret::new(value);
                }
                _ => warn!(
                    target = %name,
                    "ignoring FRESHDOCK_NOTIFY_*_BOT_TOKEN: no matching telegram target"
                ),
            }
        } else if let Some(name) = rest.strip_suffix("_PASSWORD") {
            match index.get(name).and_then(|k| targets.get_mut(k)) {
                Some(NotificationTarget::Smtp { password, .. }) => {
                    *password = Some(Secret::new(value));
                }
                _ => warn!(
                    target = %name,
                    "ignoring FRESHDOCK_NOTIFY_*_PASSWORD: no matching smtp target"
                ),
            }
        }
    }

    NotificationConfig { targets }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> std::vec::IntoIter<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_registry_table() {
        let cfg = Config::from_toml(
            r#"
            [registry.ghcr]
            username = "octocat"
            token = "ghp_xxx"
            "#,
        )
        .unwrap();
        let store = build_store(cfg, env(&[]));
        let c = store
            .get("ghcr.io")
            .expect("ghcr alias resolves to ghcr.io");
        assert_eq!(c.username.as_deref(), Some("octocat"));
        assert_eq!(c.token.expose(), "ghp_xxx");
    }

    #[test]
    fn token_without_username_is_allowed() {
        let cfg = Config::from_toml("[registry.quay]\ntoken = \"t\"\n").unwrap();
        let store = build_store(cfg, env(&[]));
        let c = store.get("quay.io").unwrap();
        assert!(c.username.is_none());
        assert_eq!(c.token.expose(), "t");
    }

    #[test]
    fn env_token_overrides_file_token_keeping_file_username() {
        let cfg =
            Config::from_toml("[registry.ghcr]\nusername = \"u\"\ntoken = \"file\"\n").unwrap();
        let store = build_store(cfg, env(&[("FRESHDOCK_REGISTRY_GHCR_TOKEN", "envtok")]));
        let c = store.get("ghcr.io").unwrap();
        // Per-field: env wins on token, file username survives.
        assert_eq!(c.token.expose(), "envtok");
        assert_eq!(c.username.as_deref(), Some("u"));
    }

    #[test]
    fn env_creates_entry_when_file_has_none() {
        let store = build_store(
            Config::default(),
            env(&[
                ("FRESHDOCK_REGISTRY_DOCKERHUB_USERNAME", "me"),
                ("FRESHDOCK_REGISTRY_DOCKERHUB_TOKEN", "pat"),
            ]),
        );
        let c = store
            .get("docker.io")
            .expect("dockerhub env maps to docker.io");
        assert_eq!(c.username.as_deref(), Some("me"));
        assert_eq!(c.token.expose(), "pat");
    }

    #[test]
    fn env_username_without_token_is_dropped() {
        let store = build_store(
            Config::default(),
            env(&[("FRESHDOCK_REGISTRY_GHCR_USERNAME", "u")]),
        );
        assert!(store.get("ghcr.io").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn unrelated_env_vars_are_ignored() {
        let store = build_store(
            Config::default(),
            env(&[("PATH", "/usr/bin"), ("FRESHDOCK_CONFIG", "x.toml")]),
        );
        assert!(store.is_empty());
    }

    #[test]
    fn canonicalize_folds_aliases_and_hosts() {
        assert_eq!(canonicalize_host("dockerhub"), "docker.io");
        assert_eq!(canonicalize_host("registry-1.docker.io"), "docker.io");
        assert_eq!(canonicalize_host("GHCR"), "ghcr.io");
        assert_eq!(canonicalize_host("quay.io"), "quay.io");
        // Unknown hosts pass through lowercased.
        assert_eq!(canonicalize_host("Reg.Example.COM"), "reg.example.com");
    }

    #[test]
    fn unknown_host_key_in_file_is_kept_verbatim() {
        let cfg = Config::from_toml("[registry.\"reg.example.com\"]\ntoken = \"t\"\n").unwrap();
        let store = build_store(cfg, env(&[]));
        assert!(store.get("reg.example.com").is_some());
    }

    // --- secret redaction ---

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(\"[REDACTED]\")");
        let c = RegistryCredentials {
            username: Some("u".into()),
            token: Secret::new("hunter2"),
        };
        assert!(
            !format!("{c:?}").contains("hunter2"),
            "token leaked via struct Debug: {c:?}"
        );
    }

    #[test]
    fn token_is_redacted_in_tracing_output() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
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

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_ansi(false)
            .finish();

        let creds = RegistryCredentials {
            username: Some("user".into()),
            token: Secret::new("supersecret-pat"),
        };
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(?creds, "loaded credentials");
            tracing::info!(token = ?creds.token, "token field");
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!out.contains("supersecret-pat"), "secret leaked: {out}");
        assert!(
            out.contains("[REDACTED]"),
            "expected redaction marker: {out}"
        );
    }

    // --- notification config ---

    fn notifications(toml: &str) -> HashMap<String, NotificationTarget> {
        Config::from_toml(toml).unwrap().notifications
    }

    #[test]
    fn parses_each_backend_type() {
        let t = notifications(
            r#"
            [notifications.hook]
            type = "webhook"
            url = "https://example.com/h"

            [notifications.chat]
            type = "discord"
            webhook_url = "https://discord.com/api/webhooks/1/a"

            [notifications.tg]
            type = "telegram"
            bot_token = "123:abc"
            chat_id = "42"

            [notifications.mail]
            type = "smtp"
            host = "smtp.example.com"
            from = "a@example.com"
            to = ["b@example.com"]
            "#,
        );
        assert!(matches!(t["hook"], NotificationTarget::Webhook { .. }));
        assert!(matches!(t["chat"], NotificationTarget::Discord { .. }));
        assert!(matches!(t["tg"], NotificationTarget::Telegram { .. }));
        assert!(matches!(t["mail"], NotificationTarget::Smtp { .. }));
    }

    #[test]
    fn unknown_backend_type_is_a_parse_error() {
        let err = Config::from_toml(
            "[notifications.x]\ntype = \"carrier-pigeon\"\nurl = \"https://e.com\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("carrier-pigeon") || err.to_string().contains("unknown"));
    }

    #[test]
    fn omitted_triggers_parse_to_none_and_smtp_defaults_apply() {
        let t = notifications(
            "[notifications.mail]\ntype = \"smtp\"\nhost = \"h\"\nfrom = \"a@e.com\"\nto = [\"b@e.com\"]\n",
        );
        match &t["mail"] {
            NotificationTarget::Smtp {
                port,
                starttls,
                triggers,
                ..
            } => {
                assert_eq!(*port, 587, "default submission port");
                assert!(*starttls, "starttls defaults on");
                assert!(
                    triggers.is_none(),
                    "omitted triggers → None (subscribe all)"
                );
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn env_overlays_telegram_token_and_smtp_password_onto_declared_targets() {
        let targets = notifications(
            r#"
            [notifications.tg]
            type = "telegram"
            bot_token = "file-token"
            chat_id = "42"

            [notifications.mail]
            type = "smtp"
            host = "h"
            from = "a@e.com"
            to = ["b@e.com"]
            "#,
        );
        let cfg = build_notifications(
            targets,
            env(&[
                ("FRESHDOCK_NOTIFY_TG_BOT_TOKEN", "env-token"),
                ("FRESHDOCK_NOTIFY_MAIL_PASSWORD", "env-pass"),
            ]),
        );
        match &cfg.targets["tg"] {
            NotificationTarget::Telegram { bot_token, .. } => {
                assert_eq!(bot_token.expose(), "env-token", "env wins over file token");
            }
            _ => unreachable!(),
        }
        match &cfg.targets["mail"] {
            NotificationTarget::Smtp { password, .. } => {
                assert_eq!(password.as_ref().unwrap().expose(), "env-pass");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn env_overlay_ignores_a_type_mismatch() {
        // A webhook target named like a telegram override must not be mutated.
        let targets =
            notifications("[notifications.tg]\ntype = \"webhook\"\nurl = \"https://e.com\"\n");
        let cfg = build_notifications(
            targets,
            env(&[("FRESHDOCK_NOTIFY_TG_BOT_TOKEN", "env-token")]),
        );
        assert!(matches!(
            cfg.targets["tg"],
            NotificationTarget::Webhook { .. }
        ));
    }

    #[test]
    fn env_overlay_matches_a_hyphenated_target_name() {
        // `[notifications.ops-mail]` must be overridden by
        // FRESHDOCK_NOTIFY_OPS_MAIL_PASSWORD (`-` normalised to `_`).
        let targets = notifications(
            "[notifications.ops-mail]\ntype = \"smtp\"\nhost = \"h\"\nfrom = \"a@e.com\"\nto = [\"b@e.com\"]\n",
        );
        let cfg = build_notifications(
            targets,
            env(&[("FRESHDOCK_NOTIFY_OPS_MAIL_PASSWORD", "env-pass")]),
        );
        match &cfg.targets["ops-mail"] {
            NotificationTarget::Smtp { password, .. } => {
                assert_eq!(password.as_ref().unwrap().expose(), "env-pass");
            }
            _ => unreachable!(),
        }
    }

    // --- [settings] table ---

    #[test]
    fn settings_table_parses_and_resolves() {
        let cfg = Config::from_toml(
            r#"
            [settings]
            default_mode = "nightly"
            cleanup = true
            prune_dangling = true
            "#,
        )
        .unwrap();
        let resolved = resolve_settings(cfg.settings);
        assert_eq!(resolved.default_mode, Some(Mode::Nightly));
        assert!(resolved.cleanup);
        assert!(resolved.prune_dangling);
        assert_eq!(
            resolved.policy_defaults().mode,
            Some(Mode::Nightly),
            "policy_defaults forwards the resolved mode"
        );
        assert!(resolved.policy_defaults().cleanup);
    }

    #[test]
    fn missing_settings_table_yields_all_defaults() {
        let cfg = Config::from_toml("[registry.ghcr]\ntoken = \"t\"\n").unwrap();
        let resolved = resolve_settings(cfg.settings);
        assert_eq!(resolved.default_mode, None);
        assert!(!resolved.cleanup);
        assert!(!resolved.prune_dangling);
    }

    #[test]
    fn invalid_default_mode_is_ignored_not_an_error() {
        // A bad mode must not abort load — it warns and falls back to None
        // (which downstream resolves to `watch`).
        let cfg = Config::from_toml("[settings]\ndefault_mode = \"hourly\"\n").unwrap();
        let resolved = resolve_settings(cfg.settings);
        assert_eq!(resolved.default_mode, None);
    }
}
