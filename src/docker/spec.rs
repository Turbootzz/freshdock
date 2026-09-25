use std::collections::HashMap;

use bollard::models::{
    ContainerConfig, ContainerCreateBody, ContainerInspectResponse, EndpointSettings, HostConfig,
    NetworkingConfig,
};
use bollard::query_parameters::{CreateContainerOptions, CreateContainerOptionsBuilder};

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("inspect response is missing the {0} field")]
    Missing(&'static str),
}

/// Everything needed to re-create a container, captured by one inspect.
///
/// The fields every backend shares sit on the struct; the runtime's own
/// settings sit in [`body`](Self::body). Shared code (the recreate cycle,
/// rollouts, the `recreate` command) reads the body only through the accessor
/// methods below, so it never depends on one daemon's types.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image_ref: String,
    /// Resolved local image **ID** (`ContainerInspectResponse.image`) the
    /// container was created from — the handle image cleanup removes. Captured
    /// at the pre-pull inspect so it is the *superseded* image even if the pull
    /// retags an unchanged `:latest`. `None` for a container with no resolved
    /// image (cleanup then no-ops).
    pub image_id: Option<String>,
    pub body: SpecBody,
}

/// The runtime-specific part of a [`ContainerSpec`].
#[derive(Debug, Clone)]
pub enum SpecBody {
    Docker(DockerSpec),
}

/// A container's settings as the Docker API reports them, replayed verbatim
/// into the create call.
#[derive(Debug, Clone, Default)]
pub struct DockerSpec {
    pub config: ContainerConfig,
    pub host_config: Option<HostConfig>,
    pub network_endpoints: Option<HashMap<String, EndpointSettings>>,
}

impl ContainerSpec {
    /// The Docker-API settings.
    pub fn docker(&self) -> &DockerSpec {
        match &self.body {
            SpecBody::Docker(d) => d,
        }
    }

    pub fn docker_mut(&mut self) -> &mut DockerSpec {
        match &mut self.body {
            SpecBody::Docker(d) => d,
        }
    }

    pub fn labels(&self) -> Option<&HashMap<String, String>> {
        self.docker().config.labels.as_ref()
    }

    /// The signal a stop sends first; `None` means the runtime's default.
    pub fn stop_signal(&self) -> Option<&str> {
        self.docker().config.stop_signal.as_deref()
    }

    /// Seconds a stop waits before killing; `None` means the runtime's default.
    pub fn stop_timeout(&self) -> Option<i64> {
        self.docker().config.stop_timeout
    }

    /// `HostConfig.NetworkMode`: `bridge`, a network name, `container:<ref>`, …
    pub fn network_mode(&self) -> Option<&str> {
        self.docker()
            .host_config
            .as_ref()
            .and_then(|hc| hc.network_mode.as_deref())
    }

    pub fn set_network_mode(&mut self, mode: String) {
        self.docker_mut()
            .host_config
            .get_or_insert_with(HostConfig::default)
            .network_mode = Some(mode);
    }

    /// Forget the settings that belong to a container's *own* network stack:
    /// hostname, domain name, exposed ports and network endpoints. For a
    /// container that borrows another's network namespace, which has none.
    pub fn drop_own_network_identity(&mut self) {
        let docker = self.docker_mut();
        docker.config.hostname = None;
        docker.config.domainname = None;
        docker.config.exposed_ports = None;
        docker.network_endpoints = None;
    }

    pub fn from_inspect(resp: ContainerInspectResponse) -> Result<Self, SpecError> {
        let name = resp
            .name
            .as_deref()
            .ok_or(SpecError::Missing("Name"))?
            .trim_start_matches('/')
            .to_owned();

        let mut config = resp.config.ok_or(SpecError::Missing("Config"))?;
        let image_ref = config
            .image
            .clone()
            .ok_or(SpecError::Missing("Config.Image"))?;

        if let (Some(id), Some(host)) = (resp.id.as_deref(), config.hostname.as_deref())
            && is_daemon_assigned_hostname(id, host)
        {
            config.hostname = None;
        }

        let network_endpoints = resp.network_settings.and_then(|ns| ns.networks);

        Ok(Self {
            name,
            image_ref,
            image_id: resp.image,
            body: SpecBody::Docker(DockerSpec {
                config,
                host_config: resp.host_config,
                network_endpoints,
            }),
        })
    }

