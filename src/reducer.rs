//! Pure reducer: deterministic, side-effect-free, exhaustively unit-tested.
//!
//! The only entry point is `reduce(state, action) -> Commands`. Everything
//! else in this module is a private helper used to keep `reduce` small.

use crate::app::{
    Action, AppState, Command, Commands, InputPopup, Mode, PendingConfirm, PopupAction, SidebarRow,
    UiFocus, Worktree,
};
use crate::keybinds::{BindableAction, Chord, MatchResult, Scope};
use crate::layout::{ChromeRects, DEFAULT_SIDEBAR_WIDTH, chrome, clamp_sidebar_width};
use crate::session::SessionId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

pub fn reduce(state: &mut AppState, action: Action) -> Commands {
    let mut cmds = Commands::new();
    match action {
        Action::SessionSpawned { session, dest } => {
            let id = session.id();
            if let Some(wt) = worktree_mut(state, dest) {
                wt.sessions.push(id);
                wt.active_tab = Some(wt.sessions.len() - 1);
                state.sessions.insert(id, session);
            } else {
                // The destination worktree was removed between request_spawn
                // and Spawned arrival. Kill the orphan PTY upstream and don't
                // register it locally — otherwise it lingers forever with no
                // UI to close it.
                cmds.push(Command::KillSession(id));
            }
        }
        Action::Key(k) => handle_key(state, k, &mut cmds),
        Action::Mouse(m) => handle_mouse(state, m, &mut cmds),
        Action::Paste(text) => handle_paste(state, text, &mut cmds),
        Action::Resize(size) => {
            state.term_size = size;
            broadcast_resize(state, &mut cmds);
        }
        Action::SessionExited(id) => {
            state.sessions.remove(&id);
            remove_session_from_worktrees(state, id);
        }
        Action::ProjectValidated {
            supervisor,
            canonical_path,
            repo_name,
            head_branch,
            setup_script,
            import_existing,
        } => {
            state.pending_op = None;
            // Slug computation is config logic — it needs the *other* projects'
            // slugs, which only the client/reducer knows. The supervisor just
            // validated the path + read HEAD; we build & persist here.
            let existing: Vec<String> = state.projects.iter().map(|p| p.slug.clone()).collect();
            let slug = crate::config::compute_slug(&repo_name, &existing);
            let main = crate::config::WorktreeConfig {
                name: head_branch.clone().unwrap_or_else(|| "main".into()),
                path: canonical_path.clone(),
                branch: head_branch,
            };
            let cfg = crate::config::ProjectConfig {
                slug,
                name: repo_name,
                path: canonical_path,
                supervisor: state.supervisors.config_name(supervisor),
                expanded: true,
                setup_script,
                worktrees: vec![main],
                launchers: Vec::new(),
                github_enabled: true,
                gh_poll_interval_secs: None,
            };
            let project = crate::app::Project::from_config(cfg, supervisor);
            let repo_path = project.repo_path.clone();
            state.projects.push(project);
            let new_idx = state.projects.len() - 1;
            if state.sidebar_selection.is_none() {
                state.sidebar_selection = Some((new_idx, None));
            }
            cmds.push(Command::SaveGlobalConfig);
            cmds.push(Command::SaveProjectConfig(new_idx));
            if import_existing {
                state.pending_op = Some("Importing existing worktrees…".into());
                cmds.push(Command::ImportWorktrees {
                    project_idx: new_idx,
                    repo_path,
                });
            }
        }
        Action::WorktreesImported {
            project_idx,
            entries,
        } => {
            state.pending_op = None;
            let Some(project) = state.projects.get_mut(project_idx) else {
                return cmds;
            };
            let mut added = 0usize;
            for wt in entries {
                // Dedup by path — the main worktree is already in the project.
                if project.worktrees.iter().any(|w| w.path == wt.path) {
                    continue;
                }
                project.worktrees.push(wt);
                added += 1;
            }
            if added > 0 {
                cmds.push(Command::SaveProjectConfig(project_idx));
                state.command_status = Some(format!(
                    "imported {added} worktree{}",
                    if added == 1 { "" } else { "s" }
                ));
            } else {
                state.command_status = Some("no new worktrees to import".into());
            }
        }
        Action::WorktreeAdded {
            project_idx,
            worktree,
        } => {
            state.pending_op = None;
            state.command_status = None;
            let term = chrome(state.term_size.as_rect(), state.sidebar_width).terminal;
            if let Some(p) = state.projects.get_mut(project_idx) {
                let cwd = worktree.path.clone();
                let script = p.setup_script.clone();
                let slug = p.slug.clone();
                let supervisor = p.supervisor;
                let wt_name = worktree.name.clone();
                p.worktrees.push(worktree);
                let wi = p.worktrees.len() - 1;
                cmds.push(Command::SaveProjectConfig(project_idx));
                // Auto-activate the new worktree and spawn a terminal in it.
                // If the project has a setup_script, write it to the PTY so it
                // runs as the user's first command — they see the output live.
                state.active_worktree = Some((project_idx, wi));
                state.sidebar_selection = Some((project_idx, Some(wi)));
                // Make sure the new row is in view if the sidebar was scrolled.
                let rows = sidebar_visible_rows(state);
                if let Some(row_idx) = rows
                    .iter()
                    .position(|r| *r == SidebarRow::Worktree(project_idx, wi))
                {
                    ensure_selection_visible(state, row_idx as u16);
                }
                cmds.push(Command::SpawnInWorktree {
                    supervisor,
                    rows: term.height,
                    cols: term.width,
                    cwd,
                    dest: (project_idx, wi),
                    initial_command: script,
                    project_slug: slug,
                    worktree_name: wt_name,
                });
            }
        }
        Action::WorktreeRemoved {
            project_idx,
            worktree_idx,
        } => {
            state.pending_op = None;
            let removed_name = state
                .projects
                .get(project_idx)
                .and_then(|p| p.worktrees.get(worktree_idx))
                .map(|w| w.name.clone());
            // Sessions were already killed at confirm time (see
            // `PendingConfirm::RemoveWorktree` handler) so the worktree's
            // `sessions` vec is empty by the time we get here.
            if let Some(p) = state.projects.get_mut(project_idx)
                && worktree_idx < p.worktrees.len()
            {
                p.worktrees.remove(worktree_idx);
            }
            // Fix up active_worktree / sidebar_selection that pointed at it.
            if let Some((pi, wi)) = state.active_worktree
                && pi == project_idx
                && wi >= worktree_idx
            {
                let wt_count = state
                    .projects
                    .get(pi)
                    .map(|p| p.worktrees.len())
                    .unwrap_or(0);
                state.active_worktree = if wt_count == 0 || wi == worktree_idx {
                    None
                } else {
                    Some((pi, wi - 1))
                };
            }
            if let Some((pi, Some(wi))) = state.sidebar_selection
                && pi == project_idx
                && wi >= worktree_idx
            {
                let wt_count = state
                    .projects
                    .get(pi)
                    .map(|p| p.worktrees.len())
                    .unwrap_or(0);
                state.sidebar_selection = if wt_count == 0 {
                    Some((pi, None))
                } else if wi == worktree_idx {
                    let next = wi.min(wt_count - 1);
                    Some((pi, Some(next)))
                } else {
                    Some((pi, Some(wi - 1)))
                };
            }
            // Drop the removed worktree's PR status and shift later indices
            // for the same project down by one, mirroring the active_worktree
            // re-key above. pr_statuses keys are positional like (pi, wi).
            state.pr_statuses.remove(&(project_idx, worktree_idx));
            let to_shift: Vec<usize> = state
                .pr_statuses
                .keys()
                .filter_map(|(pi, wi)| (*pi == project_idx && *wi > worktree_idx).then_some(*wi))
                .collect();
            for wi in to_shift {
                if let Some(status) = state.pr_statuses.remove(&(project_idx, wi)) {
                    state.pr_statuses.insert((project_idx, wi - 1), status);
                }
            }
            cmds.push(Command::SaveProjectConfig(project_idx));
            if let Some(name) = removed_name {
                state.command_status = Some(format!("removed worktree '{name}'"));
            }
        }
        Action::OperationFailed(msg) => {
            state.pending_op = None;
            state.command_status = Some(msg);
        }
        Action::SupervisorLost(sup, reason) => {
            if sup == crate::app::LOCAL {
                // The local supervisor's PTYs are gone — nothing to attach to;
                // wipe everything and exit, as before.
                state.sessions.clear();
                for p in &mut state.projects {
                    for w in &mut p.worktrees {
                        w.sessions.clear();
                        w.active_tab = None;
                    }
                }
                state.pending_op = None;
                state.mode = Mode::Normal;
                state.command_status = Some(format!("supervisor lost: {reason}"));
                state.running = false;
            } else {
                // A remote dropped — drop only its sessions, keep running.
                let name = state.supervisors.name_of(sup).to_string();
                for p in &mut state.projects {
                    if p.supervisor != sup {
                        continue;
                    }
                    for w in &mut p.worktrees {
                        for id in w.sessions.drain(..) {
                            state.sessions.remove(&id);
                        }
                        w.active_tab = None;
                    }
                }
                if let Some((pi, _)) = state.active_worktree
                    && state.projects.get(pi).map(|p| p.supervisor) == Some(sup)
                {
                    state.active_worktree = None;
                }
                state.command_status = Some(format!("remote supervisor '{name}' disconnected"));
            }
        }
        Action::SupervisorConnected { .. } => {
            // Handled entirely in `runtime::handle_action` (it mutates the live
            // connection registry, which this pure reducer can't reach). Never
            // reaches here, but the match must stay exhaustive.
        }
        Action::UsageReceived(sup, report) => {
            if let Some(popup) = state.usage_popup.as_mut() {
                popup.reports.insert(sup, report);
                let max_row = usage_visible_row_count(popup).saturating_sub(1) as u16;
                popup.cursor = popup.cursor.min(max_row);
            }
        }
        Action::LocalUsageSampled(node) => {
            if let Some(popup) = state.usage_popup.as_mut() {
                popup.client = Some(node);
                let max_row = usage_visible_row_count(popup).saturating_sub(1) as u16;
                popup.cursor = popup.cursor.min(max_row);
            }
        }
        Action::DirListed {
            dir,
            parent,
            entries,
        } => {
            if let Some(popup) = state.open_project_popup.as_mut() {
                popup.path = dir.to_string_lossy().to_string();
                popup.browser = Some(crate::app::DirBrowser {
                    dir,
                    parent,
                    entries,
                    cursor: 0,
                });
            }
        }
        Action::UpdateChecked(Ok(Some(info))) => {
            tracing::info!(latest = %info.latest_tag, "update available");
            let tag = info.latest_tag.clone();
            state.available_update = Some(info);
            if state.auto_install_after_check {
                state.auto_install_after_check = false;
                state.update_status = crate::app::UpdateStatus::Installing;
                state.command_status = Some(format!("installing {tag}…"));
                cmds.push(Command::InstallUpdate { tag });
            } else {
                state.update_status = crate::app::UpdateStatus::Idle;
                state.command_status = Some(format!("{tag} available — :update to install"));
            }
        }
        Action::UpdateChecked(Ok(None)) => {
            state.available_update = None;
            state.update_status = crate::app::UpdateStatus::Idle;
            let was_user_initiated = state.auto_install_after_check;
            state.auto_install_after_check = false;
            // Only chirp on user-initiated checks. The hourly background tick
            // shouldn't pester the status row when nothing's new.
            if was_user_initiated {
                state.command_status = Some("already on the latest release".into());
            }
        }
        Action::UpdateChecked(Err(e)) => {
            tracing::warn!("update check failed: {e}");
            state.update_status = crate::app::UpdateStatus::Idle;
            let was_user_initiated = state.auto_install_after_check;
            state.auto_install_after_check = false;
            if was_user_initiated {
                state.command_status = Some(format!("update check failed: {e}"));
            }
        }
        Action::UpdateInstalled(Ok(outcome)) => {
            state.update_status = crate::app::UpdateStatus::InstalledPendingRestart;
            state.available_update = None;
            let tag = &outcome.installed_tag;
            state.command_status = Some(if outcome.supervisor_restart_required {
                format!(
                    "installed {tag} — supervisor protocol changed; run :rs (kills sessions) then relaunch"
                )
            } else {
                format!("installed {tag} — relaunch the client to switch over (sessions preserved)")
            });
        }
        Action::UpdateInstalled(Err(e)) => {
            state.update_status = crate::app::UpdateStatus::Idle;
            state.command_status = Some(format!("update failed: {e}"));
        }
        Action::PeriodicUpdateCheck => {
            if state.update_status == crate::app::UpdateStatus::Idle {
                state.update_status = crate::app::UpdateStatus::Checking;
                cmds.push(Command::CheckForUpdate);
            }
        }
        Action::PeriodicPrCheck => {
            for (pi, p) in state.projects.iter().enumerate() {
                if p.github_enabled {
                    cmds.push(Command::FetchPrStatuses {
                        project_idx: pi,
                        repo_path: p.repo_path.clone(),
                        worktrees: worktree_paths(p),
                    });
                }
            }
        }
        Action::PrStatusesFetched {
            project_idx,
            statuses,
        } => {
            let Some(project) = state.projects.get(project_idx) else {
                return cmds;
            };
            let wt_count = project.worktrees.len();
            let mut matched = 0usize;
            for (wi, status) in &statuses {
                if *wi >= wt_count {
                    continue;
                }
                match status {
                    Some(s) => {
                        matched += 1;
                        state.pr_statuses.insert((project_idx, *wi), *s);
                    }
                    None => {
                        state.pr_statuses.remove(&(project_idx, *wi));
                    }
                }
            }
            if state.pr_refresh_in_flight {
                state.pr_refresh_in_flight = false;
                state.command_status = Some(if matched == 0 {
                    "PR status refreshed (no open PRs)".into()
                } else {
                    format!(
                        "PR status refreshed ({matched} PR{})",
                        if matched == 1 { "" } else { "s" }
                    )
                });
            }
        }
        Action::PrFetchFailed {
            project_idx: _,
            message,
        } => {
            if state.pr_refresh_in_flight {
                state.pr_refresh_in_flight = false;
                state.command_status = Some(format!("PR refresh failed: {message}"));
            }
            // Background failures are silent — log-only is enough.
        }
        Action::Quit => {
            state.running = false;
            cmds.push(Command::Shutdown);
        }
    }
    cmds
}

