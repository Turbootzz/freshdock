//! The single "is there a newer image?" path, shared by `freshdock check` and
//! the scheduler daemon (DRY). Resolves the local manifest digest, routes to
//! the registry, and returns a [`ProbeOutcome`] each caller renders or acts on.
//!
//! Routing: digest-pinned refs (`sha256:<id>` or `repo@sha256:<id>`) return
//! [`ProbeOutcome::Pinned`] before any registry call (issue #27); non-Docker-Hub
//! refs return [`ProbeOutcome::SkippedAuth`] (auth is Phase 5); Docker Hub refs
//! fetch the upstream digest.

use tracing::warn;

use crate::docker::check::DockerCheck;
use crate::registry::{ImageRef, Registry, RegistryError};

/// Verdict of probing one image reference for an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The upstream digest was fetched. `local` is the resolved local manifest
    /// digest (`None` when it couldn't be determined); `latest` is upstream.
    Fetched {
        local: Option<String>,
        latest: String,
    },
    /// The reference is pinned to a digest — there is nothing to check.
    Pinned,
    /// A non-Docker-Hub registry: skipped until Phase 5 registry auth lands.
    SkippedAuth,
    /// The registry was unreachable; degrade gracefully (retry later).
    NetworkUnavailable,
    /// The fetch failed for some other reason; the message is for display/logs.
    Error(String),
}

/// Resolve whether `image` has a newer digest upstream. See the module docs for
/// routing. Performs at most one image inspect and one registry fetch.
pub async fn probe_image(
    docker: &impl DockerCheck,
    registry: &impl Registry,
    image: &str,
) -> ProbeOutcome {
    // Digest-pinned references never move — short-circuit before touching the
    // daemon or the registry (issue #27).
    if is_pinned(image) {
        return ProbeOutcome::Pinned;
    }

    // ContainerSummary.image_id is the image *config* digest, not the *manifest*
    // digest the registry returns via Docker-Content-Digest. Resolve the local
    // manifest digest from `image inspect → RepoDigests`.
    let local = match docker.inspect_image_repo_digests(image).await {
        Ok(digests) => manifest_digest_for(image, &digests),
        Err(e) => {
            warn!(image = %image, error = %e, "image inspect failed; current digest will be unknown");
            None
        }
    };

    let image_ref = ImageRef::parse(image);
    if !is_docker_hub(&image_ref.repository) {
        return ProbeOutcome::SkippedAuth;
    }
    match registry.fetch_digest(&image_ref).await {
        Ok(d) => ProbeOutcome::Fetched { local, latest: d.0 },
        Err(RegistryError::NetworkUnavailable(reason)) => {
            warn!(repo = %image_ref.repository, %reason, "network unavailable");
            ProbeOutcome::NetworkUnavailable
        }
        Err(e) => {
            warn!(repo = %image_ref.repository, error = %e, "digest fetch failed");
            ProbeOutcome::Error(e.to_string())
        }
    }
}

/// Is this reference pinned to an immutable digest (`sha256:<id>` directly, or
/// `repo@sha256:<id>`)?
pub(crate) fn is_pinned(image: &str) -> bool {
    image.starts_with("sha256:") || image.contains('@')
}

/// The `sha256:<hex>` digest embedded in a pinned reference, for display. For
/// `repo@sha256:<hex>` it's the part after `@`; for a bare `sha256:<hex>` it's
/// the whole string. `None` for unpinned refs.
pub(crate) fn pinned_digest(image: &str) -> Option<&str> {
    if let Some((_, digest)) = image.split_once('@') {
        Some(digest)
    } else if image.starts_with("sha256:") {
        Some(image)
    } else {
        None
    }
}

/// Docker Hub references have a repo of `library/<name>` or `<owner>/<name>`.
/// Anything containing a host (`ghcr.io/...`, `quay.io/...`, `lscr.io/...`,
/// or a bare `localhost[/...]`) belongs to a private/non-Hub registry.
pub(crate) fn is_docker_hub(repository: &str) -> bool {
    let first = repository.split('/').next().unwrap_or("");
    if first.eq_ignore_ascii_case("localhost") {
        return false;
    }
    !(first.contains('.') || first.contains(':'))
}

