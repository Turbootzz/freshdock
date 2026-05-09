use async_trait::async_trait;

use super::DockerError;
use super::spec::ContainerSpec;
use crate::registry::ImageRef;
use crate::updater::RecreateOutcome;

/// Daemon operations the recreate orchestrator depends on. Abstracted as a
/// trait so unit tests can substitute a recording fake without spinning up
/// a real Docker socket.
#[async_trait]
pub trait DockerOps {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError>;
    async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError>;
    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError>;
    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError>;
    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError>;
    async fn start(&self, name_or_id: &str) -> Result<(), DockerError>;
}

/// Drive one container through the recreate cycle:
/// `inspect → pull → stop → rename → create → start`.
///
/// Health gating, removal of the `-old-` container, and rollback on failure
/// are explicitly **out of scope** for Phase 2 (per [docs/PLAN.md] §7) — they
/// land in Phase 3.
pub async fn recreate_one(
    ops: &impl DockerOps,
    name: &str,
    ts_provider: impl Fn() -> i64,
) -> Result<RecreateOutcome, DockerError> {
    let spec = ops.inspect(name).await?;
    let image_ref = ImageRef::parse(&spec.image_ref);
    // `ImageRef::parse` adds a `library/` prefix for single-component Hub
    // refs — correct for the registry API path (`/v2/library/nginx/…`),
    // wrong for `Config.Image`. Pass `&spec.image_ref` (the original string
    // captured from inspect) into `create_from_spec` so the new container's
    // `Config.Image` round-trips byte-identical (#25).
    ops.pull(&image_ref).await?;
    ops.stop(
        name,
        spec.config.stop_signal.as_deref(),
        spec.config.stop_timeout,
    )
    .await?;
    let old_name = ops.rename(name, ts_provider()).await?;
    let new_id = ops.create_from_spec(name, &spec, &spec.image_ref).await?;
    ops.start(&new_id).await?;
    Ok(RecreateOutcome::Recreated { old_name, new_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// One recorded `DockerOps` call with the args it received. Lets tests
    /// assert *what* the orchestrator passed down (e.g. that `create_from_spec`
    /// got the original image ref, not pull's normalised rendering — see #25)
    /// in addition to the call ordering.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedCall {
        Inspect {
            name: String,
        },
        Pull {
            repository: String,
            tag: String,
        },
        Stop {
            name: String,
            signal: Option<String>,
            timeout_s: Option<i64>,
        },
        Rename {
            name: String,
            ts_unix: i64,
        },
        CreateFromSpec {
            name: String,
            image: String,
        },
        Start {
            name_or_id: String,
        },
    }

    /// Recording fake that captures both the order of `DockerOps` calls and
    /// the arguments each one received.
    struct RecordingOps {
        image_ref: String,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl RecordingOps {
        fn with_image_ref(image_ref: &str) -> Self {
            Self {
                image_ref: image_ref.to_owned(),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Project the recorded call sequence down to its labels — the same
        /// shape the canonical-order test asserted before #25 added arg
        /// capture.
        fn labels(&self) -> Vec<&'static str> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| match c {
                    RecordedCall::Inspect { .. } => "inspect",
                    RecordedCall::Pull { .. } => "pull",
                    RecordedCall::Stop { .. } => "stop",
                    RecordedCall::Rename { .. } => "rename",
                    RecordedCall::CreateFromSpec { .. } => "create",
                    RecordedCall::Start { .. } => "start",
                })
                .collect()
        }

        /// The `image` argument(s) that reached `create_from_spec`. The #25
        /// regression tests pin this to the *original* `spec.image_ref`, not
        /// the parsed/normalised string returned by `pull`.
        fn create_image_args(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    RecordedCall::CreateFromSpec { image, .. } => Some(image.clone()),
                    _ => None,
                })
                .collect()
        }

        /// The `(repository, tag)` pair(s) that reached `pull`. The companion
        /// to `create_image_args`: pull receives the *normalised* `ImageRef`
        /// (e.g. `library/nginx`, `alpine`) because that's the registry API
        /// path; `create_from_spec` receives the *original* unparsed string.
        /// Pinning both sides documents the trait split.
        fn pull_args(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    RecordedCall::Pull { repository, tag } => {
                        Some((repository.clone(), tag.clone()))
                    }
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl DockerOps for RecordingOps {
        async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
            self.calls.lock().unwrap().push(RecordedCall::Inspect {
                name: name.to_owned(),
            });
            Ok(ContainerSpec {
                name: name.to_owned(),
                image_ref: self.image_ref.clone(),
                config: bollard::models::ContainerConfig::default(),
                host_config: None,
                network_endpoints: None,
            })
        }

        async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
            self.calls.lock().unwrap().push(RecordedCall::Pull {
                repository: image_ref.repository.clone(),
                tag: image_ref.tag.clone(),
            });
            Ok(())
        }

        async fn stop(
            &self,
            name: &str,
            signal: Option<&str>,
            timeout_s: Option<i64>,
        ) -> Result<(), DockerError> {
            self.calls.lock().unwrap().push(RecordedCall::Stop {
                name: name.to_owned(),
                signal: signal.map(str::to_owned),
                timeout_s,
            });
            Ok(())
        }

        async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
            self.calls.lock().unwrap().push(RecordedCall::Rename {
                name: name.to_owned(),
                ts_unix,
            });
            Ok("fd-smoke-old-1700000000".to_owned())
        }

        async fn create_from_spec(
            &self,
            name: &str,
            _spec: &ContainerSpec,
            image: &str,
        ) -> Result<String, DockerError> {
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::CreateFromSpec {
                    name: name.to_owned(),
                    image: image.to_owned(),
                });
            Ok("new-id".to_owned())
        }

        async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
            self.calls.lock().unwrap().push(RecordedCall::Start {
                name_or_id: name_or_id.to_owned(),
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn recreate_one_visits_steps_in_canonical_order() {
        let ops = RecordingOps::with_image_ref("nginx:alpine");
        let outcome = recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            outcome,
            RecreateOutcome::Recreated {
                old_name: "fd-smoke-old-1700000000".to_owned(),
                new_id: "new-id".to_owned(),
            }
        );
        assert_eq!(
            ops.labels(),
            vec!["inspect", "pull", "stop", "rename", "create", "start"],
            "the orchestrator must drive operations in this exact order — \
             reordering breaks the safety contract (e.g. starting before \
             rename would race the old container)"
        );
    }

    /// Regression test for #25. Single-component Docker Hub refs like
    /// `nginx:alpine` must reach `create_from_spec` unchanged — `ImageRef`'s
    /// `library/` prefix is for the registry API path, not the container's
    /// `Config.Image`. Without this, the recreated container's `Config.Image`
    /// drifts to `library/nginx:alpine`.
    #[tokio::test]
    async fn recreate_one_passes_original_hub_image_ref_to_create() {
        let ops = RecordingOps::with_image_ref("nginx:alpine");
        recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            ops.create_image_args(),
            vec!["nginx:alpine".to_owned()],
            "create_from_spec must receive the original spec.image_ref \
             (`nginx:alpine`), not pull's normalised rendering \
             (`library/nginx:alpine`) — see #25"
        );
    }

    /// Companion to the Hub regression test: pins that non-Hub refs (which
    /// `ImageRef::parse` already passes through) also reach `create_from_spec`
    /// byte-identical. Guards against a future refactor that re-derives the
    /// image string from `ImageRef` for *all* refs.
    #[tokio::test]
    async fn recreate_one_passes_original_non_hub_image_ref_to_create() {
        let ops = RecordingOps::with_image_ref("ghcr.io/owner/repo:v1");
        recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            ops.create_image_args(),
            vec!["ghcr.io/owner/repo:v1".to_owned()],
        );
    }

    /// The other half of the trait split: `pull` must receive the *normalised*
    /// `ImageRef` so its registry HEAD lands at `/v2/library/nginx/...`.
    /// Without this assertion, a future refactor could collapse the split
    /// (e.g. by passing `&spec.image_ref` to `pull` too) and silently break
    /// anonymous Docker Hub pulls for single-component refs.
    #[tokio::test]
    async fn recreate_one_passes_normalised_image_ref_to_pull() {
        let ops = RecordingOps::with_image_ref("nginx:alpine");
        recreate_one(&ops, "fd-smoke", || 1_700_000_000)
            .await
            .expect("recording fake never errors");

        assert_eq!(
            ops.pull_args(),
            vec![("library/nginx".to_owned(), "alpine".to_owned())],
            "pull must receive the ImageRef-parsed form (`library/nginx`, \
             `alpine`) — the registry API path needs the `library/` prefix \
             even though Config.Image must not"
        );
    }
}