/// Push a `FetchPrStatuses` for a single worktree on this project, provided
/// the project still has GitHub integration enabled. Used to refresh the
/// sidebar bar as soon as the user activates a worktree, instead of waiting
/// for the next 2-min poll. The worker thread serialises against the
/// periodic tick so this can't double up on gh calls.
fn request_pr_refresh_for(
    state: &AppState,
    project_idx: usize,
    worktree_idx: usize,
    cmds: &mut Commands,
) {
    let Some(p) = state.projects.get(project_idx) else {
        return;
    };
    if !p.github_enabled {
        return;
    }
    let Some(wt) = p.worktrees.get(worktree_idx) else {
        return;
    };
    cmds.push(Command::FetchPrStatuses {
        project_idx,
        repo_path: p.repo_path.clone(),
        worktrees: vec![(worktree_idx, wt.path.clone())],
    });
}

fn worktree_paths(project: &crate::app::Project) -> Vec<(usize, std::path::PathBuf)> {
    project
        .worktrees
        .iter()
        .enumerate()
        .map(|(wi, w)| (wi, w.path.clone()))
        .collect()
}

fn handle_paste(state: &mut AppState, text: String, cmds: &mut Commands) {
    match state.mode {
        Mode::Terminal => {
            if let Some(id) = state.focused_session_id() {
                cmds.push(Command::WritePaste(id, text));
            }
        }
        Mode::Command => {
            // Strip newlines so a multi-line paste doesn't accidentally
            // submit the ex-line mid-paste.
            // Filter all control chars (newlines, ESC, etc.); pasting an ESC
            // into the command buffer would otherwise leave a weird invisible
            // byte that nothing matches.
            for c in text.chars().filter(|c| !c.is_control()) {
                state.command.push(c);
            }
            refresh_command_completion(state);
        }
        Mode::Normal => {
            // Pastes in Normal mode are ignored — there's no edit surface.
        }
    }
}

