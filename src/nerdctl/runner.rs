//! Running the `nerdctl` binary.
//!
//! The backend never spawns a process itself; it goes through
//! [`CommandRunner`], so unit tests can replay captured output with a fake
//! instead of needing nerdctl and containerd.

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
