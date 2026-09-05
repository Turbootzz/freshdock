//! The single "is there a newer image?" path, shared by `freshdock check` and
//! the scheduler daemon (DRY). Resolves the local manifest digest, routes to
//! the registry, and returns a [`ProbeOutcome`] each caller renders or acts on.
//!
//! Routing: digest-pinned refs (`sha256:<id>` or `repo@sha256:<id>`) return
//! [`ProbeOutcome::Pinned`] before any registry call (issue #27); every other
//! ref goes to the registry, which resolves the host and runs the bearer-token
//! flow. A registry that needs (or rejects) credentials surfaces as
//! [`ProbeOutcome::AuthRequired`].

use tracing::warn;

use crate::docker::check::DockerCheck;
use crate::registry::{ImageRef, Registry, RegistryError};

/// Every manifest digest the local image is recorded under. A republished
/// multi-arch index leaves one image carrying several, so upstream is compared
/// for membership rather than against one entry (#74).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalDigests(Vec<String>);

impl LocalDigests {
    pub fn new(digests: Vec<String>) -> Self {
        Self(digests)
    }

    /// `None` when no local digest is known (a locally-built image, or a failed
    /// inspect) — an `Option` so no caller can silently read unknown as an
    /// available update and recreate a container it cannot pull.
    pub fn update_available(&self, latest: &str) -> Option<bool> {
        (!self.0.is_empty()).then(|| !self.contains(latest))
    }

    /// The digest to show as "current": the one upstream serves when we already
    /// carry it, otherwise whichever Docker reports first.
    pub fn current_for<'a>(&'a self, latest: &'a str) -> Option<&'a str> {
        if self.contains(latest) {
            return Some(latest);
        }
        self.0.first().map(String::as_str)
    }

    fn contains(&self, digest: &str) -> bool {
        self.0.iter().any(|d| d == digest)
    }
}

/// Verdict of probing one image reference for an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The upstream digest was fetched. `local` holds every manifest digest the
    /// local image is recorded under; ask
    /// [`update_available`](LocalDigests::update_available) for the verdict.
    Fetched { local: LocalDigests, latest: String },
    /// The reference is pinned to a digest — there is nothing to check.
    Pinned,
    /// The registry needs credentials we don't have. Reported, not errored —
    /// set `[registry.<name>]` creds.
    AuthRequired,
    /// Configured credentials were rejected and the anonymous fallback was also
    /// denied (a private image with a stale/wrong token). Distinct from
    /// [`AuthRequired`] — the fix is to rotate the token, not to set one.
    CredentialsRejected,
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
        Ok(digests) => LocalDigests::new(local_manifest_digests(image, &digests)),
        Err(e) => {
            warn!(image = %image, error = %e, "image inspect failed; current digest will be unknown");
            LocalDigests::default()
        }
    };

    let image_ref = ImageRef::parse(image);
    match registry.fetch_digest(&image_ref).await {
        Ok(d) => ProbeOutcome::Fetched { local, latest: d.0 },
        Err(RegistryError::NetworkUnavailable(reason)) => {
            warn!(repo = %image_ref.repository, %reason, "network unavailable");
            ProbeOutcome::NetworkUnavailable
        }
        // Distinct from a hard error: the registry simply needs credentials.
        Err(RegistryError::Auth(reason)) => {
            warn!(repo = %image_ref.repository, %reason, "registry requires credentials");
            ProbeOutcome::AuthRequired
        }
        // Creds were set but rejected, and anonymous couldn't see the image
        // either — surfaced distinctly so the operator rotates the token.
        Err(RegistryError::CredentialsRejected(host)) => {
            warn!(repo = %image_ref.repository, %host, "configured credentials rejected; anonymous access also denied");
            ProbeOutcome::CredentialsRejected
        }
        Err(e) => {
            warn!(repo = %image_ref.repository, error = %e, "digest fetch failed");
            ProbeOutcome::Error(e.to_string())
        }
    }
}

/// Is this reference pinned to an immutable digest? Either a bare `sha256:<id>`
/// or any `name@<algo>:<hex>` form — `@` only appears in a Docker reference as
/// the digest separator.
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

