use crate::layout::DEFAULT_SIDEBAR_WIDTH;
use crate::theme::ThemeKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    #[serde(default)]
    pub theme: ThemeKind,
    #[serde(default)]
    pub projects: Vec<String>,
    /// Launchers available across every project. Merged with the per-project
    /// [`ProjectConfig::launchers`] at runtime; a project-level entry with
    /// the same name overrides the global one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchers: Vec<LauncherConfig>,
    /// Default cadence (seconds) for the GitHub PR-status background poll.
    /// `None` means the runtime falls back to its built-in default (120s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_poll_interval_secs: Option<u64>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            theme: ThemeKind::default(),
            projects: Vec::new(),
            launchers: Vec::new(),
            gh_poll_interval_secs: None,
        }
    }
}

fn default_sidebar_width() -> u16 {
    DEFAULT_SIDEBAR_WIDTH
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Filename stem under `projects/` — also used as the in-memory identifier.
    /// Skipped during (de)serialization since it's the file name itself.
    #[serde(skip)]
    pub slug: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_expanded")]
    pub expanded: bool,
    /// Multi-line bash script run inside each new worktree on creation.
    /// `None` means "no setup step". Edited via `:edit` or by hand in the toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_script: Option<String>,
    #[serde(default)]
    pub worktrees: Vec<WorktreeConfig>,
    /// Named launchers: a label + a command line that's piped to the PTY as
    /// the first input. Edited by hand in the TOML for now; selected via
    /// `:launch` or the launch popup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchers: Vec<LauncherConfig>,
    /// Opt-in GitHub PR-status integration. Toggled by `:gh-enable`/`:gh-disable`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub github_enabled: bool,
    /// Per-project poll cadence override (seconds). Overrides the global
    /// `gh_poll_interval_secs`; `None` defers to the global setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_poll_interval_secs: Option<u64>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub name: String,
    pub command: String,
}

fn default_expanded() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub branch: Option<String>,
}

/// Resolve `$XDG_CONFIG_HOME/imbuia` (or `~/.config/imbuia`). Creates the
/// directory tree if needed; falls back to `.` if no env is available.
pub fn resolve_config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("imbuia");
    let _ = fs::create_dir_all(dir.join("projects"));
    dir
}

pub fn load_or_default(dir: &Path) -> (GlobalConfig, Vec<ProjectConfig>) {
    let global = load_global(dir).unwrap_or_default();
    let mut projects = Vec::with_capacity(global.projects.len());
    for slug in &global.projects {
        if !is_valid_slug(slug) {
            tracing::warn!(slug = %slug, "skipping project: invalid slug");
            continue;
        }
        match load_project(dir, slug) {
            Ok(p) => projects.push(p),
            Err(e) => tracing::warn!(slug = %slug, "skipping project: {e}"),
        }
    }
    (global, projects)
}

/// Slugs are used as filenames; reject anything that could traverse or that
/// would break the `projects/<slug>.toml` convention. Conservative on purpose.
pub fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn project_path(dir: &Path, slug: &str) -> PathBuf {
    dir.join("projects").join(format!("{slug}.toml"))
}

fn global_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

pub fn load_global(dir: &Path) -> Result<GlobalConfig> {
    let path = global_path(dir);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: GlobalConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

pub fn load_project(dir: &Path, slug: &str) -> Result<ProjectConfig> {
    anyhow::ensure!(is_valid_slug(slug), "invalid slug {slug:?}");
    let path = project_path(dir, slug);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut cfg: ProjectConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    cfg.slug = slug.to_string();
    Ok(cfg)
}

pub fn save_global(dir: &Path, cfg: &GlobalConfig) -> Result<()> {
    let path = global_path(dir);
    write_toml_atomic(&path, cfg)
}

pub fn save_project(dir: &Path, cfg: &ProjectConfig) -> Result<()> {
    let path = project_path(dir, &cfg.slug);
    write_toml_atomic(&path, cfg)
}

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(value).context("serializing toml")?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Lowercase + replace non-alphanumeric runs with `-`. Trim leading/trailing `-`.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed
    }
}

