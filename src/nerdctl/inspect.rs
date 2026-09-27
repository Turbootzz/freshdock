//! nerdctl's JSON output, and what freshdock reads from it.
//!
//! These types deserialize only the fields freshdock uses, and every one is
//! optional, so a field nerdctl adds, drops or leaves `null` in a later release
//! cannot break parsing. bollard's Docker types are deliberately not reused:
//! nerdctl's "Docker-compatible" output differs in the details (for example the
//! top-level `Image` is a reference, not an id).

use std::collections::HashMap;

use serde::Deserialize;

use crate::docker::DockerError;
use crate::docker::check::LocalImage;
use crate::docker::container::{ContainerInfo, ListedState};
use crate::health::ContainerRuntimeState;

/// One element of `nerdctl inspect` (the default, Docker-compatible mode).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerInspect {
    pub id: Option<String>,
    /// No leading `/`, unlike Docker.
    pub name: Option<String>,
    /// The image *reference* the container was created from.
    pub image: Option<String>,
    pub state: Option<State>,
    pub config: Option<Config>,
    pub host_config: Option<HostConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct State {
    pub status: Option<String>,
    pub running: Option<bool>,
    pub paused: Option<bool>,
    pub restarting: Option<bool>,
    pub exit_code: Option<i64>,
    pub health: Option<Health>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Health {
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Config {
    pub image: Option<String>,
    pub labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HostConfig {
    pub network_mode: Option<String>,
}

/// One element of `nerdctl image inspect`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInspect {
    pub id: Option<String>,
    pub repo_digests: Option<Vec<String>>,
}

/// `nerdctl version --format '{{json .}}'`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Version {
    pub client: Option<Component>,
    /// `null` when nerdctl could not reach containerd.
    pub server: Option<Server>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Server {
    pub components: Option<Vec<Component>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Component {
    pub name: Option<String>,
    pub version: Option<String>,
}

impl Version {
    pub fn client_version(&self) -> Option<&str> {
        self.client.as_ref()?.version.as_deref()
    }

    pub fn containerd_version(&self) -> Option<&str> {
        self.server
            .as_ref()?
            .components
            .as_ref()?
            .iter()
            .find(|c| c.name.as_deref() == Some("containerd"))?
            .version
            .as_deref()
    }
}

impl ContainerInspect {
    fn is_running(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.status.as_deref() == Some("running") || s.running == Some(true))
    }

    /// The reference the container was created from: `Config.Image`, else the
    /// top-level `Image` (nerdctl fills both with the same reference).
    pub fn image_ref(&self) -> Option<&str> {
        self.config
            .as_ref()
            .and_then(|c| c.image.as_deref())
            .or(self.image.as_deref())
    }

    /// The listing row for this container.
    ///
    /// `image_id` is always `None`: containerd records a container's image by
    /// reference only, so once the tag moves there is no id to report. The
    /// update check then compares by digest alone.
    pub fn to_info(&self) -> ContainerInfo {
        let state = self.state.clone().unwrap_or_default();
        ContainerInfo {
            id: self.id.clone(),
            names: self.name.clone().map(|n| vec![n]),
            image: self.image_ref().map(str::to_owned),
            image_id: None,
            labels: self.config.as_ref().and_then(|c| c.labels.clone()),
            network_mode: self
                .host_config
                .as_ref()
                .and_then(|h| h.network_mode.clone()),
            state: if state.restarting == Some(true) {
                ListedState::Restarting
            } else if state.paused == Some(true) {
                ListedState::Paused
            } else if self.is_running() {
                ListedState::Running
            } else {
                ListedState::Other
            },
        }
    }

    /// What the health gate needs, following the Docker backend's rules: not
    /// running is `Exited`, and a missing or `none` health status means no
    /// healthcheck was declared.
    pub fn runtime_state(&self) -> ContainerRuntimeState {
        if !self.is_running() {
            let exit_code = self.state.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
            return ContainerRuntimeState::Exited { exit_code };
        }
        let health = self
            .state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status.as_deref());
        match health {
            Some("healthy") => ContainerRuntimeState::HealthHealthy,
            Some("unhealthy") => ContainerRuntimeState::HealthUnhealthy,
            Some("starting") => ContainerRuntimeState::HealthStarting,
            _ => ContainerRuntimeState::RunningNoHealthcheck,
        }
    }
}

impl From<ImageInspect> for LocalImage {
    fn from(img: ImageInspect) -> Self {
        LocalImage {
            id: img.id,
            repo_digests: img.repo_digests.unwrap_or_default(),
        }
    }
}

/// Parse a JSON array printed by nerdctl.
pub fn parse_array<T: for<'de> Deserialize<'de>>(
    what: &str,
    stdout: &str,
) -> Result<Vec<T>, DockerError> {
    serde_json::from_str(stdout)
        .map_err(|e| DockerError::Runtime(format!("could not parse nerdctl {what} output: {e}")))
}

/// The readable part of nerdctl's error output. nerdctl logs through logrus, so
/// a failure looks like `time="…" level=fatal msg="1 errors: [no such object x]"`;
/// this returns the `msg` value, unescaped, or the trimmed text when there is
/// no such line.
pub fn error_message(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("level=fatal") || l.contains("level=error"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()));
    let Some(line) = line else {
        return String::new();
    };
    let Some(start) = line.find("msg=\"") else {
        return line.trim().to_owned();
    };
    let mut msg = String::new();
    let mut chars = line[start + 5..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => msg.push('\n'),
                Some(other) => msg.push(other),
                None => break,
            },
            c => msg.push(c),
        }
    }
    msg
}