/// Collect every manifest digest for an image reference from an
/// `ImageInspect.RepoDigests` list. RepoDigests entries look like
/// `repo@sha256:<hex>`; we match on the repo portion (everything before `@`)
/// against the image's repo (the input with any `@digest` and any trailing
/// `:tag` stripped) and keep the digest.
///
/// All matching entries are returned: they name the same local image, and any
/// of them may be the one upstream currently resolves to.
pub(crate) fn local_manifest_digests(image: &str, repo_digests: &[String]) -> Vec<String> {
    let want_repo =
        crate::registry::familiar_repository(strip_tag(image.split('@').next().unwrap_or(image)));
    repo_digests
        .iter()
        .filter_map(|rd| {
            let (repo, digest) = rd.split_once('@')?;
            (crate::registry::familiar_repository(repo) == want_repo).then(|| digest.to_owned())
        })
        .collect()
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
    const DIG_C: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    // --- republished multi-arch index (issue #74) ---

    #[test]
    fn collects_every_repo_digest_recorded_for_the_image() {
        // One local image, several index digests — what repeated pulls of a
        // republished multi-arch tag leave behind.
        let repo_digests = [
            format!("caddy@{DIG_A}"),
            format!("caddy@{DIG_B}"),
            format!("caddy@{DIG_C}"),
        ];
        assert_eq!(
            local_manifest_digests("caddy:latest", &repo_digests).as_slice(),
            [DIG_A.to_owned(), DIG_B.to_owned(), DIG_C.to_owned()]
        );
    }

    #[test]
    fn collecting_digests_ignores_other_repos() {
        let repo_digests = [format!("redis@{DIG_A}"), format!("caddy@{DIG_B}")];
        assert_eq!(
            local_manifest_digests("caddy:latest", &repo_digests).as_slice(),
            [DIG_B.to_owned()]
        );
    }

    #[test]
    fn local_digests_match_upstream_anywhere_in_the_list() {
        let local = LocalDigests::new(vec![DIG_A.to_owned(), DIG_C.to_owned()]);
        assert_eq!(
            local.update_available(DIG_C),
            Some(false),
            "a later entry still counts as local"
        );
        assert_eq!(local.update_available(DIG_B), Some(true));
    }

    #[test]
    fn no_local_digest_is_unknown_rather_than_an_update() {
        assert_eq!(LocalDigests::default().update_available(DIG_A), None);
    }

    #[test]
    fn current_digest_prefers_the_entry_upstream_resolves_to() {
        let local = LocalDigests::new(vec![DIG_A.to_owned(), DIG_C.to_owned()]);
        assert_eq!(local.current_for(DIG_C), Some(DIG_C));
        assert_eq!(
            local.current_for(DIG_B),
            Some(DIG_A),
            "with no match the first entry is shown, as before"
        );
        assert_eq!(LocalDigests::default().current_for(DIG_A), None);
    }

    #[tokio::test]
    async fn republished_index_carrying_the_upstream_digest_is_up_to_date() {
        // Issue #74: comparing upstream against one entry of RepoDigests can
        // never match once the platform manifest stops changing.
        let docker = FakeDocker::with_digests(&[(
            "caddy:latest",
            &[
                &format!("caddy@{DIG_A}"),
                &format!("caddy@{DIG_B}"),
                &format!("caddy@{DIG_C}"),
            ],
        )]);
        let registry = FakeRegistry::new(DIG_C);

        let ProbeOutcome::Fetched { local, latest, .. } =
            probe_image(&docker, &registry, "caddy:latest").await
        else {
            panic!("expected a fetched outcome");
        };
        assert_eq!(
            local.update_available(&latest),
            Some(false),
            "the upstream digest is already present locally"
        );
        assert_eq!(local.current_for(&latest), Some(DIG_C));
    }

    // --- local_manifest_digests / strip_tag ---

    #[test]
    fn extracts_manifest_digest_when_repo_matches() {
        let image = "nginx:alpine";
        let repo_digests = [
            "nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
        ];
        assert_eq!(
            local_manifest_digests(image, &repo_digests).as_slice(),
            [
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned()
            ]
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
            local_manifest_digests(image, &repo_digests).as_slice(),
            [
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn returns_no_digests_when_no_repo_digest_matches() {
        let image = "nginx:alpine";
        let repo_digests = ["redis@sha256:dead".to_owned()];
        assert!(local_manifest_digests(image, &repo_digests).is_empty());
    }

    #[test]
    fn returns_no_digests_for_empty_repo_digests() {
        assert!(local_manifest_digests("nginx:alpine", &[]).is_empty());
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
            local_manifest_digests(image, &repo_digests).as_slice(),
            [
                "sha256:3333333333333333333333333333333333333333333333333333333333333333"
                    .to_owned()
            ]
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
            local_manifest_digests(image, &repo_digests).as_slice(),
            [
                "sha256:4444444444444444444444444444444444444444444444444444444444444444"
                    .to_owned()
            ]
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
            local_manifest_digests(image, &repo_digests).as_slice(),
            [
                "sha256:beefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeefbeef"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn qualified_hub_reference_matches_the_familiar_repo_digest() {
        let repo_digests = ["nginx@sha256:aaa".to_owned()];
        assert_eq!(
            local_manifest_digests("docker.io/library/nginx:alpine", &repo_digests),
            vec!["sha256:aaa".to_owned()]
        );
        assert_eq!(
            local_manifest_digests("index.docker.io/library/nginx:alpine", &repo_digests),
            vec!["sha256:aaa".to_owned()]
        );
        assert_eq!(
            local_manifest_digests("library/nginx:alpine", &repo_digests),
            vec!["sha256:aaa".to_owned()]
        );
    }

    #[test]
    fn qualified_hub_user_repo_matches_its_digest() {
        let repo_digests = ["nginxinc/nginx-unprivileged@sha256:bbb".to_owned()];
        assert_eq!(
            local_manifest_digests("docker.io/nginxinc/nginx-unprivileged:1.27", &repo_digests),
            vec!["sha256:bbb".to_owned()]
        );
    }

    #[test]
    fn other_registries_still_match_literally() {
        let repo_digests = ["ghcr.io/astral-sh/uv@sha256:ccc".to_owned()];
        assert_eq!(
            local_manifest_digests("ghcr.io/astral-sh/uv:alpine", &repo_digests),
            vec!["sha256:ccc".to_owned()]
        );
        assert!(local_manifest_digests("docker.io/library/uv:alpine", &repo_digests).is_empty());
    }

    #[test]
    fn fully_qualified_repo_digests_match_a_familiar_reference() {
        // Podman records RepoDigests fully qualified.
        let repo_digests = ["docker.io/library/nginx@sha256:aaa".to_owned()];
        assert_eq!(
            local_manifest_digests("nginx:alpine", &repo_digests),
            vec!["sha256:aaa".to_owned()]
        );
        assert_eq!(
            local_manifest_digests("docker.io/library/nginx:alpine", &repo_digests),
            vec!["sha256:aaa".to_owned()]
        );
    }

    #[tokio::test]
    async fn qualified_reference_with_a_familiar_repo_digest_is_up_to_date() {
        let docker = FakeDocker::new(&[("docker.io/library/nginx:alpine", "nginx@sha256:aaa")]);
        let registry = FakeRegistry::new("sha256:aaa");

        let ProbeOutcome::Fetched { local, latest, .. } =
            probe_image(&docker, &registry, "docker.io/library/nginx:alpine").await
        else {
            panic!("expected a fetched outcome");
        };
        assert_eq!(local.update_available(&latest), Some(false));
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
        /// Several RepoDigests entries for one image — what a republished
        /// multi-arch index leaves behind (issue #74).
        fn with_digests(repo_digests: &[(&str, &[&str])]) -> Self {
            Self {
                repo_digests: repo_digests
                    .iter()
                    .map(|(img, rds)| {
                        (
                            (*img).to_owned(),
                            rds.iter().map(|rd| (*rd).to_owned()).collect(),
                        )
                    })
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

    /// What the fake registry should yield on the next `fetch_digest`.
    enum FakeResult {
        Digest(String),
        AuthRequired,
        CredentialsRejected,
    }

    struct FakeRegistry {
        result: FakeResult,
        calls: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(digest: &str) -> Self {
            Self::with(FakeResult::Digest(digest.to_owned()))
        }
        fn auth_required() -> Self {
            Self::with(FakeResult::AuthRequired)
        }
        fn credentials_rejected() -> Self {
            Self::with(FakeResult::CredentialsRejected)
        }
        fn with(result: FakeResult) -> Self {
            Self {
                result,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Registry for FakeRegistry {
        async fn fetch_digest(&self, _image: &ImageRef) -> Result<Digest, RegistryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.result {
                FakeResult::Digest(d) => Ok(Digest(d.clone())),
                FakeResult::AuthRequired => {
                    Err(RegistryError::Auth("no credentials for registry".into()))
                }
                FakeResult::CredentialsRejected => {
                    Err(RegistryError::CredentialsRejected("docker.io".into()))
                }
            }
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
                local: LocalDigests::new(vec![DIG_A.to_owned()]),
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
                local: LocalDigests::new(vec![DIG_A.to_owned()]),
                latest: DIG_B.to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn non_hub_image_is_fetched_via_the_registry() {
        // Phase 5: non-Docker-Hub refs are no longer short-circuited — the
        // registry resolves the host and runs the bearer-token flow.
        let docker = FakeDocker::new(&[]);
        let registry = FakeRegistry::new(DIG_A);
        let outcome = probe_image(&docker, &registry, "ghcr.io/owner/repo:v1").await;
        assert_eq!(
            outcome,
            ProbeOutcome::Fetched {
                local: LocalDigests::default(),
                latest: DIG_A.to_owned(),
            }
        );
        assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn registry_auth_error_maps_to_auth_required() {
        let docker = FakeDocker::new(&[]);
        let registry = FakeRegistry::auth_required();
        let outcome = probe_image(&docker, &registry, "ghcr.io/owner/private:v1").await;
        assert_eq!(outcome, ProbeOutcome::AuthRequired);
        assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn registry_credentials_rejected_maps_to_credentials_rejected() {
        let docker = FakeDocker::new(&[]);
        let registry = FakeRegistry::credentials_rejected();
        let outcome = probe_image(&docker, &registry, "ghcr.io/owner/private:v1").await;
        assert_eq!(outcome, ProbeOutcome::CredentialsRejected);
        assert_eq!(registry.calls.load(Ordering::SeqCst), 1);
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
