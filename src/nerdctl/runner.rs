//! Running the `nerdctl` binary.
//!
//! The backend never spawns a process itself; it goes through
//! [`CommandRunner`], so unit tests can replay captured output with a fake
//! instead of needing nerdctl and containerd.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

/// What one nerdctl invocation produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// The exit code; `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why an invocation produced no [`CmdOutput`] at all.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("could not run {bin}: {source}")]
    Spawn { bin: String, source: std::io::Error },
    #[error("{bin} did not finish within {}s", .timeout.as_secs())]
    TimedOut { bin: String, timeout: Duration },
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run the binary with `args` and extra environment `env`, killing it if it
    /// is still running after `timeout`.
    async fn run(
        &self,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CmdOutput, RunError>;
}

/// Variables nerdctl may need, passed through by exact name.
const INHERITED_NAMES: [&str; 11] = [
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "DOCKER_CONFIG",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Variable name prefixes nerdctl, containerd and CNI configure themselves with.
const INHERITED_PREFIXES: [&str; 4] = ["CONTAINERD_", "NERDCTL_", "CNI_", "NETCONFPATH"];

/// Proxy settings. Only a pull needs them, and a proxy URL often carries
/// `user:password@`, so they are handed to `nerdctl pull` alone (see
/// [`proxy_env`]): any other call could store them in a container's hooks.
const PROXY_NAMES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// The part of freshdock's environment every nerdctl call gets: what it needs
/// to find its tools, its config and containerd, and nothing else. `vars_os`,
/// so a variable that isn't valid UTF-8 is passed through (or skipped) rather
/// than panicking.
pub(crate) fn inherited_env(
    vars: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.filter(|(k, _)| {
        k.to_str().is_some_and(|k| {
            INHERITED_NAMES.contains(&k) || INHERITED_PREFIXES.iter().any(|p| k.starts_with(p))
        })
    })
    .collect()
}

/// The proxy settings from `vars`, for the one call that needs the network:
/// `nerdctl pull`. Values that aren't valid UTF-8 are skipped.
pub(crate) fn proxy_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(String, String)> {
    vars.filter_map(|(k, v)| {
        let k = k.into_string().ok()?;
        PROXY_NAMES.contains(&k.as_str()).then_some(())?;
        Some((k, v.into_string().ok()?))
    })
    .collect()
}

/// Runs the real nerdctl binary.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    bin: PathBuf,
}

impl ProcessRunner {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }
}

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(
        &self,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CmdOutput, RunError> {
        let bin = self.bin.display().to_string();
        let child = tokio::process::Command::new(&self.bin)
            .args(args)
            // Not freshdock's own environment: nerdctl copies its caller's
            // environment into every container's OCI hooks, where freshdock's
            // FRESHDOCK_REGISTRY_* / FRESHDOCK_NOTIFY_* secrets would be
            // stored with the container.
            .env_clear()
            .envs(inherited_env(std::env::vars_os()))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A timed-out call drops the future; this makes that kill nerdctl
            // instead of leaving it running unattended.
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| RunError::Spawn {
                bin: bin.clone(),
                source,
            })?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| RunError::TimedOut {
                bin: bin.clone(),
                timeout,
            })?
            .map_err(|source| RunError::Spawn { bin, source })?;
        Ok(CmdOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_exit_code() {
        let out = ProcessRunner::new("sh")
            .run(
                &args(&["-c", "echo out; echo err >&2; exit 3"]),
                &[],
                SECOND,
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.success());
    }

    #[tokio::test]
    async fn passes_extra_environment() {
        let env = [("FD_RUNNER_TEST".to_owned(), "value".to_owned())];
        let out = ProcessRunner::new("sh")
            .run(
                &args(&["-c", "printf %s \"$FD_RUNNER_TEST\""]),
                &env,
                SECOND,
            )
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "value");
    }

    #[test]
    fn freshdock_secrets_are_not_inherited() {
        let vars = [
            ("PATH", "/usr/bin"),
            ("HOME", "/root"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("CONTAINERD_ADDRESS", "/run/containerd/containerd.sock"),
            ("NERDCTL_TOML", "/etc/nerdctl/nerdctl.toml"),
            ("CNI_PATH", "/opt/cni/bin"),
            ("FRESHDOCK_REGISTRY_GHCR_TOKEN", "ghp_secret"),
            ("FRESHDOCK_NOTIFY_OPS_PASSWORD", "hunter2"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("SUDO_USER", "someone"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let kept: Vec<String> = inherited_env(vars.clone().into_iter())
            .into_iter()
            .map(|(k, _)| k.into_string().unwrap())
            .collect();
        assert_eq!(
            kept,
            [
                "PATH",
                "HOME",
                "CONTAINERD_ADDRESS",
                "NERDCTL_TOML",
                "CNI_PATH"
            ]
        );
        assert_eq!(
            proxy_env(vars.into_iter()),
            [("HTTPS_PROXY".to_owned(), "http://proxy:3128".to_owned())],
            "proxy settings are kept apart, for pulls only"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_variable_neither_panics_nor_leaks() {
        use std::os::unix::ffi::OsStringExt;
        let vars = [
            (
                OsString::from("PATH"),
                OsString::from_vec(b"/usr/\xffbin".to_vec()),
            ),
            (
                OsString::from_vec(b"WEIRD_\xff".to_vec()),
                OsString::from("x"),
            ),
            (
                OsString::from("HTTP_PROXY"),
                OsString::from_vec(b"http://\xff".to_vec()),
            ),
        ];
        let kept = inherited_env(vars.clone().into_iter());
        assert_eq!(
            kept.len(),
            1,
            "PATH passes through as it is; the odd name doesn't"
        );
        assert_eq!(kept[0].0, "PATH");
        assert!(proxy_env(vars.into_iter()).is_empty());
    }

    #[tokio::test]
    async fn the_child_sees_only_the_allowlist_and_the_extra_env() {
        let env = [("DOCKER_CONFIG".to_owned(), "/tmp/x".to_owned())];
        let out = ProcessRunner::new("sh")
            .run(&args(&["-c", "env"]), &env, SECOND)
            .await
            .unwrap();
        for line in out.stdout.lines() {
            let name = line.split('=').next().unwrap_or("");
            // `sh` itself may add a few (PWD, SHLVL, _).
            let shell_own = ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name);
            assert!(
                shell_own
                    || !inherited_env(std::iter::once((name.into(), OsString::new()))).is_empty(),
                "{name} reached the child"
            );
        }
        assert!(out.stdout.contains("DOCKER_CONFIG=/tmp/x"));
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_spawn_error() {
        let err = ProcessRunner::new("/nonexistent/nerdctl")
            .run(&[], &[], SECOND)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_hung_command_times_out() {
        let err = ProcessRunner::new("sh")
            .run(&args(&["-c", "sleep 5"]), &[], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::TimedOut { .. }), "{err}");
    }
}
