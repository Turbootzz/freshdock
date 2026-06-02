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

use serde::Deserialize;
use tracing::warn;

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

/// The parsed `freshdock.toml`. Only the registry credential section exists in
/// Phase 5; global defaults (poll intervals, notifications) land in later phases.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub registry: HashMap<String, RegistryCredentials>,
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

    /// Load credentials: read `path` (or the default `./freshdock.toml` when
    /// `path` is `None`), then overlay `FRESHDOCK_REGISTRY_*` env vars on top.
    ///
    /// An *explicit* path that doesn't exist is an error; a missing *default*
    /// file is not (it just yields env-only / empty credentials).
    pub fn load(path: Option<&Path>) -> Result<CredentialStore, ConfigError> {
        let config = match path {
            Some(p) => Self::read_file(p)?,
            None => {
                let default = Path::new(DEFAULT_CONFIG_FILE);
                if default.exists() {
                    Self::read_file(default)?
                } else {
                    Self::default()
                }
            }
        };
        Ok(build_store(config, std::env::vars()))
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
lscr, or a registry host. FRESHDOCK_CONFIG sets the config file path.";

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
}
