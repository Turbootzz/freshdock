use std::collections::HashMap;

use comfy_table::Table;
use comfy_table::presets::{NOTHING, UTF8_FULL};
use futures::future::join_all;
use tracing::warn;

use crate::docker::Docker;
use crate::docker::check::DockerCheck;
use crate::errors::AppError;
use crate::labels::{self, Mode};
use crate::registry::digest::DockerHub;
use crate::registry::{Digest, ImageRef, Registry, RegistryError};

const SKIPPED_AUTH: &str = "skipped: not yet supported (Phase 5)";
const NETWORK_UNAVAILABLE: &str = "network unavailable";

/// Run the read-only check: list opted-in containers, fetch latest
/// digests for those on Docker Hub, and render a status table.
///
/// Always exits with success — including when updates are detected — per
/// issue #7's acceptance criteria. Errors that prevent the table from
/// rendering at all (e.g. cannot reach the Docker socket) propagate up.
///
pub async fn run(no_color: bool) -> Result<(), AppError> {
    let docker = Docker::connect()?;
    let hub = DockerHub::new();
    let cells = collect_cells(&docker, &hub).await?;
    let mut table = build_table(no_color);
    for row in cells {
        table.add_row(Vec::from(row));
    }
    println!("{table}");
    Ok(())
}

/// Build the six status columns (`container, image, mode, current digest,
/// latest digest, update?`) for every opted-in container — the testable seam,
/// parameterised over the daemon read surface ([`DockerCheck`]) and the
/// [`Registry`]. Split from table formatting so unit tests assert individual
/// cells (and the once-per-unique-image fetch) without parsing rendered output.
async fn collect_cells(
    docker: &impl DockerCheck,
    registry: &impl Registry,
) -> Result<Vec<[String; 6]>, AppError> {
    let containers = docker.list_running().await?;

    let empty = HashMap::new();

    let mut rows: Vec<RowPrep> = Vec::new();
    for c in containers {
        let lbls = c.labels.as_ref().unwrap_or(&empty);
        let policy = labels::parse_policy(lbls, None)?;
        if !policy.enabled {
            continue;
        }
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "?".to_string());
        let image_str = c.image.unwrap_or_else(|| "?".to_string());

        rows.push(RowPrep {
            name,
            image: image_str,
            mode: policy.mode,
        });
    }

    // Fetch local *and* upstream digests once per unique image reference. A
    // homelab compose stack often has several containers sharing the same
    // image; firing duplicate `image inspect` calls or duplicate token+HEAD
    // requests would waste Docker Hub's anonymous rate budget (100 / 6h) and
    // multiply daemon round-trips by the number of duplicate containers.
    let unique = unique_images(&rows);
    let fetches = unique.into_iter().map(|img| async move {
        // ContainerSummary.image_id is the image *config* digest, not the
        // *manifest* digest the registry returns via Docker-Content-Digest.
        // Resolve the local manifest digest from `image inspect → RepoDigests`.
        let local = match docker.inspect_image_repo_digests(&img).await {
            Ok(digests) => manifest_digest_for(&img, &digests),
            Err(e) => {
                warn!(image = %img, error = %e, "image inspect failed; current digest will be unknown");
                None
            }
        };
        let outcome = fetch_for(registry, &img).await;
        (img, (local, outcome))
    });
    let by_image: HashMap<String, (Option<String>, FetchOutcome)> =
        join_all(fetches).await.into_iter().collect();

    let mut cells = Vec::with_capacity(rows.len());
    for row in rows.into_iter() {
        let (local_digest, outcome) = by_image.get(&row.image).cloned().unwrap_or((
            None,
            FetchOutcome::Error("internal: missing fetch result".into()),
        ));
        let local = local_digest
            .as_deref()
            .map(short_digest)
            .unwrap_or_else(|| "-".to_string());
        let (latest_cell, update_cell) = match outcome {
            FetchOutcome::Found(d) => {
                let update = local_digest
                    .as_deref()
                    .map(|l| if l == d.0 { "no" } else { "yes" })
                    .unwrap_or("?")
                    .to_string();
                (short_digest(&d.0), update)
            }
            FetchOutcome::SkippedAuth => (SKIPPED_AUTH.to_string(), "-".to_string()),
            FetchOutcome::NetworkUnavailable => (NETWORK_UNAVAILABLE.to_string(), "-".to_string()),
            FetchOutcome::Error(msg) => (format!("error: {msg}"), "-".to_string()),
        };
        cells.push([
            row.name,
            row.image,
            row.mode.to_string(),
            local,
            latest_cell,
            update_cell,
        ]);
    }

    Ok(cells)
}

