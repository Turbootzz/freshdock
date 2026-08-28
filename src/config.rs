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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use tracing::{info, warn};
use url::Url;

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
    /// Treat every running container as enabled unless it opts out, the
    /// Watchtower model (issue #79). Off by default: freshdock is opt-in.
    #[serde(default)]
    pub watch_all: bool,
    /// Treat a Docker Compose project as one update unit (issue #78). **On**
    /// by default, so an `Option` whose `None` resolves to `true` rather than a
    /// `bool` whose `Default` would be `false`.
    #[serde(default)]
    pub compose_aware: Option<bool>,
}

/// Validated [`Settings`], ready for the commands. `Copy` so it threads cheaply
/// through the scheduler chain alongside the other borrowed config.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedSettings {
    pub default_mode: Option<Mode>,
    pub cleanup: bool,
    pub prune_dangling: bool,
    pub watch_all: bool,
    pub compose_aware: bool,
}

/// Hand-written so `compose_aware` keeps its non-`false` default here too; a
/// derived one would quietly disagree with [`resolve_settings`].
impl Default for ResolvedSettings {
    fn default() -> Self {
        Self {
            default_mode: None,
            cleanup: false,
            prune_dangling: false,
            watch_all: false,
            compose_aware: true,
        }
    }
}

impl ResolvedSettings {
    /// The label-parsing defaults this implies (mode + cleanup + watch_all).
    /// `prune_dangling` is not a label concept, so it is not part of
    /// [`PolicyDefaults`].
    pub fn policy_defaults(&self) -> PolicyDefaults {
        PolicyDefaults {
            mode: self.default_mode,
            cleanup: self.cleanup,
            watch_all: self.watch_all,
        }
    }
}

/// Parse an env-var boolean. Env vars accept `1`/`0` alongside `true`/`false`
/// (labels stay strict `true`/`false`) because `VAR=1` is the dominant idiom in
/// compose files and Watchtower configs.
fn parse_env_bool(var: &str, raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => {
            warn!(
                var = %var,
                value = %raw,
                "ignoring invalid boolean (expected true/false/1/0)"
            );
            None
        }
    }
}

/// Overlay `FRESHDOCK_DEFAULT_MODE` / `FRESHDOCK_CLEANUP` /
/// `FRESHDOCK_PRUNE_DANGLING` / `FRESHDOCK_WATCH_ALL` /
/// `FRESHDOCK_COMPOSE_AWARE` onto the `[settings]`
/// table (env wins per field,
/// like the registry overlay), then validate. An invalid value is a warning,
/// not a hard error — the next layer (file value, then built-in default)
/// applies, so one typo can't stop the daemon from starting.
fn resolve_settings<I>(mut settings: Settings, env_vars: I) -> ResolvedSettings
where
    I: Iterator<Item = (String, String)>,
{
    for (key, value) in env_vars {
        match key.as_str() {
            // Validate eagerly so a bad env mode keeps the *file* value rather
            // than clobbering it with a string the resolution below rejects.
            "FRESHDOCK_DEFAULT_MODE" => {
                if value.parse::<Mode>().is_ok() {
                    settings.default_mode = Some(value);
                } else {
                    warn!(
                        value = %value,
                        "ignoring invalid FRESHDOCK_DEFAULT_MODE (expected one of \
                         live, nightly, weekly, monthly, watch, off)"
                    );
                }
            }
            "FRESHDOCK_CLEANUP" => {
                if let Some(flag) = parse_env_bool(&key, &value) {
                    settings.cleanup = flag;
                }
            }
            "FRESHDOCK_PRUNE_DANGLING" => {
                if let Some(flag) = parse_env_bool(&key, &value) {
                    settings.prune_dangling = flag;
                }
            }
            "FRESHDOCK_WATCH_ALL" => {
                if let Some(flag) = parse_env_bool(&key, &value) {
                    settings.watch_all = flag;
                }
            }
            "FRESHDOCK_COMPOSE_AWARE" => {
                if let Some(flag) = parse_env_bool(&key, &value) {
                    settings.compose_aware = Some(flag);
                }
            }
            _ => {}
        }
    }
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
        watch_all: settings.watch_all,
        compose_aware: settings.compose_aware.unwrap_or(true),
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
        /// Omitted means "whatever suits the transport": the default follows the
        /// resolved TLS mode (587 / 465 / 25), filled in by
        /// [`crate::notify`] where that mode is known.
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<Secret>,
        from: String,
        to: Vec<String>,
        /// Transport security. Both this and the legacy `starttls` are raw
        /// `Option`s here; [`resolve_smtp_tls`] is the single place that turns
        /// the pair into one mode (and rejects a contradictory pair).
        #[serde(default)]
        tls: Option<SmtpTls>,
        /// Legacy alias for `tls`: `true` → STARTTLS, `false` → **implicit
        /// TLS**, never plaintext.
        #[serde(default)]
        starttls: Option<bool>,
        #[serde(default)]
        triggers: Option<Vec<String>>,
    },
}

/// How the SMTP connection is secured. Three-valued because `starttls: bool`
/// could not express "no TLS at all", which is what a local catcher
/// (mailpit/MailHog) speaks — issue #57.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum SmtpTls {
    /// Upgrade a plain connection with STARTTLS (submission port 587).
    Starttls,
    /// TLS from the first byte (SMTPS, typically port 465).
    Implicit,
    /// No TLS at all. Local development only; credentials and message content
    /// travel in the clear, so [`crate::notify::smtp`] logs a warning.
    Plaintext,
}

impl fmt::Display for SmtpTls {
    /// The canonical config token, so an error message can quote a value the
    /// operator can paste straight back into `tls = "…"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SmtpTls::Starttls => "starttls",
            SmtpTls::Implicit => "implicit",
            SmtpTls::Plaintext => "none",
        })
    }
}

