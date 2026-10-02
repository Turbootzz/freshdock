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
/// [`unique_suffix`] makes (`<pid>-<start>-<boot>-<n>`: digits, but `<boot>`
/// in lowercase hex). Returns the base
/// and who made the name. Anything else, including a name that merely
/// contains the marker, is `None`.
pub fn split_marked<'a>(name: &'a str, marker: &str) -> Option<(&'a str, Maker)> {
    let (base, suffix) = name.rsplit_once(marker)?;
    if base.is_empty() {
        return None;
    }
    Some((base, suffix_maker(suffix)?))
}

/// The process that made a [`unique_suffix`]: its pid, when it started, and
/// in which boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Maker {
    pub pid: u32,
    /// The process's start, in clock ticks since boot, or 0 if it couldn't
    /// be read. With the pid it names one process within a boot, whatever the
    /// wall clock does.
    started: u64,
    /// The boot it ran in (see [`boot`]), or 0 if that couldn't be read.
    /// Containers outlive a reboot; pids and start ticks don't mean the same
    /// process in another boot.
    boot: u64,
}

impl Maker {
    /// Is the process that made the name still running? Then whatever it
    /// named may be in use by it right now. A process under the same pid that
    /// started at another time is another one (the pid was reused), so the
    /// maker is gone. Where this can't be told, assume it is running, so
    /// nothing is touched.
    pub fn alive(&self) -> bool {
        if self.boot != 0 && boot().is_some_and(|now| now != self.boot) {
            return false;
        }
        if !process_alive(self.pid) {
            return false;
        }
        if self.started == 0 {
            return true;
        }
        process_start(self.pid).is_none_or(|now| now == self.started)
    }
}

