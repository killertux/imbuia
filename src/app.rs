use crate::ipc::UsageReport;
use crate::keybinds::{Chord, KeyMap};
use crate::layout::{DEFAULT_SIDEBAR_WIDTH, TermSize};
use crate::session::{Session, SessionId};
use crate::theme::Theme;
use crossterm::event::{KeyEvent, MouseEvent};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Terminal,
    Command,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum UiFocus {
    #[default]
    Sidebar,
    Terminal,
}

/// One row in the sidebar tree: a project header or a worktree leaf.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Project(usize),
    Worktree(usize, usize),
}

pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub sessions: Vec<SessionId>,
    pub active_tab: Option<usize>,
}

pub struct Project {
    pub slug: String,
    pub name: String,
    pub repo_path: PathBuf,
    pub worktrees: Vec<Worktree>,
    pub expanded: bool,
    /// Optional bash script executed in each new worktree on creation.
    pub setup_script: Option<String>,
    /// Named launchers: label + command to feed the PTY at spawn. Edited via
    /// the TOML for now; surfaced through `:launch` and the launch popup.
    pub launchers: Vec<Launcher>,
    /// Opt-in GitHub PR-status integration (`:gh-enable`). When `true`, the
    /// runtime periodically shells out to `gh pr list` and populates
    /// [`AppState::pr_statuses`] for this project's worktrees.
    pub github_enabled: bool,
    /// Per-project override for the polling cadence (seconds). Falls back to
    /// the global setting, then a hardcoded default.
    pub gh_poll_interval_secs: Option<u64>,
}

/// Lifecycle phase of the GitHub PR associated with a worktree's branch.
///
/// Precedence (highest first): `Merged`, `Failed`, `ChangesRequested`,
/// `Running`, `Approved`, `Open`. "No PR" is the absence of an entry in
/// `AppState::pr_statuses`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrStatus {
    /// PR is open, CI green (or none), still awaiting review.
    Open,
    /// PR open and already approved; CI green. "Ready to merge".
    Approved,
    /// PR open; CI has pending or in-progress checks.
    Running,
    /// PR open; at least one CI check failed, OR the branch has merge
    /// conflicts against base.
    Failed,
    /// PR open; reviewer marked CHANGES_REQUESTED.
    ChangesRequested,
    /// PR has been merged.
    Merged,
}

