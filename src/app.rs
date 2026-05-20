use crate::ipc::UsageReport;
use crate::layout::{DEFAULT_SIDEBAR_WIDTH, TermSize};
use crate::session::{Session, SessionId};
use crate::theme::Theme;
use crossterm::event::{KeyEvent, MouseEvent};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Pending multi-key leader (vim-style chords).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Leader {
    /// `Ctrl-W` — window operations in Normal mode (sidebar resize today).
    CtrlW,
    /// `Ctrl-\` — Terminal-mode escape sequence (must be followed by `Ctrl-N`).
    CtrlBackslash,
    /// `g` — Normal-mode prefix (e.g. `gt`/`gT` for next/prev tab).
    G,
}

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
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum OpenProjectFocus {
    #[default]
    Path,
    Script,
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
    pub pending_leader: Option<Leader>,
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
    /// Active color palette. Persisted in the global config.
    pub theme: Theme,
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
            pending_leader: None,
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
            theme: Theme::default(),
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
    /// Runtime → reducer: a new project has finished validating & saving.
    /// The persistence layer's TOML schema is *not* leaked here — the runtime
    /// constructs the domain `Project` from its `ProjectConfig` first.
    ProjectOpened(Project),
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
    /// Runtime → reducer: an async operation failed; show the message.
    OperationFailed(String),
    /// Supervisor connection ended (steal-on-attach, socket EOF, read error).
    /// The reducer wipes the session map and exits — the local PTY state is
    /// gone, and any further writes would silently no-op. Relaunch to attach
    /// to a fresh supervisor.
    SupervisorLost(String),
    /// Supervisor → reducer: a fresh resource-usage snapshot arrived.
    UsageReceived(UsageReport),
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
            Action::ProjectOpened(p) => f.debug_tuple("ProjectOpened").field(&p.slug).finish(),
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
            Action::OperationFailed(s) => f.debug_tuple("OperationFailed").field(s).finish(),
            Action::SupervisorLost(s) => f.debug_tuple("SupervisorLost").field(s).finish(),
            Action::UsageReceived(_) => write!(f, "UsageReceived(..)"),
            Action::Quit => write!(f, "Quit"),
        }
    }
}

#[derive(Debug)]
pub enum Command {
    WriteKey(SessionId, KeyEvent),
    WriteMouse(SessionId, MouseEvent),
    /// Forward a paste payload to the PTY wrapped in bracketed-paste
    /// escapes (`\x1b[200~ … \x1b[201~`). A single frame regardless of size.
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
    /// Validate a path, build a project config, save it. Asynchronous: result
    /// comes back as `Action::ProjectOpened` or `Action::OperationFailed`.
    OpenProject {
        path: PathBuf,
        setup_script: Option<String>,
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
    Shutdown,
}

pub type Commands = SmallVec<[Command; 8]>;
