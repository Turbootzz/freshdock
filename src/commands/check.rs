use std::collections::HashMap;
use std::sync::Arc;

use comfy_table::Table;
use comfy_table::presets::{NOTHING, UTF8_FULL};
use futures::future::join_all;

use crate::config::CredentialStore;
use crate::docker::Docker;
use crate::docker::check::DockerCheck;
use crate::errors::AppError;
use crate::labels::{self, Mode};
use crate::probe::{self, ProbeOutcome, pinned_digest};
use crate::registry::Registry;
use crate::registry::digest::OciRegistry;

const AUTH_REQUIRED: &str = "auth required (set credentials)";
const NETWORK_UNAVAILABLE: &str = "network unavailable";
const PINNED: &str = "pinned (no check)";

/// Run the read-only check: list opted-in containers, fetch latest
/// digests for those on Docker Hub, and render a status table.
///
/// Always exits with success — including when updates are detected — per
/// issue #7's acceptance criteria. Errors that prevent the table from
/// rendering at all (e.g. cannot reach the Docker socket) propagate up.
///
pub async fn run(no_color: bool, store: Arc<CredentialStore>) -> Result<(), AppError> {
    let docker = Docker::connect(store.clone())?;
    let registry = OciRegistry::new(store);
    let cells = collect_cells(&docker, &registry).await?;
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

    // Probe each unique image reference once. A homelab compose stack often
    // has several containers sharing the same image; firing duplicate `image
    // inspect` calls or duplicate token+HEAD requests would waste Docker Hub's
    // anonymous rate budget (100 / 6h) and multiply daemon round-trips by the
    // number of duplicate containers. [`probe::probe_image`] is the same
    // "is there an update?" path the scheduler daemon uses (DRY).
    let unique = unique_images(&rows);
    let fetches = unique.into_iter().map(|img| async move {
        (
            img.clone(),
            probe::probe_image(docker, registry, &img).await,
        )
    });
    let by_image: HashMap<String, ProbeOutcome> = join_all(fetches).await.into_iter().collect();

    let mut cells = Vec::with_capacity(rows.len());
    for row in rows.into_iter() {
        let outcome = by_image
            .get(&row.image)
            .cloned()
            .unwrap_or_else(|| ProbeOutcome::Error("internal: missing fetch result".into()));
        let (current, latest, update) = render_cells(&row.image, &outcome);
        cells.push([
            row.name,
            row.image,
            row.mode.to_string(),
            current,
            latest,
            update,
        ]);
    }

    Ok(cells)
}

/// Map a [`ProbeOutcome`] to the `(current digest, latest digest, update?)`
/// table cells.
fn render_cells(image: &str, outcome: &ProbeOutcome) -> (String, String, String) {
    let dash = || "-".to_string();
    match outcome {
        ProbeOutcome::Fetched { local, latest } => {
            let current = local.as_deref().map(short_digest).unwrap_or_else(dash);
            let update = local
                .as_deref()
                .map(|l| if l == latest { "no" } else { "yes" })
                .unwrap_or("?")
                .to_string();
            (current, short_digest(latest), update)
        }
        ProbeOutcome::Pinned => {
            let current = pinned_digest(image).map(short_digest).unwrap_or_else(dash);
            (current, PINNED.to_string(), dash())
        }
        ProbeOutcome::AuthRequired => (dash(), AUTH_REQUIRED.to_string(), dash()),
        ProbeOutcome::NetworkUnavailable => (dash(), NETWORK_UNAVAILABLE.to_string(), dash()),
        ProbeOutcome::Error(msg) => (dash(), format!("error: {msg}"), dash()),
    }
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

    // --- collect_cells: command-layer table assembly (#26) ---

    use crate::docker::DockerError;
    use crate::registry::{Digest, ImageRef, RegistryError};
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
        digest: Option<String>,
        calls: AtomicUsize,
    }

    impl FakeRegistry {
        fn new(digest: &str) -> Self {
            Self {
                digest: Some(digest.to_owned()),
                calls: AtomicUsize::new(0),
            }
        }
        fn auth_required() -> Self {
            Self {
                digest: None,
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Registry for FakeRegistry {
        async fn fetch_digest(&self, _image: &ImageRef) -> Result<Digest, RegistryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.digest {
                Some(d) => Ok(Digest(d.clone())),
                None => Err(RegistryError::Auth("no credentials".into())),
            }
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
    async fn registry_without_credentials_renders_auth_required() {
        // Phase 5: a non-Docker-Hub image is now probed. With no credentials the
        // registry reports auth-required — a clean status cell, not an error row.
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
        let registry = FakeRegistry::auth_required();

        let cells = collect_cells(&docker, &registry).await.unwrap();
        assert_eq!(cells[0][4], AUTH_REQUIRED);
        assert_eq!(cells[0][5], "-");
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            1,
            "the image is probed now (no more Phase-5 short-circuit)"
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