#[derive(Clone, Debug)]
pub struct Launcher {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct InputPopup {
    pub title: String,
    pub prompt: String,
    pub buffer: String,
    pub action: PopupAction,
}

#[derive(Clone, Debug)]
pub enum PopupAction {
    NewWorktree { project_idx: usize },
}

/// Inline confirmation shown in the action bar (no modal popup). Answered with
/// `y`/`n` in Normal mode; `Esc` cancels.
#[derive(Clone, Debug)]
pub enum PendingConfirm {
    RemoveWorktree {
        project_idx: usize,
        worktree_idx: usize,
        name: String,
        repo_path: PathBuf,
        dest_path: PathBuf,
        branch: Option<String>,
    },
}

/// Multi-line edit popup driven by `ratatui-textarea`. Currently used by
/// `:edit` to edit the selected project's `setup_script`.
pub struct EditPopup {
    pub project_idx: usize,
    pub title: String,
    pub textarea: ratatui_textarea::TextArea<'static>,
}

/// "Open project" popup: a path input plus an optional setup-script textarea.
/// Tab toggles which field has focus; Enter on the path field or Ctrl-S
/// submits; Esc cancels. Letting the user pre-fill the setup script here
/// saves a follow-up `:edit` for the common case.
pub struct OpenProjectPopup {
    pub path: String,
    pub script: ratatui_textarea::TextArea<'static>,
    pub focus: OpenProjectFocus,
    /// When `true`, after the project is opened the runtime enumerates the
    /// repo's existing git worktrees and adds any not already in the
    /// project. Toggled with Space / Enter while the Import row has focus.
    pub import_existing: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum OpenProjectFocus {
    #[default]
    Path,
    Script,
    Import,
}

/// Modal picker for `:launch` — shows the project's configured launchers plus
/// a "Terminal" entry that opens a plain shell. j/k or ↑/↓ to move; Enter to
/// spawn into the active worktree; Esc cancels.
pub struct LaunchPopup {
    pub project_idx: usize,
    pub worktree_idx: usize,
    pub entries: Vec<LaunchEntry>,
    pub cursor: u16,
}

#[derive(Clone, Debug)]
pub struct LaunchEntry {
    pub label: String,
    /// `None` is the always-present "Terminal" entry (plain shell).
    pub command: Option<String>,
    pub source: LaunchSource,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LaunchSource {
    /// Built-in entry (the `Terminal` row).
    Builtin,
    /// Defined on the active project.
    Project,
    /// Defined in the global config; falls back when no project entry shadows it.
    Global,
}

/// Phase of the auto-update flow. Drives the banner in the action bar and
/// gates re-entry from `:update`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    Installing,
    /// Install finished; user needs to restart to switch over.
    InstalledPendingRestart,
}

/// Modal "resource usage" dashboard. Driven by 1 Hz `Usage` frames from the
/// supervisor while open.
#[derive(Debug, Clone)]
pub struct UsagePopup {
    pub report: Option<UsageReport>,
    /// SessionIds whose subtree is expanded.
    pub expanded: std::collections::HashSet<SessionId>,
    /// Cursor row (index into the currently-visible flat row list).
    pub cursor: u16,
}

impl UsagePopup {
    pub fn new() -> Self {
        Self {
            report: None,
            expanded: std::collections::HashSet::new(),
            cursor: 0,
        }
    }
}

/// Autocomplete state for the `:` command line.
#[derive(Clone, Debug, Default)]
pub struct CommandCompletion {
    /// Canonical command names (the first alias of each matching `CmdSpec`),
    /// plus their descriptions. Empty means no popover is shown.
    pub matches: Vec<(&'static str, &'static str)>,
    /// Currently highlighted entry. `None` means no selection yet — Enter
    /// still executes the buffer as typed.
    pub selected: Option<usize>,
}

pub struct AppState {
    pub running: bool,
    pub sessions: HashMap<SessionId, Arc<dyn Session>>,
    pub projects: Vec<Project>,
    /// Cursor in the sidebar: (project_idx, Some(worktree_idx)) on a worktree,
    /// or (project_idx, None) on a project header.
    pub sidebar_selection: Option<(usize, Option<usize>)>,
    /// Currently active worktree whose tabs are shown in the tab bar.
    pub active_worktree: Option<(usize, usize)>,
    pub sidebar_width: u16,
    /// First visible row of the sidebar tree (scroll offset).
    pub sidebar_scroll: u16,
    /// Chord accumulator for the keymap matcher. Empty when no multi-key
    /// chord is in progress. Replaces the old `Leader` enum — each leader
    /// is just the first chord of a binding now.
    pub pending_chord: SmallVec<[Chord; 4]>,
    /// When matching a chord in `Mode::Terminal`, the raw KeyEvents that
    /// would otherwise go straight to the PTY are buffered here. If the
    /// chord matches an allow-listed action they're discarded; if it
    /// fails, they're replayed to the PTY in order.
    pub pending_terminal_keys: SmallVec<[KeyEvent; 4]>,
    /// Resolved keymap (defaults overlaid with user bindings). Loaded once
    /// at startup and not mutated after.
    pub keymap: Arc<KeyMap>,
    /// Raw user keybind table from the config toml. Preserved verbatim so
    /// `Command::SaveGlobalConfig` round-trips without rewriting the user's
    /// formatting.
    pub keybinds_config: BTreeMap<String, String>,
    pub term_size: TermSize,
    pub mode: Mode,
    /// Command-mode input buffer (the part after `:`).
    pub command: String,
    /// Feedback from the most recent command (errors, hints). Cleared on `:`.
    pub command_status: Option<String>,
    /// Autocomplete suggestions for the current command-line buffer. `Some`
    /// only while in `Mode::Command` and the user is editing the first token.
    pub command_completion: Option<CommandCompletion>,
    pub ui_focus: UiFocus,
    /// Whether the help popup is showing. Dismissed by Esc or mouse click.
    pub help_open: bool,
    /// Vertical scroll offset (rows) inside the help popup. The renderer
    /// writes the last computed `max_scroll` back to [`Self::help_max_scroll`]
    /// each frame so the reducer can clamp `j`/wheel events without
    /// duplicating the help-content layout.
    pub help_scroll: u16,
    /// Last `max_scroll` written by the help renderer. Read by the reducer
    /// to clamp scroll events. Zero when help is closed or fits without
    /// scrolling. Behind a `Cell` so the renderer can update it via
    /// `&AppState`.
    pub help_max_scroll: std::cell::Cell<u16>,
    /// Active labelled-input popup (`:open` / `:worktree`), if any.
    pub popup: Option<InputPopup>,
    /// Active multi-line edit popup (`:edit`), if any.
    pub edit_popup: Option<EditPopup>,
    /// Active "open project" popup with path + setup-script fields.
    pub open_project_popup: Option<OpenProjectPopup>,
    /// Active launcher picker, if any.
    pub launch_popup: Option<LaunchPopup>,
    /// Active usage popup (`:usage`), if any.
    pub usage_popup: Option<UsagePopup>,
    /// `~/.config/imbuia` (or XDG equivalent). Resolved once at startup.
    pub config_dir: PathBuf,
    /// Description of the currently running async operation (open project,
    /// add worktree). Cleared when the result action arrives.
    pub pending_op: Option<String>,
    /// In-flight action bar confirmation (e.g. worktree delete). Mutually
    /// exclusive with normal Normal-mode key handling: while `Some`, keys are
    /// routed to the y/n/Esc prompt.
    pub pending_confirm: Option<PendingConfirm>,
    /// Active color palette. Persisted in the global config.
    pub theme: Theme,
    /// Cross-project launchers loaded from `config.toml`. Merged with the
    /// selected project's launchers at `:launch` time; project entries with
    /// the same name take precedence.
    pub global_launchers: Vec<Launcher>,
    /// `Some` once a background check discovers a newer release on GitHub.
    /// Drives the right-aligned banner in the action bar.
    pub available_update: Option<crate::updater::UpdateInfo>,
    pub update_status: UpdateStatus,
    /// `true` when the user typed `:update` (no args) and we had to kick a
    /// check first. The next `Action::UpdateChecked(Ok(Some(_)))` auto-installs
    /// instead of just setting the banner.
    pub auto_install_after_check: bool,
    /// PR status per worktree, keyed positionally as `(project_idx, worktree_idx)`.
    /// Absence means "no PR" or "integration disabled". Updated in the
    /// background by `Command::FetchPrStatuses`; never persisted.
    pub pr_statuses: HashMap<(usize, usize), PrStatus>,
    /// Global default poll interval (seconds) for GitHub PR status. `None`
    /// means use the hardcoded fallback (120s).
    pub gh_poll_interval_secs: Option<u64>,
    /// `true` while a foreground `:gh-refresh` is in flight. Background polls
    /// don't set this — they should never touch `command_status`.
    pub pr_refresh_in_flight: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            running: true,
            sessions: HashMap::new(),
            projects: Vec::new(),
            sidebar_selection: None,
            active_worktree: None,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_scroll: 0,
            pending_chord: SmallVec::new(),
            pending_terminal_keys: SmallVec::new(),
            keymap: Arc::new(crate::keybinds::defaults()),
            keybinds_config: BTreeMap::new(),
            term_size: TermSize::default(),
            mode: Mode::default(),
            command: String::new(),
            command_status: None,
            command_completion: None,
            ui_focus: UiFocus::default(),
            help_open: false,
            help_scroll: 0,
            help_max_scroll: std::cell::Cell::new(0),
            popup: None,
            edit_popup: None,
            open_project_popup: None,
            launch_popup: None,
            usage_popup: None,
            config_dir: PathBuf::new(),
            pending_op: None,
            pending_confirm: None,
            theme: Theme::default(),
            global_launchers: Vec::new(),
            available_update: None,
            update_status: UpdateStatus::Idle,
            auto_install_after_check: false,
            pr_statuses: HashMap::new(),
            gh_poll_interval_secs: None,
            pr_refresh_in_flight: false,
        }
    }