    pub fn to_create_body(&self, new_image: &str) -> ContainerCreateBody {
        let docker = self.docker();
        let cfg = &docker.config;
        ContainerCreateBody {
            hostname: cfg.hostname.clone(),
            domainname: cfg.domainname.clone(),
            user: cfg.user.clone(),
            attach_stdin: cfg.attach_stdin,
            attach_stdout: cfg.attach_stdout,
            attach_stderr: cfg.attach_stderr,
            exposed_ports: cfg.exposed_ports.clone(),
            tty: cfg.tty,
            open_stdin: cfg.open_stdin,
            stdin_once: cfg.stdin_once,
            env: cfg.env.clone(),
            cmd: cfg.cmd.clone(),
            healthcheck: cfg.healthcheck.clone(),
            args_escaped: cfg.args_escaped,
            image: Some(new_image.to_owned()),
            volumes: cfg.volumes.clone(),
            working_dir: cfg.working_dir.clone(),
            entrypoint: cfg.entrypoint.clone(),
            network_disabled: cfg.network_disabled,
            on_build: cfg.on_build.clone(),
            labels: cfg.labels.clone(),
            stop_signal: cfg.stop_signal.clone(),
            stop_timeout: cfg.stop_timeout,
            shell: cfg.shell.clone(),
            host_config: docker.host_config.clone(),
            networking_config: docker.network_endpoints.clone().map(|e| NetworkingConfig {
                endpoints_config: Some(e),
            }),
        }
    }

    pub fn to_create_options(&self) -> CreateContainerOptions {
        CreateContainerOptionsBuilder::new()
            .name(&self.name)
            .build()
    }
}

/// Docker names a container after its own first 12 id characters when no
/// hostname is given; such a name belongs to the container, not to the spec.
pub(crate) fn is_daemon_assigned_hostname(container_id: &str, hostname: &str) -> bool {
    hostname.len() == 12 && container_id.len() >= 12 && container_id.starts_with(hostname)
}

/// Splits a label map into the freshdock-managed subset (keys with the
/// `freshdock.` prefix) and the user-owned subset (everything else).
///
/// Phase 3 rollback uses this to strip lifecycle labels added by us before
/// re-attaching user labels to the recreated container.
pub fn split_labels(
    labels: &HashMap<String, String>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut managed = HashMap::new();
    let mut user = HashMap::new();
    for (k, v) in labels {
        if k.starts_with("freshdock.") {
            managed.insert(k.clone(), v.clone());
        } else {
            user.insert(k.clone(), v.clone());
        }
    }
    (managed, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_labels_partitions_freshdock_prefix_from_user_labels() {
        let labels: HashMap<String, String> = [
            ("freshdock.enable", "true"),
            ("freshdock.mode", "watch"),
            ("app", "demo"),
            ("team", "infra"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();

        let (managed, user) = split_labels(&labels);

        assert_eq!(managed.len(), 2);
        assert_eq!(
            managed.get("freshdock.enable").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            managed.get("freshdock.mode").map(String::as_str),
            Some("watch")
        );
        assert_eq!(user.len(), 2);
        assert_eq!(user.get("app").map(String::as_str), Some("demo"));
        assert_eq!(user.get("team").map(String::as_str), Some("infra"));
        assert!(
            !user.contains_key("freshdock.enable"),
            "managed labels must not leak into the user partition"
        );
    }

    #[test]
    fn split_labels_handles_empty_input() {
        let (managed, user) = split_labels(&HashMap::new());
        assert!(managed.is_empty());
        assert!(user.is_empty());
    }

    fn inspect_with(id: &str, hostname: Option<&str>) -> ContainerInspectResponse {
        ContainerInspectResponse {
            id: Some(id.to_owned()),
            name: Some("/web".to_owned()),
            config: Some(ContainerConfig {
                image: Some("nginx:alpine".to_owned()),
                hostname: hostname.map(str::to_owned),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const ID: &str = "41b589f2924cf5b22571430db5ab422ebd200b0f4f4113511eac9fbac0e41d69";

    #[test]
    fn daemon_assigned_hostname_is_dropped() {
        let spec = ContainerSpec::from_inspect(inspect_with(ID, Some("41b589f2924c"))).unwrap();
        assert_eq!(spec.docker().config.hostname, None);
        assert_eq!(spec.to_create_body("nginx:alpine").hostname, None);
    }

    #[test]
    fn explicit_hostname_round_trips() {
        let spec = ContainerSpec::from_inspect(inspect_with(ID, Some("web"))).unwrap();
        assert_eq!(spec.docker().config.hostname.as_deref(), Some("web"));
    }

    #[test]
    fn id_shaped_hostname_of_another_container_is_kept() {
        let spec = ContainerSpec::from_inspect(inspect_with(ID, Some("deadbeef1234"))).unwrap();
        assert_eq!(
            spec.docker().config.hostname.as_deref(),
            Some("deadbeef1234")
        );
    }

    #[test]
    fn is_daemon_assigned_hostname_needs_the_twelve_char_prefix() {
        assert!(is_daemon_assigned_hostname(ID, "41b589f2924c"));
        assert!(!is_daemon_assigned_hostname(ID, "41b589f2924"));
        assert!(!is_daemon_assigned_hostname(ID, "41b589f2924cf5"));
        assert!(!is_daemon_assigned_hostname("", "41b589f2924c"));
    }
}