fn handle_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    // Usage popup is modal: arrow/expand/close handled here.
    if state.usage_popup.is_some() {
        handle_usage_popup_key(state, k, cmds);
        return;
    }
    // Edit popup (multi-line textarea) is modal: route every key to the
    // textarea except Esc (cancel) and Ctrl-S (save).
    if state.edit_popup.is_some() {
        handle_edit_popup_key(state, k, cmds);
        return;
    }
    // Open-project popup is modal: Tab toggles path↔script focus, Esc cancels,
    // Enter on path or Ctrl-S submits; otherwise route the key to the focused
    // field.
    if state.open_project_popup.is_some() {
        handle_open_project_popup_key(state, k, cmds);
        return;
    }
    // Launch popup is modal: j/k or ↑/↓ move, Enter spawns, Esc cancels.
    if state.launch_popup.is_some() {
        handle_launch_popup_key(state, k, cmds);
        return;
    }
    // Help popup is modal and scrollable: j/k/arrows scroll, PageUp/Down
    // jumps, Esc/`q` (or `:` and `i`, which start a new flow) closes.
    if state.help_open {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.help_open = false;
                state.help_scroll = 0;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                bump_help_scroll(state, 1);
                return;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                bump_help_scroll(state, -1);
                return;
            }
            KeyCode::PageDown => {
                bump_help_scroll(state, 10);
                return;
            }
            KeyCode::PageUp => {
                bump_help_scroll(state, -10);
                return;
            }
            KeyCode::Home => {
                state.help_scroll = 0;
                return;
            }
            KeyCode::End => {
                state.help_scroll = state.help_max_scroll.get();
                return;
            }
            // `:` and `i` close help and start the new flow.
            KeyCode::Char(':') | KeyCode::Char('i') => {
                state.help_open = false;
                state.help_scroll = 0;
                // fall through so the keystroke kicks the new mode
            }
            _ => return, // any other key is consumed (h/l are scroll no-ops here)
        }
    }

    // Input popup is modal: keys edit the popup buffer.
    if state.popup.is_some() {
        handle_popup_key(state, k, cmds);
        return;
    }

    // Inline action-bar confirmation (e.g. worktree delete). Only intercepts
    // in Normal mode so the user can still escape into Command/Terminal by
    // dismissing first with Esc.
    if state.pending_confirm.is_some() && matches!(state.mode, Mode::Normal) {
        handle_pending_confirm_key(state, k, cmds);
        return;
    }

    match state.mode {
        Mode::Normal => handle_normal_key(state, k, cmds),
        Mode::Terminal => handle_terminal_key(state, k, cmds),
        Mode::Command => handle_command_key(state, k, cmds),
    }
}

/// Normal-mode key handling. Builds up a multi-key chord against the live
/// keymap; on exact match, fires the action; on prefix match, waits for the
/// next key; otherwise discards (single-key sequences also propagate to a
/// few hardcoded affordances like the `Esc` reset).
fn handle_normal_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    // Esc unconditionally cancels any chord in progress.
    if k.code == KeyCode::Esc {
        state.pending_chord.clear();
        return;
    }
    let chord = Chord::from_event(&k);
    state.pending_chord.push(chord);
    let seq: Vec<Chord> = state.pending_chord.iter().copied().collect();
    match state.keymap.lookup(Scope::Normal, &seq) {
        MatchResult::Exact(action) => {
            state.pending_chord.clear();
            dispatch_action(state, action, cmds);
        }
        MatchResult::Prefix(_) => {
            // Keep `pending_chord`; render layer will optionally show the
            // which-key hint for `<Space>` etc.
        }
        MatchResult::None => {
            state.pending_chord.clear();
        }
    }
}

fn switch_tab(state: &mut AppState, delta: i32) {
    let Some((pi, wi)) = state.active_worktree else {
        return;
    };
    let Some(wt) = worktree_mut(state, (pi, wi)) else {
        return;
    };
    if wt.sessions.is_empty() {
        return;
    }
    let len = wt.sessions.len() as i32;
    let current = wt.active_tab.unwrap_or(0) as i32;
    let next = ((current + delta).rem_euclid(len)) as usize;
    wt.active_tab = Some(next);
}

