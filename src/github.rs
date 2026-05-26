//! Thin wrapper around the `gh` CLI for surfacing per-branch PR status in the
//! sidebar. Kept dependency-free (just `serde_json`) and synchronous — the
//! runtime calls into this from a `std::thread::spawn` like the other blocking
//! ops.
//!
//! Strategy: run `gh pr status --json …` inside each worktree's CWD. `gh`
//! resolves "the PR for the checked-out branch" itself, so we don't have to
//! match branch names against a project-wide list.

use crate::app::PrStatus;
use crate::proc::output_with_timeout;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Cap on a single `gh pr list` invocation. Network-bound (auth refresh,
/// TLS, GitHub API), so we tolerate more than a local git op — but not
/// enough to wedge the polling worker if the network goes away.
const GH_TIMEOUT: Duration = Duration::from_secs(15);

/// One PR as returned by `gh pr list --json …`. Field set is intentionally
/// minimal — only what we need to classify into a [`PrStatus`].
#[derive(Debug, Clone, Deserialize)]
pub struct PrInfo {
    /// Kept so test fixtures + future code can refer to it; not read in the
    /// hot path because `gh pr status` only ever returns the current branch's
    /// PR (we already know which branch).
    #[serde(rename = "headRefName", default)]
    #[allow(dead_code)]
    pub head_ref_name: String,
    /// `"OPEN" | "CLOSED" | "MERGED"` per the GitHub GraphQL enum.
    pub state: String,
    /// `null | "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED"` etc.
    #[serde(rename = "reviewDecision", default)]
    pub review_decision: Option<String>,
    /// `"MERGEABLE" | "CONFLICTING" | "UNKNOWN"`. Used to surface merge
    /// conflicts as the same "needs attention" color as CI failures.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// `gh` serialises this as an array of check objects; we only need each
    /// entry's overall conclusion/status so an enum-like string is enough.
    #[serde(rename = "statusCheckRollup", default)]
    pub status_check_rollup: Vec<CheckRollupEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckRollupEntry {
    /// `"COMPLETED" | "IN_PROGRESS" | "PENDING" | "QUEUED" | …`
    #[serde(default)]
    pub status: Option<String>,
    /// `"SUCCESS" | "FAILURE" | "CANCELLED" | "TIMED_OUT" | …` — only set
    /// once status == "COMPLETED".
    #[serde(default)]
    pub conclusion: Option<String>,
    /// Some entries (legacy commit statuses) use `state` instead of
    /// status/conclusion: `"PENDING" | "SUCCESS" | "FAILURE" | "ERROR"`.
    #[serde(default)]
    pub state: Option<String>,
}

/// `true` once we've verified `gh --version` exits 0. Cached per process —
/// the `gh` install isn't expected to come and go.
pub fn gh_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let mut cmd = Command::new("gh");
        cmd.arg("--version");
        output_with_timeout(&mut cmd, Duration::from_secs(3))
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Shell out to `gh pr list --head <branch>` from `repo_path`. Returns the
/// first matching PR's classification, or `None` if there are no PRs for the
/// branch. Run from the *main repo CWD* (not the worktree's CWD) so gh's
/// repo resolution is unambiguous — `gh pr status` inside a non-main git
/// worktree doesn't reliably resolve the branch.
pub fn fetch_pr_by_branch(repo_path: &Path, branch: &str) -> Result<Option<PrStatus>> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(repo_path)
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "headRefName,state,reviewDecision,mergeable,statusCheckRollup",
        ]);
    let out = output_with_timeout(&mut cmd, GH_TIMEOUT)
        .map_err(|e| anyhow!("gh pr list: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!("gh pr list failed ({}): {}", out.status, stderr));
    }
    let prs: Vec<PrInfo> =
        serde_json::from_slice(&out.stdout).map_err(|e| anyhow!("parsing gh json: {e}"))?;
    let Some(pr) = prs.first() else {
        tracing::info!(%branch, "gh: no PR for branch");
        return Ok(None);
    };
    let sig = signals(pr);
    let status = classify_signals(&sig);
    // One-line, all signals. Greppable in `imbuia.log` so the user can
    // correlate a sidebar bar with what gh actually reported.
    tracing::info!(
        %branch,
        state = %pr.state,
        review = ?pr.review_decision,
        mergeable = ?pr.mergeable,
        checks = pr.status_check_rollup.len(),
        failed = sig.failed_checks,
        pending = sig.pending_checks,
        conflict = sig.conflict,
        approved = sig.approved,
        changes_requested = sig.changes_requested,
        classified = ?status,
        "gh: classify",
    );
    Ok(status)
}

/// Diagnostic snapshot of the signals classify uses to derive [`PrStatus`].
/// Logged on every fetch so the user can correlate the sidebar bar with the
/// raw PR data without re-running `gh` by hand.
#[derive(Debug, Default)]
pub struct Signals {
    pub merged: bool,
    pub closed: bool,
    pub conflict: bool,
    pub failed_checks: usize,
    pub pending_checks: usize,
    pub approved: bool,
    pub changes_requested: bool,
}

