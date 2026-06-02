//! Bearer-token auth primitives shared by the OCI digest flow (Phase 5, P5-1).
//!
//! The protocol is the Docker registry v2 token dance, identical across Docker
//! Hub, GHCR, Quay, and lscr.io: an unauthenticated request gets a `401` with a
//! `WWW-Authenticate: Bearer realm=...,service=...,scope=...` challenge; the
//! client exchanges that (optionally with credentials) for a bearer token at
//! the realm, then retries. One flow, parameterised by the challenge — not one
//! copy per registry.

use std::fmt;
use std::time::{Duration, Instant};

/// Token lifetime when the realm response omits `expires_in`.
const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// Refresh a little before the server-stated expiry to avoid racing the edge.
const EXPIRY_SKEW: Duration = Duration::from_secs(5);

/// A parsed `WWW-Authenticate: Bearer ...` challenge. `service`/`scope` are
/// optional because some registries omit them (the client then synthesises the
/// scope from the repository it wants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// Parse a `WWW-Authenticate` header value. Returns `None` for a missing realm
/// or a non-`Bearer` scheme (e.g. `Basic`) — the caller turns that into a typed
/// auth error rather than panicking.
pub fn parse_www_authenticate(header: &str) -> Option<Challenge> {
    let trimmed = header.trim();
    // Case-insensitive `Bearer` scheme, followed by whitespace (or nothing).
    let rest = trimmed
        .get(..6)
        .filter(|s| s.eq_ignore_ascii_case("Bearer"))?;
    let rest = &trimmed[rest.len()..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None; // e.g. "Bearerish" is not the Bearer scheme.
    }

    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for part in rest.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => realm = Some(value),
            "service" => service = Some(value),
            "scope" => scope = Some(value),
            _ => {}
        }
    }

    Some(Challenge {
        realm: realm?,
        service,
        scope,
    })
}

/// A bearer token with its computed expiry, cached per `(host, scope)`. `Debug`
/// redacts the token so it can never reach a log line, even if a `CachedToken`
/// is ever traced.
#[derive(Clone)]
pub struct CachedToken {
    token: String,
    expires_at: Instant,
}

impl fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedToken")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CachedToken {
    /// Build a cache entry from the realm response. `expires_in` is the
    /// server-stated lifetime in seconds (falls back to [`DEFAULT_TTL`]); the
    /// skew and a 1-second floor are applied with saturating math so a tiny or
    /// zero `expires_in` can't underflow `Instant` or be instantly stale.
    pub fn new(token: String, expires_in_secs: Option<u64>, now: Instant) -> Self {
        let ttl = expires_in_secs.map_or(DEFAULT_TTL, Duration::from_secs);
        let effective = ttl.saturating_sub(EXPIRY_SKEW).max(Duration::from_secs(1));
        Self {
            token,
            expires_at: now + effective,
        }
    }

    /// The token, if it hasn't expired as of `now`.
    pub fn valid_token(&self, now: Instant) -> Option<&str> {
        (now < self.expires_at).then_some(self.token.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_challenge() {
        let c = parse_www_authenticate(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        )
        .unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(c.scope.as_deref(), Some("repository:library/alpine:pull"));
    }

    #[test]
    fn parses_challenge_without_scope() {
        let c = parse_www_authenticate(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#)
            .unwrap();
        assert_eq!(c.realm, "https://ghcr.io/token");
        assert_eq!(c.service.as_deref(), Some("ghcr.io"));
        assert!(c.scope.is_none());
    }

    #[test]
    fn scheme_is_case_insensitive_and_tolerates_spacing() {
        let c =
            parse_www_authenticate(r#"bearer  realm = "https://r/token" , service="s""#).unwrap();
        assert_eq!(c.realm, "https://r/token");
        assert_eq!(c.service.as_deref(), Some("s"));
    }

    #[test]
    fn rejects_non_bearer_and_missing_realm() {
        assert!(parse_www_authenticate(r#"Basic realm="x""#).is_none());
        assert!(parse_www_authenticate(r#"Bearer service="s""#).is_none());
        assert!(parse_www_authenticate("Bearerish realm=\"x\"").is_none());
        assert!(parse_www_authenticate("").is_none());
    }

    #[test]
    fn token_validity_respects_expiry() {
        let now = Instant::now();
        let t = CachedToken::new("tok".into(), Some(300), now);
        assert_eq!(t.valid_token(now), Some("tok"));
        // Past its (skewed) expiry it reports no token.
        assert_eq!(t.valid_token(now + Duration::from_secs(600)), None);
    }

    #[test]
    fn zero_expires_in_does_not_panic_and_floors_to_one_second() {
        let now = Instant::now();
        let t = CachedToken::new("tok".into(), Some(0), now);
        // Floor keeps it briefly valid rather than underflowing.
        assert_eq!(t.valid_token(now), Some("tok"));
    }

    #[test]
    fn cached_token_debug_redacts_the_token() {
        let t = CachedToken::new("supersecret".into(), Some(60), Instant::now());
        assert!(
            !format!("{t:?}").contains("supersecret"),
            "token leaked via CachedToken Debug"
        );
    }
}
