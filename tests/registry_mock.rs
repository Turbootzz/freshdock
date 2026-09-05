//! HTTP-mocked tests for the OCI registry bearer-token flow.
//!
//! These drive the full Docker registry v2 token dance against a wiremock
//! server — unauthenticated HEAD → `401` + `WWW-Authenticate` challenge → token
//! exchange at the realm → authenticated retry — so error and credential paths
//! the live tests can't reach (401 without a challenge, missing digest header,
//! token caching, basic-auth forwarding) are locked in.

use std::sync::Arc;

use freshdock::config::{Config, CredentialStore, build_store};
use freshdock::registry::digest::OciRegistry;
use freshdock::registry::{ImageRef, Registry, RegistryError};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const SAMPLE_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Matches a request that carries no `Authorization` header — used so the
/// challenge (401) mock and the authenticated (200) mock are mutually exclusive
/// regardless of wiremock's multi-match precedence.
struct NoAuthHeader;
impl Match for NoAuthHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key("authorization")
    }
}

/// Matches a request that *carries* an `Authorization` header — the complement of
/// [`NoAuthHeader`], so a token endpoint can reject the authenticated attempt and
/// grant the anonymous one without depending on the exact basic-auth value.
struct HasAuthHeader;
impl Match for HasAuthHeader {
    fn matches(&self, req: &Request) -> bool {
        req.headers.contains_key("authorization")
    }
}

/// A store with a single, deliberately wrong Docker Hub credential, keyed so it
/// resolves for `docker.io` images.
fn store_with_bad_dockerhub_creds() -> Arc<CredentialStore> {
    let config = Config::from_toml("[registry.dockerhub]\nusername = \"u\"\ntoken = \"wrong\"\n")
        .expect("valid toml");
    Arc::new(build_store(config, std::iter::empty::<(String, String)>()))
}

/// The shared client, which every test host has a CA store for.
fn test_client() -> reqwest::Client {
    freshdock::http::client().expect("http client")
}

fn anonymous_registry(server: &MockServer) -> OciRegistry {
    OciRegistry::with_base_url(
        test_client(),
        Arc::new(CredentialStore::default()),
        &server.uri(),
    )
}

/// The `WWW-Authenticate` challenge value pointing the client back at the mock's
/// `/token` endpoint, optionally pinning a `scope`.
fn bearer_challenge(server: &MockServer, scope: Option<&str>) -> String {
    let realm = format!("{}/token", server.uri());
    match scope {
        Some(scope) => {
            format!(r#"Bearer realm="{realm}",service="registry.docker.io",scope="{scope}""#)
        }
        None => format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
    }
}

/// Mount the 401 challenge for an image's manifest HEAD (unauthenticated only).
async fn mount_challenge(server: &MockServer, manifest_path: &str, scope: Option<&str>) {
    Mock::given(method("HEAD"))
        .and(path(manifest_path.to_string()))
        .and(NoAuthHeader)
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", bearer_challenge(server, scope).as_str()),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn happy_path_follows_challenge_then_returns_digest() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let digest = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("happy path should succeed");
    assert_eq!(digest.0, SAMPLE_DIGEST);
}

#[tokio::test]
async fn token_is_cached_across_requests() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    // The token endpoint must be hit exactly once across two fetches.
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"token": "test-token", "expires_in": 300})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let image = ImageRef::parse("alpine");
    registry.fetch_digest(&image).await.expect("first fetch");
    registry
        .fetch_digest(&image)
        .await
        .expect("second fetch (cached token)");
    // `.expect(1)` is verified on server drop.
}

