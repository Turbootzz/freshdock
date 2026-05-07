//! Integration tests for the anonymous Docker Hub digest path.
//!
//! These hit the public registry. They gracefully skip (not fail) when
//! the runner has no network — that is one of the explicit acceptance
//! criteria for issue #6 ("CI hygiene").

use freshdock::registry::digest::DockerHub;
use freshdock::registry::{ImageRef, Registry, RegistryError};

fn is_sha256(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("sha256:") {
        hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

#[tokio::test]
async fn library_alpine_latest_returns_valid_digest() {
    let hub = DockerHub::new();
    match hub.fetch_digest(&ImageRef::parse("alpine:latest")).await {
        Ok(d) => assert!(is_sha256(&d.0), "unexpected digest shape: {}", d.0),
        Err(RegistryError::NetworkUnavailable(reason)) => {
            eprintln!("skipped: {reason}");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[tokio::test]
async fn nginx_unprivileged_returns_valid_digest() {
    let hub = DockerHub::new();
    match hub
        .fetch_digest(&ImageRef::parse("nginxinc/nginx-unprivileged:latest"))
        .await
    {
        Ok(d) => assert!(is_sha256(&d.0), "unexpected digest shape: {}", d.0),
        Err(RegistryError::NetworkUnavailable(reason)) => {
            eprintln!("skipped: {reason}");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}
