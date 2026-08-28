use std::collections::HashMap;
use std::sync::Arc;

use comfy_table::Table;
use comfy_table::presets::{NOTHING, UTF8_FULL};
use futures::future::join_all;
use tracing::warn;

use crate::config::{CredentialStore, ResolvedSettings};
use crate::docker::Docker;
use crate::docker::check::DockerCheck;
use crate::errors::AppError;
use crate::format::short_digest;
use crate::labels::{self, Mode, PolicyDefaults};
use crate::probe::{self, ProbeOutcome, pinned_digest};
use crate::registry::Registry;
use crate::registry::digest::OciRegistry;

const AUTH_REQUIRED: &str = "auth required (set credentials)";
const CREDENTIALS_REJECTED: &str = "credentials rejected (check token)";
const NETWORK_UNAVAILABLE: &str = "network unavailable";
const PINNED: &str = "pinned (no check)";

/// Run the read-only check: list opted-in containers, fetch latest
/// digests for those on Docker Hub, and render a status table.
///
/// Always exits with success — including when updates are detected — per
/// issue #7's acceptance criteria. Errors that prevent the table from
/// rendering at all (e.g. cannot reach the Docker socket) propagate up.
///
pub async fn run(
    no_color: bool,
    store: Arc<CredentialStore>,
    settings: ResolvedSettings,
) -> Result<(), AppError> {
    let docker = Docker::connect(store.clone()).await?;
    let registry = OciRegistry::new(store);
    let own_id_prefix = crate::selfid::own_container_id_prefix();
    let cells = collect_cells(
        &docker,
        &registry,
        settings.policy_defaults(),
        own_id_prefix.as_deref(),
    )
    .await?;
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
    defaults: PolicyDefaults,
    own_id_prefix: Option<&str>,
) -> Result<Vec<[String; 6]>, AppError> {
    let containers = docker.list_running().await?;

    let empty = HashMap::new();

    let mut rows: Vec<RowPrep> = Vec::new();
    for c in containers {
        let lbls = c.labels.as_ref().unwrap_or(&empty);
        // Warn-and-skip like the scheduler: one bad label (on any container,
        // now that watch_all parses unlabelled bystanders too) must not take
        // down the whole table — `check` always prints and exits 0 (#7).
        let policy = match labels::parse_policy(lbls, defaults) {
            Ok(p) => p,
            Err(e) => {
                let name = c
                    .names
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|s| s.trim_start_matches('/'))
                    .unwrap_or("?");
                warn!(container = %name, error = %e, "invalid freshdock labels; skipping");
                continue;
            }
        };
        if !policy.enabled {
            continue;
        }
        // The scheduler never auto-targets freshdock's own container, so the
        // table must not promise it either.
        if policy.auto_enabled && crate::selfid::is_own_container(own_id_prefix, c.id.as_deref()) {
            continue;
        }
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "?".to_string());
        for note in labels::watchtower_diagnostics(lbls) {
            warn!(container = %name, %note, "watchtower label");
        }
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
            let current = local
                .current_for(latest)
                .map(short_digest)
                .unwrap_or_else(dash);
            let update = match local.update_available(latest) {
                Some(true) => "yes",
                Some(false) => "no",
                None => "?",
            };
            (current, short_digest(latest), update.to_string())
        }
        ProbeOutcome::Pinned => {
            let current = pinned_digest(image).map(short_digest).unwrap_or_else(dash);
            (current, PINNED.to_string(), dash())
        }
        ProbeOutcome::AuthRequired => (dash(), AUTH_REQUIRED.to_string(), dash()),
        ProbeOutcome::CredentialsRejected => (dash(), CREDENTIALS_REJECTED.to_string(), dash()),
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
    const DIG_C: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

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
            let one_each: Vec<(&str, &[&str])> = repo_digests
                .iter()
                .map(|(img, rd)| (*img, std::slice::from_ref(rd)))
                .collect();
            Self::with_digests(containers, &one_each)
        }
        /// Images recorded under several manifest digests each — what a
        /// republished multi-arch index leaves behind (#74).
        fn with_digests(
            containers: Vec<ContainerSummary>,
            repo_digests: &[(&str, &[&str])],
        ) -> Self {
            Self {
                containers,
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

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
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

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(
            cells[0][5], "yes",
            "differing digests must report an update"
        );
    }

    #[tokio::test]
    async fn republished_index_already_pulled_renders_no() {
        // Issue #74: Docker appends an index digest per pull, so the oldest one
        // sits at RepoDigests[0] forever. Upstream's current digest is further
        // down the list — membership decides, not position.
        let docker = FakeDocker::with_digests(
            vec![summary(
                "caddy",
                "caddy:latest",
                &[("freshdock.enable", "true")],
            )],
            &[(
                "caddy:latest",
                &[&format!("caddy@{DIG_A}"), &format!("caddy@{DIG_B}")],
            )],
        );
        let registry = FakeRegistry::new(DIG_B);

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(cells[0][5], "no", "the upstream digest is already local");
        assert_eq!(
            cells[0][3],
            short_digest(DIG_B),
            "the current digest must be the one upstream resolves to, not the oldest recorded"
        );
    }

    #[tokio::test]
    async fn stale_local_image_still_renders_yes() {
        // The guard on the fix: several local digests, none of them upstream's.
        let docker = FakeDocker::with_digests(
            vec![summary(
                "caddy",
                "caddy:latest",
                &[("freshdock.enable", "true")],
            )],
            &[(
                "caddy:latest",
                &[&format!("caddy@{DIG_A}"), &format!("caddy@{DIG_B}")],
            )],
        );
        let registry = FakeRegistry::new(DIG_C);

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(cells[0][5], "yes");
        assert_eq!(
            cells[0][3],
            short_digest(DIG_A),
            "with no match the first recorded digest is still what we show"
        );
    }

    #[tokio::test]
    async fn locally_built_image_with_no_repo_digests_renders_unknown() {
        // Nothing to compare against: report `?`, never `yes` — recreating
        // would replace a local build with an unrelated registry image.
        let docker = FakeDocker::with_digests(
            vec![summary("app", "myapp:dev", &[("freshdock.enable", "true")])],
            &[("myapp:dev", &[])],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(cells[0][3], "-", "no current digest is known");
        assert_eq!(cells[0][5], "?");
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

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(cells[0][4], AUTH_REQUIRED);
        assert_eq!(cells[0][5], "-");
        assert_eq!(
            registry.calls.load(Ordering::SeqCst),
            1,
            "the image is probed now (no more Phase-5 short-circuit)"
        );
    }

    #[test]
    fn credentials_rejected_renders_distinct_status() {
        // A rejected token (private image) must read differently from "no creds"
        // so the operator rotates rather than sets a credential.
        let (current, latest, update) =
            render_cells("alpine:3.19", &ProbeOutcome::CredentialsRejected);
        assert_eq!(latest, CREDENTIALS_REJECTED);
        assert_ne!(latest, AUTH_REQUIRED);
        assert_eq!(current, "-");
        assert_eq!(update, "-");
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

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
        assert_eq!(cells.len(), 1, "only the opted-in container gets a row");
        assert_eq!(cells[0][0], "on");
    }

    #[tokio::test]
    async fn global_default_mode_applies_when_container_omits_mode_label() {
        // enable=true with no freshdock.mode: the [settings] default_mode wins
        // over the built-in `watch` fallback.
        let docker = FakeDocker::new(
            vec![summary(
                "web",
                "alpine:3.19",
                &[("freshdock.enable", "true")],
            )],
            &[("alpine:3.19", &format!("alpine@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(
            &docker,
            &registry,
            PolicyDefaults {
                mode: Some(Mode::Live),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            cells[0][2], "live",
            "the global default_mode applies when no freshdock.mode label is set"
        );
    }

    // --- watch_all opt-out mode + self guard (issue #79) ---

    const SELF_ID: &str = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";
    const SELF_PREFIX: &str = "abc123def456";

    /// Defaults with the opt-out mode switched on.
    fn watch_all() -> PolicyDefaults {
        PolicyDefaults {
            watch_all: true,
            ..Default::default()
        }
    }

    /// Give a summary a fixed container id, for the self-guard tests.
    fn with_id(summary: ContainerSummary, id: &str) -> ContainerSummary {
        ContainerSummary {
            id: Some(id.to_owned()),
            ..summary
        }
    }

    #[tokio::test]
    async fn watch_all_lists_unlabelled_container() {
        let docker = FakeDocker::new(
            vec![summary("web", "alpine:3.19", &[])],
            &[("alpine:3.19", &format!("alpine@{DIG_A}"))],
        );
        let registry = FakeRegistry::new(DIG_B);

        let cells = collect_cells(
            &docker,
            &registry,
            PolicyDefaults {
                mode: Some(Mode::Nightly),
                ..watch_all()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(cells.len(), 1, "an unlabelled container is listed");
        assert_eq!(cells[0][0], "web");
        assert_eq!(cells[0][2], "nightly", "the default mode fills the cell");
        assert_eq!(cells[0][5], "yes");
    }

    #[tokio::test]
    async fn watch_all_skips_own_container() {
        let docker = FakeDocker::new(
            vec![
                with_id(summary("freshdock", "alpine:3.19", &[]), SELF_ID),
                summary("web", "redis:7", &[]),
            ],
            &[
                ("alpine:3.19", &format!("alpine@{DIG_A}")),
                ("redis:7", &format!("redis@{DIG_A}")),
            ],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry, watch_all(), Some(SELF_PREFIX))
            .await
            .unwrap();
        assert_eq!(cells.len(), 1, "our own container gets no row");
        assert_eq!(cells[0][0], "web");
    }

    #[tokio::test]
    async fn watch_all_still_omits_enable_false() {
        let docker = FakeDocker::new(
            vec![
                summary("out", "alpine:3.19", &[("freshdock.enable", "false")]),
                summary(
                    "wt-out",
                    "redis:7",
                    &[("com.centurylinklabs.watchtower.enable", "false")],
                ),
                summary("in", "nginx:1.27", &[]),
            ],
            &[
                ("alpine:3.19", &format!("alpine@{DIG_A}")),
                ("redis:7", &format!("redis@{DIG_A}")),
                ("nginx:1.27", &format!("nginx@{DIG_A}")),
            ],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry, watch_all(), None)
            .await
            .unwrap();
        assert_eq!(cells.len(), 1, "both opt-out labels still exclude");
        assert_eq!(cells[0][0], "in");
    }

    #[tokio::test]
    async fn invalid_labels_on_one_container_do_not_break_the_table() {
        // A stray bad label on a bystander (visible at all only under
        // watch_all) must cost that container its row, not the whole table.
        let docker = FakeDocker::new(
            vec![
                summary("bad", "alpine:3.19", &[("freshdock.notify", "1")]),
                summary("web", "redis:7", &[]),
            ],
            &[
                ("alpine:3.19", &format!("alpine@{DIG_A}")),
                ("redis:7", &format!("redis@{DIG_A}")),
            ],
        );
        let registry = FakeRegistry::new(DIG_A);

        let cells = collect_cells(&docker, &registry, watch_all(), None)
            .await
            .unwrap();
        assert_eq!(cells.len(), 1, "the parse failure skips only that row");
        assert_eq!(cells[0][0], "web");
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

        let cells = collect_cells(&docker, &registry, PolicyDefaults::default(), None)
            .await
            .unwrap();
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