    /// Session currently visible in the terminal pane, if any.
    pub fn focused_session_id(&self) -> Option<SessionId> {
        let (p, w) = self.active_worktree?;
        let wt = self.projects.get(p)?.worktrees.get(w)?;
        let idx = wt.active_tab?;
        wt.sessions.get(idx).copied()
    }
}

/// Mocked project tree, retained for unit tests only. Real loading goes
/// through `config::load_or_default` at startup.
#[cfg(test)]
pub fn mock_projects() -> Vec<Project> {
    fn wt(name: &str) -> Worktree {
        Worktree {
            name: name.into(),
            path: PathBuf::from("."),
            branch: Some(name.into()),
            sessions: Vec::new(),
            active_tab: None,
        }
    }
    fn proj(slug: &str, name: &str, worktrees: Vec<Worktree>, expanded: bool) -> Project {
        Project {
            slug: slug.into(),
            name: name.into(),
            repo_path: PathBuf::from("."),
            worktrees,
            expanded,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        }
    }
    vec![
        proj("imbuia", "imbuia", vec![wt("main"), wt("feat-x")], true),
        proj("brick", "brick", vec![wt("main"), wt("dev")], true),
        proj("scratch", "scratch", vec![wt("main")], false),
    ]
}

impl Project {
    pub fn from_config(cfg: crate::config::ProjectConfig) -> Self {
        let worktrees = cfg
            .worktrees
            .into_iter()
            .map(|w| Worktree {
                name: w.name,
                path: w.path,
                branch: w.branch,
                sessions: Vec::new(),
                active_tab: None,
            })
            .collect();
        Project {
            slug: cfg.slug,
            name: cfg.name,
            repo_path: cfg.path,
            worktrees,
            expanded: cfg.expanded,
            setup_script: cfg.setup_script,
            launchers: cfg
                .launchers
                .into_iter()
                .map(|l| Launcher {
                    name: l.name,
                    command: l.command,
                })
                .collect(),
            github_enabled: cfg.github_enabled,
            gh_poll_interval_secs: cfg.gh_poll_interval_secs,
        }
    }