#[tokio::test]
async fn unauthorized_without_challenge_is_a_typed_auth_error() {
    let server = MockServer::start().await;
    // 401 with NO www-authenticate header.
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let err = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("401 without a Bearer challenge must be a typed error, not a panic");
    assert!(matches!(err, RegistryError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn credentials_are_forwarded_to_the_token_endpoint() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/owner/repo/manifests/latest", None).await;
    // The token request must carry the exact HTTP Basic auth from the store:
    // base64("u:pat") == "dTpwYXQ=".
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(header("authorization", "Basic dTpwYXQ="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/owner/repo/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    // Credentials keyed by the image's host (ghcr.io).
    let config = Config::from_toml("[registry.ghcr]\nusername = \"u\"\ntoken = \"pat\"\n").unwrap();
    let store = build_store(config, std::iter::empty::<(String, String)>());
    let registry = OciRegistry::with_base_url(test_client(), Arc::new(store), &server.uri());

    let digest = registry
        .fetch_digest(&ImageRef::parse("ghcr.io/owner/repo"))
        .await
        .expect("authenticated fetch should succeed");
    assert_eq!(digest.0, SAMPLE_DIGEST);
}

#[tokio::test]
async fn synthesises_pull_scope_when_challenge_omits_it() {
    let server = MockServer::start().await;
    // Challenge with no scope → client must synthesise repository:<path>:pull.
    mount_challenge(
        &server,
        "/v2/nginxinc/nginx-unprivileged/manifests/latest",
        None,
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param(
            "scope",
            "repository:nginxinc/nginx-unprivileged:pull",
        ))
        .and(query_param("service", "registry.docker.io"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/nginxinc/nginx-unprivileged/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    registry
        .fetch_digest(&ImageRef::parse("nginxinc/nginx-unprivileged"))
        .await
        .expect("synthesised scope must match the repo path");
}

#[tokio::test]
async fn accept_header_advertises_manifest_and_index_types() {
    use wiremock::matchers::header_regex;
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .and(header_regex(
            "accept",
            r"application/vnd\.docker\.distribution\.manifest\.v2\+json",
        ))
        .and(header_regex(
            "accept",
            r"application/vnd\.oci\.image\.index\.v1\+json",
        ))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("accept header must advertise both manifest and index types");
}

#[tokio::test]
async fn manifest_404_propagates_as_http_error() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let err = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("404 should propagate");
    assert!(matches!(err, RegistryError::Http(_)), "got {err:?}");
}

#[tokio::test]
async fn missing_digest_header_returns_typed_error() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "test-token"})))
        .mount(&server)
        .await;
    // 200 OK with NO Docker-Content-Digest header.
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let err = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("missing digest header should be reported");
    assert!(matches!(err, RegistryError::MissingDigest), "got {err:?}");
}

#[tokio::test]
async fn token_endpoint_failure_maps_to_auth_error() {
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let registry = anonymous_registry(&server);
    let err = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("a 401 from the realm should surface as an auth error");
    assert!(matches!(err, RegistryError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn rejected_creds_on_public_image_falls_back_to_anonymous() {
    // A stale/wrong Docker Hub token must NOT break a public image: the token
    // endpoint rejects the authenticated attempt, the client retries anonymously,
    // and the digest still resolves.
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/library/alpine/manifests/latest", None).await;
    // Authenticated token request → rejected.
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(HasAuthHeader)
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    // Anonymous token request → granted.
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(NoAuthHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "anon-token"})))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/alpine/manifests/latest"))
        .and(header("authorization", "Bearer anon-token"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", SAMPLE_DIGEST),
        )
        .mount(&server)
        .await;

    let registry = OciRegistry::with_base_url(
        test_client(),
        store_with_bad_dockerhub_creds(),
        &server.uri(),
    );
    let digest = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect("public image must survive a rejected credential via anonymous fallback");
    assert_eq!(digest.0, SAMPLE_DIGEST);
}

#[tokio::test]
async fn rejected_creds_on_private_image_surfaces_credentials_rejected() {
    // Bad creds AND a genuinely private image (anonymous also denied) → a distinct
    // typed error so the operator knows to rotate the token, not just set one.
    let server = MockServer::start().await;
    mount_challenge(&server, "/v2/owner/repo/manifests/latest", None).await;
    // Both the authenticated and the anonymous token attempts are denied.
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let registry = OciRegistry::with_base_url(
        test_client(),
        store_with_bad_dockerhub_creds(),
        &server.uri(),
    );
    let err = registry
        .fetch_digest(&ImageRef::parse("owner/repo"))
        .await
        .expect_err("private image + rejected creds + anonymous denied must error");
    assert!(
        matches!(err, RegistryError::CredentialsRejected(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn unreachable_endpoint_surfaces_as_network_unavailable() {
    // Bind then drop a port so it's almost certainly free during preflight.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dead = format!("http://127.0.0.1:{port}");

    let registry =
        OciRegistry::with_base_url(test_client(), Arc::new(CredentialStore::default()), &dead);
    let err = registry
        .fetch_digest(&ImageRef::parse("alpine"))
        .await
        .expect_err("unreachable endpoint must error");
    assert!(
        matches!(err, RegistryError::NetworkUnavailable(_)),
        "got {err:?}"
    );
}