/// Classify a failed nerdctl call from its error message, so shared code can
/// tell "not found" and "conflict" apart from everything else.
pub fn classify_error(message: String) -> DockerError {
    let lower = message.to_ascii_lowercase();
    // nerdctl's own wording for a missing container or image only. A bare
    // "not found" is also what an exec of a missing binary ("executable file
    // not found in $PATH") or a registry ("manifest ... not found") says, and
    // neither means the container or image is gone.
    if lower.contains("no such object")
        || lower.contains("no such container")
        || lower.contains("no such image")
    {
        DockerError::NotFound(message)
    } else if lower.contains("conflict") || lower.contains("already used") {
        DockerError::Conflict(message)
    } else {
        DockerError::Runtime(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn inspect_fixture(name: &str) -> ContainerInspect {
        parse_array::<ContainerInspect>("inspect", &fixture(name))
            .unwrap()
            .remove(0)
    }

    #[test]
    fn a_full_container_becomes_a_listing_row() {
        let info = inspect_fixture("nerdctl_full_inspect.json").to_info();
        assert_eq!(
            info.id.as_deref(),
            Some("c5b06b49b252243d79987734a25a6ddc81a186ecbc4209f4035eec36d6231e67")
        );
        assert_eq!(info.names, Some(vec!["fd-full".to_owned()]));
        assert_eq!(
            info.image.as_deref(),
            Some("docker.io/library/nginx:1.27-alpine")
        );
        assert_eq!(info.image_id, None);
        let labels = info.labels.unwrap();
        assert_eq!(
            labels.get("freshdock.enable").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels.get("freshdock.mode").map(String::as_str),
            Some("nightly")
        );
        assert_eq!(info.network_mode.as_deref(), Some("fdtest-net"));
    }

    #[test]
    fn a_shared_namespace_container_reports_the_owner_by_full_id() {
        let info = inspect_fixture("nerdctl_netdep_inspect.json").to_info();
        assert_eq!(
            info.network_mode.as_deref(),
            Some("container:b196d3ba3a8a5e7d4b54a070f45d7522286de1aad1c1a875a478627a2aad79e8")
        );
        assert_eq!(info.state, ListedState::Running);
    }

    #[test]
    fn listing_state_follows_the_state_flags() {
        let with_state = |state: State| ContainerInspect {
            state: Some(state),
            ..Default::default()
        };
        let running = State {
            status: Some("running".to_owned()),
            running: Some(true),
            ..Default::default()
        };
        assert_eq!(
            with_state(running.clone()).to_info().state,
            ListedState::Running
        );
        let restarting = State {
            restarting: Some(true),
            ..running.clone()
        };
        assert_eq!(
            with_state(restarting).to_info().state,
            ListedState::Restarting
        );
        let paused = State {
            paused: Some(true),
            ..running
        };
        assert_eq!(with_state(paused).to_info().state, ListedState::Paused);
        let exited = State {
            status: Some("exited".to_owned()),
            running: Some(false),
            ..Default::default()
        };
        assert_eq!(with_state(exited).to_info().state, ListedState::Other);
    }

    #[test]
    fn runtime_state_maps_health_like_the_docker_backend() {
        let running_with = |health: Option<&str>| ContainerInspect {
            state: Some(State {
                status: Some("running".to_owned()),
                running: Some(true),
                health: health.map(|h| Health {
                    status: Some(h.to_owned()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            running_with(Some("healthy")).runtime_state(),
            ContainerRuntimeState::HealthHealthy
        );
        assert_eq!(
            running_with(Some("unhealthy")).runtime_state(),
            ContainerRuntimeState::HealthUnhealthy
        );
        assert_eq!(
            running_with(Some("starting")).runtime_state(),
            ContainerRuntimeState::HealthStarting
        );
        assert_eq!(
            running_with(None).runtime_state(),
            ContainerRuntimeState::RunningNoHealthcheck
        );
        assert_eq!(
            running_with(Some("none")).runtime_state(),
            ContainerRuntimeState::RunningNoHealthcheck
        );
    }

    #[test]
    fn a_stopped_container_is_exited_with_its_code() {
        let stopped = ContainerInspect {
            state: Some(State {
                status: Some("exited".to_owned()),
                running: Some(false),
                exit_code: Some(137),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            stopped.runtime_state(),
            ContainerRuntimeState::Exited { exit_code: 137 }
        );
        assert_eq!(
            ContainerInspect::default().runtime_state(),
            ContainerRuntimeState::Exited { exit_code: 0 }
        );
    }

    #[test]
    fn image_inspect_keeps_id_and_repo_digests() {
        let img: LocalImage =
            parse_array::<ImageInspect>("image inspect", &fixture("nerdctl_image_inspect.json"))
                .unwrap()
                .remove(0)
                .into();
        assert_eq!(
            img.id.as_deref(),
            Some("sha256:6769dc3a703c719c1d2756bda113659be28ae16cf0da58dd5fd823d6b9a050ea")
        );
        assert_eq!(
            img.repo_digests,
            ["nginx@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10"]
        );
    }

    #[test]
    fn version_reads_client_and_containerd() {
        let v: Version = serde_json::from_str(
            r#"{"Client":{"Version":"v2.3.4"},"Server":{"Components":[
                {"Name":"containerd","Version":"v2.3.2"},{"Name":"runc","Version":"1.5.0"}]}}"#,
        )
        .unwrap();
        assert_eq!(v.client_version(), Some("v2.3.4"));
        assert_eq!(v.containerd_version(), Some("v2.3.2"));
        let unreachable: Version =
            serde_json::from_str(r#"{"Client":{"Version":"v2.3.4"},"Server":null}"#).unwrap();
        assert_eq!(unreachable.containerd_version(), None);
    }

    #[test]
    fn unparseable_output_is_a_runtime_error() {
        let err = parse_array::<ImageInspect>("image inspect", "not json").unwrap_err();
        assert!(matches!(err, DockerError::Runtime(_)), "{err}");
    }

    #[test]
    fn error_message_extracts_the_logrus_msg() {
        assert_eq!(
            error_message(
                "time=\"2026-09-25T18:52:57Z\" level=fatal msg=\"1 errors: [no such object does-not-exist]\"\n"
            ),
            "1 errors: [no such object does-not-exist]"
        );
        assert_eq!(
            error_message(
                "time=\"t\" level=warning msg=\"noise\"\ntime=\"t\" level=fatal msg=\"1 errors:\\nno such container: x\"\n"
            ),
            "1 errors:\nno such container: x"
        );
        assert_eq!(error_message("plain failure\n\n"), "plain failure");
        assert_eq!(error_message(""), "");
    }

    #[test]
    fn other_not_found_errors_are_not_a_missing_object() {
        for message in [
            "OCI runtime exec failed: exec failed: unable to start container process: exec: \"sh\": executable file not found in $PATH",
            "failed to resolve reference \"docker.io/library/nope:1\": docker.io/library/nope:1: not found",
            "unknown logging driver \"\": not found",
        ] {
            let e = classify_error(message.to_owned());
            assert!(!e.is_not_found(), "{message}");
            assert!(matches!(e, DockerError::Runtime(_)), "{message}");
        }
    }

    #[test]
    fn captured_error_text_classifies() {
        // Captured from nerdctl 2.3.4 on the testbed.
        assert!(
            classify_error("1 errors: [no such object does-not-exist]".to_owned()).is_not_found()
        );
        assert!(
            classify_error("1 errors:\nno such container: does-not-exist".to_owned())
                .is_not_found()
        );
        assert!(
            classify_error(
                "1 errors:\nconflict: unable to delete docker.io/library/nginx:1.27-alpine \
                 (cannot be forced) - image is being used by running container b196d3ba3a8a"
                    .to_owned()
            )
            .is_conflict()
        );
        assert!(
            classify_error(
                "name-store error\nname \"fd-plain\" is already used by ID \"b196d3ba3a8a\""
                    .to_owned()
            )
            .is_conflict()
        );
        let other = classify_error("failed to dial containerd".to_owned());
        assert!(matches!(other, DockerError::Runtime(_)), "{other}");
    }
}
