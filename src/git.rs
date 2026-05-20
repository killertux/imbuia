use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Command;

fn run(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(c) = cwd {
        cmd.arg("-C").arg(c);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| anyhow!("failed to spawn git: {e}"))?;
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
    let out = cmd
        .output()
        .map_err(|e| anyhow!("failed to spawn git: {e}"))?;
    if out.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

/// Create a worktree at `dest` for `branch`. If `branch` doesn't exist locally,
/// retry with `-b` to create it from current HEAD.
pub fn worktree_add(repo: &Path, dest: &Path, branch: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_str = dest.to_string_lossy().to_string();
    // First attempt: existing branch.
    let direct = run(&["worktree", "add", &dest_str, branch], Some(repo));
    if direct.is_ok() {
        return Ok(());
    }
    // Fallback: create the branch.
    run(&["worktree", "add", "-b", branch, &dest_str], Some(repo))?;
    Ok(())
}

/// Remove the worktree at `dest` and (optionally) delete the local branch.
/// Uses `--force` so dirty worktrees are still removed — the user is the one
/// who asked for this. Branch deletion failures are reported but don't abort.
pub fn worktree_remove(repo: &Path, dest: &Path, branch: Option<&str>) -> Result<()> {
    let dest_str = dest.to_string_lossy().to_string();
    run(&["worktree", "remove", "--force", &dest_str], Some(repo))?;
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
