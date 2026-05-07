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
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub auth_base: String,     // e.g. "https://auth.docker.io"
    pub registry_base: String, // e.g. "https://registry-1.docker.io"
}

impl Endpoints {
    pub fn docker_hub() -> Self {
        Self {
            auth_base: "https://auth.docker.io".into(),
            registry_base: "https://registry-1.docker.io".into(),
        }
    }
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
        let auth = probe(&self.endpoints.auth_base);
        let registry = probe(&self.endpoints.registry_base);
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

async fn probe(base_url: &str) -> Result<(), RegistryError> {
    let url = Url::parse(base_url).map_err(|e| {
        RegistryError::NetworkUnavailable(format!("invalid endpoint url {base_url}: {e}"))
    })?;
    let host = url
        .host_str()
        .ok_or_else(|| RegistryError::NetworkUnavailable(format!("no host in {base_url}")))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addr = format!("{host}:{port}");
    let connect = TcpStream::connect(&addr);
    match timeout(PREFLIGHT_TIMEOUT, connect).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(RegistryError::NetworkUnavailable(format!("{addr}: {e}"))),
        Err(_) => Err(RegistryError::NetworkUnavailable(format!(
            "{addr}: connect timeout"
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
