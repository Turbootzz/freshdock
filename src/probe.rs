//! The single "is there a newer image?" path, shared by `freshdock check` and
//! the scheduler daemon (DRY). Resolves the tag's local image, routes to the
//! registry, and returns a [`ProbeOutcome`] each caller turns into a verdict.
//!
//! Routing: digest-pinned refs (`sha256:<id>` or `repo@sha256:<id>`) return
//! [`ProbeOutcome::Pinned`] before any registry call (issue #27); every other
//! ref goes to the registry, which resolves the host and runs the bearer-token
//! flow. A registry that needs (or rejects) credentials surfaces as
//! [`ProbeOutcome::AuthRequired`].

use bollard::models::ContainerSummary;
use tracing::warn;

use crate::docker::DockerError;
use crate::docker::check::DockerCheck;
use crate::docker::spec::SpecError;
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

    /// The verdict for one container: upstream's digest is not among the tag's
    /// local digests, or the container is [`behind_tag`].
    pub fn update_available_for(
        &self,
        latest: &str,
        container_image_id: Option<&str>,
        tag_image_id: Option<&str>,
    ) -> Option<bool> {
        // Gated on known digests: otherwise a locally built image reads as stale.
        if !self.0.is_empty() && behind_tag(container_image_id, tag_image_id) {
            return Some(true);
        }
        self.update_available(latest)
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

/// Is the container running an image the tag no longer resolves to?
pub fn behind_tag(container_image_id: Option<&str>, tag_image_id: Option<&str>) -> bool {
    matches!((container_image_id, tag_image_id), (Some(c), Some(t)) if c != t)
}

/// Verdict of probing one image reference for an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The upstream digest was fetched. Ask
    /// [`update_available_for`](LocalDigests::update_available_for) for the verdict.
    Fetched {
        local: LocalDigests,
        latest: String,
        tag_image_id: Option<String>,
    },
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

/// What to probe for one container: its reference and the image it runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProbeTarget {
    /// The reference the container was created from, e.g. `nginx:alpine`.
    pub image: String,
    /// The id of the image the container runs, when the daemon reported one.
    pub image_id: Option<String>,
}

impl ProbeTarget {
    /// A target with no known image id; the verdict is then membership only.
    #[cfg(test)]
    pub fn from_ref(image: &str) -> Self {
        Self {
            image: image.to_owned(),
            image_id: None,
        }
    }
}

/// The probe target for a listed container. A listing `Image` that is not a bare
/// image id is `Config.Image` verbatim, so it needs no inspect.
pub async fn resolve_target(
    docker: &impl DockerCheck,
    container: &ContainerSummary,
) -> Result<ProbeTarget, DockerError> {
    let image = container
        .image
        .as_deref()
        .ok_or(DockerError::Spec(SpecError::Missing("Image")))?;
    if !is_image_id(image) {
        return Ok(ProbeTarget {
            image: image.to_owned(),
            image_id: container.image_id.clone(),
        });
    }
    let id = container
        .id
        .as_deref()
        .ok_or(DockerError::Spec(SpecError::Missing("Id")))?;
    let identity = docker.container_image(id).await?;
    Ok(ProbeTarget {
        image: identity.reference,
        image_id: identity.image_id,
    })
}

