//! Ex-style command registry (`:q`, `:open`, `:set key=value`, …).
//!
//! Each command is a `CmdSpec` entry in the `COMMANDS` table. Adding a new
//! one is just appending to the table and writing a `fn(...)` handler.

use crate::app::{AppState, Command, Commands, EditPopup, InputPopup, PopupAction, UsagePopup};
use crate::reducer::{
    close_current_tab, launch_in_active_worktree, open_new_tab_in_active, set_sidebar_width,
};
use crate::theme::{Theme, ThemeKind};
use std::path::PathBuf;

/// Handler for an ex-style command. Mutates state and may emit side-effecting
/// commands; reports errors by setting `state.command_status`.
type CmdHandler = fn(&mut AppState, args: &[&str], cmds: &mut Commands);

/// Specification for an ex-style command. Adding a new command is just adding
/// an entry to `COMMANDS` below.
pub struct CmdSpec {
    pub names: &'static [&'static str],
    pub usage: &'static str,
    pub description: &'static str,
    pub handler: CmdHandler,
}

pub const COMMANDS: &[CmdSpec] = &[
    CmdSpec {
        names: &["q", "quit"],
        usage: ":q[uit]",
        description: "Quit imbuia.",
        handler: cmd_quit,
    },
    CmdSpec {
        names: &["tabnew"],
        usage: ":tabnew",
        description: "Open a new terminal in the active worktree.",
        handler: cmd_tabnew,
    },
    CmdSpec {
        names: &["tabclose"],
        usage: ":tabclose",
        description: "Close the current terminal.",
        handler: cmd_tabclose,
    },
    CmdSpec {
        names: &["set"],
        usage: ":set key=value",
        description: "Set an option (e.g. :set sidebar.width=24).",
        handler: cmd_set,
    },
    CmdSpec {
        names: &["help", "h"],
        usage: ":help",
        description: "Show this list of commands. Press Esc to close.",
        handler: cmd_help,
    },
    CmdSpec {
        names: &["open"],
        usage: ":open [path]",
        description: "Open a project at <path>. With no arg, opens an input popup.",
        handler: cmd_open,
    },
    CmdSpec {
        names: &["worktree", "wt"],
        usage: ":worktree [branch]",
        description: "Create a worktree for <branch> in the selected project.",
        handler: cmd_worktree,
    },
    CmdSpec {
        names: &["edit", "e"],
        usage: ":edit",
        description: "Open the selected project's TOML in $EDITOR.",
        handler: cmd_edit,
    },
    CmdSpec {
        names: &["worktree-remove", "wr"],
        usage: ":worktree-remove",
        description: "Delete the selected worktree (files + local branch).",
        handler: cmd_worktree_remove,
    },
    CmdSpec {
        names: &["import"],
        usage: ":import",
        description: "Import any git worktrees of the selected project not already in imbuia.",
        handler: cmd_import,
    },
    CmdSpec {
        names: &["restart-supervisor", "rs"],
        usage: ":restart-supervisor",
        description: "Kill the PTY supervisor and all its sessions; respawn on next launch.",
        handler: cmd_restart_supervisor,
    },
    CmdSpec {
        names: &["usage", "u"],
        usage: ":usage",
        description: "Show live memory/CPU usage per session + its descendants.",
        handler: cmd_usage,
    },
    CmdSpec {
        names: &["reconnect", "rc"],
        usage: ":reconnect",
        description: "Reconnect the selected project's (disconnected) remote supervisor.",
        handler: cmd_reconnect,
    },
    CmdSpec {
        names: &["launch", "l"],
        usage: ":launch [name]",
        description: "Launch a named command in a new tab (or pick from a popup).",
        handler: cmd_launch,
    },
    CmdSpec {
        names: &["update"],
        usage: ":update [check]",
        description: "Install the latest release; `check` only re-runs the version check.",
        handler: cmd_update,
    },
    CmdSpec {
        names: &["gh-enable"],
        usage: ":gh-enable",
        description: "Re-enable GitHub PR status indicators (default on).",
        handler: cmd_gh_enable,
    },
    CmdSpec {
        names: &["gh-disable"],
        usage: ":gh-disable",
        description: "Disable GitHub PR status indicators for the selected project.",
        handler: cmd_gh_disable,
    },
    CmdSpec {
        names: &["gh-refresh"],
        usage: ":gh-refresh",
        description: "Force a PR status refresh for the selected project now.",
        handler: cmd_gh_refresh,
    },
    CmdSpec {
        names: &["palette"],
        usage: ":palette",
        description: "Open the command palette (also Ctrl-P).",
        handler: cmd_palette,
    },
    CmdSpec {
        names: &["log-path"],
        usage: ":log-path",
        description: "Show the path to the client log file (tail -f to debug).",
        handler: cmd_log_path,
    },
];

