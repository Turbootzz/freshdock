use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use super::{Digest, ImageRef, Registry, RegistryError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(2);

const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.index.v1+json";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

/// Where the auth + registry endpoints live. Defaults to Docker Hub;
/// Phase 5 (P5-1) will add bearer-token registries by constructing
/// alternative `Endpoints` values without touching the protocol logic.
///
/// Construction goes through [`Endpoints::new`] (or [`Endpoints::docker_hub`])
/// so trailing slashes get normalised once and the host:port we preflight
/// against is parsed once instead of on every request.
#[derive(Debug, Clone)]
pub struct Endpoints {
    auth_base: String,
    registry_base: String,
    auth_authority: String,
    registry_authority: String,
}

impl Endpoints {
    pub fn new(
        auth_base: impl Into<String>,
        registry_base: impl Into<String>,
    ) -> Result<Self, RegistryError> {
        let auth_base = normalize(auth_base.into())?;
        let registry_base = normalize(registry_base.into())?;
        let auth_authority = authority_of(&auth_base)?;
        let registry_authority = authority_of(&registry_base)?;
        Ok(Self {
            auth_base,
            registry_base,
            auth_authority,
            registry_authority,
        })
    }

    pub fn docker_hub() -> Self {
        Self::new("https://auth.docker.io", "https://registry-1.docker.io")
            .expect("docker hub endpoint URLs are static and valid")
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

pub struct DockerHub {
    client: Client,
    endpoints: Endpoints,
}

impl Default for DockerHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerHub {
    pub fn new() -> Self {
        Self::with_endpoints(Endpoints::docker_hub())
    }

    pub fn with_endpoints(endpoints: Endpoints) -> Self {
        let client = Client::builder()
            .user_agent(concat!("freshdock/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client construction with default config cannot fail");
        Self { client, endpoints }
    }

    async fn preflight(&self) -> Result<(), RegistryError> {
        let auth = probe(&self.endpoints.auth_authority);
        let registry = probe(&self.endpoints.registry_authority);
        let (a, r) = tokio::join!(auth, registry);
        a.and(r)
    }

    async fn fetch_token(&self, repo: &str) -> Result<String, RegistryError> {
        let url = format!("{}/token", self.endpoints.auth_base);
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("service", "registry.docker.io"),
                ("scope", &format!("repository:{repo}:pull")),
            ])
            .send()
            .await?
            .error_for_status()
            .map_err(|e| RegistryError::Auth(e.to_string()))?;
        let body: TokenResponse = resp.json().await?;
        Ok(body.token)
    }
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

#[async_trait::async_trait]
impl Registry for DockerHub {
    async fn fetch_digest(&self, image: &ImageRef) -> Result<Digest, RegistryError> {
        self.preflight().await?;
        let token = self.fetch_token(&image.repository).await?;

        let url = format!(
            "{}/v2/{repo}/manifests/{tag}",
            self.endpoints.registry_base,
            repo = image.repository,
            tag = image.tag,
        );
        let resp = self
            .client
            .head(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(ACCEPT, ACCEPT_MANIFESTS)
            .send()
            .await?
            .error_for_status()?;

        if let Some(limit) = resp.headers().get("ratelimit-limit") {
            info!(
                limit = ?limit,
                remaining = ?resp.headers().get("ratelimit-remaining"),
                repo = %image.repository,
                "docker hub rate limit"
            );
        } else {
            debug!(repo = %image.repository, "no ratelimit headers on response");
        }

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
        let e = Endpoints::new("https://auth.example.com/", "https://reg.example.com/").unwrap();
        assert_eq!(e.auth_base, "https://auth.example.com");
        assert_eq!(e.registry_base, "https://reg.example.com");
    }

    #[test]
    fn endpoints_cache_authority_with_default_port() {
        let e = Endpoints::new("https://auth.example.com", "https://reg.example.com").unwrap();
        assert_eq!(e.auth_authority, "auth.example.com:443");
        assert_eq!(e.registry_authority, "reg.example.com:443");
    }

    #[test]
    fn endpoints_cache_authority_with_explicit_port() {
        let e = Endpoints::new("http://localhost:5000", "http://localhost:5001").unwrap();
        assert_eq!(e.auth_authority, "localhost:5000");
        assert_eq!(e.registry_authority, "localhost:5001");
    }

    #[test]
    fn endpoints_reject_garbage_url() {
        let err = Endpoints::new("not a url", "https://reg.example.com").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidEndpoint(_)));
    }

    #[test]
    fn docker_hub_endpoints_resolve() {
        let e = Endpoints::docker_hub();
        assert_eq!(e.auth_authority, "auth.docker.io:443");
        assert_eq!(e.registry_authority, "registry-1.docker.io:443");
    }
}
