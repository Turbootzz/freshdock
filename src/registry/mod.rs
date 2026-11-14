pub mod auth;
pub mod digest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub repository: String,
    pub tag: String,
}

impl ImageRef {
    /// Parse a `name[:tag]` reference.
    ///
    /// Single-component refs (`alpine`) are namespaced as `library/alpine`,
    /// matching Docker Hub's convention for official images. Multi-component
    /// refs (`nginxinc/nginx-unprivileged`) pass through unchanged. A leading Hub
    /// host is dropped, and the default tag is `latest`.
    pub fn parse(input: &str) -> Self {
        let (name, tag) = match input.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, t),
            _ => (input, "latest"),
        };
        let name = strip_docker_hub_host(name);
        let repository = if name.contains('/') {
            name.to_owned()
        } else {
            format!("library/{name}")
        };
        Self {
            repository,
            tag: tag.to_owned(),
        }
    }
}

/// The two hosts Docker folds to Hub. Narrower than `config::canonicalize_host`.
pub(crate) fn is_docker_hub_host(host: &str) -> bool {
    host == "docker.io" || host == "index.docker.io"
}

/// Drops a leading Hub host, unless the rest is a registry of its own.
fn strip_docker_hub_host(name: &str) -> &str {
    let (host, rest) = digest::split_repository(name);
    let rest_stays_on_hub =
        !rest.is_empty() && is_docker_hub_host(digest::split_repository(rest).0);
    if is_docker_hub_host(host) && rest_stays_on_hub {
        rest
    } else {
        name
    }
}

/// Docker's familiar repository name: no Hub host, and no bare `library/`.
pub(crate) fn familiar_repository(repository: &str) -> &str {
    let (host, path) = digest::split_repository(repository);
    if is_docker_hub_host(host) {
        path.strip_prefix("library/")
            .filter(|rest| !rest.contains('/'))
            .unwrap_or(path)
    } else {
        repository
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("network unavailable: {0}")]
    NetworkUnavailable(String),
    #[error("registry HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Configured credentials were rejected by the token endpoint (401/403) and
    /// the anonymous fallback was also denied — i.e. the image is genuinely
    /// private *and* the token is wrong/stale. Distinct from [`Auth`] ("no
    /// credentials configured") so the operator knows to rotate, not to set, a
    /// token. When the anonymous fallback succeeds, no error is returned at all;
    /// the rejection surfaces as a `warn!` and the digest flows through.
    #[error("configured credentials rejected for {0}")]
    CredentialsRejected(String),
    #[error("manifest digest header missing or unparseable")]
    MissingDigest,
    #[error("invalid endpoint url: {0}")]
    InvalidEndpoint(String),
}

#[async_trait::async_trait]
pub trait Registry: Send + Sync {
    async fn fetch_digest(&self, image: &ImageRef) -> Result<Digest, RegistryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_component_ref_gets_library_prefix() {
        let r = ImageRef::parse("alpine");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn single_component_ref_with_tag() {
        let r = ImageRef::parse("alpine:3.20");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "3.20");
    }

    #[test]
    fn multi_component_ref_passes_through() {
        let r = ImageRef::parse("nginxinc/nginx-unprivileged:latest");
        assert_eq!(r.repository, "nginxinc/nginx-unprivileged");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn multi_component_ref_without_tag_defaults_to_latest() {
        let r = ImageRef::parse("nginxinc/nginx-unprivileged");
        assert_eq!(r.repository, "nginxinc/nginx-unprivileged");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn docker_io_library_prefix_is_normalised() {
        let r = ImageRef::parse("docker.io/library/nginx:alpine");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "alpine");
    }

    #[test]
    fn docker_io_single_component_gets_library_prefix() {
        let r = ImageRef::parse("docker.io/nginx:alpine");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "alpine");
    }

    #[test]
    fn index_docker_io_prefix_is_normalised() {
        let r = ImageRef::parse("index.docker.io/library/nginx");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn registry_1_docker_io_user_repo_keeps_its_namespace() {
        // Folding it would probe `nginxinc/...`, not the tag the container runs.
        let r = ImageRef::parse("registry-1.docker.io/nginxinc/nginx-unprivileged:1.27");
        assert_eq!(
            r.repository,
            "registry-1.docker.io/nginxinc/nginx-unprivileged"
        );
        assert_eq!(r.tag, "1.27");
    }

    #[test]
    fn hub_host_is_kept_when_the_remainder_is_not_a_hub_path() {
        let r = ImageRef::parse("docker.io/localhost/foo:1");
        assert_eq!(r.repository, "docker.io/localhost/foo");
        assert_eq!(r.tag, "1");
    }

    #[test]
    fn docker_namespace_is_not_mistaken_for_the_hub_host() {
        let r = ImageRef::parse("docker/welcome-to-docker");
        assert_eq!(r.repository, "docker/welcome-to-docker");
    }

    #[test]
    fn familiar_repository_drops_hub_host_and_library() {
        assert_eq!(familiar_repository("docker.io/library/nginx"), "nginx");
        assert_eq!(
            familiar_repository("index.docker.io/library/nginx"),
            "nginx"
        );
        assert_eq!(familiar_repository("library/nginx"), "nginx");
        assert_eq!(familiar_repository("nginx"), "nginx");
        assert_eq!(familiar_repository("docker.io/nginxinc/x"), "nginxinc/x");
        assert_eq!(familiar_repository("nginxinc/x"), "nginxinc/x");
        assert_eq!(
            familiar_repository("docker.io/library/foo/bar"),
            "library/foo/bar",
            "library/ is a namespace only when nothing follows it"
        );
        assert_eq!(
            familiar_repository("registry-1.docker.io/library/nginx"),
            "registry-1.docker.io/library/nginx"
        );
        assert_eq!(
            familiar_repository("ghcr.io/astral-sh/uv"),
            "ghcr.io/astral-sh/uv"
        );
        assert_eq!(
            familiar_repository("localhost:5000/library/x"),
            "localhost:5000/library/x"
        );
    }

    #[test]
    fn registry_host_in_ref_is_kept_as_part_of_repository() {
        // We don't yet support non-Docker-Hub registries, but ensure the
        // parser doesn't mangle a `host:port/repo` shape — the colon is
        // followed by a path, not a tag.
        let r = ImageRef::parse("ghcr.io/owner/image");
        assert_eq!(r.repository, "ghcr.io/owner/image");
        assert_eq!(r.tag, "latest");
    }
}