pub(crate) fn execute_command(state: &mut AppState, s: &str, cmds: &mut Commands) {
    if s.is_empty() {
        return;
    }
    let parts: Vec<&str> = s.split_whitespace().collect();
    let Some((name, args)) = parts.split_first() else {
        return;
    };
    if let Some(spec) = COMMANDS.iter().find(|s| s.names.contains(name)) {
        (spec.handler)(state, args, cmds);
    } else {
        state.command_status = Some(format!("not a command: :{s}"));
    }
}

pub(crate) fn cmd_usage(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    state.usage_popup = Some(UsagePopup::new(state.supervisors.clone()));
    cmds.push(Command::SubscribeUsage);
}

fn cmd_reconnect(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    use crate::app::LOCAL;
    let Some((pi, _)) = state.sidebar_selection else {
        state.command_status = Some("select a project first".into());
        return;
    };
    let Some(project) = state.projects.get(pi) else {
        return;
    };
    let sup = project.supervisor;
    if sup == LOCAL {
        state.command_status = Some("local supervisor is always connected".into());
        return;
    }
    let name = state.supervisors.name_of(sup).to_string();
    if state.supervisors.is_connected(sup) {
        state.command_status = Some(format!("supervisor '{name}' already connected"));
        return;
    }
    state.command_status = Some(format!("reconnecting to '{name}'…"));
    cmds.push(Command::ReconnectSupervisor(sup));
}

fn cmd_restart_supervisor(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    state.command_status = Some("restarting supervisor — all sessions killed".into());
    // Drop every local session reference; the supervisor side terminates
    // the children when it processes Shutdown.
    state.sessions.clear();
    for p in &mut state.projects {
        for w in &mut p.worktrees {
            w.sessions.clear();
            w.active_tab = None;
        }
    }
    cmds.push(Command::RestartSupervisor);
    // Exit; on next launch the client will see no socket and spawn a fresh
    // supervisor. (Restarting in-place would require teaching the client
    // reader thread to reconnect — a future iteration.)
    state.running = false;
}

fn cmd_quit(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    state.running = false;
}

fn cmd_tabnew(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    open_new_tab_in_active(state, cmds);
}

fn cmd_tabclose(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    close_current_tab(state, cmds);
}

fn cmd_set(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    handle_set(state, args, cmds);
}

fn cmd_help(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    state.help_open = true;
}

pub(crate) fn cmd_open(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    if let Some(path) = args.first() {
        // `:open <path>` is the quick form — always targets the local
        // supervisor. `~`/relative resolution happens supervisor-side now.
        state.pending_op = Some(format!("Opening {path}…"));
        cmds.push(Command::OpenProject {
            supervisor: crate::app::LOCAL,
            path: PathBuf::from(path),
            setup_script: None,
            import_existing: false,
        });
    } else {
        use crate::app::{LOCAL, OpenProjectFocus, OpenProjectPopup};
        let mut script = ratatui_textarea::TextArea::default();
        script.set_placeholder_text(
            "optional: bash run in each new worktree on creation (Tab to focus, Ctrl-S to save)",
        );
        state.open_project_popup = Some(OpenProjectPopup {
            path: String::new(),
            script,
            focus: OpenProjectFocus::default(),
            import_existing: false,
            supervisor: LOCAL,
            browser: None,
        });
        // Kick off an initial directory listing (the supervisor's home) so the
        // browser has something to show.
        cmds.push(Command::ListDir {
            supervisor: LOCAL,
            path: None,
        });
    }
}

pub(crate) fn cmd_worktree(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    let Some(project_idx) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    let p = &state.projects[project_idx];
    let project_name = p.name.clone();
    let repo_path = p.repo_path.clone();
    if let Some(branch) = args.first() {
        state.pending_op = Some(format!("Creating worktree '{branch}'…"));
        cmds.push(Command::AddWorktree {
            project_idx,
            repo_path,
            branch: (*branch).to_string(),
        });
    } else {
        state.popup = Some(InputPopup {
            title: format!("New worktree (project: {project_name})"),
            prompt: "branch".into(),
            buffer: String::new(),
            action: PopupAction::NewWorktree { project_idx },
        });
    }
}