/// One row in the usage popup's flattened view.
#[derive(Debug, Clone)]
pub enum UsageRow<'a> {
    /// Section header naming a supervisor (`local` or a remote's name).
    SupervisorHeader { name: String },
    Session {
        session_id: SessionId,
        usage: &'a crate::ipc::SessionUsage,
        expanded: bool,
    },
    Process {
        depth: u16,
        node: &'a crate::ipc::ProcessNode,
    },
    /// A supervisor's own process (labelled by the preceding header).
    Supervisor(&'a crate::ipc::ProcessNode),
    /// This client process (one row, after all supervisor sections).
    Client(&'a crate::ipc::ProcessNode),
}

/// Flatten the popup state into the rows currently visible to the user. One
/// section per supervisor (its sessions + its own process), then a single
/// `Client` row sampled locally.
pub fn usage_visible_rows<'a>(popup: &'a crate::app::UsagePopup) -> Vec<UsageRow<'a>> {
    let mut rows = Vec::new();
    for (id, report) in &popup.reports {
        rows.push(UsageRow::SupervisorHeader {
            name: popup.supervisors.name_of(*id).to_string(),
        });
        for su in &report.sessions {
            let expanded = popup.expanded.contains(&su.session_id);
            rows.push(UsageRow::Session {
                session_id: su.session_id,
                usage: su,
                expanded,
            });
            if expanded {
                for child in &su.root.children {
                    push_process_rows(&mut rows, child, 1);
                }
            }
        }
        rows.push(UsageRow::Supervisor(&report.supervisor));
    }
    if let Some(client) = popup.client.as_ref() {
        rows.push(UsageRow::Client(client));
    }
    rows
}

fn push_process_rows<'a>(
    rows: &mut Vec<UsageRow<'a>>,
    node: &'a crate::ipc::ProcessNode,
    depth: u16,
) {
    rows.push(UsageRow::Process { depth, node });
    for c in &node.children {
        push_process_rows(rows, c, depth + 1);
    }
}

fn usage_visible_row_count(popup: &crate::app::UsagePopup) -> usize {
    usage_visible_rows(popup).len()
}

fn handle_usage_popup_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    let Some(popup) = state.usage_popup.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.usage_popup = None;
            cmds.push(Command::UnsubscribeUsage);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let max = usage_visible_row_count(popup).saturating_sub(1) as u16;
            popup.cursor = popup.cursor.saturating_add(1).min(max);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            popup.cursor = popup.cursor.saturating_sub(1);
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            if let Some(id) = session_id_at_cursor(popup) {
                popup.expanded.insert(id);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if let Some(id) = session_id_at_cursor(popup) {
                popup.expanded.remove(&id);
            }
        }
        _ => {}
    }
}

