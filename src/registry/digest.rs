use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::auth::{CachedToken, parse_www_authenticate};
use super::{Digest, ImageRef, Registry, RegistryError};
use crate::config::{CredentialStore, canonicalize_host};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(2);

const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.index.v1+json";

/// Realm response. The token field is `token` on Docker Hub but `access_token`
/// on some OCI registries; we accept either. `expires_in` drives the cache TTL.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_token(self) -> Option<String> {
        self.token.or(self.access_token)
    }
}

/// A registry API base URL plus its preflight authority. Used to point the
/// registry at a mock server in tests; production derives the base per image
/// host via [`base_for_host`].
#[derive(Debug, Clone)]
pub struct Endpoints {
    registry_base: String,
    registry_authority: String,
}

impl Endpoints {
    pub fn new(registry_base: impl Into<String>) -> Result<Self, RegistryError> {
        let registry_base = normalize(registry_base.into())?;
        let registry_authority = authority_of(&registry_base)?;
        Ok(Self {
            registry_base,
            registry_authority,
        })
    }
}

fn normalize(s: String) -> Result<String, RegistryError> {
    let trimmed = s.trim_end_matches('/').to_string();
    Url::parse(&trimmed).map_err(|e| RegistryError::InvalidEndpoint(format!("{trimmed}: {e}")))?;
    Ok(trimmed)
}

