//! Self-update: GitHub release check + shell-out install.
//!
//! Two surfaces:
//!
//! * [`check_for_update`] hits `api.github.com/.../releases/latest` and
//!   returns `Some(info)` if a strictly-newer semver is published.
//! * [`install_update`] pipes the embedded [`install.sh`](../../install.sh)
//!   into `sh`, with env vars steering it at the same directory the running
//!   binary lives in. After it exits successfully, it exec's the freshly
//!   installed binary with `--print-protocol-version` and compares the
//!   number against [`crate::ipc::PROTOCOL_VERSION`] to decide whether the
//!   user has to restart the supervisor (kills sessions) or can keep them
//!   alive across a client-only relaunch.
//!
//! Both functions block on the network / disk / subprocesses. Call them
//! from a `std::thread::spawn`, never from the reducer.

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const RELEASES_URL: &str = "https://api.github.com/repos/killertux/imbuia/releases/latest";
const INSTALL_SCRIPT: &str = include_str!("../install.sh");
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    /// `v0.4.0` as published on GitHub.
    pub latest_tag: String,
    #[allow(dead_code)] // parsed for the comparison; carried for future UI
    pub latest_version: Version,
}

#[derive(Clone, Debug)]
pub struct InstallOutcome {
    #[allow(dead_code)] // carried for future logging / status detail
    pub installed_to: PathBuf,
    /// `true` when the new binary's IPC protocol differs from this one's
    /// — i.e. the running supervisor cannot talk to the new client.
    pub supervisor_restart_required: bool,
    pub installed_tag: String,
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version must be valid semver")
}

/// Returns `Some(info)` only if `info.latest_version > current_version()`.
/// Network failures bubble up via `anyhow::Error`; callers should swallow
/// them on the background path so the user isn't pestered by transient
/// connectivity issues.
pub fn check_for_update() -> Result<Option<UpdateInfo>> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent(format!("imbuia/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();

    let body: serde_json::Value = agent
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("GET {RELEASES_URL}"))?
        .body_mut()
        .read_json()
        .context("parse releases JSON")?;

    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("releases JSON missing `tag_name`"))?
        .to_string();

    let version_str = tag.strip_prefix('v').unwrap_or(&tag);
    let latest = Version::parse(version_str)
        .with_context(|| format!("`tag_name` {tag:?} not parseable as semver"))?;
    if latest <= current_version() {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        latest_tag: tag,
        latest_version: latest,
    }))
}

/// Pipe the embedded install script into `sh` and wait for it. On success,
/// run the freshly-installed binary with `--print-protocol-version` to
/// figure out whether the supervisor has to be restarted.
pub fn install_update(info: &UpdateInfo) -> Result<InstallOutcome> {
    let exe = std::env::current_exe().context("locate running binary")?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current_exe() has no parent: {}", exe.display()))?
        .to_path_buf();

    tracing::info!(
        tag = %info.latest_tag,
        install_dir = %install_dir.display(),
        "running install script"
    );

    let output = Command::new("sh")
        .arg("-s")
        .env("IMBUIA_VERSION", &info.latest_tag)
        .env("IMBUIA_INSTALL_DIR", &install_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn `sh -s` to run install script")
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("install: missing stdin"))?
                .write_all(INSTALL_SCRIPT.as_bytes())
                .context("write install script to sh stdin")?;
            child.wait_with_output().context("wait for sh")
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "install script failed ({}): {}{}",
            output.status,
            stdout.trim(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", stderr.trim())
            }
        );
    }

    let new_binary = install_dir.join("imbuia");
    let supervisor_restart_required = match detect_protocol_change(&new_binary) {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!(
                "couldn't probe new binary's protocol version, assuming restart needed: {e}"
            );
            true
        }
    };

    Ok(InstallOutcome {
        installed_to: install_dir,
        supervisor_restart_required,
        installed_tag: info.latest_tag.clone(),
    })
}

fn detect_protocol_change(new_binary: &std::path::Path) -> Result<bool> {
    let out = Command::new(new_binary)
        .arg("--print-protocol-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("exec {} --print-protocol-version", new_binary.display()))?;
    if !out.status.success() {
        bail!(
            "{} --print-protocol-version exited with {}",
            new_binary.display(),
            out.status
        );
    }
    let printed = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .context("parse new binary's protocol version")?;
    Ok(printed != crate::ipc::PROTOCOL_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_parses() {
        // If this panics, Cargo.toml's `version` isn't valid semver.
        let _ = current_version();
    }

    #[test]
    fn embedded_install_script_has_shebang() {
        assert!(INSTALL_SCRIPT.starts_with("#!/bin/sh"));
    }
}