/// Resolve the project the sidebar is "on". Prefers an explicit selection;
/// falls back to the active worktree's project.
fn selected_project_idx(state: &AppState) -> Option<usize> {
    if let Some((p, _)) = state.sidebar_selection
        && state.projects.get(p).is_some()
    {
        return Some(p);
    }
    state.active_worktree.map(|(p, _)| p)
}

fn cmd_import(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    let Some(pi) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    let repo_path = state.projects[pi].repo_path.clone();
    state.pending_op = Some("Importing existing worktrees…".into());
    cmds.push(Command::ImportWorktrees {
        project_idx: pi,
        repo_path,
    });
}

pub(crate) fn cmd_worktree_remove(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    let Some((pi, Some(wi))) = state.sidebar_selection else {
        state.command_status = Some("select a worktree in the sidebar first".into());
        return;
    };
    let Some(project) = state.projects.get(pi) else {
        return;
    };
    let Some(wt) = project.worktrees.get(wi) else {
        return;
    };
    if wt.path == project.repo_path {
        state.command_status = Some("can't remove the main worktree".into());
        return;
    }
    state.pending_confirm = Some(crate::app::PendingConfirm::RemoveWorktree {
        project_idx: pi,
        worktree_idx: wi,
        name: wt.name.clone(),
        repo_path: project.repo_path.clone(),
        dest_path: wt.path.clone(),
        branch: wt.branch.clone(),
    });
}

pub(crate) fn cmd_edit(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    let Some(project_idx) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    let project = &state.projects[project_idx];
    let initial = project.setup_script.clone().unwrap_or_default();
    let lines: Vec<String> = if initial.is_empty() {
        vec![String::new()]
    } else {
        initial.split('\n').map(str::to_string).collect()
    };
    let mut textarea = ratatui_textarea::TextArea::new(lines);
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    let title = format!("Setup script — {} (Ctrl-S save · Esc cancel)", project.name);
    state.edit_popup = Some(EditPopup {
        project_idx,
        title,
        textarea,
    });
}

fn handle_set(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    // Accept `:set key=value` or `:set key value`.
    let (key, value): (String, String) = match args {
        [single] => match single.split_once('=') {
            Some((k, v)) => (k.trim().into(), v.trim().into()),
            None => {
                state.command_status = Some("usage: :set key=value".into());
                return;
            }
        },
        [k, v] => ((*k).into(), (*v).into()),
        _ => {
            state.command_status = Some("usage: :set key=value".into());
            return;
        }
    };

    match key.as_str() {
        "sidebar.width" => match value.parse::<u16>() {
            Ok(w) => set_sidebar_width(state, w, cmds),
            Err(_) => state.command_status = Some(format!("invalid width: {value}")),
        },
        "theme" => match ThemeKind::parse(&value) {
            Some(kind) => {
                state.theme = Theme::for_kind(kind);
                cmds.push(Command::SaveGlobalConfig);
            }
            None => {
                state.command_status = Some(format!(
                    "invalid theme: {value} (expected dark|light|gruber_darker)"
                ))
            }
        },
        _ => state.command_status = Some(format!("unknown setting: {key}")),
    }
}

pub(crate) fn cmd_launch(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    use crate::app::{LaunchEntry, LaunchPopup, LaunchSource};

    let Some((pi, wi)) = state.active_worktree else {
        state.command_status = Some("no active worktree — pick one first".into());
        return;
    };

    if let Some(name) = args.first() {
        // Direct launch by name. "terminal"/"t" is the always-available plain
        // shell; everything else looks up project launchers first, then the
        // global fallback list.
        let cmd_text = if name.eq_ignore_ascii_case("terminal") || *name == "t" {
            None
        } else {
            let project_match = state
                .projects
                .get(pi)
                .and_then(|p| {
                    p.launchers
                        .iter()
                        .find(|l| l.name.eq_ignore_ascii_case(name))
                })
                .map(|l| l.command.clone());
            let resolved = project_match.or_else(|| {
                state
                    .global_launchers
                    .iter()
                    .find(|l| l.name.eq_ignore_ascii_case(name))
                    .map(|l| l.command.clone())
            });
            match resolved {
                Some(c) => Some(c),
                None => {
                    state.command_status = Some(format!("no launcher named '{name}'"));
                    return;
                }
            }
        };
        launch_in_active_worktree(state, cmd_text, cmds);
        return;
    }

    // No arg → open the picker. Order: Terminal first, then project launchers,
    // then global launchers (deduped by case-insensitive name; the project
    // entry wins when a name collides — closer scope beats wider scope).
    let mut entries = vec![LaunchEntry {
        label: "Terminal".into(),
        command: None,
        source: LaunchSource::Builtin,
    }];
    let mut seen: Vec<String> = Vec::new();
    if let Some(project) = state.projects.get(pi) {
        for l in &project.launchers {
            entries.push(LaunchEntry {
                label: l.name.clone(),
                command: Some(l.command.clone()),
                source: LaunchSource::Project,
            });
            seen.push(l.name.to_ascii_lowercase());
        }
    }
    for l in &state.global_launchers {
        if seen.contains(&l.name.to_ascii_lowercase()) {
            continue;
        }
        entries.push(LaunchEntry {
            label: l.name.clone(),
            command: Some(l.command.clone()),
            source: LaunchSource::Global,
        });
    }
    state.launch_popup = Some(LaunchPopup {
        project_idx: pi,
        worktree_idx: wi,
        entries,
        cursor: 0,
    });
}