impl std::str::FromStr for SmtpTls {
    type Err = String;

    /// One case-insensitive matcher for both the TOML `tls = "…"` value and the
    /// env URL's `?tls=…`, so the two can never accept different tokens
    /// (the [`crate::labels::Mode`] convention).
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "starttls" => Ok(SmtpTls::Starttls),
            "implicit" => Ok(SmtpTls::Implicit),
            "none" => Ok(SmtpTls::Plaintext),
            _ => Err(format!(
                "unknown smtp tls mode `{raw}` (expected starttls, implicit, or none)"
            )),
        }
    }
}

impl TryFrom<String> for SmtpTls {
    type Error = String;

    /// serde's entry point (`#[serde(try_from = "String")]`), delegating to
    /// [`FromStr`](std::str::FromStr) so the file and the env URL share one
    /// matcher and one error message.
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

/// Collapse the `tls` / legacy `starttls` pair into the one mode the transport
/// is built from.
///
/// Setting both is only an error when the two *contradict* each other. An
/// agreeing pair (`tls = "starttls"` with `starttls = true`, or
/// `tls = "implicit"` with `starttls = false`) is the normal shape of a
/// half-finished migration — the operator added the new key and has not yet
/// deleted the old line — and two statements of the same intent are not
/// ambiguous. Rejecting them would drop the target at dispatcher build,
/// silently disabling notifications behind a single startup warning.
pub fn resolve_smtp_tls(tls: Option<SmtpTls>, starttls: Option<bool>) -> Result<SmtpTls, String> {
    match (tls, starttls) {
        // Both keys, saying the same thing.
        (Some(SmtpTls::Starttls), Some(true)) => Ok(SmtpTls::Starttls),
        (Some(SmtpTls::Implicit), Some(false)) => Ok(SmtpTls::Implicit),
        // Both keys, disagreeing — including every plaintext pairing, since the
        // legacy boolean cannot express "no TLS" at all. Name both values so the
        // operator knows exactly which line to delete.
        (Some(tls), Some(starttls)) => Err(format!(
            "tls = \"{tls}\" contradicts starttls = {starttls}; remove the legacy starttls key"
        )),
        (Some(tls), None) => Ok(tls),
        // `starttls = false` predates the plaintext mode and always meant
        // implicit TLS; keep that meaning so an existing config can't silently
        // downgrade to cleartext.
        (None, Some(true)) => Ok(SmtpTls::Starttls),
        (None, Some(false)) => Ok(SmtpTls::Implicit),
        (None, None) => Ok(SmtpTls::Starttls),
    }
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

    pub fn len(&self) -> usize {
        self.by_host.len()
    }

    /// Canonical hosts with credentials loaded, sorted for stable log/test
    /// output. Returns host names only — never a [`Secret`] — so it is safe to
    /// log at any level.
    pub fn hosts(&self) -> Vec<&str> {
        let mut hosts: Vec<&str> = self.by_host.keys().map(String::as_str).collect();
        hosts.sort_unstable();
        hosts
    }
}

/// Emit a one-line startup confirmation of which registries have credentials, so
/// an operator can tell whether their `FRESHDOCK_REGISTRY_*` env vars were picked
/// up. The empty case is `info!` (not `debug!`) on purpose: someone who set a
/// credential and sees "anonymous access only" immediately knows the var name was
/// wrong. Logs host names only — never a [`Secret`].
fn log_credentials_loaded(credentials: &CredentialStore) {
    if credentials.is_empty() {
        info!("no registry credentials loaded; using anonymous access only");
    } else {
        info!(
            count = credentials.len(),
            registries = ?credentials.hosts(),
            "registry credentials loaded"
        );
    }
}

impl Config {
    /// Parse a `freshdock.toml` body. Pure (no I/O) so tests don't touch disk.
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }

    /// Load the config: read `path` (or the default `./freshdock.toml` when
    /// `path` is `None`), then overlay `FRESHDOCK_REGISTRY_*` /
    /// `FRESHDOCK_NOTIFY_*` / `FRESHDOCK_DEFAULT_MODE` / `FRESHDOCK_CLEANUP` /
    /// `FRESHDOCK_PRUNE_DANGLING` / `FRESHDOCK_WATCH_ALL` /
    /// `FRESHDOCK_COMPOSE_AWARE` env vars on top.
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
        // rest; all three env overlays read the same process env (`vars()` is
        // cheap).
        let notifications =
            build_notifications(std::mem::take(&mut config.notifications), std::env::vars());
        let settings = resolve_settings(std::mem::take(&mut config.settings), std::env::vars());
        let credentials = Arc::new(build_store(config, std::env::vars()));
        log_credentials_loaded(&credentials);
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

/// Documentation of the recognised env vars, surfaced in `--help`.
pub const ENV_VAR_HELP: &str = "Registry credentials may also be supplied via environment, which \
overrides the config file:\n  FRESHDOCK_REGISTRY_<NAME>_USERNAME   e.g. FRESHDOCK_REGISTRY_GHCR_USERNAME\n  \
FRESHDOCK_REGISTRY_<NAME>_TOKEN      e.g. FRESHDOCK_REGISTRY_GHCR_TOKEN\n<NAME> is dockerhub, ghcr, quay, \
lscr, or a registry host.\nNotification targets can be declared from the environment alone (no file) via a \
shoutrrr-style URL, with an optional trigger filter:\n  FRESHDOCK_NOTIFY_<NAME>_URL          discord://token@id | \
telegram://token@telegram?chats=id | smtp://user:pass@host[:port]/?from=a&to=b&tls=starttls|implicit|none | \
https://host/hook\n  \
FRESHDOCK_NOTIFY_<NAME>_TRIGGERS     comma list of available,succeeded,failed (default all)\nA target's secret may \
also be overridden on its own (<NAME> is the [notifications.<NAME>] table name, upper-cased with '-' as '_'):\n  \
FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN    (telegram)\n  \
FRESHDOCK_NOTIFY_<NAME>_PASSWORD     (smtp)\nUse plain alphanumeric target names so two can't map to the \
same variable (e.g. `ops-mail` and `ops_mail` collide).\n[settings] defaults may be supplied or overridden \
the same way:\n  FRESHDOCK_DEFAULT_MODE               live|nightly|weekly|monthly|watch|off\n  \
FRESHDOCK_CLEANUP                    true/false/1/0\n  FRESHDOCK_PRUNE_DANGLING             true/false/1/0\n  \
FRESHDOCK_WATCH_ALL                  true/false/1/0 (watch every container unless it opts out)\n  \
FRESHDOCK_COMPOSE_AWARE              true/false/1/0 (roll a compose project out as one unit; on by default)\n\
Run flags have env forms too, the flag winning: FRESHDOCK_INTERVAL, FRESHDOCK_TICK, FRESHDOCK_STOP_TIMEOUT \
(see `freshdock run --help`).\nNO_COLOR (any non-empty value) disables colored output.\nFRESHDOCK_CONFIG \
sets the config file path.";

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

/// Decode a URL component that `url` hands back still-encoded (userinfo). Query
/// values are already decoded by `query_pairs`, so this is only for username /
/// password — e.g. an SMTP login that had to write `@` as `%40`.
fn pct_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Split a comma-separated value, trimming each item and dropping empties.
fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a comma-separated `_TRIGGERS` value into a subscription list, or `None`
/// (subscribe to all) when it is empty/blank — tokens are validated later by
/// [`crate::notify`], so a bad one warns-and-skips the whole target there.
fn split_triggers(raw: &str) -> Option<Vec<String>> {
    let list = split_csv(raw);
    (!list.is_empty()).then_some(list)
}

/// Set the `triggers` field on any backend variant (one slot, four shapes).
fn set_triggers(target: &mut NotificationTarget, triggers: Option<Vec<String>>) {
    let slot = match target {
        NotificationTarget::Webhook { triggers, .. }
        | NotificationTarget::Discord { triggers, .. }
        | NotificationTarget::Telegram { triggers, .. }
        | NotificationTarget::Smtp { triggers, .. } => triggers,
    };
    *slot = triggers;
}

/// Parse a shoutrrr-style notification URL into a target (issue #54), so a
/// target can be declared entirely from `FRESHDOCK_NOTIFY_<NAME>_URL` with no
/// file — the schemes Watchtower migrants know. `triggers` is left `None` for
/// the caller to fill from `_TRIGGERS`. A bad URL / unknown scheme is an `Err`
/// the caller logs and skips, never a hard failure (resilience parity with the
/// other env overlays). `name` only labels diagnostics.
fn parse_notify_url(name: &str, raw: &str) -> Result<NotificationTarget, String> {
    let url = Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    match url.scheme() {
        // Generic webhook: the URL *is* the endpoint, kept whole as a secret.
        "http" | "https" => Ok(NotificationTarget::Webhook {
            url: Secret::new(raw.to_string()),
            triggers: None,
        }),
        // discord://TOKEN@WEBHOOK_ID → the real Discord webhook URL.
        "discord" => {
            let token = url.username();
            let id = url.host_str().unwrap_or_default();
            if token.is_empty() || id.is_empty() {
                return Err("expected discord://TOKEN@WEBHOOK_ID".to_string());
            }
            Ok(NotificationTarget::Discord {
                webhook_url: Secret::new(format!("https://discord.com/api/webhooks/{id}/{token}")),
                triggers: None,
            })
        }
        // telegram://BOT_TOKEN@telegram?chats=CHAT_ID. A bot token embeds a `:`
        // (`<id>:<secret>`), which the URL parser splits into user:password —
        // rejoin them to recover the original token.
        "telegram" => {
            // The bot id is the userinfo username; a token's `<id>:<secret>`
            // colon is parsed as user:password, so rejoin them. An empty id
            // (`telegram://:secret@…`) is not a valid token.
            if url.username().is_empty() {
                return Err("expected telegram://BOT_TOKEN@telegram?chats=CHAT_ID".to_string());
            }
            // Percent-decode each part (as the SMTP arm does) so an encoded
            // credential round-trips instead of reaching the API still-encoded.
            let id = pct_decode(url.username());
            let bot_token = match url.password() {
                Some(pass) => format!("{id}:{}", pct_decode(pass)),
                None => id,
            };
            let chats: Vec<String> = url
                .query_pairs()
                .filter(|(k, _)| k == "chats" || k == "chat_id")
                .flat_map(|(_, v)| split_csv(&v))
                .collect();
            let Some(chat_id) = chats.first().cloned() else {
                return Err("telegram:// requires ?chats=<id>".to_string());
            };
            if chats.len() > 1 {
                warn!(
                    target = %name,
                    "telegram target supports a single chat; using the first and ignoring the rest"
                );
            }
            Ok(NotificationTarget::Telegram {
                bot_token: Secret::new(bot_token),
                chat_id,
                triggers: None,
            })
        }
        // smtp://[user:pass@]host[:port]/?from=…&to=a,b&tls=starttls|implicit|none.
        "smtp" => {
            let host = url
                .host_str()
                .filter(|h| !h.is_empty())
                .ok_or("smtp:// requires a host")?
                .to_string();
            // No port in the URL leaves `None`: the default depends on the
            // resolved tls mode, which `notify::build_target` fills in.
            let port = url.port();
            let username = (!url.username().is_empty()).then(|| pct_decode(url.username()));
            let password = url.password().map(|p| Secret::new(pct_decode(p)));

            let mut from = None;
            let mut to: Vec<String> = Vec::new();
            let mut tls = None;
            let mut starttls = None;
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "from" => from = Some(value.into_owned()),
                    "to" => to.extend(split_csv(&value)),
                    // The strictness here is deliberately asymmetric: an invalid
                    // `?tls=` value is a hard error that kills the target at
                    // declaration, while an invalid legacy `?starttls=` warns
                    // and falls through to the default. The new key is strict
                    // from day one (nothing depends on its leniency yet); the
                    // legacy key keeps the historical leniency configs may rely
                    // on, and its fallback — STARTTLS — is never a security
                    // downgrade.
                    "tls" => tls = Some(value.parse::<SmtpTls>()?),
                    "starttls" => {
                        if let Some(flag) = parse_env_bool("smtp starttls", &value) {
                            starttls = Some(flag);
                        }
                    }
                    _ => {}
                }
            }
            let from = from.ok_or("smtp:// requires ?from=<addr>")?;
            if to.is_empty() {
                return Err("smtp:// requires ?to=<addr>[,<addr>…]".to_string());
            }
            // Resolve the pair here, once: a contradictory URL is skipped at
            // declaration time, and the target carries the single resolved mode
            // rather than a raw pair for `notify::build_target` to map again.
            let tls = resolve_smtp_tls(tls, starttls)?;
            Ok(NotificationTarget::Smtp {
                host,
                port,
                username,
                password,
                from,
                to,
                tls: Some(tls),
                starttls: None,
                triggers: None,
            })
        }
        other => Err(format!(
            "unknown scheme `{other}` (expected discord, telegram, smtp, http, or https)"
        )),
    }
}

