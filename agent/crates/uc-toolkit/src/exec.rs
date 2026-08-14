// SPDX-License-Identifier: Apache-2.0

//! Time-bounded external-command execution, shared by the fleet-health probes.
//!
//! Every probe in this crate shells out to a host tool (`smartctl`, `zpool`,
//! `apt`, `docker`). On a *sick* host those tools can block forever — a
//! suspended ZFS pool puts `zpool status` into uninterruptible (D-state) sleep,
//! and a held dpkg frontend lock stalls apt indefinitely. A probe that inherits
//! that hang takes the whole agent with it, so **no command in this crate is
//! ever run without a deadline**.
//!
//! `kill_on_drop(true)` means the timeout path signals the child rather than
//! orphaning it. A process already wedged in D-state cannot actually be killed
//! until the kernel lets it out — but the *agent* is free the instant the
//! timeout fires, which is the property that matters.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Default per-command deadline.
pub const DEFAULT_TIMEOUT_SECS: u64 = 20;

/// Hard ceiling on a caller-supplied deadline.
pub const MAX_TIMEOUT_SECS: u64 = 300;

/// Captured output of a completed command.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
}

/// Why a command did not produce usable output.
#[derive(Debug, Clone)]
pub enum ExecError {
    /// The binary is not installed / not on PATH. Probes translate this into a
    /// `*_available: false` result, never an error.
    NotFound(String),
    /// The deadline elapsed. Probes translate this into a partial result with
    /// `error` set — never a hang.
    Timeout { program: String, secs: u64 },
    /// Spawn or I/O failure (including EPERM on the executable itself).
    Io(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "{p} not found on PATH"),
            Self::Timeout { program, secs } => {
                write!(f, "{program} timed out after {secs}s (killed)")
            }
            Self::Io(m) => write!(f, "{m}"),
        }
    }
}

impl ExecError {
    /// True when the tool simply isn't installed — the "not available, not an
    /// error" case every probe reports cleanly.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

/// Normalize a caller-supplied timeout: 0 means "use the default", and anything
/// larger than [`MAX_TIMEOUT_SECS`] is clamped.
pub fn clamp_timeout(secs: u64) -> Duration {
    let secs = match secs {
        0 => DEFAULT_TIMEOUT_SECS,
        n => n.min(MAX_TIMEOUT_SECS),
    };
    Duration::from_secs(secs)
}

/// Run `program args…` with a hard deadline, capturing stdout/stderr.
///
/// stdin is `/dev/null` so nothing can ever block waiting for input, and the
/// child is killed if the deadline elapses. A non-zero exit is **not** an error
/// here — it is returned in [`CmdOutput`] so callers can decide.
pub async fn run(program: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput, ExecError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_elapsed) => Err(ExecError::Timeout {
            program: program.to_string(),
            secs: timeout.as_secs(),
        }),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ExecError::NotFound(program.to_string()))
        }
        Ok(Err(e)) => Err(ExecError::Io(format!("{program}: {e}"))),
        Ok(Ok(out)) => Ok(CmdOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code(),
            success: out.status.success(),
        }),
    }
}

/// Cheap presence probe: does this binary exist on PATH at all?
///
/// Runs the program's own `--version` (universally read-only for the tools this
/// crate touches) with a short deadline and reports only whether it *ran*.
pub async fn is_available(program: &str, version_flag: &str) -> bool {
    match run(program, &[version_flag], Duration::from_secs(5)).await {
        Err(ExecError::NotFound(_)) => false,
        Err(_) => false,
        Ok(_) => true,
    }
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_timeout_applies_default_and_ceiling() {
        assert_eq!(clamp_timeout(0), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(clamp_timeout(7), Duration::from_secs(7));
        assert_eq!(
            clamp_timeout(99_999),
            Duration::from_secs(MAX_TIMEOUT_SECS),
            "an absurd timeout is clamped, not honored"
        );
    }

    #[tokio::test]
    async fn run_captures_stdout_and_success() {
        let out = run("echo", &["hello-probe"], Duration::from_secs(5))
            .await
            .expect("echo runs");
        assert!(out.success);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout.trim(), "hello-probe");
    }

    #[tokio::test]
    async fn missing_binary_is_not_found_not_a_hang() {
        let err = run(
            "uuda-definitely-not-a-real-binary",
            &[],
            Duration::from_secs(5),
        )
        .await
        .expect_err("must not succeed");
        assert!(err.is_not_found(), "expected NotFound, got {err:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_returned_not_raised() {
        // `false` exits 1; that is data, not an error.
        let out = run("sh", &["-c", "exit 3"], Duration::from_secs(5))
            .await
            .expect("sh runs");
        assert!(!out.success);
        assert_eq!(out.exit_code, Some(3));
    }

    /// The property the whole probe family depends on: a command that never
    /// returns must not hold the agent. This is the `zpool status` case.
    #[tokio::test]
    async fn a_hanging_command_times_out_and_is_killed() {
        let started = std::time::Instant::now();
        let err = run("sleep", &["30"], Duration::from_millis(200))
            .await
            .expect_err("must time out");
        assert!(
            matches!(err, ExecError::Timeout { .. }),
            "expected Timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must fire promptly, took {:?}",
            started.elapsed()
        );
    }
}