    pub fn to_config(&self) -> crate::config::ProjectConfig {
        crate::config::ProjectConfig {
            slug: self.slug.clone(),
            name: self.name.clone(),
            path: self.repo_path.clone(),
            expanded: self.expanded,
            setup_script: self.setup_script.clone(),
            worktrees: self
                .worktrees
                .iter()
                .map(|w| crate::config::WorktreeConfig {
                    name: w.name.clone(),
                    path: w.path.clone(),
                    branch: w.branch.clone(),
                })
                .collect(),
            launchers: self
                .launchers
                .iter()
                .map(|l| crate::config::LauncherConfig {
                    name: l.name.clone(),
                    command: l.command.clone(),
                })
                .collect(),
            github_enabled: self.github_enabled,
            gh_poll_interval_secs: self.gh_poll_interval_secs,
        }
    }
}

pub enum Action {
    /// Emitted by the runtime after `Command::SpawnInWorktree` succeeds.
    SessionSpawned {
        session: Arc<dyn Session>,
        dest: (usize, usize),
    },
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(TermSize),
    SessionExited(SessionId),
    /// Supervisor → reducer (via the client reader): a path validated as a git
    /// repo. The reducer derives the slug (config logic — needs the other
    /// projects' slugs), builds the `Project`, and persists it.
    /// `import_existing` is forwarded from the originating `Command::OpenProject`
    /// — when `true`, the reducer fires an `ImportWorktrees` command once
    /// the project is in `state.projects`.
    ProjectValidated {
        canonical_path: PathBuf,
        repo_name: String,
        head_branch: Option<String>,
        setup_script: Option<String>,
        import_existing: bool,
    },
    /// Runtime → reducer: a worktree finished `git worktree add`.
    WorktreeAdded {
        project_idx: usize,
        worktree: Worktree,
    },
    /// Runtime → reducer: `git worktree remove` completed; drop it from state.
    WorktreeRemoved {
        project_idx: usize,
        worktree_idx: usize,
    },
    /// Runtime → reducer: `git worktree list` returned these entries; the
    /// reducer adds whichever ones aren't already in the project.
    WorktreesImported {
        project_idx: usize,
        entries: Vec<Worktree>,
    },
    /// Runtime → reducer: an async operation failed; show the message.
    OperationFailed(String),
    /// Supervisor connection ended (steal-on-attach, socket EOF, read error).
    /// The reducer wipes the session map and exits — the local PTY state is
    /// gone, and any further writes would silently no-op. Relaunch to attach
    /// to a fresh supervisor.
    SupervisorLost(String),
    /// Supervisor → reducer: a fresh resource-usage snapshot arrived.
    UsageReceived(UsageReport),
    /// Background updater finished a check. `Ok(None)` means "up to date".
    UpdateChecked(Result<Option<crate::updater::UpdateInfo>, String>),
    /// Updater thread finished an install attempt.
    UpdateInstalled(Result<crate::updater::InstallOutcome, String>),
    /// Hourly tick from the runtime telling the reducer to fire a check.
    /// Goes through the reducer (rather than being emitted as a Command
    /// directly) so the reducer remains the single dispatch authority.
    PeriodicUpdateCheck,
    /// Periodic tick from the runtime that asks the reducer to emit one
    /// `FetchPrStatuses` command per gh-enabled project whose poll interval
    /// has elapsed.
    PeriodicPrCheck,
    /// Background fetcher result for a single project: one entry per
    /// worktree-index in the order they were polled. `None` clears the
    /// existing entry (no PR / per-worktree failure).
    PrStatusesFetched {
        project_idx: usize,
        statuses: Vec<(usize, Option<PrStatus>)>,
    },
    /// Background fetcher failure. Surfaced to `command_status` only when a
    /// foreground `:gh-refresh` was in flight; silent otherwise.
    PrFetchFailed {
        project_idx: usize,
        message: String,
    },
    Quit,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::SessionSpawned { session, dest } => f
                .debug_struct("SessionSpawned")
                .field("id", &session.id())
                .field("dest", dest)
                .finish(),
            Action::Key(k) => f.debug_tuple("Key").field(k).finish(),
            Action::Mouse(m) => f.debug_tuple("Mouse").field(m).finish(),
            Action::Paste(s) => f.debug_tuple("Paste").field(&s.len()).finish(),
            Action::Resize(sz) => f.debug_tuple("Resize").field(sz).finish(),
            Action::SessionExited(id) => f.debug_tuple("SessionExited").field(id).finish(),
            Action::ProjectValidated {
                repo_name,
                import_existing,
                ..
            } => f
                .debug_struct("ProjectValidated")
                .field("repo_name", repo_name)
                .field("import_existing", import_existing)
                .finish(),
            Action::WorktreeAdded {
                project_idx,
                worktree,
            } => f
                .debug_struct("WorktreeAdded")
                .field("project_idx", project_idx)
                .field("worktree", &worktree.name)
                .finish(),
            Action::WorktreeRemoved {
                project_idx,
                worktree_idx,
            } => f
                .debug_struct("WorktreeRemoved")
                .field("project_idx", project_idx)
                .field("worktree_idx", worktree_idx)
                .finish(),
            Action::WorktreesImported {
                project_idx,
                entries,
            } => f
                .debug_struct("WorktreesImported")
                .field("project_idx", project_idx)
                .field("count", &entries.len())
                .finish(),
            Action::OperationFailed(s) => f.debug_tuple("OperationFailed").field(s).finish(),
            Action::SupervisorLost(s) => f.debug_tuple("SupervisorLost").field(s).finish(),
            Action::UsageReceived(_) => write!(f, "UsageReceived(..)"),
            Action::UpdateChecked(r) => f.debug_tuple("UpdateChecked").field(r).finish(),
            Action::UpdateInstalled(r) => f.debug_tuple("UpdateInstalled").field(r).finish(),
            Action::PeriodicUpdateCheck => write!(f, "PeriodicUpdateCheck"),
            Action::PeriodicPrCheck => write!(f, "PeriodicPrCheck"),
            Action::PrStatusesFetched {
                project_idx,
                statuses,
            } => f
                .debug_struct("PrStatusesFetched")
                .field("project_idx", project_idx)
                .field("count", &statuses.len())
                .finish(),
            Action::PrFetchFailed {
                project_idx,
                message,
            } => f
                .debug_struct("PrFetchFailed")
                .field("project_idx", project_idx)
                .field("message", message)
                .finish(),
            Action::Quit => write!(f, "Quit"),
        }
    }
}