/// Find the manifest digest for an image reference inside an `ImageInspect.RepoDigests`
/// list. RepoDigests entries look like `repo@sha256:<hex>`; we match on the repo
/// portion (everything before `@`) against the image's repo (the input with any
/// `@digest` and any trailing `:tag` stripped) and return the digest.
pub(crate) fn manifest_digest_for(image: &str, repo_digests: &[String]) -> Option<String> {
    let want_repo = strip_tag(image.split('@').next()?);
    repo_digests.iter().find_map(|rd| {
        let (repo, digest) = rd.split_once('@')?;
        (repo == want_repo).then(|| digest.to_owned())
    })
}

/// Strip a trailing `:tag` from an image reference without confusing it for a
/// `host:port` separator. A colon is a tag separator only when it appears
/// after the last `/` (or when there is no `/` at all). Anything else —
/// `localhost:5000/repo`, `registry.example.com:443/repo` — must round-trip
/// untouched so the RepoDigests entry's `repo` portion still matches.
pub(crate) fn strip_tag(image_no_digest: &str) -> &str {
    match (image_no_digest.rfind(':'), image_no_digest.rfind('/')) {
        (Some(colon), Some(slash)) if colon > slash => &image_no_digest[..colon],
        (Some(colon), None) => &image_no_digest[..colon],
        _ => image_no_digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;
    use crate::registry::Digest;
    use async_trait::async_trait;
    use bollard::models::ContainerSummary;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIG_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIG_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // --- is_docker_hub ---

    #[test]
    fn docker_hub_namespacing() {
        assert!(is_docker_hub("library/alpine"));
        assert!(is_docker_hub("nginxinc/nginx-unprivileged"));
        assert!(!is_docker_hub("ghcr.io/owner/repo"));
        assert!(!is_docker_hub("quay.io/foo/bar"));
        assert!(!is_docker_hub("lscr.io/linuxserver/sonarr"));
    }

    #[test]
    fn localhost_is_not_docker_hub() {
        assert!(!is_docker_hub("localhost/image"));
        assert!(!is_docker_hub("LOCALHOST/repo"));
        assert!(!is_docker_hub("localhost:5000/repo"));
    }

    // --- manifest_digest_for / strip_tag ---

    #[test]
    fn extracts_manifest_digest_when_repo_matches() {
        let image = "nginx:alpine";
        let repo_digests = [
            "nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
        ];
        assert_eq!(
            manifest_digest_for(image, &repo_digests).as_deref(),
            Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn extracts_manifest_digest_for_namespaced_repo() {
        let image = "ghcr.io/owner/repo:v1";
        let repo_digests = [
            "other/thing@sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .to_owned(),
            "ghcr.io/owner/repo@sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .to_owned(),
        ];
        assert_eq!(
            manifest_digest_for(image, &repo_digests).as_deref(),
            Some("sha256:2222222222222222222222222222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn returns_none_when_no_repo_digest_matches() {
        let image = "nginx:alpine";
        let repo_digests = ["redis@sha256:dead".to_owned()];
        assert_eq!(manifest_digest_for(image, &repo_digests), None);
    }

    #[test]
    fn returns_none_for_empty_repo_digests() {
        assert_eq!(manifest_digest_for("nginx:alpine", &[]), None);
    }

    #[test]
    fn handles_host_port_in_registry_reference() {
        // The hostname `localhost:5000` contains a colon that must NOT be
        // mistaken for a tag separator. The RepoDigests entry preserves the
        // host:port verbatim, so we must too.
        let image = "localhost:5000/repo:v1";
        let repo_digests = [
            "localhost:5000/repo@sha256:3333333333333333333333333333333333333333333333333333333333333333"
                .to_owned(),
        ];
        assert_eq!(
            manifest_digest_for(image, &repo_digests).as_deref(),
            Some("sha256:3333333333333333333333333333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn handles_host_port_with_no_tag() {
        // No tag at all — the only colon is the host:port separator.
        let image = "localhost:5000/repo";
        let repo_digests = [
            "localhost:5000/repo@sha256:4444444444444444444444444444444444444444444444444444444444444444"
                .to_owned(),
        ];
        assert_eq!(
            manifest_digest_for(image, &repo_digests).as_deref(),
            Some("sha256:4444444444444444444444444444444444444444444444444444444444444444")
        );
    }

    #[test]
    fn handles_image_already_pinned_to_digest() {
        // When the running image is referenced by digest, the input string has
        // no tag — we should still recover the matching repo_digest entry.
        let image = "nginx@sha256:beef";
        let repo_digests = [
            "nginx@sha256:beefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeef"
                .to_owned(),
        ];
        assert_eq!(
            manifest_digest_for(image, &repo_digests).as_deref(),
            Some("sha256:beefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeef")
        );
    }

    // --- pinned-ref detection (#27) ---

    #[test]
    fn detects_pinned_references() {
        assert!(is_pinned("sha256:abcabc"));
        assert!(is_pinned("alpine@sha256:abcabc"));
        assert!(!is_pinned("alpine:3.19"));
        assert!(!is_pinned("ghcr.io/owner/repo:v1"));
    }

    #[test]
    fn pinned_digest_extracts_the_sha() {
        assert_eq!(pinned_digest("sha256:abc"), Some("sha256:abc"));
        assert_eq!(pinned_digest("alpine@sha256:abc"), Some("sha256:abc"));
        assert_eq!(pinned_digest("alpine:3.19"), None);
    }

    // --- probe_image routing ---

    /// Recording fake that counts inspect calls so we can prove the pinned
    /// short-circuit never touches the daemon.
    struct FakeDocker {
        repo_digests: HashMap<String, Vec<String>>,
        inspect_calls: AtomicUsize,
    }

    impl FakeDocker {
        fn new(repo_digests: &[(&str, &str)]) -> Self {
            Self {
                repo_digests: repo_digests
                    .iter()
                    .map(|(img, rd)| ((*img).to_owned(), vec![(*rd).to_owned()]))
                    .collect(),
                inspect_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl DockerCheck for FakeDocker {
        async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
            Ok(vec![])
        }
        async fn inspect_image_repo_digests(
            &self,
            image: &str,
        ) -> Result<Vec<String>, DockerError> {
            self.inspect_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.repo_digests.get(image).cloned().unwrap_or_default())
        }
    }

    struct FakeRegistry {
        digest: String,
        calls: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(digest: &str) -> Self {
            Self {
                digest: digest.to_owned(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Registry for FakeRegistry {
        async fn fetch_digest(&self, _image: &ImageRef) -> Result<Digest, RegistryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Digest(self.digest.clone()))
        }
    }

    #[tokio::test]
    async fn equal_digests_report_no_update() {
        let docker = FakeDocker::new(&[("alpine:3.19", &format!("alpine@{DIG_A}"))]);
        let registry = FakeRegistry::new(DIG_A);
        let outcome = probe_image(&docker, &registry, "alpine:3.19").await;
        assert_eq!(
            outcome,
            ProbeOutcome::Fetched {
                local: Some(DIG_A.to_owned()),
                latest: DIG_A.to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn differing_digests_report_an_update() {
        let docker = FakeDocker::new(&[("alpine:3.19", &format!("alpine@{DIG_A}"))]);
        let registry = FakeRegistry::new(DIG_B);
        let outcome = probe_image(&docker, &registry, "alpine:3.19").await;
        assert_eq!(
            outcome,
            ProbeOutcome::Fetched {
                local: Some(DIG_A.to_owned()),
                latest: DIG_B.to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn non_hub_is_skipped_without_a_registry_call() {
        let docker = FakeDocker::new(&[]);
        let registry = FakeRegistry::new(DIG_A);
        let outcome = probe_image(&docker, &registry, "ghcr.io/owner/repo:v1").await;
        assert_eq!(outcome, ProbeOutcome::SkippedAuth);
        assert_eq!(registry.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pinned_digest_ref_short_circuits_before_any_io() {
        let docker = FakeDocker::new(&[]);
        let registry = FakeRegistry::new(DIG_A);

        for image in ["sha256:abcdef0123456789", "alpine@sha256:abcdef0123456789"] {
            let outcome = probe_image(&docker, &registry, image).await;
            assert_eq!(outcome, ProbeOutcome::Pinned, "image={image}");
        }
        assert_eq!(
            docker.inspect_calls.load(Ordering::SeqCst),
            0,
            "a pinned ref must not trigger an image inspect"
        );
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            0,
            "a pinned ref must not trigger a registry call (issue #27)"
        );
    }
}
