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
        let local_digest = c.image_id;

        rows.push(RowPrep {
            name,
            image: image_str,
            local_digest,
            mode: policy.mode,
        });
    }

    let fetches = rows.iter().map(|r| fetch_for(&hub, &r.image));
    let fetched = join_all(fetches).await;

    let mut table = build_table(no_color);
    for (row, latest) in rows.into_iter().zip(fetched.into_iter()) {
        let local = row
            .local_digest
            .as_deref()
            .map(short_digest)
            .unwrap_or_else(|| "-".to_string());
        let (latest_cell, update_cell) = match latest {
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
            format!("{:?}", row.mode).to_lowercase(),
            local,
            latest_cell,
            update_cell,
        ]);
    }

    println!("{table}");
    Ok(())
}

struct RowPrep {
    name: String,
    image: String,
    local_digest: Option<String>,
    mode: Mode,
}

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
/// Anything containing a host (`ghcr.io/...`, `quay.io/...`, `lscr.io/...`)
/// belongs to a registry that needs Phase 5's bearer-token auth path.
fn is_docker_hub(repository: &str) -> bool {
    let first = repository.split('/').next().unwrap_or("");
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
    fn short_digest_truncates_sha256() {
        let d = "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(short_digest(d), "sha256:abcdef012345…");
    }

    #[test]
    fn short_digest_passes_through_non_sha() {
        assert_eq!(short_digest("alpine:latest"), "alpine:latest");
    }
}