fn session_id_at_cursor(popup: &crate::app::UsagePopup) -> Option<SessionId> {
    let rows = usage_visible_rows(popup);
    match rows.get(popup.cursor as usize)? {
        UsageRow::Session { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

/// Spawn a tab in the active worktree with an optional `initial_command` fed
/// to the PTY as the first input. Used by `:launch` and the launcher popup.
/// `None` is equivalent to [`open_new_tab_in_active`].
pub(crate) fn launch_in_active_worktree(
    state: &mut AppState,
    initial_command: Option<String>,
    cmds: &mut Commands,
) {
    let Some((pi, wi)) = state.active_worktree else {
        state.command_status = Some("no active worktree — pick one first".into());
        return;
    };
    launch_in_worktree(state, pi, wi, initial_command, cmds);
}

/// Same as [`launch_in_active_worktree`] but targets an explicit
/// `(project_idx, worktree_idx)` — used by the launcher popup, which pins to
/// the worktree that was active when the popup was opened.
pub(crate) fn launch_in_worktree(
    state: &mut AppState,
    pi: usize,
    wi: usize,
    initial_command: Option<String>,
    cmds: &mut Commands,
) {
    let term = chrome(state.term_size.as_rect(), state.sidebar_width).terminal;
    let (cwd, slug, wt_name, supervisor) = match state
        .projects
        .get(pi)
        .and_then(|p| p.worktrees.get(wi).map(|w| (p, w)))
    {
        Some((p, w)) => (w.path.clone(), p.slug.clone(), w.name.clone(), p.supervisor),
        None => return,
    };
    cmds.push(Command::SpawnInWorktree {
        supervisor,
        rows: term.height,
        cols: term.width,
        cwd,
        dest: (pi, wi),
        initial_command,
        project_slug: slug,
        worktree_name: wt_name,
    });
}

pub(crate) fn open_new_tab_in_active(state: &mut AppState, cmds: &mut Commands) {
    let Some((pi, wi)) = state.active_worktree else {
        return;
    };
    let term = chrome(state.term_size.as_rect(), state.sidebar_width).terminal;
    let (cwd, slug, wt_name, supervisor) = match state
        .projects
        .get(pi)
        .and_then(|p| p.worktrees.get(wi).map(|w| (p, w)))
    {
        Some((p, w)) => (w.path.clone(), p.slug.clone(), w.name.clone(), p.supervisor),
        None => return,
    };
    cmds.push(Command::SpawnInWorktree {
        supervisor,
        rows: term.height,
        cols: term.width,
        cwd,
        dest: (pi, wi),
        initial_command: None,
        project_slug: slug,
        worktree_name: wt_name,
    });
}

/// Close the tab currently visible in the active worktree.
/// MVP semantics: always close (no `!`-style force confirmation yet).
pub(crate) fn close_current_tab(state: &mut AppState, cmds: &mut Commands) {
    let Some((pi, wi)) = state.active_worktree else {
        return;
    };
    let session_id = {
        let Some(wt) = worktree_mut(state, (pi, wi)) else {
            return;
        };
        let Some(at) = wt.active_tab else { return };
        if at >= wt.sessions.len() {
            return;
        }
        let id = wt.sessions.remove(at);
        if wt.sessions.is_empty() {
            wt.active_tab = None;
        } else if at >= wt.sessions.len() {
            wt.active_tab = Some(wt.sessions.len() - 1);
        }
        id
    };
    state.sessions.remove(&session_id);
    // Ask the supervisor to terminate the child. Local removal stops
    // rendering; the kill stops the process so we don't leak shells.
    cmds.push(Command::KillSession(session_id));
}

fn worktree_mut(state: &mut AppState, dest: (usize, usize)) -> Option<&mut Worktree> {
    state
        .projects
        .get_mut(dest.0)
        .and_then(|p| p.worktrees.get_mut(dest.1))
}

/// Flatten the currently-visible sidebar rows (respecting collapsed projects).
fn sidebar_visible_rows(state: &AppState) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    for (pi, p) in state.projects.iter().enumerate() {
        rows.push(SidebarRow::Project(pi));
        if p.expanded {
            for wi in 0..p.worktrees.len() {
                rows.push(SidebarRow::Worktree(pi, wi));
            }
        }
    }
    rows
}

fn selection_to_row(state: &AppState) -> Option<SidebarRow> {
    let (p, w) = state.sidebar_selection?;
    Some(match w {
        Some(wi) => SidebarRow::Worktree(p, wi),
        None => SidebarRow::Project(p),
    })
}

fn row_to_selection(row: SidebarRow) -> (usize, Option<usize>) {
    match row {
        SidebarRow::Project(p) => (p, None),
        SidebarRow::Worktree(p, w) => (p, Some(w)),
    }
}

fn move_sidebar_selection(state: &mut AppState, delta: i32) {
    let rows = sidebar_visible_rows(state);
    if rows.is_empty() {
        state.sidebar_selection = None;
        state.sidebar_scroll = 0;
        return;
    }
    let current = selection_to_row(state)
        .and_then(|r| rows.iter().position(|x| *x == r))
        .unwrap_or(0);
    let next = (current as i32 + delta).clamp(0, rows.len() as i32 - 1) as usize;
    state.sidebar_selection = Some(row_to_selection(rows[next]));
    ensure_selection_visible(state, next as u16);
}

/// Keep `row_idx` (an index into `sidebar_visible_rows`) within the visible
/// window by scrolling the sidebar up or down as needed.
fn ensure_selection_visible(state: &mut AppState, row_idx: u16) {
    let regions = chrome(state.term_size.as_rect(), state.sidebar_width);
    let inner = sidebar_inner_rows(&regions);
    if inner == 0 {
        return;
    }
    if row_idx < state.sidebar_scroll {
        state.sidebar_scroll = row_idx;
    } else if row_idx >= state.sidebar_scroll + inner {
        state.sidebar_scroll = row_idx + 1 - inner;
    }
}

fn activate_sidebar_selection(state: &mut AppState, cmds: &mut Commands) {
    let Some(row) = selection_to_row(state) else {
        return;
    };
    match row {
        SidebarRow::Project(pi) => {
            if let Some(p) = state.projects.get_mut(pi) {
                p.expanded = !p.expanded;
                cmds.push(Command::SaveProjectConfig(pi));
                clamp_sidebar_scroll(state);
            }
        }
        SidebarRow::Worktree(pi, wi) => activate_worktree(state, pi, wi, cmds),
    }
}

fn clamp_sidebar_scroll(state: &mut AppState) {
    let regions = chrome(state.term_size.as_rect(), state.sidebar_width);
    let inner = sidebar_inner_rows(&regions);
    let total = sidebar_visible_rows(state).len() as u16;
    let max_scroll = total.saturating_sub(inner);
    if state.sidebar_scroll > max_scroll {
        state.sidebar_scroll = max_scroll;
    }
}

fn activate_worktree(state: &mut AppState, pi: usize, wi: usize, cmds: &mut Commands) {
    let changed = state.active_worktree != Some((pi, wi));
    state.active_worktree = Some((pi, wi));
    if changed {
        request_pr_refresh_for(state, pi, wi, cmds);
    }
    // Focus stays on the sidebar — the user can press `l` (or click) to
    // interact with the terminal once they're ready. No auto-spawn: the
    // user opens a tab explicitly via `o` or `:tabnew`.
    let has_sessions = state
        .projects
        .get_mut(pi)
        .and_then(|p| p.worktrees.get_mut(wi))
        .map(|wt| {
            if !wt.sessions.is_empty() && wt.active_tab.is_none() {
                wt.active_tab = Some(0);
            }
            !wt.sessions.is_empty()
        })
        .unwrap_or(false);
    // Switching into a worktree with no live sessions can't stay in
    // Terminal mode — there's no PTY to forward keys to. Drop back to
    // Normal so the user isn't stuck with an inert terminal mode.
    if !has_sessions && state.mode == Mode::Terminal {
        state.mode = Mode::Normal;
    }
}

fn handle_terminal_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    // crossterm sometimes delivers 0x1C as Ctrl-4. Normalise so the keymap
    // matcher sees the canonical Ctrl-\ shape.
    let k = if is_ctrl_backslash(&k) {
        ctrl_backslash_event()
    } else {
        k
    };
    let chord = Chord::from_event(&k);
    state.pending_chord.push(chord);
    state.pending_terminal_keys.push(k);
    let seq: Vec<Chord> = state.pending_chord.iter().copied().collect();
    match state.keymap.lookup(Scope::Terminal, &seq) {
        MatchResult::Exact(action) => {
            state.pending_chord.clear();
            state.pending_terminal_keys.clear();
            dispatch_action(state, action, cmds);
        }
        MatchResult::Prefix(_) => {
            // Chord in progress — buffered keys stay queued; nothing reaches
            // the PTY until we know whether the chord resolves.
        }
        MatchResult::None => {
            // Replay every buffered key to the PTY in order, then clear.
            // This is what keeps `\\` + arbitrary-key working: the user's
            // shell sees both characters once the chord is rejected.
            if let Some(id) = state.focused_session_id() {
                for buffered in state.pending_terminal_keys.drain(..) {
                    cmds.push(Command::WriteKey(id, buffered));
                }
            } else {
                state.pending_terminal_keys.clear();
            }
            state.pending_chord.clear();
        }
    }
}

/// Translate a `BindableAction` into existing helpers/commands. All the
/// behaviour lives in those helpers — this is just the name layer.
fn dispatch_action(state: &mut AppState, action: BindableAction, cmds: &mut Commands) {
    match action {
        BindableAction::FocusSidebar => state.ui_focus = UiFocus::Sidebar,
        BindableAction::FocusTerminal => state.ui_focus = UiFocus::Terminal,
        BindableAction::SidebarUp => {
            if state.ui_focus == UiFocus::Sidebar {
                move_sidebar_selection(state, -1);
            }
        }
        BindableAction::SidebarDown => {
            if state.ui_focus == UiFocus::Sidebar {
                move_sidebar_selection(state, 1);
            }
        }
        BindableAction::ActivateSelection => {
            if state.ui_focus == UiFocus::Sidebar {
                activate_sidebar_selection(state, cmds);
            }
        }
        BindableAction::OpenTab => open_new_tab_in_active(state, cmds),
        BindableAction::CloseTab => close_current_tab(state, cmds),
        BindableAction::EnterTerminalMode => state.mode = Mode::Terminal,
        BindableAction::EnterCommandMode => {
            state.mode = Mode::Command;
            state.command.clear();
            state.command_status = None;
            state.command_completion = None;
            state.help_open = false;
            refresh_command_completion(state);
        }
        BindableAction::NextTab => switch_tab(state, 1),
        BindableAction::PrevTab => switch_tab(state, -1),
        BindableAction::SidebarGrow => apply_sidebar_resize(state, 2, cmds),
        BindableAction::SidebarShrink => apply_sidebar_resize(state, -2, cmds),
        BindableAction::SidebarReset => set_sidebar_width(state, DEFAULT_SIDEBAR_WIDTH, cmds),
        BindableAction::OpenProjectPopup => crate::commands::cmd_open(state, &[], cmds),
        BindableAction::NewWorktree => crate::commands::cmd_worktree(state, &[], cmds),
        BindableAction::RemoveWorktree => crate::commands::cmd_worktree_remove(state, &[], cmds),
        BindableAction::EditProject => crate::commands::cmd_edit(state, &[], cmds),
        BindableAction::LaunchPicker => crate::commands::cmd_launch(state, &[], cmds),
        BindableAction::UsagePopup => crate::commands::cmd_usage(state, &[], cmds),
        BindableAction::HelpPopup => state.help_open = true,
        BindableAction::Quit => state.running = false,
        BindableAction::LeaveTerminal => state.mode = Mode::Normal,
    }
}

fn handle_command_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    let plain_or_shift = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
    match k.code {
        KeyCode::Char(c) if plain_or_shift => {
            state.command.push(c);
            refresh_command_completion(state);
        }
        KeyCode::Backspace => {
            state.command.pop();
            refresh_command_completion(state);
        }
        KeyCode::Tab => {
            // Tab cycles forward through suggestions; Shift-Tab cycles back.
            cycle_command_completion(state, 1);
        }
        KeyCode::BackTab => cycle_command_completion(state, -1),
        KeyCode::Down => cycle_command_completion(state, 1),
        KeyCode::Up => cycle_command_completion(state, -1),
        KeyCode::Enter => {
            // If the user has actively highlighted a suggestion, Enter accepts
            // it into the buffer (so they can add args / press Enter again to
            // execute). Otherwise Enter executes what's typed.
            if let Some(comp) = state.command_completion.as_ref()
                && let Some(i) = comp.selected
                && let Some((name, _)) = comp.matches.get(i)
            {
                state.command = (*name).to_string();
                state.command_completion = None;
                refresh_command_completion(state);
                return;
            }
            let buf = std::mem::take(&mut state.command);
            state.command_completion = None;
            state.mode = Mode::Normal;
            crate::commands::execute_command(state, buf.trim(), cmds);
        }
        KeyCode::Esc => {
            // First Esc dismisses the suggestion popover; a second Esc exits
            // command mode. Feels closer to vim's wildmenu behavior.
            if state.command_completion.is_some() {
                state.command_completion = None;
            } else {
                state.command.clear();
                state.mode = Mode::Normal;
            }
        }
        _ => {}
    }
}

