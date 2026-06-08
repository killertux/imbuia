use crate::layout::DEFAULT_SIDEBAR_WIDTH;
use crate::theme::ThemeKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// User keybinding overrides (action name → vim-style binding string).
    /// Populated with every default on first launch; thereafter the user
    /// owns the table. Unknown action names and unparseable bindings are
    /// logged and ignored — the action falls back to its compile-time
    /// default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keybinds: BTreeMap<String, String>,
    /// When set, the client connects to this remote supervisor over TCP+TLS
    /// instead of spawning/attaching the local Unix-socket supervisor. The
    /// supervisor's public key is pinned on first connect (TOFU) in
    /// `known_hosts`, not stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteConfig>,
}

/// Address of a remote supervisor. `url` is a plain `host:port` (no scheme).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            theme: ThemeKind::default(),
            projects: Vec::new(),
            launchers: Vec::new(),
            gh_poll_interval_secs: None,
            keybinds: BTreeMap::new(),
            remote: None,
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
    /// GitHub PR-status integration. **Enabled by default**; disable with
    /// `:gh-disable`. The default keeps the toml clean — only an explicit
    /// disable gets persisted.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub github_enabled: bool,
    /// Per-project poll cadence override (seconds). Overrides the global
    /// `gh_poll_interval_secs`; `None` defers to the global setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_poll_interval_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
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
    let mut global = load_global(dir).unwrap_or_default();
    // First launch (or any state where keybinds is empty): seed the toml
    // with every default so the user can discover/customise them. Failure
    // to write is logged, not fatal — the in-memory map still has them.
    if global.keybinds.is_empty() {
        global.keybinds = crate::keybinds::defaults_as_config();
        if let Err(e) = save_global(dir, &global) {
            tracing::warn!("seeding default keybinds failed: {e}");
        }
    }
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

// ---------------------------------------------------------------------------
// Remote-transport trust files (live alongside config.toml in the config dir).
// ---------------------------------------------------------------------------

/// PKCS#8 private key for this host's long-lived TLS identity (mode 0600).
/// The same file is used whether the process acts as a client or supervisor.
pub fn identity_path(dir: &Path) -> PathBuf {
    dir.join("identity.key")
}

/// Client-side TOFU store: `host:port <sha256-fingerprint>` per line.
pub fn known_hosts_path(dir: &Path) -> PathBuf {
    dir.join("known_hosts")
}

/// Supervisor-side allow-list: `<sha256-fingerprint>  # optional comment`.
pub fn authorized_keys_path(dir: &Path) -> PathBuf {
    dir.join("authorized_keys")
}

/// Parse a trust file, yielding the first two whitespace tokens of each
/// non-empty, non-`#` line. Missing file → empty. Used for both known_hosts
/// (host, fp) and authorized_keys (fp, _comment).
fn read_trust_lines(path: &Path) -> Vec<(String, Option<String>)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let first = it.next()?.to_string();
            let second = it.next().map(str::to_string);
            Some((first, second))
        })
        .collect()
}

/// Look up the pinned fingerprint for `host` in the client's known_hosts.
pub fn known_host_fingerprint(dir: &Path, host: &str) -> Option<String> {
    read_trust_lines(&known_hosts_path(dir))
        .into_iter()
        .find(|(h, _)| h == host)
        .and_then(|(_, fp)| fp)
}

/// Append a freshly-trusted `host -> fingerprint` to known_hosts (TOFU).
pub fn append_known_host(dir: &Path, host: &str, fingerprint: &str) -> Result<()> {
    use std::io::Write;
    let path = known_hosts_path(dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{host} {fingerprint}").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The set of client fingerprints the supervisor will admit.
pub fn load_authorized_fingerprints(dir: &Path) -> std::collections::HashSet<String> {
    read_trust_lines(&authorized_keys_path(dir))
        .into_iter()
        .map(|(fp, _)| fp)
        .collect()
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
            keybinds: BTreeMap::new(),
            remote: None,
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
            keybinds: BTreeMap::new(),
            remote: None,
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

    #[test]
    fn remote_config_round_trips_in_global() {
        let dir = std::env::temp_dir().join(format!("imbuia-cfg-remote-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                url: "example.com:7777".into(),
            }),
            ..GlobalConfig::default()
        };
        save_global(&dir, &global).unwrap();
        let loaded = load_global(&dir).unwrap();
        assert_eq!(loaded.remote.unwrap().url, "example.com:7777");
        // Absence stays absent (skip_serializing_if).
        let plain = GlobalConfig::default();
        save_global(&dir, &plain).unwrap();
        assert!(load_global(&dir).unwrap().remote.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn known_hosts_tofu_and_authorized_keys() {
        let dir = std::env::temp_dir().join(format!("imbuia-cfg-trust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Unknown host before any append.
        assert_eq!(known_host_fingerprint(&dir, "h:1"), None);
        append_known_host(&dir, "h:1", "abc123").unwrap();
        append_known_host(&dir, "other:2", "def456").unwrap();
        assert_eq!(
            known_host_fingerprint(&dir, "h:1").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            known_host_fingerprint(&dir, "other:2").as_deref(),
            Some("def456")
        );
        assert_eq!(known_host_fingerprint(&dir, "missing:3"), None);

        // authorized_keys: comments + blank lines ignored, comment after fp ok.
        fs::write(
            authorized_keys_path(&dir),
            "# allowed clients\nabc123  # laptop\n\ndef456\n",
        )
        .unwrap();
        let set = load_authorized_fingerprints(&dir);
        assert!(set.contains("abc123"));
        assert!(set.contains("def456"));
        assert!(!set.contains("# allowed clients"));
        assert_eq!(set.len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }
}
