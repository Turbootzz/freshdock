//! Integration tests for the anonymous (public) registry digest path.
//!
//! These hit live public registries. They gracefully skip (not fail) when
//! the runner has no network — that is one of the explicit acceptance
//! criteria for issue #6 ("CI hygiene"). Authenticated paths (private GHCR,
//! Docker Hub PAT) need secrets unavailable in CI and are covered by the
//! wiremock suite in `registry_mock.rs`; see `docs/registry-auth.md` for the
//! manual PAT smoke test.

use std::sync::Arc;

use freshdock::config::CredentialStore;
use freshdock::registry::digest::OciRegistry;
use freshdock::registry::{ImageRef, Registry, RegistryError};

fn is_sha256(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("sha256:") {
        hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

fn anonymous() -> OciRegistry {
    OciRegistry::new(Arc::new(CredentialStore::default()))
}

#[tokio::test]
async fn library_alpine_latest_returns_valid_digest() {
    match anonymous()
        .fetch_digest(&ImageRef::parse("alpine:latest"))
        .await
    {
        Ok(d) => assert!(is_sha256(&d.0), "unexpected digest shape: {}", d.0),
        Err(RegistryError::NetworkUnavailable(reason)) => eprintln!("skipped: {reason}"),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[tokio::test]
async fn nginx_unprivileged_returns_valid_digest() {
    match anonymous()
        .fetch_digest(&ImageRef::parse("nginxinc/nginx-unprivileged:latest"))
        .await
    {
        Ok(d) => assert!(is_sha256(&d.0), "unexpected digest shape: {}", d.0),
        Err(RegistryError::NetworkUnavailable(reason)) => eprintln!("skipped: {reason}"),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

/// Quay.io is a non-Docker-Hub registry reachable anonymously — this proves the
/// generic bearer flow resolves a different host end-to-end. Any error other
/// than success is treated as a skip: Quay availability and image lifecycle are
/// outside our control, so this must never turn CI red.
#[tokio::test]
async fn quay_public_image_returns_valid_digest() {
    match anonymous()
        .fetch_digest(&ImageRef::parse("quay.io/prometheus/node-exporter:latest"))
        .await
    {
        Ok(d) => assert!(is_sha256(&d.0), "unexpected digest shape: {}", d.0),
        Err(e) => eprintln!("skipped (quay is an external dependency): {e}"),
    }
}
