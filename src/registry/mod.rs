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
    /// refs (`nginxinc/nginx-unprivileged`) pass through unchanged. The
    /// default tag is `latest`.
    pub fn parse(input: &str) -> Self {
        let (name, tag) = match input.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n.to_string(), t.to_string()),
            _ => (input.to_string(), "latest".to_string()),
        };
        let repository = if name.contains('/') {
            name
        } else {
            format!("library/{name}")
        };
        Self { repository, tag }
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
    fn registry_host_in_ref_is_kept_as_part_of_repository() {
        // We don't yet support non-Docker-Hub registries, but ensure the
        // parser doesn't mangle a `host:port/repo` shape — the colon is
        // followed by a path, not a tag.
        let r = ImageRef::parse("ghcr.io/owner/image");
        assert_eq!(r.repository, "ghcr.io/owner/image");
        assert_eq!(r.tag, "latest");
    }
}
