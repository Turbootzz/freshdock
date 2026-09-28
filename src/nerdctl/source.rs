//! Telling an image pulled from a registry from one built on this host.
//!
//! Docker gives an image a `RepoDigests` entry only once it came from (or went
//! to) a registry, so a locally built image has none and freshdock never
//! updates it: it can't know what upstream would mean for it. nerdctl lists a
//! `RepoDigests` entry for every image, local builds included
//! (`lore-server@sha256:...`), so that test alone would let freshdock replace
//! a local build with whatever a registry serves under the same name.
//!
//! containerd itself remembers where content came from: a pull labels the
//! image's top-level blob `containerd.io/distribution.source.<host>=<repo>`,
//! and a build, `commit` or `load` doesn't. nerdctl can't show content labels,
//! so freshdock reads them with `ctr content ls`, read-only.

use std::path::{Path, PathBuf};

use crate::registry::ImageRef;
use crate::registry::digest::split_repository;
use crate::registry::is_docker_hub_host;

const SOURCE_LABEL: &str = "containerd.io/distribution.source.";

/// The `ctr` to run: the one next to `nerdctl_path` when that names a
/// directory holding one (the nerdctl-full bundle installs both together),
/// else `ctr` on `PATH`.
pub fn ctr_path(nerdctl_path: &Path) -> PathBuf {
    nerdctl_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("ctr"))
        .filter(|ctr| ctr.is_file())
        .unwrap_or_else(|| PathBuf::from("ctr"))
}

/// The `(host, repository)` pairs a `ctr content ls` row's labels record as
/// sources. containerd appends further repositories to one label with commas
/// (`...docker.io=library/a,library/b`), and `ctr` also joins labels with
/// commas, so a piece without `=` continues the label before it.
pub fn sources(listing: &str, digest: &str) -> Vec<(String, String)> {
    let Some(labels) = listing.lines().find_map(|line| {
        let mut columns = line.split('\t');
        (columns.next() == Some(digest)).then(|| columns.next_back().unwrap_or(""))
    }) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut host: Option<&str> = None;
    for piece in labels.split(',') {
        let value = match piece.split_once('=') {
            Some((key, value)) => {
                host = key.strip_prefix(SOURCE_LABEL);
                value
            }
            None => piece,
        };
        if let Some(host) = host
            && !value.is_empty()
        {
            found.push((host.to_ascii_lowercase(), value.to_owned()));
        }
    }
    found
}

/// Where a `RepoDigests` entry (`repo@sha256:...`) says the image lives, the
/// way containerd names a source: the host without its port (containerd
/// labels by hostname only), and the path within the registry.
pub fn registry_of(repo_digest: &str) -> Option<(String, String, &str)> {
    let (repo, digest) = repo_digest.rsplit_once('@')?;
    let repository = ImageRef::parse(repo).repository;
    let (host, path) = split_repository(&repository);
    let host = if is_docker_hub_host(host) {
        "docker.io"
    } else {
        host.rsplit_once(':')
            .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
            .map_or(host, |(name, _)| name)
    };
    Some((host.to_ascii_lowercase(), path.to_owned(), digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPINE: &str = "sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc";

    fn listing(digest: &str, labels: &str) -> String {
        format!("DIGEST\tSIZE\tAGE\tLABELS\n{digest}\t9.226kB\t2 days\t{labels}\n")
    }

    #[test]
    fn a_pulled_images_source_is_read() {
        let out = listing(
            ALPINE,
            "containerd.io/distribution.source.docker.io=library/alpine,\
             containerd.io/gc.ref.content.m.0=sha256:c64c",
        );
        assert_eq!(
            sources(&out, ALPINE),
            [("docker.io".to_owned(), "library/alpine".to_owned())]
        );
    }

    #[test]
    fn several_repositories_in_one_label_are_all_read() {
        let out = listing(
            ALPINE,
            "containerd.io/distribution.source.ghcr.io=a/one,b/two,\
             containerd.io/gc.ref.content.config=sha256:43,\
             containerd.io/distribution.source.docker.io=library/alpine",
        );
        assert_eq!(
            sources(&out, ALPINE),
            [
                ("ghcr.io".to_owned(), "a/one".to_owned()),
                ("ghcr.io".to_owned(), "b/two".to_owned()),
                ("docker.io".to_owned(), "library/alpine".to_owned()),
            ]
        );
    }

    #[test]
    fn a_local_build_has_no_source() {
        let out = listing(
            ALPINE,
            "containerd.io/gc.ref.content.config=sha256:4399,\
             containerd.io/gc.ref.content.l.0=sha256:062e",
        );
        assert!(sources(&out, ALPINE).is_empty());
    }

    #[test]
    fn a_digest_not_listed_has_no_source() {
        assert!(sources("DIGEST\tSIZE\tAGE\tLABELS\n", ALPINE).is_empty());
        let other = listing(
            "sha256:0000",
            "containerd.io/distribution.source.docker.io=library/alpine",
        );
        assert!(sources(&other, ALPINE).is_empty());
    }

    #[test]
    fn repo_digests_map_to_containerds_source_names() {
        assert_eq!(
            registry_of(&format!("alpine@{ALPINE}")),
            Some(("docker.io".to_owned(), "library/alpine".to_owned(), ALPINE))
        );
        assert_eq!(
            registry_of(&format!("docker.io/library/lore-server@{ALPINE}")),
            Some((
                "docker.io".to_owned(),
                "library/lore-server".to_owned(),
                ALPINE
            ))
        );
        assert_eq!(
            registry_of(&format!("ghcr.io/immich-app/immich-server@{ALPINE}")),
            Some((
                "ghcr.io".to_owned(),
                "immich-app/immich-server".to_owned(),
                ALPINE
            ))
        );
        // containerd labels by hostname, without the port.
        assert_eq!(
            registry_of(&format!("127.0.0.1:15000/freshdock-live/shared@{ALPINE}")),
            Some((
                "127.0.0.1".to_owned(),
                "freshdock-live/shared".to_owned(),
                ALPINE
            ))
        );
        assert_eq!(registry_of("alpine:3.20"), None);
    }

    #[test]
    fn ctr_is_looked_for_next_to_nerdctl() {
        assert_eq!(ctr_path(Path::new("nerdctl")), PathBuf::from("ctr"));
        let dir = crate::nerdctl::private::PrivateDir::new("ctrpath").unwrap();
        let nerdctl = dir.path().join("nerdctl");
        assert_eq!(ctr_path(&nerdctl), PathBuf::from("ctr"), "no ctr there");
        std::fs::write(dir.path().join("ctr"), "").unwrap();
        assert_eq!(ctr_path(&nerdctl), dir.path().join("ctr"));
    }
}