/// Recompute the suggestion list based on the current `state.command` buffer.
/// Suggestions are shown only while editing the first whitespace-separated
/// token (the command name); once the user types a space they're in
/// argument-land and the popover hides.
fn refresh_command_completion(state: &mut AppState) {
    let buf = &state.command;
    if buf.contains(char::is_whitespace) {
        state.command_completion = None;
        return;
    }
    let prefix = buf.as_str();
    let matches: Vec<(&'static str, &'static str)> = crate::commands::COMMANDS
        .iter()
        .filter_map(|spec| {
            let name = *spec.names.first()?;
            if name.starts_with(prefix) {
                Some((name, spec.description))
            } else {
                None
            }
        })
        .collect();
    if matches.is_empty() {
        state.command_completion = None;
    } else {
        state.command_completion = Some(crate::app::CommandCompletion {
            matches,
            selected: None,
        });
    }
}

fn cycle_command_completion(state: &mut AppState, delta: i32) {
    let Some(comp) = state.command_completion.as_mut() else {
        return;
    };
    if comp.matches.is_empty() {
        return;
    }
    let len = comp.matches.len() as i32;
    let next = match comp.selected {
        None => {
            if delta >= 0 {
                0
            } else {
                len - 1
            }
        }
        Some(i) => (i as i32 + delta).rem_euclid(len),
    };
    comp.selected = Some(next as usize);
}

fn handle_pending_confirm_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let Some(confirm) = state.pending_confirm.take() else {
                return;
            };
            match confirm {
                PendingConfirm::RemoveWorktree {
                    project_idx,
                    worktree_idx,
                    name,
                    repo_path,
                    dest_path,
                    branch,
                } => {
                    // Kill sessions *before* the git op so shells aren't
                    // holding the worktree's CWD/files open while git is
                    // trying to delete them. KillSession is fire-and-forget
                    // (SIGKILL via the supervisor); the git remove can race
                    // with reaping safely.
                    if let Some(w) = state
                        .projects
                        .get_mut(project_idx)
                        .and_then(|p| p.worktrees.get_mut(worktree_idx))
                    {
                        for id in w.sessions.drain(..) {
                            state.sessions.remove(&id);
                            cmds.push(Command::KillSession(id));
                        }
                        w.active_tab = None;
                    }
                    state.pending_op = Some(format!("Removing worktree '{name}'…"));
                    cmds.push(Command::RemoveWorktree {
                        project_idx,
                        worktree_idx,
                        repo_path,
                        dest_path,
                        branch,
                    });
                }
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.pending_confirm = None;
            state.command_status = Some("cancelled".into());
        }
        _ => {}
    }
}

fn handle_popup_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    let Some(popup) = state.popup.as_mut() else {
        return;
    };
    let plain_or_shift = k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT;
    match k.code {
        KeyCode::Char(c) if plain_or_shift => popup.buffer.push(c),
        KeyCode::Backspace => {
            popup.buffer.pop();
        }
        KeyCode::Esc => {
            state.popup = None;
        }
        KeyCode::Enter => {
            let popup = state.popup.take().unwrap();
            dispatch_popup(state, popup, cmds);
        }
        _ => {}
    }
}

fn dispatch_popup(state: &mut AppState, popup: InputPopup, cmds: &mut Commands) {
    let arg = popup.buffer.trim().to_string();
    if arg.is_empty() {
        return;
    }
    match popup.action {
        PopupAction::NewWorktree { project_idx } => {
            if let Some(p) = state.projects.get(project_idx) {
                state.pending_op = Some(format!("Creating worktree '{arg}'…"));
                cmds.push(Command::AddWorktree {
                    project_idx,
                    repo_path: p.repo_path.clone(),
                    branch: arg,
                });
            }
        }
    }
}

fn ctrl_backslash_event() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL)
}

