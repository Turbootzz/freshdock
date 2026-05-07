//! HTTP-mocked tests for the DockerHub registry implementation.
//!
//! These exercise the protocol layer (token negotiation → manifest HEAD →
//! digest extraction) against a wiremock server, so error paths the
//! happy-path live tests can't reach (401 on token, 404 on manifest,
//! response missing `Docker-Content-Digest`, ...) are locked in.

use freshdock::registry::digest::{DockerHub, Endpoints};
use freshdock::registry::{ImageRef, Registry, RegistryError};
use serde_json::json;
use wiremock::matchers::{header, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SAMPLE_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

async fn hub_pointed_at(server: &MockServer) -> DockerHub {
    DockerHub::with_endpoints(Endpoints {
        auth_base: server.uri(),
        registry_base: server.uri(),
    })
}

#[tokio::test]
async fn happy_path_returns_manifest_digest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    let digest = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("happy path should succeed");
    assert_eq!(digest.0, SAMPLE_DIGEST);
}

#[tokio::test]
async fn token_endpoint_unauthorized_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    let err = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("401 should propagate as Auth error");
    assert!(matches!(err, RegistryError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn manifest_404_propagates_as_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    let err = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("404 should propagate");
    assert!(matches!(err, RegistryError::Http(_)), "got {err:?}");
}

#[tokio::test]
async fn missing_digest_header_returns_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
        .mount(&server)
        .await;
    // 200 OK with NO Docker-Content-Digest header.
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    let err = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("missing header should be reported");
    assert!(matches!(err, RegistryError::MissingDigest), "got {err:?}");
}

#[tokio::test]
async fn bearer_token_is_forwarded_to_manifest_head() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "secret-abc"})))
        .mount(&server)
        .await;
    // Only matches when Authorization carries the exact bearer token.
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer secret-abc"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    let digest = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("auth header forwarding should work");
    assert_eq!(digest.0, SAMPLE_DIGEST);
}

#[tokio::test]
async fn accept_header_advertises_manifest_and_index_types() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
        .mount(&server)
        .await;
    // Require both a manifest type and an index type in the Accept header.
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header_regex(
            "accept",
            "application/vnd\\.docker\\.distribution\\.manifest\\.v2\\+json",
        ))
        .and(header_regex(
            "accept",
            "application/vnd\\.oci\\.image\\.index\\.v1\\+json",
        ))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    hub.fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("accept header must include both manifest and index types");
}

#[tokio::test]
async fn token_request_uses_repository_pull_scope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param(
            "scope",
            "repository:nginxinc/nginx-unprivileged:pull",
        ))
        .and(query_param("service", "registry.docker.io"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/nginxinc/nginx-unprivileged/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    hub.fetch_digest(&ImageRef::parse("nginxinc/nginx-unprivileged"))
        .await
        .expect("scope must match repo path");
}

#[tokio::test]
async fn library_prefix_propagates_into_token_scope() {
    let server = MockServer::start().await;
    // Single-component refs become library/<name> in the token scope.
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("scope", "repository:library/alpine:pull"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "t"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let hub = hub_pointed_at(&server).await;
    hub.fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("library/* scope must be applied for single-component refs");
}

#[tokio::test]
async fn unreachable_endpoint_surfaces_as_network_unavailable() {
    // A localhost port that nothing is listening on. Any small unused port
    // works; the OS will refuse the connection immediately.
    let dead = "http://127.0.0.1:1";
    let hub = DockerHub::with_endpoints(Endpoints {
        auth_base: dead.into(),
        registry_base: dead.into(),
    });
    let err = hub
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("unreachable endpoint must error");
    assert!(
        matches!(err, RegistryError::NetworkUnavailable(_)),
        "got {err:?}"
    );
}
