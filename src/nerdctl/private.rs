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
}