#[derive(Debug)]
pub enum Command {
    WriteKey(SessionId, KeyEvent),
    WriteMouse(SessionId, MouseEvent),
    /// Forward a paste payload to the PTY. The transport chunks it and wraps it
    /// in bracketed-paste escapes (`\x1b[200~ … \x1b[201~`) only when the inner
    /// app enabled bracketed paste.
    WritePaste(SessionId, String),
    ResizePty(SessionId, u16, u16),
    /// Ask the runtime to spawn a PTY for the given destination worktree.
    /// On success it dispatches `Action::SessionSpawned { dest, .. }`.
    /// `initial_command`, when set, is written verbatim to the PTY immediately
    /// after spawn (followed by a newline) so the shell executes it.
    SpawnInWorktree {
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        dest: (usize, usize),
        initial_command: Option<String>,
        /// Carried verbatim to the supervisor so it can identify which
        /// project/worktree this session belongs to when reporting it back
        /// at attach time. Pure metadata — supervisor doesn't interpret it.
        project_slug: String,
        worktree_name: String,
    },
    /// Ask the supervisor to terminate the given session (e.g. on `:tabclose`).
    KillSession(SessionId),
    /// Send `Shutdown` to the supervisor and wire down the local session map.
    /// The client process keeps running and will auto-spawn a fresh
    /// supervisor on its next attach attempt (today: at next start).
    RestartSupervisor,
    /// Start receiving `UsageReport` frames (1 Hz).
    SubscribeUsage,
    /// Stop receiving usage frames.
    UnsubscribeUsage,
    /// Validate a path (supervisor-side), then build + save a project config
    /// (client-side). Asynchronous: the supervisor replies and the client
    /// reader posts `Action::ProjectValidated` or `Action::OperationFailed`.
    OpenProject {
        path: PathBuf,
        setup_script: Option<String>,
        /// If `true`, the reducer auto-dispatches `ImportWorktrees` once the
        /// project lands. Set by the `[x] Import existing worktrees` toggle
        /// in the open-project popup.
        import_existing: bool,
    },
    /// Run `git worktree list --porcelain` and append every entry not
    /// already in the project. Asynchronous.
    ImportWorktrees {
        project_idx: usize,
        repo_path: PathBuf,
    },
    /// Run `git worktree add` and persist. Asynchronous.
    AddWorktree {
        project_idx: usize,
        repo_path: PathBuf,
        branch: String,
    },
    /// Run `git worktree remove --force` and `git branch -D`. Asynchronous.
    /// On success the runtime emits `Action::WorktreeRemoved`.
    RemoveWorktree {
        project_idx: usize,
        worktree_idx: usize,
        repo_path: PathBuf,
        dest_path: PathBuf,
        branch: Option<String>,
    },
    /// Persist global config (sidebar width + project list).
    SaveGlobalConfig,
    /// Persist a project's config.
    SaveProjectConfig(usize),
    /// Ask the updater to hit GitHub and report back via
    /// [`Action::UpdateChecked`]. Spawned as a thread by the runtime.
    CheckForUpdate,
    /// Install the given release tag in a background thread; result comes
    /// back as [`Action::UpdateInstalled`].
    InstallUpdate {
        tag: String,
    },
    /// For each worktree, resolve its current HEAD live via `git symbolic-ref`
    /// then query `gh pr list --head <branch>` from the project's main repo.
    /// Posts back an [`Action::PrStatusesFetched`]. Spawned on a background
    /// thread.
    FetchPrStatuses {
        project_idx: usize,
        /// CWD for the `gh` invocations (project's main repo dir).
        repo_path: PathBuf,
        /// `(worktree_idx, worktree_cwd)` — branch resolved live in the
        /// background thread, so a `git switch` inside a worktree picks up
        /// on the next refresh without needing to re-open the project.
        worktrees: Vec<(usize, PathBuf)>,
    },
    Shutdown,
}

pub type Commands = SmallVec<[Command; 8]>;