/// Declare notification targets from `FRESHDOCK_NOTIFY_<NAME>_URL` (a
/// shoutrrr-style URL) plus an optional `FRESHDOCK_NOTIFY_<NAME>_TRIGGERS`
/// (issue #54). Unlike the secret overlay below, env here can *create* a target
/// — the last gap to a file-free deployment. Additive: a name already declared
/// in the file wins, so a richer file target is never clobbered by an env URL.
fn declare_env_targets(
    targets: &mut HashMap<String, NotificationTarget>,
    env: &[(String, String)],
) {
    // File-declared names in env-var form, so a collision check is O(1).
    let file_names: HashSet<String> = targets.keys().map(|k| notify_env_name(k)).collect();

    // Gather the URL + triggers per `<NAME>` first (two vars, one target).
    let mut decls: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for (key, value) in env {
        let Some(rest) = key.strip_prefix("FRESHDOCK_NOTIFY_") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix("_URL") {
            decls.entry(name.to_string()).or_default().0 = Some(value.clone());
        } else if let Some(name) = rest.strip_suffix("_TRIGGERS") {
            decls.entry(name.to_string()).or_default().1 = Some(value.clone());
        }
    }

    for (env_name, (url, triggers)) in decls {
        let Some(url) = url else {
            // A `_TRIGGERS` with no `_URL` and no matching file target has
            // nothing to attach to. (A file target sets its triggers in-file.)
            if !file_names.contains(&env_name) {
                warn!(
                    target = %env_name,
                    "ignoring FRESHDOCK_NOTIFY_*_TRIGGERS: no FRESHDOCK_NOTIFY_*_URL declares this target"
                );
            }
            continue;
        };
        if file_names.contains(&env_name) {
            warn!(
                target = %env_name,
                "ignoring FRESHDOCK_NOTIFY_*_URL: a notification target with this name is already declared in the file"
            );
            continue;
        }
        match parse_notify_url(&env_name, &url) {
            Ok(mut target) => {
                set_triggers(&mut target, triggers.as_deref().and_then(split_triggers));
                // Key by the lower-cased env name; it surfaces only in logs and
                // as the dispatcher's target name, and feeds the secret overlay
                // below via `notify_env_name`.
                targets.insert(env_name.to_ascii_lowercase(), target);
            }
            Err(reason) => warn!(
                target = %env_name,
                %reason,
                "ignoring invalid FRESHDOCK_NOTIFY_*_URL"
            ),
        }
    }
}