fn is_ctrl_backslash(k: &KeyEvent) -> bool {
    // crossterm parses 0x1C either as Ctrl-\ (under extended keyboard
    // protocols) or Ctrl-4 (legacy raw-byte mapping). Treat both as Ctrl-\.
    k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// Clamp the help popup's scroll offset against the last `max_scroll` the
/// renderer reported. Without this, pressing `j` past the end would inflate
/// `help_scroll` past what the viewport can show — visually fine, but `k`
/// would then take many presses to start moving back up.
fn bump_help_scroll(state: &mut AppState, delta: i32) {
    let max = state.help_max_scroll.get();
    let cur = state.help_scroll as i32;
    let next = (cur + delta).clamp(0, max as i32);
    state.help_scroll = next as u16;
}

fn handle_mouse(state: &mut AppState, m: MouseEvent, cmds: &mut Commands) {
    if state.help_open {
        match m.kind {
            MouseEventKind::ScrollUp => bump_help_scroll(state, -3),
            MouseEventKind::ScrollDown => bump_help_scroll(state, 3),
            MouseEventKind::Down(MouseButton::Left) => {
                state.help_open = false;
                state.help_scroll = 0;
            }
            _ => {}
        }
        return;
    }
    if state.popup.is_some() {
        if is_left_down(&m) {
            state.popup = None;
        }
        return;
    }
    let regions = chrome(state.term_size.as_rect(), state.sidebar_width);
    if rect_contains(regions.sidebar, m.column, m.row) {
        handle_sidebar_mouse(state, m, &regions, cmds);
    } else if rect_contains(regions.tab_bar, m.column, m.row) {
        handle_tab_bar_mouse(state, m, &regions);
    } else if rect_contains(regions.terminal, m.column, m.row) {
        handle_terminal_mouse(state, m, &regions, cmds);
    }
    // action_bar: clicks ignored for now.
}

fn rect_contains(r: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn is_left_down(m: &MouseEvent) -> bool {
    matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
}

fn handle_sidebar_mouse(
    state: &mut AppState,
    m: MouseEvent,
    regions: &ChromeRects,
    cmds: &mut Commands,
) {
    state.ui_focus = UiFocus::Sidebar;
    match m.kind {
        MouseEventKind::ScrollUp => {
            state.sidebar_scroll = state.sidebar_scroll.saturating_sub(3);
            return;
        }
        MouseEventKind::ScrollDown => {
            scroll_sidebar_down(state, regions, 3);
            return;
        }
        _ => {}
    }
    if !is_left_down(&m) {
        return;
    }
    // Title eats one row; tree items start at sidebar.y + 1.
    let Some(row_off) = m.row.checked_sub(regions.sidebar.y + 1) else {
        return;
    };
    let absolute = row_off as usize + state.sidebar_scroll as usize;
    let visible = sidebar_visible_rows(state);
    let Some(entry) = visible.get(absolute).copied() else {
        return;
    };
    state.sidebar_selection = Some(row_to_selection(entry));
    activate_sidebar_selection(state, cmds);
}

fn sidebar_inner_rows(regions: &ChromeRects) -> u16 {
    regions.sidebar.height.saturating_sub(1) // title row
}

fn scroll_sidebar_down(state: &mut AppState, regions: &ChromeRects, step: u16) {
    let total = sidebar_visible_rows(state).len() as u16;
    let inner = sidebar_inner_rows(regions);
    let max_scroll = total.saturating_sub(inner);
    state.sidebar_scroll = (state.sidebar_scroll + step).min(max_scroll);
}

/// Each tab is rendered as ` N ` (3 cols) + space (1 col) = 4 cols.
const TAB_SLOT_WIDTH: u16 = 4;

fn handle_tab_bar_mouse(state: &mut AppState, m: MouseEvent, regions: &ChromeRects) {
    if !is_left_down(&m) {
        return;
    }
    let Some((pi, wi)) = state.active_worktree else {
        return;
    };
    let Some(col_off) = m.column.checked_sub(regions.tab_bar.x) else {
        return;
    };
    let idx = (col_off / TAB_SLOT_WIDTH) as usize;
    let Some(wt) = worktree_mut(state, (pi, wi)) else {
        return;
    };
    if idx < wt.sessions.len() {
        wt.active_tab = Some(idx);
        state.ui_focus = UiFocus::Terminal;
    }
}

fn handle_terminal_mouse(
    state: &mut AppState,
    mut m: MouseEvent,
    regions: &ChromeRects,
    cmds: &mut Commands,
) {
    state.ui_focus = UiFocus::Terminal;
    let Some(id) = state.focused_session_id() else {
        return;
    };
    // Left-click in the terminal pane implicitly enters Terminal mode — the
    // user expectation is "click into the shell to start typing in it".
    if is_left_down(&m) && state.mode == Mode::Normal {
        state.mode = Mode::Terminal;
    }
    m.column -= regions.terminal.x;
    m.row -= regions.terminal.y;
    cmds.push(Command::WriteMouse(id, m));
}

fn apply_sidebar_resize(state: &mut AppState, delta: i16, cmds: &mut Commands) {
    let proposed = (state.sidebar_width as i32 + delta as i32).max(0) as u16;
    set_sidebar_width(state, proposed, cmds);
}

pub(crate) fn set_sidebar_width(state: &mut AppState, width: u16, cmds: &mut Commands) {
    let new = clamp_sidebar_width(width, state.term_size.cols);
    if new == state.sidebar_width {
        return;
    }
    state.sidebar_width = new;
    broadcast_resize(state, cmds);
    cmds.push(Command::SaveGlobalConfig);
}

fn broadcast_resize(state: &AppState, cmds: &mut Commands) {
    let term = chrome(state.term_size.as_rect(), state.sidebar_width).terminal;
    for id in state.sessions.keys().copied() {
        cmds.push(Command::ResizePty(id, term.height, term.width));
    }
}

fn handle_launch_popup_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    match k.code {
        KeyCode::Esc => {
            state.launch_popup = None;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(popup) = state.launch_popup.as_mut() {
                let max = popup.entries.len().saturating_sub(1) as u16;
                popup.cursor = popup.cursor.saturating_add(1).min(max);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(popup) = state.launch_popup.as_mut() {
                popup.cursor = popup.cursor.saturating_sub(1);
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            if let Some(popup) = state.launch_popup.as_mut() {
                popup.cursor = 0;
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(popup) = state.launch_popup.as_mut() {
                popup.cursor = popup.entries.len().saturating_sub(1) as u16;
            }
        }
        KeyCode::Enter => {
            if let Some(popup) = state.launch_popup.take() {
                let entry = popup.entries.get(popup.cursor as usize).cloned();
                if let Some(entry) = entry {
                    // Pin launch to the worktree that was active when the
                    // popup opened — guards against the user switching tabs
                    // via mouse before pressing Enter.
                    launch_in_worktree(
                        state,
                        popup.project_idx,
                        popup.worktree_idx,
                        entry.command,
                        cmds,
                    );
                }
            }
        }
        _ => {}
    }
}

fn handle_open_project_popup_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    use crate::app::OpenProjectFocus;
    // Esc cancels.
    if matches!(k.code, KeyCode::Esc) {
        state.open_project_popup = None;
        return;
    }
    let Some(focus) = state.open_project_popup.as_ref().map(|p| p.focus) else {
        return;
    };

    // Ctrl-S submits the browser's current directory as the repo from any focus.
    let ctrl_s = k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('s') | KeyCode::Char('S'));
    if ctrl_s {
        submit_open_project(state, cmds, None);
        return;
    }

    // Tab cycles Path → Supervisor → Script → Import → Path.
    if matches!(k.code, KeyCode::Tab) {
        if let Some(popup) = state.open_project_popup.as_mut() {
            popup.focus = match popup.focus {
                OpenProjectFocus::Path => OpenProjectFocus::Supervisor,
                OpenProjectFocus::Supervisor => OpenProjectFocus::Script,
                OpenProjectFocus::Script => OpenProjectFocus::Import,
                OpenProjectFocus::Import => OpenProjectFocus::Path,
            };
        }
        return;
    }

    match focus {
        OpenProjectFocus::Path => handle_browser_key(state, k, cmds),
        OpenProjectFocus::Supervisor => {
            // Left/Right/Space/Enter cycle the target supervisor; switching
            // re-lists from the new supervisor's home.
            if matches!(
                k.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Char(' ')
                    | KeyCode::Char('h')
                    | KeyCode::Char('l')
                    | KeyCode::Enter
            ) {
                let next = {
                    let dir = &state.supervisors;
                    state
                        .open_project_popup
                        .as_ref()
                        .map(|p| dir.next_after(p.supervisor))
                };
                if let (Some(next), Some(popup)) = (next, state.open_project_popup.as_mut()) {
                    popup.supervisor = next;
                    popup.browser = None;
                    cmds.push(Command::ListDir {
                        supervisor: next,
                        path: None,
                    });
                }
            }
        }
        OpenProjectFocus::Script => {
            if let Some(popup) = state.open_project_popup.as_mut() {
                popup.script.input(crossterm_key_to_input(k));
            }
        }
        OpenProjectFocus::Import => {
            if matches!(k.code, KeyCode::Char(' ') | KeyCode::Enter)
                && let Some(popup) = state.open_project_popup.as_mut()
            {
                popup.import_existing = !popup.import_existing;
            }
        }
    }
}

/// Key handling for the directory browser (Path focus).
fn handle_browser_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    let Some(popup) = state.open_project_popup.as_mut() else {
        return;
    };
    let supervisor = popup.supervisor;
    let Some(browser) = popup.browser.as_mut() else {
        return; // listing not arrived yet
    };
    let n = browser.entries.len() as u16;
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => {
            browser.cursor = browser.cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') if n > 0 => {
            browser.cursor = (browser.cursor + 1).min(n - 1);
        }
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
            if let Some(parent) = browser.parent.clone() {
                cmds.push(Command::ListDir {
                    supervisor,
                    path: Some(parent),
                });
            }
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if let Some(entry) = browser.entries.get(browser.cursor as usize) {
                let path = browser.dir.join(&entry.name);
                if entry.is_repo {
                    // Open this repo directly.
                    submit_open_project(state, cmds, Some(path));
                } else {
                    // Descend.
                    cmds.push(Command::ListDir {
                        supervisor,
                        path: Some(path),
                    });
                }
            }
        }
        _ => {}
    }
}