fn authority_of(base: &str) -> Result<String, RegistryError> {
    let url =
        Url::parse(base).map_err(|e| RegistryError::InvalidEndpoint(format!("{base}: {e}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| RegistryError::InvalidEndpoint(format!("no host in {base}")))?;
    let port = url.port_or_known_default().unwrap_or(443);
    Ok(format!("{host}:{port}"))
}

/// Split a repository into `(registry host, path-within-registry)`. A leading
/// segment that looks like a host (contains `.` or `:`, or is `localhost`) is
/// the registry; otherwise it's Docker Hub and the whole repository is the path
/// (already `library/<name>` for single-component refs). Shared with the daemon
/// pull so credentials resolve against the same host (DRY).
pub(crate) fn split_repository(repository: &str) -> (&str, &str) {
    let first = repository.split('/').next().unwrap_or("");
    let is_host =
        first.eq_ignore_ascii_case("localhost") || first.contains('.') || first.contains(':');
    if is_host {
        let path = repository.split_once('/').map_or("", |(_, p)| p);
        (first, path)
    } else {
        ("docker.io", repository)
    }
}

/// The registry API base URL for a host. Docker Hub's API host
/// (`registry-1.docker.io`) differs from the `docker.io` name used in refs;
/// every other host is addressed directly over https. Insecure/plain-HTTP
/// registries are out of scope (PLAN §5.5).
fn base_for_host(host: &str) -> String {
    if canonicalize_host(host) == "docker.io" {
        "https://registry-1.docker.io".to_string()
    } else {
        format!("https://{host}")
    }
}

/// Map a transport-level send error to a clean verdict: connection/timeout
/// failures degrade to `NetworkUnavailable` (retry later); anything else is a
/// genuine HTTP error.
fn classify_send_error(e: reqwest::Error) -> RegistryError {
    if e.is_connect() || e.is_timeout() {
        RegistryError::NetworkUnavailable(e.to_string())
    } else {
        RegistryError::Http(e)
    }
}

/// An OCI registry digest client. One bearer-token flow (challenge → realm
/// exchange → retry) serves Docker Hub, GHCR, Quay, lscr.io, and any other
/// `WWW-Authenticate: Bearer` registry; credentials (when configured for the
/// host) are sent to the realm so private repositories resolve too. Tokens are
/// cached per `(host, scope)` for their stated lifetime.
pub struct OciRegistry {
    client: Client,
    store: Arc<CredentialStore>,
    token_cache: Mutex<HashMap<String, CachedToken>>,
    /// Forces the registry base at a fixed URL (mock server) for tests; `None`
    /// in production, where the base is derived from each image's host.
    registry_override: Option<Endpoints>,
}

impl OciRegistry {
    pub fn new(store: Arc<CredentialStore>) -> Self {
        Self::build(store, None)
    }

    /// Test seam: route every request at `base_url` (a mock server). The auth
    /// realm is still discovered from the challenge the mock returns.
    pub fn with_base_url(store: Arc<CredentialStore>, base_url: &str) -> Self {
        let endpoints = Endpoints::new(base_url).expect("test base url must be valid");
        Self::build(store, Some(endpoints))
    }

    fn build(store: Arc<CredentialStore>, registry_override: Option<Endpoints>) -> Self {
        Self {
            client: crate::http::client(),
            store,
            token_cache: Mutex::new(HashMap::new()),
            registry_override,
        }
    }

    /// Resolve `(host, path, base url, preflight authority)` for a repository.
    fn resolve(&self, repository: &str) -> (String, String, String, String) {
        let (host, path) = split_repository(repository);
        match &self.registry_override {
            Some(ep) => (
                host.to_string(),
                path.to_string(),
                ep.registry_base.clone(),
                ep.registry_authority.clone(),
            ),
            None => {
                let base = base_for_host(host);
                let authority =
                    authority_of(&base).expect("derived registry base is a valid https url");
                (host.to_string(), path.to_string(), base, authority)
            }
        }
    }

    fn cached_token(&self, key: &str) -> Option<String> {
        let now = Instant::now();
        let cache = self.token_cache.lock().expect("token cache mutex poisoned");
        cache
            .get(key)
            .and_then(|t| t.valid_token(now))
            .map(str::to_string)
    }

    fn store_token(&self, key: String, token: String, expires_in: Option<u64>) {
        let entry = CachedToken::new(token, expires_in, Instant::now());
        self.token_cache
            .lock()
            .expect("token cache mutex poisoned")
            .insert(key, entry);
    }

    async fn head_manifest(
        &self,
        base: &str,
        path: &str,
        tag: &str,
        token: Option<&str>,
    ) -> Result<reqwest::Response, RegistryError> {
        let url = format!("{base}/v2/{path}/manifests/{tag}");
        let mut req = self.client.head(&url).header(ACCEPT, ACCEPT_MANIFESTS);
        if let Some(token) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        req.send().await.map_err(classify_send_error)
    }

    /// Exchange a challenge for a bearer token at its realm, sending credentials
    /// for `host` when the store has them. Returns the token + its lifetime.
    ///
    /// If configured credentials are *rejected* (401/403), retry the exchange
    /// anonymously so public images keep resolving when a token is stale, and
    /// `warn!` so the downgrade is never silent. Only when the anonymous retry is
    /// also denied (a genuinely private image) does this surface
    /// [`RegistryError::CredentialsRejected`].
    async fn request_token(
        &self,
        realm: &str,
        service: Option<&str>,
        scope: &str,
        host: &str,
    ) -> Result<(String, Option<u64>), RegistryError> {
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(service) = service {
            query.push(("service", service));
        }
        query.push(("scope", scope));

        let creds = self.store.get(host);
        let mut req = self.client.get(realm).query(&query);
        if let Some(creds) = creds {
            req = req.basic_auth(
                creds.username.clone().unwrap_or_default(),
                Some(creds.token.expose()),
            );
        }

        let resp = req.send().await.map_err(classify_send_error)?;
        // Only an explicit rejection is an auth failure; 429/5xx are transient
        // HTTP errors, not "wrong credentials", so don't mislabel them.
        if is_denied(&resp) {
            // With no credentials sent, a denial is the plain "needs auth" case.
            if creds.is_none() {
                return Err(RegistryError::Auth(format!(
                    "token endpoint denied access (status {})",
                    resp.status()
                )));
            }
            // Credentials *were* sent and rejected. Retry anonymously so a public
            // image still resolves; warn so a stale token doesn't silently
            // downgrade us to the (much smaller) anonymous rate budget.
            warn!(
                host = %host,
                status = %resp.status(),
                "configured registry credentials were rejected; retrying anonymously"
            );
            let anon = self
                .client
                .get(realm)
                .query(&query)
                .send()
                .await
                .map_err(classify_send_error)?;
            if is_denied(&anon) {
                // Anonymous also denied → genuinely private *and* bad creds.
                return Err(RegistryError::CredentialsRejected(host.to_string()));
            }
            return finish_token(anon).await;
        }
        finish_token(resp).await
    }
}

/// Did the registry explicitly reject the request (401/403)? 429/5xx are
/// transient and handled elsewhere, so they are deliberately excluded.
fn is_denied(resp: &reqwest::Response) -> bool {
    matches!(
        resp.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    )
}

/// Parse a successful token-endpoint response into `(token, lifetime)`. Shared by
/// the authenticated and anonymous-fallback paths of [`OciRegistry::request_token`].
async fn finish_token(resp: reqwest::Response) -> Result<(String, Option<u64>), RegistryError> {
    let resp = resp.error_for_status()?;
    let body: TokenResponse = resp.json().await?;
    let expires_in = body.expires_in;
    let token = body
        .into_token()
        .ok_or_else(|| RegistryError::Auth("token response had no token field".into()))?;
    Ok((token, expires_in))
}

async fn probe(authority: &str) -> Result<(), RegistryError> {
    let connect = TcpStream::connect(authority);
    match timeout(PREFLIGHT_TIMEOUT, connect).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(RegistryError::NetworkUnavailable(format!(
            "{authority}: {e}"
        ))),
        Err(_) => Err(RegistryError::NetworkUnavailable(format!(
            "{authority}: connect timeout"
        ))),
    }
}

fn log_rate_limit(host: &str, resp: &reqwest::Response) {
    if let Some(limit) = resp.headers().get("ratelimit-limit") {
        info!(
            host = %host,
            limit = ?limit,
            remaining = ?resp.headers().get("ratelimit-remaining"),
            "registry rate limit"
        );
    } else {
        debug!(host = %host, "no ratelimit headers on response");
    }
}

#[async_trait::async_trait]
impl Registry for OciRegistry {
    async fn fetch_digest(&self, image: &ImageRef) -> Result<Digest, RegistryError> {
        let (host, path, base, authority) = self.resolve(&image.repository);
        // Fail fast (and cleanly) when the registry can't be reached at all.
        probe(&authority).await?;

        let scope = format!("repository:{path}:pull");
        let cache_key = format!("{host}|{scope}");

        // Try a cached token first; otherwise the first HEAD is unauthenticated
        // and we follow the 401 challenge.
        let mut token = self.cached_token(&cache_key);
        let mut resp = self
            .head_manifest(&base, &path, &image.tag, token.as_deref())
            .await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            let challenge = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_www_authenticate)
                .ok_or_else(|| {
                    RegistryError::Auth(
                        "registry returned 401 without a Bearer challenge".to_string(),
                    )
                })?;
            // Honour the challenge's scope verbatim; synthesise one only if the
            // registry didn't pin it.
            let scope = challenge.scope.clone().unwrap_or(scope);
            let (new_token, expires_in) = self
                .request_token(
                    &challenge.realm,
                    challenge.service.as_deref(),
                    &scope,
                    &host,
                )
                .await?;
            self.store_token(cache_key, new_token.clone(), expires_in);
            token = Some(new_token);
            resp = self
                .head_manifest(&base, &path, &image.tag, token.as_deref())
                .await?;
        }

        // A persistent 401/403 means the credentials (or lack thereof) don't
        // grant access — a clear, typed signal rather than a generic HTTP error.
        if matches!(
            resp.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(RegistryError::Auth(format!(
                "registry denied access to {host}/{path} (status {})",
                resp.status()
            )));
        }
        let resp = resp.error_for_status()?;

        log_rate_limit(&host, &resp);

        let digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .ok_or(RegistryError::MissingDigest)?
            .to_string();
        Ok(Digest(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_strip_trailing_slash() {
        let e = Endpoints::new("https://reg.example.com/").unwrap();
        assert_eq!(e.registry_base, "https://reg.example.com");
    }

    #[test]
    fn endpoints_cache_authority_with_default_port() {
        let e = Endpoints::new("https://reg.example.com").unwrap();
        assert_eq!(e.registry_authority, "reg.example.com:443");
    }

    #[test]
    fn endpoints_cache_authority_with_explicit_port() {
        let e = Endpoints::new("http://localhost:5001").unwrap();
        assert_eq!(e.registry_authority, "localhost:5001");
    }

    #[test]
    fn endpoints_reject_garbage_url() {
        let err = Endpoints::new("not a url").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidEndpoint(_)));
    }

    #[test]
    fn splits_docker_hub_repositories() {
        assert_eq!(
            split_repository("library/alpine"),
            ("docker.io", "library/alpine")
        );
        assert_eq!(
            split_repository("nginxinc/nginx-unprivileged"),
            ("docker.io", "nginxinc/nginx-unprivileged")
        );
    }

    #[test]
    fn splits_host_qualified_repositories() {
        assert_eq!(
            split_repository("ghcr.io/owner/repo"),
            ("ghcr.io", "owner/repo")
        );
        assert_eq!(split_repository("quay.io/foo/bar"), ("quay.io", "foo/bar"));
        assert_eq!(
            split_repository("localhost:5000/repo"),
            ("localhost:5000", "repo")
        );
    }

    #[test]
    fn base_url_maps_docker_hub_to_its_api_host() {
        assert_eq!(base_for_host("docker.io"), "https://registry-1.docker.io");
        assert_eq!(base_for_host("ghcr.io"), "https://ghcr.io");
        assert_eq!(base_for_host("quay.io"), "https://quay.io");
    }
}