/// Is this a bare image id rather than a reference?
pub(crate) fn is_image_id(image: &str) -> bool {
    image
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
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
    let (local, tag_image_id) = match docker.inspect_image(image).await {
        Ok(img) => (
            LocalDigests::new(local_manifest_digests(image, &img.repo_digests)),
            img.id,
        ),
        Err(e) => {
            warn!(image = %image, error = %e, "image inspect failed; current digest will be unknown");
            (LocalDigests::default(), None)
        }
    };

    let image_ref = ImageRef::parse(image);
    match registry.fetch_digest(&image_ref).await {
        Ok(d) => ProbeOutcome::Fetched {
            local,
            latest: d.0,
            tag_image_id,
        },
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
    use crate::docker::check::{ContainerImage, LocalImage};
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
        images: HashMap<String, LocalImage>,
        /// Container id to what an inspect reports for it.
        containers: HashMap<String, ContainerImage>,
        inspect_calls: AtomicUsize,
        container_inspects: AtomicUsize,
    }

    impl FakeDocker {
        fn new(repo_digests: &[(&str, &str)]) -> Self {
            let one_each: Vec<(&str, &[&str])> = repo_digests
                .iter()
                .map(|(img, rd)| (*img, std::slice::from_ref(rd)))
                .collect();
            Self::with_digests(&one_each)
        }
        /// Several RepoDigests entries for one image — what a republished
        /// multi-arch index leaves behind (issue #74).
        fn with_digests(repo_digests: &[(&str, &[&str])]) -> Self {
            Self {
                images: repo_digests
                    .iter()
                    .map(|(img, rds)| {
                        (
                            (*img).to_owned(),
                            LocalImage {
                                id: None,
                                repo_digests: rds.iter().map(|rd| (*rd).to_owned()).collect(),
                            },
                        )
                    })
                    .collect(),
                containers: HashMap::new(),
                inspect_calls: AtomicUsize::new(0),
                container_inspects: AtomicUsize::new(0),
            }
        }
        /// The id the tag resolves to locally.
        fn with_image_id(mut self, image: &str, id: &str) -> Self {
            self.images.entry(image.to_owned()).or_default().id = Some(id.to_owned());
            self
        }
        fn with_container(mut self, id: &str, reference: &str, image_id: Option<&str>) -> Self {
            self.containers.insert(
                id.to_owned(),
                ContainerImage {
                    reference: reference.to_owned(),
                    image_id: image_id.map(str::to_owned),
                },
            );
            self
        }
        fn inspect_calls(&self) -> usize {
            self.inspect_calls.load(Ordering::SeqCst)
        }
        fn container_inspects(&self) -> usize {
            self.container_inspects.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl DockerCheck for FakeDocker {
        async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
            Ok(vec![])
        }
        async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError> {
            self.inspect_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.images.get(image).cloned().unwrap_or_default())
        }
        async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError> {
            self.container_inspects.fetch_add(1, Ordering::SeqCst);
            self.containers
                .get(id)
                .cloned()
                .ok_or(DockerError::Spec(SpecError::Missing("Config.Image")))
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
                tag_image_id: None,
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
                tag_image_id: None,
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
                tag_image_id: None,
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

    // --- one container's verdict against the tag ---

    const OLD_ID: &str = "sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";
    const NEW_ID: &str = "sha256:0e7f2f0e2e8b4a4b8c3d1a5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192";

    fn summary(id: &str, image: &str) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_owned()),
            image: Some(image.to_owned()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn behind_the_tag_is_an_update_even_when_its_own_image_has_no_digests() {
        let docker = FakeDocker::new(&[("caddy:latest", &format!("caddy@{DIG_B}"))])
            .with_image_id("caddy:latest", NEW_ID);
        let registry = FakeRegistry::new(DIG_B);

        let ProbeOutcome::Fetched {
            local,
            latest,
            tag_image_id,
        } = probe_image(&docker, &registry, "caddy:latest").await
        else {
            panic!("expected a fetched outcome");
        };
        assert_eq!(
            local.update_available(&latest),
            Some(false),
            "membership alone reports the tag as up to date"
        );
        assert_eq!(
            local.update_available_for(&latest, Some(OLD_ID), tag_image_id.as_deref()),
            Some(true),
            "the container runs an older image than the tag"
        );
    }

    #[test]
    fn same_image_as_the_tag_uses_membership() {
        let local = LocalDigests::new(vec![DIG_A.to_owned()]);
        assert_eq!(
            local.update_available_for(DIG_A, Some(NEW_ID), Some(NEW_ID)),
            Some(false)
        );
        assert_eq!(
            local.update_available_for(DIG_B, Some(NEW_ID), Some(NEW_ID)),
            Some(true)
        );
    }

    #[test]
    fn an_unknown_local_digest_is_not_an_update_even_when_the_ids_differ() {
        assert_eq!(
            LocalDigests::default().update_available_for(DIG_A, Some(OLD_ID), Some(NEW_ID)),
            None
        );
    }

    #[test]
    fn unknown_ids_fall_back_to_membership() {
        let local = LocalDigests::new(vec![DIG_A.to_owned()]);
        assert_eq!(
            local.update_available_for(DIG_A, None, Some(NEW_ID)),
            Some(false)
        );
        assert_eq!(
            local.update_available_for(DIG_A, Some(OLD_ID), None),
            Some(false)
        );
        assert_eq!(
            LocalDigests::default().update_available_for(DIG_A, None, None),
            None
        );
    }

    #[tokio::test]
    async fn resolve_target_reads_config_image_and_image_id_from_the_inspect() {
        let docker = FakeDocker::new(&[]).with_container("c1", "nginx:alpine", Some(OLD_ID));
        let target = resolve_target(&docker, &summary("c1", OLD_ID))
            .await
            .unwrap();
        assert_eq!(target.image, "nginx:alpine");
        assert_eq!(target.image_id.as_deref(), Some(OLD_ID));
    }

    #[tokio::test]
    async fn resolve_target_propagates_an_inspect_failure() {
        // The listing names an id, so the inspect is the only source there is.
        let docker = FakeDocker::new(&[]);
        assert!(
            resolve_target(&docker, &summary("c1", OLD_ID))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn resolve_target_uses_the_listing_when_it_names_a_reference() {
        let docker = FakeDocker::new(&[]);
        let summary = ContainerSummary {
            image_id: Some(OLD_ID.to_owned()),
            ..summary("c1", "nginx:alpine")
        };
        let target = resolve_target(&docker, &summary).await.unwrap();
        assert_eq!(target.image, "nginx:alpine");
        assert_eq!(target.image_id.as_deref(), Some(OLD_ID));
        assert_eq!(docker.container_inspects(), 0);
    }

    #[tokio::test]
    async fn resolve_target_inspects_only_when_the_listing_carries_an_image_id() {
        let docker = FakeDocker::new(&[]).with_container("c1", "nginx:alpine", Some(OLD_ID));
        let target = resolve_target(&docker, &summary("c1", OLD_ID))
            .await
            .unwrap();
        assert_eq!(target.image, "nginx:alpine");
        assert_eq!(target.image_id.as_deref(), Some(OLD_ID));
        assert_eq!(docker.container_inspects(), 1);
    }

    #[tokio::test]
    async fn resolve_target_errors_when_the_listing_has_no_image() {
        let docker = FakeDocker::new(&[]);
        let summary = ContainerSummary {
            image: None,
            ..summary("c1", "ignored")
        };
        assert!(resolve_target(&docker, &summary).await.is_err());
    }

    #[tokio::test]
    async fn a_container_created_by_id_stays_pinned_without_any_image_inspect() {
        // `Config.Image` is itself an id: there is no tag to follow.
        let docker = FakeDocker::new(&[]).with_container("c1", OLD_ID, Some(OLD_ID));
        let registry = FakeRegistry::new(DIG_A);
        let target = resolve_target(&docker, &summary("c1", OLD_ID))
            .await
            .unwrap();
        let outcome = probe_image(&docker, &registry, &target.image).await;
        assert_eq!(outcome, ProbeOutcome::Pinned);
        assert_eq!(docker.inspect_calls(), 0);
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
