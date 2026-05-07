use std::collections::HashMap;

use comfy_table::Table;
use comfy_table::presets::{NOTHING, UTF8_FULL};
use futures::future::join_all;
use tracing::warn;

use crate::docker::Docker;
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
pub async fn run(no_color: bool) -> Result<(), AppError> {
    let docker = Docker::connect()?;
    let containers = docker.list_running().await?;
    let hub = DockerHub::new();

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
        // ContainerSummary.image_id is the image *config* digest, not the
        // *manifest* digest the registry returns via Docker-Content-Digest.
        // Comparing them is meaningless. Phase 2 (P2-1) will resolve the
        // local manifest digest via image inspect → RepoDigests.
        let local_digest: Option<String> = None;

        rows.push(RowPrep {
            name,
            image: image_str,
            local_digest,
            mode: policy.mode,
        });
    }

    // Fetch digests once per unique image reference. A homelab compose
    // stack often has several containers sharing the same image; firing
    // duplicate token+HEAD requests would burn the Docker Hub anonymous
    // rate budget (100 / 6h) for nothing.
    let unique = unique_images(&rows);
    let hub_ref = &hub;
    let fetches = unique.into_iter().map(|img| async move {
        let outcome = fetch_for(hub_ref, &img).await;
        (img, outcome)
    });
    let by_image: HashMap<String, FetchOutcome> = join_all(fetches).await.into_iter().collect();

    let mut table = build_table(no_color);
    for row in rows.into_iter() {
        let local = row
            .local_digest
            .as_deref()
            .map(short_digest)
            .unwrap_or_else(|| "-".to_string());
        let outcome = by_image
            .get(&row.image)
            .cloned()
            .unwrap_or(FetchOutcome::Error("internal: missing fetch result".into()));
        let (latest_cell, update_cell) = match outcome {
            FetchOutcome::Found(d) => {
                let update = row
                    .local_digest
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
        table.add_row(vec![
            row.name,
            row.image,
            row.mode.to_string(),
            local,
            latest_cell,
            update_cell,
        ]);
    }

    println!("{table}");
    Ok(())
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
    local_digest: Option<String>,
    mode: Mode,
}

#[derive(Clone)]
enum FetchOutcome {
    Found(Digest),
    SkippedAuth,
    NetworkUnavailable,
    Error(String),
}

async fn fetch_for(hub: &DockerHub, image: &str) -> FetchOutcome {
    let image_ref = ImageRef::parse(image);
    if !is_docker_hub(&image_ref.repository) {
        return FetchOutcome::SkippedAuth;
    }
    match hub.fetch_digest(&image_ref).await {
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
            local_digest: None,
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
}