/// Order-preserving deduplication of image references across all rows.
fn unique_images(rows: &[RowPrep]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        if seen.insert(&r.image) {
            out.push(r.image.clone());
        }
    }
    out
}

struct RowPrep {
    name: String,
    image: String,
    mode: Mode,
}

#[derive(Clone)]
enum FetchOutcome {
    Found(Digest),
    SkippedAuth,
    NetworkUnavailable,
    Error(String),
}

async fn fetch_for(registry: &impl Registry, image: &str) -> FetchOutcome {
    let image_ref = ImageRef::parse(image);
    if !is_docker_hub(&image_ref.repository) {
        return FetchOutcome::SkippedAuth;
    }
    match registry.fetch_digest(&image_ref).await {
        Ok(d) => FetchOutcome::Found(d),
        Err(RegistryError::NetworkUnavailable(reason)) => {
            warn!(repo = %image_ref.repository, %reason, "network unavailable");
            FetchOutcome::NetworkUnavailable
        }
        Err(e) => {
            warn!(repo = %image_ref.repository, error = %e, "digest fetch failed");
            FetchOutcome::Error(e.to_string())
        }
    }
}

/// Docker Hub references have a repo of `library/<name>` or `<owner>/<name>`.
/// Anything containing a host (`ghcr.io/...`, `quay.io/...`, `lscr.io/...`,
/// or a bare `localhost[/...]`) belongs to a private/non-Hub registry.
fn is_docker_hub(repository: &str) -> bool {
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
fn manifest_digest_for(image: &str, repo_digests: &[String]) -> Option<String> {
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
fn strip_tag(image_no_digest: &str) -> &str {
    match (image_no_digest.rfind(':'), image_no_digest.rfind('/')) {
        (Some(colon), Some(slash)) if colon > slash => &image_no_digest[..colon],
        (Some(colon), None) => &image_no_digest[..colon],
        _ => image_no_digest,
    }
}

fn short_digest(d: &str) -> String {
    if let Some(hex) = d.strip_prefix("sha256:") {
        format!("sha256:{}…", &hex[..hex.len().min(12)])
    } else if d.len() > 19 {
        format!("{}…", &d[..19])
    } else {
        d.to_string()
    }
}

fn build_table(no_color: bool) -> Table {
    let mut t = Table::new();
    t.load_preset(if no_color { NOTHING } else { UTF8_FULL });
    t.set_header(vec![
        "container",
        "image",
        "mode",
        "current digest",
        "latest digest",
        "update?",
    ]);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn short_digest_truncates_sha256() {
        let d = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(short_digest(d), "sha256:abcdef012345…");
    }

    #[test]
    fn short_digest_passes_through_non_sha() {
        assert_eq!(short_digest("alpine:latest"), "alpine:latest");
    }

    fn row(image: &str) -> RowPrep {
        RowPrep {
            name: "n".into(),
            image: image.into(),
            mode: Mode::Watch,
        }
    }

    #[test]
    fn unique_images_deduplicates_preserving_first_occurrence_order() {
        let rows = vec![
            row("postgres:16-alpine"),
            row("redis:7"),
            row("postgres:16-alpine"),
            row("nginx:latest"),
            row("redis:7"),
        ];
        assert_eq!(
            unique_images(&rows),
            vec!["postgres:16-alpine", "redis:7", "nginx:latest"]
        );
    }

    #[test]
    fn unique_images_treats_distinct_tags_as_distinct() {
        let rows = vec![row("postgres:16"), row("postgres:17")];
        assert_eq!(unique_images(&rows), vec!["postgres:16", "postgres:17"]);
    }

    #[test]
    fn unique_images_on_empty_input_is_empty() {
        let rows: Vec<RowPrep> = vec![];
        assert!(unique_images(&rows).is_empty());
    }

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

    // --- run_with / collect_cells: command-layer table assembly (#26) ---

    use crate::docker::DockerError;
    use async_trait::async_trait;
    use bollard::models::ContainerSummary;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DIG_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIG_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn summary(name: &str, image: &str, labels: &[(&str, &str)]) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{name}")]),
            image: Some(image.to_owned()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// Recording fake daemon: serves a fixed container list + per-image
    /// RepoDigests, and counts `inspect_image_repo_digests` calls so the
    /// dedupe contract can be asserted.
    struct FakeDocker {
        containers: Vec<ContainerSummary>,
        repo_digests: HashMap<String, Vec<String>>,
        inspect_calls: AtomicUsize,
    }

    impl FakeDocker {
        fn new(containers: Vec<ContainerSummary>, repo_digests: &[(&str, &str)]) -> Self {
            let repo_digests = repo_digests
                .iter()
                .map(|(img, rd)| ((*img).to_owned(), vec![(*rd).to_owned()]))
                .collect();
            Self {
                containers,
                repo_digests,
                inspect_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl DockerCheck for FakeDocker {
        async fn list_running(&self) -> Result<Vec<ContainerSummary>, DockerError> {
            Ok(self.containers.clone())
        }
        async fn inspect_image_repo_digests(
            &self,
            image: &str,
        ) -> Result<Vec<String>, DockerError> {
            self.inspect_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.repo_digests.get(image).cloned().unwrap_or_default())
        }
    }

    /// Fake registry that returns a fixed upstream digest and counts calls —
    /// lets the dedupe assertion verify exactly one fetch per unique image
    /// without standing up a wiremock server.
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
    async fn matching_local_and_upstream_digest_renders_no() {
        let docker = FakeDocker::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true")],
            )],
            &[("alpine:3.19", &format!("alpine@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0][0], "web");
        assert_eq!(
            cells[0][2], "watch",
            "enable=true with no mode defaults to watch"
        );
        assert_eq!(cells[0][5], "no", "equal digests must report no update");
    }

    #[tokio::test]
    async fn differing_digest_renders_yes() {
        let docker = FakeDocker::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true")],
            )],
            &[("alpine:3.19", &format!("alpine@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_B);

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(
            cells[0][5], "yes",
            "differing digests must report an update"
        );
    }

    #[tokio::test]
    async fn non_hub_image_is_skipped_and_registry_not_called() {
        let docker = FakeDocker::new(
            vec![summary(
                "priv",
                "ghcr.io/owner/repo:v1",
                &[("freshdock.enable", "true")],
            )],
            &[(
                "ghcr.io/owner/repo:v1",
                &format!("ghcr.io/owner/repo@{DIG_A}"),
            )],
        );
        let registry = FakeRegistry::new(DIG_B);

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(cells[0][4], SKIPPED_AUTH);
        assert_eq!(cells[0][5], "-");
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            0,
            "non-Hub images must short-circuit before any registry call"
        );
    }

    #[tokio::test]
    async fn disabled_container_is_omitted() {
        let docker = FakeDocker::new(
            vec![
                summary("on", "alpine:3.19", &[("freshdock.enable", "true")]),
                summary("off", "redis:7", &[]),
            ],
            &[("alpine:3.19", &format!("alpine@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(cells.len(), 1, "only the opted-in container gets a row");
        assert_eq!(cells[0][0], "on");
    }

    #[tokio::test]
    async fn duplicate_image_across_containers_fetches_once() {
        let docker = FakeDocker::new(
            vec![
                summary("a", "redis:7", &[("freshdock.enable", "true")]),
                summary("b", "redis:7", &[("freshdock.enable", "true")]),
            ],
            &[("redis:7", &format!("redis@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(cells.len(), 2, "both containers still get their own row");
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            1,
            "the shared image must be fetched exactly once (rate-budget contract)"
        );
        assert_eq!(
            docker.inspect_calls.load(Ordering::SeqCst),
            1,
            "local digest inspect must also dedupe to one call per unique image"
        );
    }
}
