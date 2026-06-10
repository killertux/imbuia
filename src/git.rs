use crate::proc::output_with_timeout;
use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Default cap for cheap git invocations (rev-parse, symbolic-ref, branch,
/// worktree list). These finish in milliseconds; the timeout is the
/// "something is wedged" cutoff so a poll-loop worker can't pin.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longer cap for git ops that touch the working tree on disk
/// (`worktree add` / `remove`). A monorepo's checkout/delete can take
/// minutes on slow filesystems; the cap is still a safety net for wedged
/// hooks or frozen NFS, not a productive wait.
const GIT_LONG_TIMEOUT: Duration = Duration::from_secs(600);

fn run(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    run_with(args, cwd, GIT_TIMEOUT)
}

fn run_with(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(c) = cwd {
        cmd.arg("-C").arg(c);
    }
    cmd.args(args);
    let out = output_with_timeout(&mut cmd, timeout)
        .map_err(|e| anyhow!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            stderr
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Returns Ok(()) if `path` is inside a git work tree.
pub fn validate_repo(path: &Path) -> Result<()> {
    let out = run(&["rev-parse", "--is-inside-work-tree"], Some(path))?;
    if out != "true" {
        return Err(anyhow!("{} is not inside a git work tree", path.display()));
    }
    Ok(())
}

/// Returns the current HEAD branch name, or `None` if HEAD is detached.
pub fn head_branch(path: &Path) -> Result<Option<String>> {
    // Attempt symbolic-ref first; if HEAD is detached this exits non-zero.
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path);
    cmd.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let out =
        output_with_timeout(&mut cmd, GIT_TIMEOUT).map_err(|e| anyhow!("git symbolic-ref: {e}"))?;
    if out.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

/// Create a worktree at `dest` for `branch`. If `branch` doesn't exist
/// locally, create it — based on the default remote's HEAD branch (e.g.
/// `origin/main`) after a fetch, so new worktrees start from the freshest
/// upstream code rather than the local clone's possibly-stale HEAD. Falls
/// back to current HEAD when the repo has no remote or the fetch fails
/// (offline).
pub fn worktree_add(repo: &Path, dest: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_str = dest.to_string_lossy().to_string();
    // First attempt: existing branch.
    let direct = run_with(
        &["worktree", "add", &dest_str, branch],
        Some(repo),
        GIT_LONG_TIMEOUT,
    );
    if direct.is_ok() {
        return Ok(());
    }
    // Fallback: create the branch.
    if let Some(start) = fresh_remote_start(repo) {
        // `--no-track`: the new branch is the user's own; tracking the
        // remote's main would make a later `git pull` merge it silently.
        run_with(
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                branch,
                &dest_str,
                &start,
            ],
            Some(repo),
            GIT_LONG_TIMEOUT,
        )?;
    } else {
        run_with(
            &["worktree", "add", "-b", branch, &dest_str],
            Some(repo),
            GIT_LONG_TIMEOUT,
        )?;
    }
    Ok(())
}

/// Fetch the default remote and return its HEAD branch as a start point for
/// new branches (e.g. `origin/main`). `None` means "use local HEAD": no
/// remote configured, fetch failed (offline), or the remote HEAD can't be
/// resolved.
fn fresh_remote_start(repo: &Path) -> Option<String> {
    let remote = default_remote(repo)?;
    if let Err(e) = run_with(&["fetch", &remote], Some(repo), GIT_LONG_TIMEOUT) {
        tracing::warn!(remote = %remote, "fetch failed, basing new branch on local HEAD: {e}");
        return None;
    }
    let start = remote_head(repo, &remote);
    if start.is_none() {
        tracing::warn!(remote = %remote, "couldn't resolve remote HEAD, basing new branch on local HEAD");
    }
    start
}

/// The remote to base new branches on: `origin` if present, else the first
/// configured remote, else `None` (local-only repo).
fn default_remote(repo: &Path) -> Option<String> {
    let out = run(&["remote"], Some(repo)).ok()?;
    let mut remotes = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = remotes.next()?.to_string();
    if first == "origin" || remotes.all(|r| r != "origin") {
        Some(first)
    } else {
        Some("origin".to_string())
    }
}

/// Resolve the remote's default branch (e.g. `origin/main`). The symbolic
/// ref `refs/remotes/<remote>/HEAD` is set on clone but may be missing on
/// repos that grew the remote later — in that case ask the remote directly
/// via `remote set-head --auto` (network) and retry.
fn remote_head(repo: &Path, remote: &str) -> Option<String> {
    let head_ref = format!("{remote}/HEAD");
    if let Ok(r) = run(&["rev-parse", "--abbrev-ref", &head_ref], Some(repo)) {
        return Some(r);
    }
    run(&["remote", "set-head", remote, "--auto"], Some(repo)).ok()?;
    run(&["rev-parse", "--abbrev-ref", &head_ref], Some(repo)).ok()
}

/// One row from `git worktree list --porcelain`. `branch` is `None` for
/// detached HEAD worktrees (we still report them so the caller can decide).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListEntry {
    pub path: std::path::PathBuf,
    pub branch: Option<String>,
}

/// Enumerate every worktree git knows about for `repo`. Uses porcelain
/// output — stable and easy to parse:
///
/// ```text
/// worktree /Users/me/proj
/// HEAD abcd
/// branch refs/heads/main
///
/// worktree /Users/me/proj-worktrees/feat
/// HEAD beef
/// branch refs/heads/feat
/// ```
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeListEntry>> {
    let out = run(&["worktree", "list", "--porcelain"], Some(repo))?;
    let mut entries = Vec::new();
    let mut path: Option<std::path::PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(prev) = path.take() {
                entries.push(WorktreeListEntry {
                    path: prev,
                    branch: branch.take(),
                });
            }
            path = Some(std::path::PathBuf::from(p));
            branch = None;
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
    }
    if let Some(prev) = path {
        entries.push(WorktreeListEntry { path: prev, branch });
    }
    Ok(entries)
}

/// Remove the worktree at `dest` and (optionally) delete the local branch.
/// Uses `--force` so dirty worktrees are still removed — the user is the one
/// who asked for this. Branch deletion failures are reported but don't abort.
pub fn worktree_remove(repo: &Path, dest: &Path, branch: Option<&str>) -> Result<()> {
    let dest_str = dest.to_string_lossy().to_string();
    run_with(
        &["worktree", "remove", "--force", &dest_str],
        Some(repo),
        GIT_LONG_TIMEOUT,
    )?;
    if let Some(b) = branch {
        // `-D` (force) — the worktree we just removed was likely on this branch
        // so `-d` would refuse on "not merged" grounds. The user asked to nuke it.
        // Failure here is non-fatal: branch may have been deleted already, or
        // never existed (detached HEAD case), and the worktree is already gone.
        if let Err(e) = run(&["branch", "-D", b], Some(repo)) {
            tracing::warn!(branch = %b, "branch delete failed (worktree already removed): {e}");
        }
    }
    Ok(())
}
