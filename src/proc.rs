//! Subprocess execution helpers with timeouts. Used by `git.rs` and
//! `github.rs` so a hung network or stuck filesystem can't pin a blocking
//! worker forever (the `gh` poll runs on a 30 s tick; one stuck invocation
//! would otherwise block every subsequent fetch).
//!
//! The implementation polls `Child::try_wait` on a short interval. We don't
//! drain stdout/stderr concurrently — pipe-buffer fill is a theoretical
//! concern but the commands we run here produce small bounded output (well
//! under the ~64 KiB Unix pipe buffer). Document the limit in the helper's
//! doc-comment rather than building a reader-thread harness for it.

use anyhow::{Result, anyhow};
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd` to completion, killing it (SIGKILL on Unix) if it hasn't
/// exited within `timeout`. Returns the same shape as `Command::output()`
/// on success.
///
/// Caveats:
/// - stdout/stderr are read *after* exit. If the child writes more than the
///   pipe buffer (~64 KiB on Linux/macOS) it will block; the poll loop
///   won't observe completion and the timeout will fire. Only use for
///   commands with bounded output.
/// - On timeout, we send SIGKILL and `wait()` to reap before returning the
///   error, so the caller's task can return cleanly.
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("spawn failed: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|e| anyhow!("try_wait: {e}"))? {
            Some(status) => return Ok(collect_output(child, status)),
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!("timed out after {:?}", timeout));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Identical to `std::process::Output` — re-declared so callers don't have
/// to import the std type alongside ours.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn collect_output(mut child: Child, status: ExitStatus) -> Output {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_returns_output() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo hi"]);
        let out = output_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn slow_command_times_out() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5"]);
        let err = output_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }
}