/// Build + dispatch the `OpenProject` command. `explicit` overrides the path
/// (e.g. a highlighted repo dir); otherwise the browser's current dir is used.
fn submit_open_project(
    state: &mut AppState,
    cmds: &mut Commands,
    explicit: Option<std::path::PathBuf>,
) {
    let Some(popup) = state.open_project_popup.take() else {
        return;
    };
    let path = explicit.unwrap_or_else(|| {
        popup
            .browser
            .as_ref()
            .map(|b| b.dir.clone())
            .unwrap_or_else(|| std::path::PathBuf::from(popup.path.trim()))
    });
    if path.as_os_str().is_empty() {
        state.command_status = Some("path required".into());
        state.open_project_popup = Some(popup);
        return;
    }
    let script_text = popup.script.lines().join("\n");
    let trimmed = script_text.trim_end_matches('\n').trim().to_string();
    let setup_script = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };
    state.pending_op = Some(format!("Opening {}…", path.display()));
    cmds.push(Command::OpenProject {
        supervisor: popup.supervisor,
        path,
        setup_script,
        import_existing: popup.import_existing,
    });
}

fn handle_edit_popup_key(state: &mut AppState, k: KeyEvent, cmds: &mut Commands) {
    // Esc cancels without saving.
    if matches!(k.code, KeyCode::Esc) {
        state.edit_popup = None;
        return;
    }
    // Ctrl-S commits: pull the textarea text, store on the project, persist.
    if k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('s') | KeyCode::Char('S'))
    {
        if let Some(popup) = state.edit_popup.take() {
            let text = popup.textarea.lines().join("\n");
            let trimmed = text.trim_end_matches('\n').to_string();
            if let Some(p) = state.projects.get_mut(popup.project_idx) {
                p.setup_script = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                cmds.push(Command::SaveProjectConfig(popup.project_idx));
                state.command_status = Some(format!("saved setup script for '{}'", p.name));
            }
        }
        return;
    }
    // Otherwise feed the key into the textarea.
    if let Some(popup) = state.edit_popup.as_mut() {
        popup.textarea.input(crossterm_key_to_input(k));
    }
}

/// Convert our crossterm `KeyEvent` to `ratatui_textarea::Input`. We can't
/// pass crossterm events directly because ratatui-textarea uses a different
/// crossterm version internally (via `ratatui-crossterm`).
fn crossterm_key_to_input(k: KeyEvent) -> ratatui_textarea::Input {
    use ratatui_textarea::{Input, Key};
    let key = match k.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Enter => Key::Enter,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Tab => Key::Tab,
        KeyCode::Delete => Key::Delete,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Esc => Key::Esc,
        KeyCode::F(n) => Key::F(n),
        _ => Key::Null,
    };
    Input {
        key,
        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
        alt: k.modifiers.contains(KeyModifiers::ALT),
        shift: k.modifiers.contains(KeyModifiers::SHIFT),
    }
}

fn remove_session_from_worktrees(state: &mut AppState, id: SessionId) {
    for project in &mut state.projects {
        for wt in &mut project.worktrees {
            if let Some(pos) = wt.sessions.iter().position(|s| *s == id) {
                wt.sessions.remove(pos);
                if let Some(at) = wt.active_tab {
                    if wt.sessions.is_empty() {
                        wt.active_tab = None;
                    } else if at >= wt.sessions.len() {
                        wt.active_tab = Some(wt.sessions.len() - 1);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "reducer_tests.rs"]
mod tests;