/// Assemble notification targets from the file plus the `FRESHDOCK_NOTIFY_*`
/// environment in two passes: first *declare* env-only targets from
/// `_URL`/`_TRIGGERS` (issue #54), then *overlay* `_BOT_TOKEN`/`_PASSWORD`
/// secrets onto any target (file- or env-declared). Injecting `env_vars` keeps
/// this pure and testable.
pub fn build_notifications<I>(
    mut targets: HashMap<String, NotificationTarget>,
    env_vars: I,
) -> NotificationConfig
where
    I: Iterator<Item = (String, String)>,
{
    // Buffer the env once: two passes read it, and the iterator is single-use.
    let env: Vec<(String, String)> = env_vars.collect();

    // Pass 1 — declare targets the file didn't (env can *create* one here).
    declare_env_targets(&mut targets, &env);

    // Pass 2 — overlay a secret onto an already-declared target (file or env),
    // so a Telegram token or SMTP password can stay out of any URL/file.
    // Re-index after declaration so env-declared targets are reachable too.
    let index: HashMap<String, String> = targets
        .keys()
        .map(|k| (notify_env_name(k), k.clone()))
        .collect();

    for (key, value) in &env {
        let Some(rest) = key.strip_prefix("FRESHDOCK_NOTIFY_") else {
            continue;
        };
        if let Some(name) = rest.strip_suffix("_BOT_TOKEN") {
            match index.get(name).and_then(|k| targets.get_mut(k)) {
                Some(NotificationTarget::Telegram { bot_token, .. }) => {
                    *bot_token = Secret::new(value.clone());
                }
                _ => warn!(
                    target = %name,
                    "ignoring FRESHDOCK_NOTIFY_*_BOT_TOKEN: no matching telegram target"
                ),
            }
        } else if let Some(name) = rest.strip_suffix("_PASSWORD") {
            match index.get(name).and_then(|k| targets.get_mut(k)) {
                Some(NotificationTarget::Smtp { password, .. }) => {
                    *password = Some(Secret::new(value.clone()));
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

    #[test]
    fn hosts_returns_sorted_canonical_hosts() {
        let cfg = Config::from_toml(
            r#"
            [registry.ghcr]
            token = "g"
            [registry.dockerhub]
            username = "u"
            token = "d"
            [registry."reg.example.com"]
            token = "r"
            "#,
        )
        .unwrap();
        let store = build_store(cfg, env(&[]));
        // Aliases fold to canonical hosts; output is sorted for stable logs.
        assert_eq!(store.hosts(), ["docker.io", "ghcr.io", "reg.example.com"]);
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());
    }

    #[test]
    fn hosts_empty_for_default_store() {
        let store = CredentialStore::default();
        assert!(store.hosts().is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
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

    /// A second dispatcher, registered once and never dropped, that discards
    /// everything — it keeps tracing-core off its single-dispatcher fast path,
    /// where callsite interest is rebuilt from the *rebuilding thread's*
    /// subscriber and a parallel test holding none can blank a capture below.
    /// See the fuller note on the twin in [`crate::notify`]'s tests.
    static KEEPALIVE: std::sync::LazyLock<tracing::Dispatch> = std::sync::LazyLock::new(|| {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_ansi(false)
                .finish(),
        )
    });

    /// Run `f` with a temporary tracing subscriber that writes to an in-memory
    /// buffer, and return everything it logged — so a test can assert on (or
    /// prove the absence of) a value in real log output.
    fn capture_logs(f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        std::sync::LazyLock::force(&KEEPALIVE);

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
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn token_is_redacted_in_tracing_output() {
        let creds = RegistryCredentials {
            username: Some("user".into()),
            token: Secret::new("supersecret-pat"),
        };
        let out = capture_logs(|| {
            tracing::info!(?creds, "loaded credentials");
            tracing::info!(token = ?creds.token, "token field");
        });
        assert!(!out.contains("supersecret-pat"), "secret leaked: {out}");
        assert!(
            out.contains("[REDACTED]"),
            "expected redaction marker: {out}"
        );
    }

    #[test]
    fn credential_summary_log_never_leaks_token() {
        // Exercise the real `Config::load` logging path: prove it can't regress
        // into formatting a token even when the store holds one.
        let cfg = Config::from_toml(
            "[registry.dockerhub]\nusername = \"u\"\ntoken = \"supersecret-pat\"\n",
        )
        .unwrap();
        let store = build_store(cfg, env(&[]));
        let out = capture_logs(|| log_credentials_loaded(&store));
        assert!(!out.contains("supersecret-pat"), "secret leaked: {out}");
        assert!(out.contains("docker.io"), "host should be present: {out}");
    }

    #[test]
    fn credential_summary_logs_anonymous_when_empty() {
        let out = capture_logs(|| log_credentials_loaded(&CredentialStore::default()));
        assert!(
            out.contains("anonymous"),
            "empty store must announce anonymous-only: {out}"
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
                tls,
                starttls,
                triggers,
                ..
            } => {
                assert!(
                    port.is_none(),
                    "omitted port → None; the default follows the resolved tls mode"
                );
                assert!(tls.is_none(), "omitted tls → None (resolver defaults it)");
                assert!(starttls.is_none(), "legacy alias omitted → None");
                assert!(
                    triggers.is_none(),
                    "omitted triggers → None (subscribe all)"
                );
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn smtp_tls_none_parses() {
        let t = notifications(
            "[notifications.mail]\ntype = \"smtp\"\nhost = \"h\"\nfrom = \"a@e.com\"\nto = [\"b@e.com\"]\ntls = \"none\"\n",
        );
        match &t["mail"] {
            NotificationTarget::Smtp { tls, starttls, .. } => {
                assert_eq!(*tls, Some(SmtpTls::Plaintext));
                assert!(starttls.is_none(), "the legacy alias stays unset");
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn smtp_unknown_tls_mode_is_a_parse_error() {
        let err = Config::from_toml(
            "[notifications.mail]\ntype = \"smtp\"\nhost = \"h\"\nfrom = \"a@e.com\"\nto = [\"b@e.com\"]\ntls = \"ssl\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("ssl"), "{err}");
    }

    #[test]
    fn smtp_tls_parses_case_insensitively_and_displays_canonically() {
        for (raw, mode) in [
            ("starttls", SmtpTls::Starttls),
            (" Implicit ", SmtpTls::Implicit),
            ("NONE", SmtpTls::Plaintext),
        ] {
            assert_eq!(raw.parse::<SmtpTls>(), Ok(mode), "parsing {raw:?}");
        }
        assert_eq!(SmtpTls::Starttls.to_string(), "starttls");
        assert_eq!(SmtpTls::Implicit.to_string(), "implicit");
        assert_eq!(SmtpTls::Plaintext.to_string(), "none");
        // Display emits a token FromStr accepts, so an error message that names
        // a mode always names a value the operator can paste back into config.
        for mode in [SmtpTls::Starttls, SmtpTls::Implicit, SmtpTls::Plaintext] {
            assert_eq!(mode.to_string().parse::<SmtpTls>(), Ok(mode));
        }
        // serde's entry point delegates to the same matcher and the same error.
        let err = SmtpTls::try_from("ssl".to_string()).unwrap_err();
        assert_eq!(
            err,
            "unknown smtp tls mode `ssl` (expected starttls, implicit, or none)"
        );
        assert_eq!("ssl".parse::<SmtpTls>().unwrap_err(), err);
    }

    #[test]
    fn resolve_smtp_tls_matrix() {
        // Neither key, or `tls` alone.
        assert_eq!(resolve_smtp_tls(None, None), Ok(SmtpTls::Starttls));
        for mode in [SmtpTls::Starttls, SmtpTls::Implicit, SmtpTls::Plaintext] {
            assert_eq!(resolve_smtp_tls(Some(mode), None), Ok(mode));
        }
        // The legacy alias alone: true → STARTTLS, false → implicit TLS (never plaintext).
        assert_eq!(resolve_smtp_tls(None, Some(true)), Ok(SmtpTls::Starttls));
        assert_eq!(resolve_smtp_tls(None, Some(false)), Ok(SmtpTls::Implicit));
        // Both keys *agreeing* is a half-finished migration, not a mistake: the
        // operator added `tls` and left the old line in place. Two statements
        // that say the same thing resolve; dropping the target here would
        // silently disable notifications.
        assert_eq!(
            resolve_smtp_tls(Some(SmtpTls::Starttls), Some(true)),
            Ok(SmtpTls::Starttls)
        );
        assert_eq!(
            resolve_smtp_tls(Some(SmtpTls::Implicit), Some(false)),
            Ok(SmtpTls::Implicit)
        );
        // Only a contradiction errors — two crosswise, plus both plaintext pairs
        // (the legacy key cannot express "no TLS" at all). The message names
        // both values so the operator knows which line to delete.
        for (tls, starttls) in [
            (SmtpTls::Starttls, false),
            (SmtpTls::Implicit, true),
            (SmtpTls::Plaintext, true),
            (SmtpTls::Plaintext, false),
        ] {
            let err = resolve_smtp_tls(Some(tls), Some(starttls)).unwrap_err();
            assert!(err.contains(&format!("tls = \"{tls}\"")), "{err}");
            assert!(err.contains(&format!("starttls = {starttls}")), "{err}");
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

    // --- notification targets declared from the environment (issue #54) ---

    #[test]
    fn env_url_declares_generic_webhook() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[("FRESHDOCK_NOTIFY_OPS_URL", "https://example.com/hook")]),
        );
        match &cfg.targets["ops"] {
            NotificationTarget::Webhook { url, triggers } => {
                assert_eq!(url.expose(), "https://example.com/hook");
                assert!(triggers.is_none(), "omitted _TRIGGERS → subscribe all");
            }
            other => panic!("expected webhook, got {other:?}"),
        }
    }

    #[test]
    fn env_url_declares_discord_reconstructing_the_webhook_url() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[("FRESHDOCK_NOTIFY_OPS_URL", "discord://tok123@456789")]),
        );
        match &cfg.targets["ops"] {
            NotificationTarget::Discord { webhook_url, .. } => {
                assert_eq!(
                    webhook_url.expose(),
                    "https://discord.com/api/webhooks/456789/tok123"
                );
            }
            other => panic!("expected discord, got {other:?}"),
        }
    }

    #[test]
    fn env_url_declares_telegram_rejoining_the_token_and_taking_the_chat() {
        // A bot token embeds a `:` the URL parser splits into user:password —
        // the target must recover the original `<id>:<secret>` form.
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_TG_URL",
                "telegram://111:AAA-bbb@telegram?chats=42",
            )]),
        );
        match &cfg.targets["tg"] {
            NotificationTarget::Telegram {
                bot_token, chat_id, ..
            } => {
                assert_eq!(bot_token.expose(), "111:AAA-bbb");
                assert_eq!(chat_id, "42");
            }
            other => panic!("expected telegram, got {other:?}"),
        }
    }

    #[test]
    fn env_url_telegram_rejects_an_empty_bot_id() {
        // `telegram://:secret@…` has no bot id — must be rejected, not built
        // into a target with a `:secret` token that only fails at send time.
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_TG_URL",
                "telegram://:secret@telegram?chats=42",
            )]),
        );
        assert!(cfg.targets.is_empty(), "an empty bot id must be rejected");
    }

    #[test]
    fn env_url_telegram_percent_decodes_the_token() {
        // An encoded character in the token (here `%3A` → `:`) must round-trip,
        // matching the SMTP userinfo handling.
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_TG_URL",
                "telegram://111:AA%3ABB@telegram?chats=42",
            )]),
        );
        match &cfg.targets["tg"] {
            NotificationTarget::Telegram { bot_token, .. } => {
                assert_eq!(bot_token.expose(), "111:AA:BB");
            }
            other => panic!("expected telegram, got {other:?}"),
        }
    }

    #[test]
    fn env_url_telegram_accepts_the_chat_id_alias() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_TG_URL",
                "telegram://111:AAA@telegram?chat_id=99",
            )]),
        );
        match &cfg.targets["tg"] {
            NotificationTarget::Telegram { chat_id, .. } => assert_eq!(chat_id, "99"),
            other => panic!("expected telegram, got {other:?}"),
        }
    }

    #[test]
    fn env_url_declares_smtp_decoding_userinfo_and_recipients() {
        // `@`/`:` in the login are percent-encoded so they don't break parsing;
        // they must be decoded back. Recipients are a comma list in `?to=`.
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_MAIL_URL",
                "smtp://user%40corp:pa%3Ass@mail.example.com:2525/?from=ops%40example.com&to=a%40x.com,b%40y.com&starttls=false",
            )]),
        );
        match &cfg.targets["mail"] {
            NotificationTarget::Smtp {
                host,
                port,
                username,
                password,
                from,
                to,
                tls,
                starttls,
                ..
            } => {
                assert_eq!(host, "mail.example.com");
                assert_eq!(*port, Some(2525));
                assert_eq!(username.as_deref(), Some("user@corp"));
                assert_eq!(password.as_ref().unwrap().expose(), "pa:ss");
                assert_eq!(from, "ops@example.com");
                assert_eq!(to, &["a@x.com".to_string(), "b@y.com".to_string()]);
                // The legacy `?starttls=false` is mapped once, here at parse
                // time: the target carries the resolved mode, not the raw pair.
                assert_eq!(
                    *tls,
                    Some(SmtpTls::Implicit),
                    "legacy starttls=false → implicit TLS"
                );
                assert!(starttls.is_none(), "the legacy value is not carried on");
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn env_url_smtp_omits_the_port_and_resolves_the_default_tls() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_MAIL_URL",
                "smtp://mail.example.com/?from=a@e.com&to=b@e.com",
            )]),
        );
        match &cfg.targets["mail"] {
            NotificationTarget::Smtp {
                port,
                tls,
                starttls,
                username,
                password,
                ..
            } => {
                assert!(
                    port.is_none(),
                    "no port in the URL → defaulted from the tls mode later"
                );
                assert_eq!(
                    *tls,
                    Some(SmtpTls::Starttls),
                    "no tls param → resolved to the default once, at parse time"
                );
                assert!(starttls.is_none());
                assert!(username.is_none());
                assert!(password.is_none());
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn env_url_tls_param() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[(
                "FRESHDOCK_NOTIFY_MAIL_URL",
                "smtp://mail.example.com:1025/?from=a@e.com&to=b@e.com&tls=none",
            )]),
        );
        match &cfg.targets["mail"] {
            NotificationTarget::Smtp { tls, starttls, .. } => {
                assert_eq!(*tls, Some(SmtpTls::Plaintext));
                assert!(starttls.is_none());
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn env_url_rejects_a_contradictory_tls_and_starttls_pair() {
        let err = parse_notify_url(
            "mail",
            "smtp://h/?from=a@e.com&to=b@e.com&tls=none&starttls=false",
        )
        .unwrap_err();
        assert!(err.contains("tls = \"none\""), "{err}");
        assert!(err.contains("starttls = false"), "{err}");
    }

    #[test]
    fn env_url_accepts_an_agreeing_tls_and_starttls_pair() {
        // Both params saying the same thing (a not-yet-cleaned-up migration)
        // must resolve, not drop the target.
        let target = parse_notify_url(
            "mail",
            "smtp://h/?from=a@e.com&to=b@e.com&tls=starttls&starttls=true",
        )
        .expect("an agreeing pair resolves");
        match target {
            NotificationTarget::Smtp { tls, starttls, .. } => {
                assert_eq!(tls, Some(SmtpTls::Starttls));
                assert!(starttls.is_none());
            }
            other => panic!("expected smtp, got {other:?}"),
        }
    }

    #[test]
    fn env_url_rejects_an_unknown_tls_mode() {
        let err =
            parse_notify_url("mail", "smtp://h/?from=a@e.com&to=b@e.com&tls=ssl").unwrap_err();
        assert!(err.contains("ssl"), "the bad value is named: {err}");
    }

    #[test]
    fn env_triggers_limit_the_subscription_of_an_env_declared_target() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[
                ("FRESHDOCK_NOTIFY_OPS_URL", "https://example.com/hook"),
                ("FRESHDOCK_NOTIFY_OPS_TRIGGERS", "succeeded, failed"),
            ]),
        );
        match &cfg.targets["ops"] {
            NotificationTarget::Webhook { triggers, .. } => {
                assert_eq!(
                    triggers.as_deref(),
                    Some(&["succeeded".to_string(), "failed".to_string()][..])
                );
            }
            other => panic!("expected webhook, got {other:?}"),
        }
    }

    #[test]
    fn env_url_with_an_unknown_scheme_is_skipped_not_fatal() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[("FRESHDOCK_NOTIFY_OPS_URL", "carrier-pigeon://nest")]),
        );
        assert!(
            cfg.targets.is_empty(),
            "an unparseable scheme warns and is dropped"
        );
    }

    #[test]
    fn env_url_does_not_clobber_a_file_declared_target_of_the_same_name() {
        let file = notifications(
            "[notifications.ops]\ntype = \"webhook\"\nurl = \"https://file.example/hook\"\n",
        );
        let cfg = build_notifications(
            file,
            env(&[("FRESHDOCK_NOTIFY_OPS_URL", "https://env.example/hook")]),
        );
        match &cfg.targets["ops"] {
            NotificationTarget::Webhook { url, .. } => {
                assert_eq!(
                    url.expose(),
                    "https://file.example/hook",
                    "the file declaration wins; the env URL is skipped"
                );
            }
            other => panic!("expected webhook, got {other:?}"),
        }
    }

    #[test]
    fn env_triggers_without_a_url_declare_nothing() {
        let cfg = build_notifications(
            HashMap::new(),
            env(&[("FRESHDOCK_NOTIFY_OPS_TRIGGERS", "succeeded")]),
        );
        assert!(
            cfg.targets.is_empty(),
            "_TRIGGERS alone has no target to attach to"
        );
    }

    #[test]
    fn env_declared_target_with_a_bad_trigger_is_dropped_by_the_dispatcher() {
        // The token is validated later, in the dispatcher: a bad one warns and
        // skips the whole target (same resilience as a file-declared one).
        let cfg = build_notifications(
            HashMap::new(),
            env(&[
                ("FRESHDOCK_NOTIFY_OPS_URL", "https://example.com/hook"),
                ("FRESHDOCK_NOTIFY_OPS_TRIGGERS", "bogus"),
            ]),
        );
        let dispatcher = crate::notify::Dispatcher::from_config(cfg, crate::http::client());
        assert!(dispatcher.is_empty());
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
        let resolved = resolve_settings(cfg.settings, env(&[]));
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
        let resolved = resolve_settings(cfg.settings, env(&[]));
        assert_eq!(resolved.default_mode, None);
        assert!(!resolved.cleanup);
        assert!(!resolved.prune_dangling);
    }

    #[test]
    fn invalid_default_mode_is_ignored_not_an_error() {
        // A bad mode must not abort load — it warns and falls back to None
        // (which downstream resolves to `watch`).
        let cfg = Config::from_toml("[settings]\ndefault_mode = \"hourly\"\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[]));
        assert_eq!(resolved.default_mode, None);
    }

    #[test]
    fn env_default_mode_overrides_file_mode() {
        let cfg = Config::from_toml("[settings]\ndefault_mode = \"nightly\"\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_DEFAULT_MODE", "weekly")]));
        assert_eq!(resolved.default_mode, Some(Mode::Weekly));
    }

    #[test]
    fn env_invalid_default_mode_keeps_file_value() {
        // A bad env mode must not clobber a valid file mode — warn and keep
        // the file layer.
        let cfg = Config::from_toml("[settings]\ndefault_mode = \"nightly\"\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_DEFAULT_MODE", "hourly")]));
        assert_eq!(resolved.default_mode, Some(Mode::Nightly));
    }

    #[test]
    fn env_bools_accept_true_false_1_0_case_insensitive() {
        for (raw, expected) in [
            ("1", true),
            ("0", false),
            ("TRUE", true),
            (" false ", false),
        ] {
            let resolved = resolve_settings(
                Settings::default(),
                env(&[
                    ("FRESHDOCK_CLEANUP", raw),
                    ("FRESHDOCK_PRUNE_DANGLING", raw),
                ]),
            );
            assert_eq!(resolved.cleanup, expected, "cleanup: {raw:?}");
            assert_eq!(resolved.prune_dangling, expected, "prune_dangling: {raw:?}");
        }
    }

    #[test]
    fn env_invalid_bool_keeps_file_value() {
        let cfg = Config::from_toml("[settings]\ncleanup = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_CLEANUP", "maybe")]));
        assert!(resolved.cleanup);
    }

    #[test]
    fn watch_all_parses_from_settings_table() {
        let cfg = Config::from_toml("[settings]\nwatch_all = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[]));
        assert!(resolved.watch_all);
        // Absent key keeps the opt-in default.
        let cfg = Config::from_toml("[settings]\ncleanup = true\n").unwrap();
        assert!(!resolve_settings(cfg.settings, env(&[])).watch_all);
    }

    #[test]
    fn env_watch_all_overrides_file() {
        let cfg = Config::from_toml("[settings]\nwatch_all = false\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_WATCH_ALL", "1")]));
        assert!(resolved.watch_all);

        let cfg = Config::from_toml("[settings]\nwatch_all = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_WATCH_ALL", "false")]));
        assert!(!resolved.watch_all, "env can also turn it back off");
    }

    #[test]
    fn invalid_env_watch_all_is_ignored_keeping_file_value() {
        let cfg = Config::from_toml("[settings]\nwatch_all = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_WATCH_ALL", "maybe")]));
        assert!(resolved.watch_all);
    }

    #[test]
    fn compose_aware_is_on_unless_it_is_turned_off() {
        // The unsafe case is an isolated update inside a compose stack, so an
        // absent setting has to mean "on".
        assert!(resolve_settings(Config::default().settings, env(&[])).compose_aware);
        assert!(ResolvedSettings::default().compose_aware);

        let cfg = Config::from_toml("[settings]\ncompose_aware = false\n").unwrap();
        assert!(!resolve_settings(cfg.settings, env(&[])).compose_aware);

        let cfg = Config::from_toml("[settings]\ncompose_aware = true\n").unwrap();
        assert!(resolve_settings(cfg.settings, env(&[])).compose_aware);
    }

    #[test]
    fn env_compose_aware_overrides_file() {
        let cfg = Config::from_toml("[settings]\ncompose_aware = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_COMPOSE_AWARE", "0")]));
        assert!(!resolved.compose_aware);

        let cfg = Config::from_toml("[settings]\ncompose_aware = false\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_COMPOSE_AWARE", "true")]));
        assert!(resolved.compose_aware, "env can also turn it back on");
    }

    #[test]
    fn invalid_env_compose_aware_is_ignored_keeping_the_file_value() {
        let cfg = Config::from_toml("[settings]\ncompose_aware = false\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[("FRESHDOCK_COMPOSE_AWARE", "yes")]));
        assert!(!resolved.compose_aware);
    }

    #[test]
    fn resolved_settings_forward_watch_all_to_policy_defaults() {
        let cfg = Config::from_toml("[settings]\nwatch_all = true\n").unwrap();
        let resolved = resolve_settings(cfg.settings, env(&[]));
        assert!(resolved.policy_defaults().watch_all);
        assert!(!ResolvedSettings::default().policy_defaults().watch_all);
    }

    #[test]
    fn env_settings_apply_with_no_file_table() {
        // Unlike notification targets, env alone can establish settings — there
        // is no per-name declaration to anchor to.
        let resolved = resolve_settings(
            Settings::default(),
            env(&[
                ("FRESHDOCK_DEFAULT_MODE", "live"),
                ("FRESHDOCK_CLEANUP", "true"),
                ("FRESHDOCK_PRUNE_DANGLING", "1"),
            ]),
        );
        assert_eq!(resolved.default_mode, Some(Mode::Live));
        assert!(resolved.cleanup);
        assert!(resolved.prune_dangling);
    }
}
