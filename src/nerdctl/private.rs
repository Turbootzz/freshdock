//! Owner-only temporary files, for what must not go on a command line.
//!
//! Anything in nerdctl's arguments is readable by every local user through
//! `/proc/<pid>/cmdline`. Container environment variables and registry
//! credentials are written to a [`PrivateDir`] instead, and nerdctl is given
//! the path.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A temporary directory only its owner can enter, removed when dropped.
#[derive(Debug)]
pub struct PrivateDir {
    dir: PathBuf,
}

impl PrivateDir {
    /// Create one under the system temp directory. The name is unpredictable,
    /// and creation fails rather than reuse a directory someone else made.
    pub fn new(purpose: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("freshdock-{purpose}-{}", unique_suffix()));
        create_private_dir(&dir)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Write `contents` to a new owner-only file `name` in the directory.
    pub fn write(&self, name: &str, contents: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.dir.join(name);
        let mut file = create_private_file(&path)?;
        file.write_all(contents)?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Split `<base><marker><suffix>` where `<suffix>` is exactly what
/// [`unique_suffix`] makes (`<pid>-<nanos>-<n>`, all digits). Returns the base
/// and the pid of the process that made the name. Anything else, including a
/// name that merely contains the marker, is `None`.
pub fn split_marked<'a>(name: &'a str, marker: &str) -> Option<(&'a str, u32)> {
    let (base, suffix) = name.rsplit_once(marker)?;
    if base.is_empty() {
        return None;
    }
    Some((base, suffix_pid(suffix)?))
}

/// The pid in a [`unique_suffix`], or `None` if `suffix` isn't one.
fn suffix_pid(suffix: &str) -> Option<u32> {
    let parts: Vec<&str> = suffix.split('-').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    parts[0].parse().ok()
}

/// Remove the private directories a killed freshdock left in the temp
/// directory (a normal exit removes its own). Only those whose process is
/// gone; returns how many were removed.
pub fn remove_stale_dirs() -> usize {
    remove_stale_dirs_in(&std::env::temp_dir())
}

fn remove_stale_dirs_in(tmp: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let pid = name
            .strip_prefix("freshdock-")
            .and_then(|rest| rest.split_once('-'))
            .filter(|(purpose, _)| purpose.bytes().all(|b| b.is_ascii_lowercase()))
            .and_then(|(_, suffix)| suffix_pid(suffix));
        if let Some(pid) = pid
            && !process_alive(pid)
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Is the process that made a marked name still running? Then whatever it
/// named may be in use by it right now. Where this can't be told (no
/// `/proc`), assume it is, so nothing is touched.
pub fn process_alive(pid: u32) -> bool {
    let proc = Path::new("/proc");
    !proc.is_dir() || proc.join(pid.to_string()).exists()
}

/// A suffix for temporary names: the process id, the time, and a counter, so
/// two in one process never collide.
pub fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_dir_is_owner_only_and_removed_on_drop() {
        let dir = PrivateDir::new("test").unwrap();
        let file = dir.write("f", b"secret").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir.path()), 0o700);
            assert_eq!(mode(&file), 0o600);
        }
        let path = dir.path().to_owned();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn suffixes_never_repeat() {
        assert_ne!(unique_suffix(), unique_suffix());
    }

    #[test]
    fn marked_names_split_only_on_the_exact_suffix() {
        const M: &str = "-freshdock-check-";
        let made = format!("web{M}{}", unique_suffix());
        assert_eq!(split_marked(&made, M), Some(("web", std::process::id())));
        assert_eq!(
            split_marked("db-freshdock-check-12-34-5", M),
            Some(("db", 12))
        );
        for name in [
            "db-freshdock-check-12-34",     // too few parts
            "db-freshdock-check-12-34-5-6", // too many
            "db-freshdock-check-12-34-x",   // not digits
            "db-freshdock-check-",          // no suffix
            "-freshdock-check-12-34-5",     // no base
            "my-freshdock-check-list",      // merely contains the marker
            "db-freshdock-check-12--5",     // empty part
        ] {
            assert_eq!(split_marked(name, M), None, "{name}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_dead_process_private_dirs_are_removed() {
        let tmp = PrivateDir::new("sweeptest").unwrap();
        let mk = |name: &str| {
            let p = tmp.path().join(name);
            std::fs::create_dir(&p).unwrap();
            p
        };
        let dead = mk("freshdock-env-999999999-1-0");
        let live = mk(&format!("freshdock-pull-{}-1-0", std::process::id()));
        let other = mk("freshdock-env-notasuffix");
        let unrelated = mk("something-else-999999999-1-0");
        assert_eq!(remove_stale_dirs_in(tmp.path()), 1);
        assert!(!dead.exists());
        assert!(live.exists() && other.exists() && unrelated.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_follows_proc() {
        assert!(process_alive(std::process::id()));
        // Far above any pid_max Linux allows (2^22).
        assert!(!process_alive(999_999_999));
    }
}