/// Pick a fresh slug for `name`, avoiding collisions in `existing`.
pub fn compute_slug(name: &str, existing: &[String]) -> String {
    let base = slugify(name);
    if !existing.iter().any(|s| s == &base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|s| s == &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("Imbuia"), "imbuia");
        assert_eq!(slugify("My Project!"), "my-project");
        assert_eq!(slugify("  weird   spaces  "), "weird-spaces");
        assert_eq!(slugify("////"), "project");
    }

    #[test]
    fn slug_collision_appends_counter() {
        let existing = vec!["imbuia".into(), "imbuia-2".into()];
        assert_eq!(compute_slug("Imbuia", &existing), "imbuia-3");
    }

    #[test]
    fn rejects_traversal_slug() {
        assert!(!is_valid_slug("../etc"));
        assert!(!is_valid_slug("foo/bar"));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug(&"a".repeat(65)));
        assert!(is_valid_slug("imbuia"));
        assert!(is_valid_slug("foo-bar_2"));
    }

    #[test]
    fn load_or_default_skips_bad_slugs() {
        let dir = std::env::temp_dir().join(format!("imbuia-cfg-badslug-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("projects")).unwrap();
        let global = GlobalConfig {
            sidebar_width: 30,
            theme: ThemeKind::default(),
            projects: vec!["../etc".into(), "ok".into()],
            launchers: Vec::new(),
            gh_poll_interval_secs: None,
        };
        save_global(&dir, &global).unwrap();
        let proj = ProjectConfig {
            slug: "ok".into(),
            name: "Ok".into(),
            path: PathBuf::from("/tmp"),
            expanded: true,
            setup_script: None,
            worktrees: vec![],
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        };
        save_project(&dir, &proj).unwrap();
        let (_, ps) = load_or_default(&dir);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].slug, "ok");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trip_global_and_project() {
        let dir = std::env::temp_dir().join(format!("imbuia-cfg-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("projects")).unwrap();

        let global = GlobalConfig {
            sidebar_width: 30,
            theme: ThemeKind::Light,
            projects: vec!["foo".into()],
            launchers: Vec::new(),
            gh_poll_interval_secs: None,
        };
        save_global(&dir, &global).unwrap();
        let loaded = load_global(&dir).unwrap();
        assert_eq!(loaded.sidebar_width, 30);
        assert_eq!(loaded.theme, ThemeKind::Light);
        assert_eq!(loaded.projects, vec!["foo"]);

        let proj = ProjectConfig {
            slug: "foo".into(),
            name: "Foo".into(),
            path: PathBuf::from("/tmp/foo"),
            expanded: false,
            setup_script: Some("echo hi".into()),
            worktrees: vec![WorktreeConfig {
                name: "main".into(),
                path: PathBuf::from("/tmp/foo"),
                branch: Some("main".into()),
            }],
            launchers: vec![LauncherConfig {
                name: "claude".into(),
                command: "claude".into(),
            }],
            github_enabled: false,
            gh_poll_interval_secs: None,
        };
        save_project(&dir, &proj).unwrap();
        let loaded = load_project(&dir, "foo").unwrap();
        assert_eq!(loaded.name, "Foo");
        assert!(!loaded.expanded);
        assert_eq!(loaded.path, PathBuf::from("/tmp/foo"));
        assert_eq!(loaded.worktrees.len(), 1);
        assert_eq!(loaded.worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(loaded.slug, "foo");
        assert_eq!(loaded.setup_script.as_deref(), Some("echo hi"));
        assert_eq!(loaded.launchers.len(), 1);
        assert_eq!(loaded.launchers[0].name, "claude");
        assert_eq!(loaded.launchers[0].command, "claude");

        fs::remove_dir_all(&dir).unwrap();
    }
}