pub fn signals(pr: &PrInfo) -> Signals {
    let mut s = Signals::default();
    s.merged = pr.state.eq_ignore_ascii_case("MERGED");
    s.closed = !s.merged && !pr.state.eq_ignore_ascii_case("OPEN");
    s.conflict = matches!(
        pr.mergeable.as_deref(),
        Some(m) if m.eq_ignore_ascii_case("CONFLICTING")
    );
    for c in &pr.status_check_rollup {
        let status = c.status.as_deref().unwrap_or("").to_ascii_uppercase();
        let conclusion = c.conclusion.as_deref().unwrap_or("").to_ascii_uppercase();
        let state = c.state.as_deref().unwrap_or("").to_ascii_uppercase();
        if state == "FAILURE" || state == "ERROR" {
            s.failed_checks += 1;
        } else if state == "PENDING" || state == "EXPECTED" {
            s.pending_checks += 1;
        }
        match (status.as_str(), conclusion.as_str()) {
            ("COMPLETED", "FAILURE")
            | ("COMPLETED", "TIMED_OUT")
            | ("COMPLETED", "CANCELLED")
            | ("COMPLETED", "ACTION_REQUIRED") => s.failed_checks += 1,
            ("IN_PROGRESS", _) | ("QUEUED", _) | ("PENDING", _) | ("WAITING", _) => {
                s.pending_checks += 1
            }
            _ => {}
        }
    }
    s.approved = matches!(
        pr.review_decision.as_deref(),
        Some(d) if d.eq_ignore_ascii_case("APPROVED")
    );
    s.changes_requested = matches!(
        pr.review_decision.as_deref(),
        Some(d) if d.eq_ignore_ascii_case("CHANGES_REQUESTED")
    );
    s
}

/// Classify a single PR. Returns `None` only for closed-not-merged. The
/// sidebar shows *something* for every open PR so users can tell at a glance.
///
/// Precedence: Merged > Failed (CI failure or merge conflict) >
/// ChangesRequested > Running > Approved > Open.
#[cfg(test)]
pub fn classify(pr: &PrInfo) -> Option<PrStatus> {
    classify_signals(&signals(pr))
}

pub fn classify_signals(s: &Signals) -> Option<PrStatus> {
    if s.merged {
        return Some(PrStatus::Merged);
    }
    if s.closed {
        return None;
    }
    if s.failed_checks > 0 || s.conflict {
        return Some(PrStatus::Failed);
    }
    if s.changes_requested {
        return Some(PrStatus::ChangesRequested);
    }
    if s.pending_checks > 0 {
        return Some(PrStatus::Running);
    }
    if s.approved {
        return Some(PrStatus::Approved);
    }
    Some(PrStatus::Open)
}

/// Convenience for tests: classify against a synthesised `PrInfo`.
#[cfg(test)]
pub fn _classify_test(
    state: &str,
    review: Option<&str>,
    mergeable: Option<&str>,
    checks: &[(&str, &str)],
) -> Option<PrStatus> {
    let pr = PrInfo {
        head_ref_name: "x".into(),
        state: state.into(),
        review_decision: review.map(String::from),
        mergeable: mergeable.map(String::from),
        status_check_rollup: checks
            .iter()
            .map(|(status, conclusion)| CheckRollupEntry {
                status: Some((*status).into()),
                conclusion: Some((*conclusion).into()),
                state: None,
            })
            .collect(),
    };
    classify(&pr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_beats_everything() {
        assert_eq!(
            _classify_test(
                "MERGED",
                Some("CHANGES_REQUESTED"),
                Some("CONFLICTING"),
                &[("COMPLETED", "FAILURE")]
            ),
            Some(PrStatus::Merged)
        );
    }

    #[test]
    fn failure_beats_changes_requested() {
        assert_eq!(
            _classify_test(
                "OPEN",
                Some("CHANGES_REQUESTED"),
                None,
                &[("COMPLETED", "FAILURE")]
            ),
            Some(PrStatus::Failed)
        );
    }

    #[test]
    fn conflict_is_failure() {
        assert_eq!(
            _classify_test("OPEN", Some("APPROVED"), Some("CONFLICTING"), &[]),
            Some(PrStatus::Failed)
        );
    }

    #[test]
    fn changes_requested_beats_running() {
        assert_eq!(
            _classify_test(
                "OPEN",
                Some("CHANGES_REQUESTED"),
                None,
                &[("IN_PROGRESS", "")]
            ),
            Some(PrStatus::ChangesRequested)
        );
    }

    #[test]
    fn pending_when_only_in_progress() {
        assert_eq!(
            _classify_test("OPEN", None, None, &[("IN_PROGRESS", "")]),
            Some(PrStatus::Running)
        );
    }

    #[test]
    fn approved_open_returns_approved() {
        assert_eq!(
            _classify_test(
                "OPEN",
                Some("APPROVED"),
                Some("MERGEABLE"),
                &[("COMPLETED", "SUCCESS")]
            ),
            Some(PrStatus::Approved)
        );
    }

    #[test]
    fn review_required_open_returns_open() {
        assert_eq!(
            _classify_test(
                "OPEN",
                Some("REVIEW_REQUIRED"),
                Some("MERGEABLE"),
                &[("COMPLETED", "SUCCESS")]
            ),
            Some(PrStatus::Open)
        );
    }

    #[test]
    fn running_beats_approved() {
        assert_eq!(
            _classify_test(
                "OPEN",
                Some("APPROVED"),
                Some("MERGEABLE"),
                &[("IN_PROGRESS", "")]
            ),
            Some(PrStatus::Running)
        );
    }

    #[test]
    fn closed_without_merge_returns_none() {
        assert_eq!(_classify_test("CLOSED", None, None, &[]), None);
    }
}
