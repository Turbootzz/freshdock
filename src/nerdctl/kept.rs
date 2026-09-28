//! The digest-named image records freshdock made, and must remove again.
//!
//! Before a pull moves a tag that a sibling still runs, freshdock names the
//! old image by its digest (`Nerdctl::keep_current`). Those names have to be
//! removed once no container runs the image, possibly days later (siblings on
//! different schedules) and possibly by another freshdock process (a one-off
//! `recreate` keeps, the daemon releases). So the list lives in a root-owned
//! state file, not only in memory: a restart in between would otherwise leave
//! the old image held on disk for good, indistinguishable from a record a user
//! made.
//!
//! Every change locks the file, rereads it and writes it back, so concurrent
//! freshdock processes can share it. Where the file can't be used (freshdock
//! not running as root, say) the list falls back to memory, with a warning.

use std::collections::BTreeSet;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::warn;

/// Where the daemon and one-off runs keep the list.
pub const DEFAULT_STATE_FILE: &str = "/var/lib/freshdock/nerdctl-kept-images";

/// The kept names for one containerd namespace.
#[derive(Debug)]
pub struct KeptStore {
    namespace: String,
    /// `None` once the file proved unusable, or for an in-memory store.
    file: Mutex<Option<PathBuf>>,
    memory: Mutex<BTreeSet<String>>,
}

impl KeptStore {
    /// A store that lives only as long as this process (tests).
    pub fn in_memory(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            file: Mutex::new(None),
            memory: Mutex::new(BTreeSet::new()),
        }
    }

    /// A store backed by `path`, shared with other freshdock processes.
    pub fn persistent(namespace: &str, path: impl Into<PathBuf>) -> Self {
        Self {
            namespace: namespace.to_owned(),
            file: Mutex::new(Some(path.into())),
            memory: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn insert(&self, name: &str) {
        self.change(|names| {
            names.insert(name.to_owned());
        });
    }

    pub fn remove(&self, name: &str) {
        self.change(|names| {
            names.remove(name);
        });
    }

    /// The names for this namespace, as stored now (including those another
    /// freshdock process added).
    pub fn names(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        self.change(|names| out = names.clone());
        out
    }

    /// Apply `f` to this namespace's names, persisting the result.
    fn change(&self, mut f: impl FnMut(&mut BTreeSet<String>)) {
        let path = self.file.lock().unwrap().clone();
        if let Some(path) = path {
            match update_file(&path, &self.namespace, &mut f) {
                Ok(()) => return,
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "cannot use the state file for kept images; keeping the list in \
                         memory, so a restart may leave an old image named by its digest"
                    );
                    *self.file.lock().unwrap() = None;
                }
            }
        }
        f(&mut self.memory.lock().unwrap());
    }
}

/// Lock `path`, read every namespace's names, apply `f` to `namespace`'s, and
/// write it back. Created owner-only, in an owner-only directory. On an error
/// `f` may or may not have run; the caller then applies it in memory, which is
/// safe because every change here (insert, remove, read) is idempotent.
fn update_file(
    path: &Path,
    namespace: &str,
    f: &mut dyn FnMut(&mut BTreeSet<String>),
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir_all(dir)?;
    }
    let mut file = open_private(path)?;
    file.lock()?;
    let mut body = String::new();
    file.read_to_string(&mut body)?;
    let mut others = Vec::new();
    let mut mine = BTreeSet::new();
    for line in body.lines() {
        match line.split_once('\t') {
            Some((ns, name)) if ns == namespace => {
                mine.insert(name.to_owned());
            }
            Some(_) => others.push(line.to_owned()),
            None => {}
        }
    }
    let before = mine.clone();
    f(&mut mine);
    if mine != before {
        let mut out = others.join("\n");
        for name in &mine {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(namespace);
            out.push('\t');
            out.push_str(name);
        }
        if !out.is_empty() {
            out.push('\n');
        }
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nerdctl::private::PrivateDir;

    #[test]
    fn names_survive_a_new_store_on_the_same_file() {
        let dir = PrivateDir::new("keptstore").unwrap();
        let path = dir.path().join("state/kept");
        let first = KeptStore::persistent("ns", &path);
        first.insert("repo@sha256:aaa");
        first.insert("repo@sha256:bbb");
        first.remove("repo@sha256:aaa");
        // A restart: a new process reads what the old one kept.
        let second = KeptStore::persistent("ns", &path);
        assert_eq!(second.names(), ["repo@sha256:bbb".to_owned()].into());
    }

    #[test]
    fn namespaces_are_kept_apart_in_one_file() {
        let dir = PrivateDir::new("keptstore").unwrap();
        let path = dir.path().join("kept");
        let a = KeptStore::persistent("a", &path);
        let b = KeptStore::persistent("b", &path);
        a.insert("x@sha256:1");
        b.insert("y@sha256:2");
        a.remove("x@sha256:1");
        assert!(a.names().is_empty());
        assert_eq!(b.names(), ["y@sha256:2".to_owned()].into());
    }

    #[test]
    fn another_process_additions_are_seen() {
        let dir = PrivateDir::new("keptstore").unwrap();
        let path = dir.path().join("kept");
        let daemon = KeptStore::persistent("ns", &path);
        let one_off = KeptStore::persistent("ns", &path);
        one_off.insert("repo@sha256:ccc");
        assert!(daemon.names().contains("repo@sha256:ccc"));
    }

    #[cfg(unix)]
    #[test]
    fn the_state_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = PrivateDir::new("keptstore").unwrap();
        let path = dir.path().join("state/kept");
        KeptStore::persistent("ns", &path).insert("repo@sha256:ddd");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn an_unusable_file_falls_back_to_memory() {
        let store = KeptStore::persistent("ns", "/proc/definitely/not/writable/kept");
        store.insert("repo@sha256:eee");
        store.insert("repo@sha256:fff");
        assert_eq!(
            store.names(),
            ["repo@sha256:eee".to_owned(), "repo@sha256:fff".to_owned()].into(),
            "nothing is lost when the file can't be used"
        );
    }

    #[test]
    fn an_in_memory_store_works_without_a_file() {
        let store = KeptStore::in_memory("ns");
        store.insert("repo@sha256:ggg");
        assert!(store.names().contains("repo@sha256:ggg"));
        store.remove("repo@sha256:ggg");
        assert!(store.names().is_empty());
    }
}