fn cmd_gh_enable(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    if !crate::github::gh_available() {
        state.command_status = Some("`gh` not found in PATH — install GitHub CLI first".into());
        return;
    }
    let Some(pi) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    state.projects[pi].github_enabled = true;
    state.command_status = Some("GitHub PR status enabled".into());
    let repo_path = state.projects[pi].repo_path.clone();
    let worktrees = state.projects[pi]
        .worktrees
        .iter()
        .enumerate()
        .map(|(wi, w)| (wi, w.path.clone()))
        .collect();
    cmds.push(Command::SaveProjectConfig(pi));
    cmds.push(Command::FetchPrStatuses {
        project_idx: pi,
        repo_path,
        worktrees,
    });
}

fn cmd_gh_disable(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    let Some(pi) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    state.projects[pi].github_enabled = false;
    state.pr_statuses.retain(|(p, _), _| *p != pi);
    state.command_status = Some("GitHub PR status disabled".into());
    cmds.push(Command::SaveProjectConfig(pi));
}

fn cmd_palette(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    crate::reducer::open_palette(state);
}

fn cmd_log_path(state: &mut AppState, _args: &[&str], _cmds: &mut Commands) {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    let path = cache.join("imbuia").join("imbuia.log");
    state.command_status = Some(format!("log: {}", path.display()));
}

fn cmd_gh_refresh(state: &mut AppState, _args: &[&str], cmds: &mut Commands) {
    let Some(pi) = selected_project_idx(state) else {
        state.command_status = Some("select a project in the sidebar first".into());
        return;
    };
    if !state.projects[pi].github_enabled {
        state.command_status = Some("GitHub integration disabled — run :gh-enable first".into());
        return;
    }
    if !crate::github::gh_available() {
        state.command_status = Some("`gh` not found in PATH".into());
        return;
    }
    let repo_path = state.projects[pi].repo_path.clone();
    let worktrees: Vec<_> = state.projects[pi]
        .worktrees
        .iter()
        .enumerate()
        .map(|(wi, w)| (wi, w.path.clone()))
        .collect();
    state.command_status = Some("refreshing PR status…".into());
    state.pr_refresh_in_flight = true;
    cmds.push(Command::FetchPrStatuses {
        project_idx: pi,
        repo_path,
        worktrees,
    });
}

pub(crate) fn cmd_update(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    use crate::app::UpdateStatus;
    if state.update_status == UpdateStatus::Installing {
        state.command_status = Some("update already in progress".into());
        return;
    }
    // `:update check` only re-runs the version check — never auto-installs.
    if matches!(args.first(), Some(arg) if arg.eq_ignore_ascii_case("check")) {
        state.auto_install_after_check = false;
        state.update_status = UpdateStatus::Checking;
        state.command_status = Some("checking for updates…".into());
        cmds.push(Command::CheckForUpdate);
        return;
    }
    // `:update` no-arg: install if we already know of one, else kick a check
    // and let `Action::UpdateChecked(Ok(Some(_)))` auto-install when it lands.
    if let Some(info) = state.available_update.clone() {
        state.update_status = UpdateStatus::Installing;
        state.command_status = Some(format!("installing {}…", info.latest_tag));
        cmds.push(Command::InstallUpdate {
            tag: info.latest_tag,
        });
        return;
    }
    state.auto_install_after_check = true;
    state.update_status = UpdateStatus::Checking;
    state.command_status = Some("checking for updates…".into());
    cmds.push(Command::CheckForUpdate);
}
