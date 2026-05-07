use std::time::Duration;

use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use super::{Digest, ImageRef, Registry, RegistryError};

const AUTH_HOST: &str = "auth.docker.io";
const AUTH_URL: &str = "https://auth.docker.io/token";
const REGISTRY_URL: &str = "https://registry-1.docker.io/v2";

const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.index.v1+json";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

pub struct DockerHub {
    client: Client,
}

impl Default for DockerHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerHub {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(concat!("freshdock/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client construction with default config cannot fail");
        Self { client }
    }

    async fn preflight() -> Result<(), RegistryError> {
        let connect = TcpStream::connect(format!("{AUTH_HOST}:443"));
        match timeout(Duration::from_secs(2), connect).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(RegistryError::NetworkUnavailable(e.to_string())),
            Err(_) => Err(RegistryError::NetworkUnavailable(
                "connect timeout".to_string(),
            )),
        }
    }

    async fn fetch_token(&self, repo: &str) -> Result<String, RegistryError> {
        let resp = self
            .client
            .get(AUTH_URL)
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

#[async_trait::async_trait]
impl Registry for DockerHub {
    async fn fetch_digest(&self, image: &ImageRef) -> Result<Digest, RegistryError> {
        Self::preflight().await?;
        let token = self.fetch_token(&image.repository).await?;

        let url = format!(
            "{REGISTRY_URL}/{repo}/manifests/{tag}",
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