/// The maker in a [`unique_suffix`], or `None` if `suffix` isn't one.
fn suffix_maker(suffix: &str) -> Option<Maker> {
    let parts: Vec<&str> = suffix.split('-').collect();
    let [pid, started, boot, n] = parts.as_slice() else {
        return None;
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let hex = |p: &str| {
        !p.is_empty()
            && p.len() <= 16
            && p.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !digits(pid) || !digits(started) || !hex(boot) || !digits(n) {
        return None;
    }
    Some(Maker {
        pid: pid.parse().ok()?,
        started: started.parse().ok()?,
        boot: u64::from_str_radix(boot, 16).ok()?,
    })
}

/// This boot: the first 12 hex digits of the kernel's random boot id, as a
/// number (never 0 when read).
fn boot() -> Option<u64> {
    static BOOT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *BOOT.get_or_init(|| {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let hex: String = id
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(12)
            .collect();
        u64::from_str_radix(&hex, 16).ok().filter(|b| *b != 0)
    })
}

/// When `pid` started, in clock ticks since boot (`/proc/<pid>/stat`), which
/// no wall-clock change moves.
fn process_start(pid: u32) -> Option<u64> {
    start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Field 22 (`starttime`) of a `/proc/<pid>/stat` line. The command name
/// (field 2) is in parentheses and may itself contain spaces and `)`, so
/// count from the last `)`.
fn start_ticks(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
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
        let maker = name
            .strip_prefix("freshdock-")
            .and_then(|rest| rest.split_once('-'))
            .filter(|(purpose, _)| purpose.bytes().all(|b| b.is_ascii_lowercase()))
            .and_then(|(_, suffix)| suffix_maker(suffix));
        if let Some(maker) = maker
            && !maker.alive()
            && owned_by_us(&entry.path())
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Does `path` belong to the user freshdock runs as? Its own private
/// directories always do; anything else in the temp directory, even under a
/// matching name, is another user's and left alone. When this can't be told,
/// the answer is no.
#[cfg(unix)]
fn owned_by_us(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // /proc/self is owned by the process's effective uid.
    match (
        std::fs::metadata("/proc/self"),
        std::fs::symlink_metadata(path),
    ) {
        (Ok(me), Ok(it)) => it.uid() == me.uid(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn owned_by_us(_path: &Path) -> bool {
    false
}

/// Is there a process with this pid? Where this can't be told (no `/proc`),
/// assume there is, so nothing is touched.
pub fn process_alive(pid: u32) -> bool {
    let proc = Path::new("/proc");
    !proc.is_dir() || proc.join(pid.to_string()).exists()
}

/// A suffix for temporary names: the process id, when the process started,
/// in which boot (see [`Maker`]), and a counter, so two in one process never
/// collide, nor two in different boots.
pub fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let started = process_start(pid).unwrap_or(0);
    let boot = boot().unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid}-{started}-{boot:x}-{n}")
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
        let pid = |name| split_marked(name, M).map(|(base, maker)| (base, maker.pid));
        assert_eq!(pid(&made), Some(("web", std::process::id())));
        assert_eq!(pid("db-freshdock-check-12-34-ab-5"), Some(("db", 12)));
        for name in [
            "db-freshdock-check-12-34-5",      // too few parts
            "db-freshdock-check-12-34-ab-5-6", // too many
            "db-freshdock-check-12-34-ab-x",   // not digits
            "db-freshdock-check-12-34-xyz-5",  // boot not hex
            "db-freshdock-check-12-34-AB-5",   // boot not lowercase
            "db-freshdock-check-",             // no suffix
            "-freshdock-check-12-34-ab-5",     // no base
            "my-freshdock-check-list",         // merely contains the marker
            "db-freshdock-check-12--ab-5",     // empty part
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
        let dead = mk("freshdock-env-999999999-1-0-0");
        let live = mk(&format!("freshdock-pull-{}", unique_suffix()));
        let other = mk("freshdock-env-notasuffix");
        let unrelated = mk("something-else-999999999-1-0-0");
        assert_eq!(remove_stale_dirs_in(tmp.path()), 1);
        assert!(!dead.exists());
        assert!(live.exists() && other.exists() && unrelated.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_our_own_directories_count_as_ours() {
        use std::os::unix::fs::MetadataExt;
        let tmp = PrivateDir::new("ownertest").unwrap();
        assert!(owned_by_us(tmp.path()));
        // `/` belongs to root; unless this test runs as root, it isn't ours.
        if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
            assert!(!owned_by_us(Path::new("/")));
        }
        assert!(!owned_by_us(Path::new("/definitely/not/here")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_reused_after_the_name_was_made_is_not_its_maker() {
        let made = format!("web-x-{}", unique_suffix());
        let (_, maker) = split_marked(&made, "-x-").unwrap();
        assert!(maker.alive(), "made by this very process");
        // The same pid, but a process that started at another time: the pid
        // was reused, so this process isn't the one that made it.
        let reused = format!("web-x-{}-1-{:x}-0", std::process::id(), boot().unwrap());
        let (_, old) = split_marked(&reused, "-x-").unwrap();
        assert!(!old.alive());
    }

    #[test]
    fn the_start_time_is_read_past_an_awkward_command_name() {
        let mut fields = vec!["S"; 19];
        fields.push("12345");
        let line = format!("42 (a) b (c)) {} 0 0", fields.join(" "));
        assert_eq!(start_ticks(&line), Some(12345));
        assert_eq!(start_ticks("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_suffix_records_this_process_start() {
        let started = process_start(std::process::id()).expect("readable on Linux");
        assert!(started > 0);
        let made = format!("web-x-{}", unique_suffix());
        let (_, maker) = split_marked(&made, "-x-").unwrap();
        assert_eq!(maker.started, started);
    }

    #[test]
    fn an_unknown_start_never_counts_as_gone() {
        // Recorded as 0 where the start couldn't be read: only a missing
        // process makes the maker gone then.
        let (_, maker) =
            split_marked(&format!("web-x-{}-0-0-0", std::process::id()), "-x-").unwrap();
        assert!(maker.alive());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_from_another_boot_is_never_this_process() {
        // Same pid and start as this process, as an early systemd service can
        // get again after a reboot, but another boot: its maker is gone.
        let now = boot().expect("readable on Linux");
        let other = if now == 1 { 2 } else { 1 };
        let pid = std::process::id();
        let started = process_start(pid).unwrap();
        let same = format!("web-x-{pid}-{started}-{now:x}-0");
        let before = format!("web-x-{pid}-{started}-{other:x}-0");
        assert!(split_marked(&same, "-x-").unwrap().1.alive());
        assert!(!split_marked(&before, "-x-").unwrap().1.alive());
        assert_ne!(unique_suffix(), before.trim_start_matches("web-x-"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_follows_proc() {
        assert!(process_alive(std::process::id()));
        // Far above any pid_max Linux allows (2^22).
        assert!(!process_alive(999_999_999));
    }
}
