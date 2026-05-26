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
        Action::ProjectOpened {
            project,
            import_existing,
        } => {
            state.pending_op = None;
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
            // Close any live sessions in that worktree before removing it.
            let session_ids: Vec<SessionId> = state
                .projects
                .get(project_idx)
                .and_then(|p| p.worktrees.get(worktree_idx))
                .map(|w| w.sessions.clone())
                .unwrap_or_default();
            for id in session_ids {
                state.sessions.remove(&id);
                cmds.push(Command::KillSession(id));
            }
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
        Action::SupervisorLost(reason) => {
            // Drop every session ref — the supervisor's gone, the PTYs are
            // gone, any future write would be a silent no-op.
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
        }
        Action::UsageReceived(report) => {
            if let Some(popup) = state.usage_popup.as_mut() {
                popup.report = Some(report);
                let max_row = usage_visible_row_count(popup).saturating_sub(1) as u16;
                popup.cursor = popup.cursor.min(max_row);
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
    // jumps, Esc (or `:` and `i`, which start a new flow) closes.
    if state.help_open {
        match k.code {
            KeyCode::Esc => {
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
    Session {
        session_id: SessionId,
        usage: &'a crate::ipc::SessionUsage,
        expanded: bool,
    },
    Process {
        depth: u16,
        node: &'a crate::ipc::ProcessNode,
    },
    Supervisor(&'a crate::ipc::ProcessNode),
    Client(&'a crate::ipc::ProcessNode),
}

/// Flatten the popup state into the rows currently visible to the user.
pub fn usage_visible_rows<'a>(popup: &'a crate::app::UsagePopup) -> Vec<UsageRow<'a>> {
    let mut rows = Vec::new();
    let Some(report) = popup.report.as_ref() else {
        return rows;
    };
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
    if let Some(client) = report.client.as_ref() {
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
    let (cwd, slug, wt_name) = match state
        .projects
        .get(pi)
        .and_then(|p| p.worktrees.get(wi).map(|w| (p, w)))
    {
        Some((p, w)) => (w.path.clone(), p.slug.clone(), w.name.clone()),
        None => return,
    };
    cmds.push(Command::SpawnInWorktree {
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
    let (cwd, slug, wt_name) = match state
        .projects
        .get(pi)
        .and_then(|p| p.worktrees.get(wi).map(|w| (p, w)))
    {
        Some((p, w)) => (w.path.clone(), p.slug.clone(), w.name.clone()),
        None => return,
    };
    cmds.push(Command::SpawnInWorktree {
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
    // Ctrl-S submits from any focus.
    let ctrl_s = k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char('s') | KeyCode::Char('S'));
    // Enter submits when on Path; on Import it toggles the flag (so the
    // user can both navigate to + check it with one Enter). On Script it
    // still inserts a newline via the textarea path below.
    let on_path = state
        .open_project_popup
        .as_ref()
        .is_some_and(|p| p.focus == OpenProjectFocus::Path);
    let on_import = state
        .open_project_popup
        .as_ref()
        .is_some_and(|p| p.focus == OpenProjectFocus::Import);
    let enter_submit = matches!(k.code, KeyCode::Enter) && on_path;
    if ctrl_s || enter_submit {
        if let Some(popup) = state.open_project_popup.take() {
            let path = popup.path.trim().to_string();
            if path.is_empty() {
                state.open_project_popup = Some(popup);
                state.command_status = Some("path required".into());
                return;
            }
            let home = crate::commands::current_home();
            let expanded = crate::commands::expand_user_path(&path, home.as_deref());
            let script_text = popup.script.lines().join("\n");
            let trimmed = script_text.trim_end_matches('\n').trim().to_string();
            let setup_script = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            state.pending_op = Some(format!("Opening {}…", expanded.display()));
            cmds.push(Command::OpenProject {
                path: expanded,
                setup_script,
                import_existing: popup.import_existing,
            });
        }
        return;
    }
    // Tab cycles Path → Script → Import → Path.
    if matches!(k.code, KeyCode::Tab) {
        if let Some(popup) = state.open_project_popup.as_mut() {
            popup.focus = match popup.focus {
                OpenProjectFocus::Path => OpenProjectFocus::Script,
                OpenProjectFocus::Script => OpenProjectFocus::Import,
                OpenProjectFocus::Import => OpenProjectFocus::Path,
            };
        }
        return;
    }
    // Space (or Enter) on the Import row toggles the flag.
    if on_import && matches!(k.code, KeyCode::Char(' ') | KeyCode::Enter) {
        if let Some(popup) = state.open_project_popup.as_mut() {
            popup.import_existing = !popup.import_existing;
        }
        return;
    }
    // Route to the focused field.
    if let Some(popup) = state.open_project_popup.as_mut() {
        match popup.focus {
            OpenProjectFocus::Path => match k.code {
                KeyCode::Backspace => {
                    popup.path.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    popup.path.push(c);
                }
                _ => {}
            },
            OpenProjectFocus::Script => {
                popup.script.input(crossterm_key_to_input(k));
            }
            OpenProjectFocus::Import => {
                // No other keys do anything when the Import row is focused;
                // ignore so a stray keystroke doesn't fall through.
            }
        }
    }
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
mod tests {
    use super::*;
    use crate::app::{Project, Worktree, mock_projects};
    use crate::commands::expand_user_path;
    use crate::layout::{MIN_SIDEBAR_WIDTH, TermSize};
    use crate::session::FakeSession;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::path::PathBuf;

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn quit_stops_running_and_emits_shutdown() {
        let mut s = AppState::new();
        let cmds = reduce(&mut s, Action::Quit);
        assert!(!s.running);
        assert!(matches!(cmds.as_slice(), [Command::Shutdown]));
    }

    #[test]
    fn key_without_active_session_is_noop() {
        let mut s = AppState::new();
        let cmds = reduce(&mut s, Action::Key(plain('x')));
        assert!(cmds.is_empty());
    }

    #[test]
    fn ctrl_w_then_gt_widens_sidebar() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(plain('>')));
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH + 2);
        assert!(s.pending_chord.is_empty());
    }

    #[test]
    fn ctrl_w_then_lt_narrows_sidebar() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        let _ = reduce(&mut s, Action::Key(plain('<')));
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH - 2);
    }

    #[test]
    fn ctrl_w_then_eq_resets_sidebar() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.sidebar_width = 40;
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        let _ = reduce(&mut s, Action::Key(plain('=')));
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn sidebar_resize_clamps_to_min() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.sidebar_width = MIN_SIDEBAR_WIDTH;
        let mut cmds = Commands::new();
        apply_sidebar_resize(&mut s, -50, &mut cmds);
        assert_eq!(s.sidebar_width, MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn sidebar_resize_clamps_to_half_term() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.sidebar_width = 20;
        let mut cmds = Commands::new();
        apply_sidebar_resize(&mut s, 200, &mut cmds);
        assert_eq!(s.sidebar_width, 50); // half of 100
    }

    #[test]
    fn sidebar_resize_broadcasts_to_sessions() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.projects = mock_projects();
        let fake = FakeSession::new(7);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        let cmds = reduce(&mut s, Action::Key(plain('>')));
        // Width change broadcasts a ResizePty and autosaves global config.
        assert!(matches!(
            cmds.as_slice(),
            [Command::ResizePty(7, _, _), Command::SaveGlobalConfig]
        ));
    }

    #[test]
    fn ctrl_w_then_unknown_clears_leader_silently() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        let cmds = reduce(&mut s, Action::Key(plain('x')));
        assert!(cmds.is_empty());
        assert!(s.pending_chord.is_empty());
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }

    #[test]
    fn session_exit_does_not_quit() {
        let mut s = AppState::new();
        let fake = FakeSession::new(1);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        let cmds = reduce(&mut s, Action::SessionExited(1));
        assert!(s.running);
        assert!(cmds.is_empty());
    }

    #[test]
    fn mode_starts_normal() {
        let s = AppState::new();
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn i_enters_terminal_mode() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::Key(plain('i')));
        assert_eq!(s.mode, Mode::Terminal);
    }

    #[test]
    fn ctrl_4_aliases_ctrl_backslash() {
        // crossterm delivers 0x1c as Ctrl-4; this is the user-visible chord.
        let mut s = AppState::new();
        s.mode = Mode::Terminal;
        let _ = reduce(&mut s, Action::Key(ctrl('4')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(ctrl('n')));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn ctrl_backslash_then_ctrl_n_exits_terminal_mode() {
        let mut s = AppState::new();
        s.mode = Mode::Terminal;
        let _ = reduce(&mut s, Action::Key(ctrl('\\')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(ctrl('n')));
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.pending_chord.is_empty());
    }

    #[test]
    fn ctrl_backslash_then_other_passes_through_to_pty() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.mode = Mode::Terminal;
        let fake = FakeSession::new(9);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        s.projects = vec![Project {
            slug: "p".into(),
            name: "p".into(),
            repo_path: PathBuf::from("."),
            worktrees: vec![Worktree {
                name: "w".into(),
                path: PathBuf::from("."),
                branch: Some("w".into()),
                sessions: vec![9],
                active_tab: Some(0),
            }],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        }];
        s.active_worktree = Some((0, 0));

        let _ = reduce(&mut s, Action::Key(ctrl('\\')));
        let cmds = reduce(&mut s, Action::Key(plain('a')));
        // Both Ctrl-\ and 'a' should be forwarded.
        assert!(matches!(
            cmds.as_slice(),
            [Command::WriteKey(9, _), Command::WriteKey(9, _)]
        ));
        assert_eq!(s.mode, Mode::Terminal);
    }

    #[test]
    fn colon_enters_command_mode_and_clears_buffer() {
        let mut s = AppState::new();
        s.command = "stale".into();
        let _ = reduce(&mut s, Action::Key(plain(':')));
        assert_eq!(s.mode, Mode::Command);
        assert!(s.command.is_empty());
    }

    #[test]
    fn esc_cancels_command_mode() {
        let mut s = AppState::new();
        s.mode = Mode::Command;
        s.command = "tabnew".into();
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.command.is_empty());
    }

    #[test]
    fn command_chars_and_backspace_edit_buffer() {
        let mut s = AppState::new();
        s.mode = Mode::Command;
        let _ = reduce(&mut s, Action::Key(plain('q')));
        let _ = reduce(&mut s, Action::Key(plain('!')));
        assert_eq!(s.command, "q!");
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        assert_eq!(s.command, "q");
    }

    #[test]
    fn command_submit_q_quits() {
        let mut s = AppState::new();
        s.mode = Mode::Command;
        s.command = "q".into();
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(!s.running);
        assert_eq!(s.mode, Mode::Normal);
        assert!(cmds.is_empty());
    }

    #[test]
    fn ctrl_w_chord_inert_in_terminal_mode() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.mode = Mode::Terminal;
        // No session → key just goes nowhere; the point is the chord must NOT trigger.
        let _ = reduce(&mut s, Action::Key(ctrl('w')));
        assert!(s.pending_chord.is_empty());
        let cmds = reduce(&mut s, Action::Key(plain('>')));
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        // No writes (no active session in this test).
        assert!(cmds.is_empty());
    }

    #[test]
    fn key_in_normal_mode_does_not_forward_to_session() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let fake = FakeSession::new(5);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        s.projects = vec![Project {
            slug: "p".into(),
            name: "p".into(),
            repo_path: PathBuf::from("."),
            worktrees: vec![Worktree {
                name: "w".into(),
                path: PathBuf::from("."),
                branch: Some("w".into()),
                sessions: vec![5],
                active_tab: Some(0),
            }],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        }];
        s.active_worktree = Some((0, 0));
        let cmds = reduce(&mut s, Action::Key(plain('a')));
        assert!(cmds.is_empty());
    }

    #[test]
    fn key_in_terminal_mode_forwards_to_focused_session() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.mode = Mode::Terminal;
        let fake = FakeSession::new(8);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        s.projects = vec![Project {
            slug: "p".into(),
            name: "p".into(),
            repo_path: PathBuf::from("."),
            worktrees: vec![Worktree {
                name: "w".into(),
                path: PathBuf::from("."),
                branch: Some("w".into()),
                sessions: vec![8],
                active_tab: Some(0),
            }],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        }];
        s.active_worktree = Some((0, 0));
        let cmds = reduce(&mut s, Action::Key(plain('a')));
        assert!(matches!(cmds.as_slice(), [Command::WriteKey(8, _)]));
    }

    fn mk_state_with_mock_projects() -> AppState {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.projects = mock_projects();
        s.sidebar_selection = Some((0, None)); // first project header
        s
    }

    #[test]
    fn launch_with_no_arg_opens_popup_with_terminal_first() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.projects[0].launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude".into(),
        }];
        let cmds = submit_command(&mut s, "launch");
        assert!(cmds.is_empty());
        let popup = s.launch_popup.as_ref().expect("launch popup open");
        assert_eq!(popup.entries.len(), 2);
        assert_eq!(popup.entries[0].label, "Terminal");
        assert!(popup.entries[0].command.is_none());
        assert_eq!(popup.entries[1].label, "claude");
        assert_eq!(popup.entries[1].command.as_deref(), Some("claude"));
    }

    #[test]
    fn launch_with_named_arg_spawns_directly() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.projects[0].launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude --resume".into(),
        }];
        let cmds = submit_command(&mut s, "launch claude");
        assert!(s.launch_popup.is_none());
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { initial_command: Some(c), .. }]
                if c == "claude --resume"
        ));
    }

    #[test]
    fn launch_terminal_spawns_plain_shell() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        let cmds = submit_command(&mut s, "launch terminal");
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree {
                initial_command: None,
                ..
            }]
        ));
    }

    #[test]
    fn launch_named_falls_back_to_global() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.global_launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude --global".into(),
        }];
        let cmds = submit_command(&mut s, "launch claude");
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { initial_command: Some(c), .. }]
                if c == "claude --global"
        ));
    }

    #[test]
    fn launch_project_shadows_global_with_same_name() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.projects[0].launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude --project".into(),
        }];
        s.global_launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude --global".into(),
        }];
        let cmds = submit_command(&mut s, "launch claude");
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { initial_command: Some(c), .. }]
                if c == "claude --project"
        ));
    }

    #[test]
    fn launch_popup_includes_globals_with_dedup() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.projects[0].launchers = vec![crate::app::Launcher {
            name: "dev".into(),
            command: "pnpm dev".into(),
        }];
        s.global_launchers = vec![
            crate::app::Launcher {
                name: "dev".into(),
                command: "ignored".into(),
            },
            crate::app::Launcher {
                name: "repl".into(),
                command: "node".into(),
            },
        ];
        let _ = submit_command(&mut s, "launch");
        let popup = s.launch_popup.as_ref().unwrap();
        let labels: Vec<_> = popup.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["Terminal", "dev", "repl"]);
        assert_eq!(popup.entries[1].source, crate::app::LaunchSource::Project);
        assert_eq!(popup.entries[2].source, crate::app::LaunchSource::Global);
    }

    #[test]
    fn launch_unknown_name_sets_status() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        let cmds = submit_command(&mut s, "launch nope");
        assert!(cmds.is_empty());
        assert!(s.command_status.as_deref().unwrap().contains("nope"));
    }

    #[test]
    fn launch_popup_enter_spawns_pinned_to_open_worktree() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        s.projects[0].launchers = vec![crate::app::Launcher {
            name: "claude".into(),
            command: "claude".into(),
        }];
        let _ = submit_command(&mut s, "launch");
        let _ = reduce(&mut s, Action::Key(plain('j'))); // move to claude
        // Swap active worktree away to verify the popup remembers its target.
        s.active_worktree = Some((0, 1));
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(s.launch_popup.is_none());
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { dest, initial_command: Some(c), .. }]
                if *dest == (0, 0) && c == "claude"
        ));
    }

    #[test]
    fn h_and_l_switch_ui_focus() {
        let mut s = AppState::new();
        s.ui_focus = UiFocus::Sidebar;
        let _ = reduce(&mut s, Action::Key(plain('l')));
        assert_eq!(s.ui_focus, UiFocus::Terminal);
        let _ = reduce(&mut s, Action::Key(plain('h')));
        assert_eq!(s.ui_focus, UiFocus::Sidebar);
    }

    #[test]
    fn j_walks_visible_rows() {
        let mut s = mk_state_with_mock_projects();
        // imbuia (header) → imbuia/main → imbuia/feat-x → brick (header) → ...
        let _ = reduce(&mut s, Action::Key(plain('j')));
        assert_eq!(s.sidebar_selection, Some((0, Some(0))));
        let _ = reduce(&mut s, Action::Key(plain('j')));
        assert_eq!(s.sidebar_selection, Some((0, Some(1))));
        let _ = reduce(&mut s, Action::Key(plain('j')));
        assert_eq!(s.sidebar_selection, Some((1, None)));
    }

    #[test]
    fn j_clamps_at_last_row() {
        let mut s = mk_state_with_mock_projects();
        for _ in 0..50 {
            let _ = reduce(&mut s, Action::Key(plain('j')));
        }
        // scratch is collapsed, so last visible row is the scratch header.
        assert_eq!(s.sidebar_selection, Some((2, None)));
    }

    #[test]
    fn k_clamps_at_first_row() {
        let mut s = mk_state_with_mock_projects();
        for _ in 0..5 {
            let _ = reduce(&mut s, Action::Key(plain('k')));
        }
        assert_eq!(s.sidebar_selection, Some((0, None)));
    }

    #[test]
    fn jk_inert_when_terminal_focused() {
        let mut s = mk_state_with_mock_projects();
        s.ui_focus = UiFocus::Terminal;
        let initial = s.sidebar_selection;
        let _ = reduce(&mut s, Action::Key(plain('j')));
        assert_eq!(s.sidebar_selection, initial);
    }

    #[test]
    fn enter_on_project_header_toggles_expand() {
        let mut s = mk_state_with_mock_projects();
        s.sidebar_selection = Some((2, None)); // scratch (collapsed)
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(s.projects[2].expanded);
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(!s.projects[2].expanded);
    }

    #[test]
    fn enter_on_empty_worktree_activates_without_spawning() {
        let mut s = mk_state_with_mock_projects();
        s.sidebar_selection = Some((0, Some(0)));
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(s.active_worktree, Some((0, 0)));
        // Focus stays on sidebar; no PTY auto-spawn.
        assert_eq!(s.ui_focus, UiFocus::Sidebar);
        assert!(cmds.is_empty());
        assert!(s.projects[0].worktrees[0].sessions.is_empty());
    }

    #[test]
    fn enter_on_worktree_with_sessions_doesnt_respawn() {
        let mut s = mk_state_with_mock_projects();
        let fake = FakeSession::new(11);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        s.sidebar_selection = Some((0, Some(0)));
        s.projects[0].worktrees[0].active_tab = None; // pretend no active tab
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(s.active_worktree, Some((0, 0)));
        assert!(cmds.is_empty());
        // Pre-existing sessions: active_tab restored to 0.
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
    }

    #[test]
    fn session_spawned_appends_to_worktree_and_focuses_tab() {
        let mut s = mk_state_with_mock_projects();
        let fake = FakeSession::new(42);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (1, 0),
            },
        );
        assert_eq!(s.projects[1].worktrees[0].sessions, vec![42]);
        assert_eq!(s.projects[1].worktrees[0].active_tab, Some(0));
        assert!(s.sessions.contains_key(&42));
    }

    fn mk_state_with_two_tabs() -> AppState {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: FakeSession::new(101),
                dest: (0, 0),
            },
        );
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: FakeSession::new(102),
                dest: (0, 0),
            },
        );
        // active_tab is now Some(1) (last spawned).
        s
    }

    #[test]
    fn gt_advances_tab_with_wrap() {
        let mut s = mk_state_with_two_tabs();
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
        let _ = reduce(&mut s, Action::Key(plain('g')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(plain('t')));
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
        assert!(s.pending_chord.is_empty());
    }

    #[test]
    fn shift_g_t_retreats_tab_with_wrap() {
        let mut s = mk_state_with_two_tabs();
        s.projects[0].worktrees[0].active_tab = Some(0);
        let _ = reduce(&mut s, Action::Key(plain('g')));
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)),
        );
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
    }

    #[test]
    fn g_then_unknown_clears_leader() {
        let mut s = mk_state_with_two_tabs();
        let _ = reduce(&mut s, Action::Key(plain('g')));
        let _ = reduce(&mut s, Action::Key(plain('z')));
        assert!(s.pending_chord.is_empty());
        // active_tab unchanged
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
    }

    #[test]
    fn o_emits_spawn_for_active_worktree() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((1, 0));
        let cmds = reduce(&mut s, Action::Key(plain('o')));
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { dest: (1, 0), .. }]
        ));
    }

    #[test]
    fn o_inert_without_active_worktree() {
        let mut s = mk_state_with_mock_projects();
        let cmds = reduce(&mut s, Action::Key(plain('o')));
        assert!(cmds.is_empty());
    }

    #[test]
    fn x_closes_current_tab_and_drops_session() {
        let mut s = mk_state_with_two_tabs();
        let _ = reduce(&mut s, Action::Key(plain('x')));
        // We removed the active tab (index 1). 101 remains.
        assert_eq!(s.projects[0].worktrees[0].sessions, vec![101]);
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
        assert!(!s.sessions.contains_key(&102));
        assert!(s.sessions.contains_key(&101));
    }

    #[test]
    fn x_on_last_tab_leaves_active_worktree_with_no_tabs() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: FakeSession::new(33),
                dest: (0, 0),
            },
        );
        let _ = reduce(&mut s, Action::Key(plain('x')));
        assert_eq!(s.active_worktree, Some((0, 0)));
        assert!(s.projects[0].worktrees[0].sessions.is_empty());
        assert_eq!(s.projects[0].worktrees[0].active_tab, None);
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn tab_keys_inert_in_terminal_mode() {
        let mut s = mk_state_with_two_tabs();
        s.mode = Mode::Terminal;
        // 'g' should be forwarded to the PTY (it's just a letter in Terminal mode).
        let cmds = reduce(&mut s, Action::Key(plain('g')));
        assert!(s.pending_chord.is_empty());
        assert!(matches!(cmds.as_slice(), [Command::WriteKey(_, _)]));
    }

    fn submit_command(s: &mut AppState, line: &str) -> Commands {
        s.mode = Mode::Command;
        s.command = line.into();
        reduce(
            s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        )
    }

    #[test]
    fn cmd_tabnew_emits_spawn_for_active_worktree() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((2, 0));
        let cmds = submit_command(&mut s, "tabnew");
        assert!(matches!(
            cmds.as_slice(),
            [Command::SpawnInWorktree { dest: (2, 0), .. }]
        ));
    }

    #[test]
    fn cmd_tabclose_closes_current_tab() {
        let mut s = mk_state_with_two_tabs();
        let _ = submit_command(&mut s, "tabclose");
        assert_eq!(s.projects[0].worktrees[0].sessions, vec![101]);
        assert!(!s.sessions.contains_key(&102));
    }

    #[test]
    fn cmd_set_sidebar_width_space_separated() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = submit_command(&mut s, "set sidebar.width 30");
        assert_eq!(s.sidebar_width, 30);
        assert!(s.command_status.is_none());
    }

    #[test]
    fn cmd_set_sidebar_width_equals_form() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = submit_command(&mut s, "set sidebar.width=40");
        assert_eq!(s.sidebar_width, 40);
        assert!(s.command_status.is_none());
    }

    #[test]
    fn cmd_set_invalid_width_sets_status() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        let _ = submit_command(&mut s, "set sidebar.width=abc");
        assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(s.command_status.as_deref(), Some("invalid width: abc"));
    }

    #[test]
    fn cmd_unknown_sets_status() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "frobnicate");
        assert!(
            s.command_status
                .as_deref()
                .unwrap()
                .starts_with("not a command")
        );
    }

    #[test]
    fn edit_command_opens_popup_with_existing_script() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.projects[0].setup_script = Some("npm install\nnpm run dev".into());
        s.sidebar_selection = Some((0, None));
        let _ = submit_command(&mut s, "edit");
        let popup = s.edit_popup.as_ref().expect("popup opened");
        assert_eq!(popup.project_idx, 0);
        assert_eq!(popup.textarea.lines(), &["npm install", "npm run dev"]);
    }

    #[test]
    fn edit_popup_ctrl_s_saves_and_closes() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.sidebar_selection = Some((1, None));
        let _ = submit_command(&mut s, "edit");
        // Type a script via direct mutation (would otherwise need full key sim).
        s.edit_popup
            .as_mut()
            .unwrap()
            .textarea
            .insert_str("echo hi");
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );
        assert!(s.edit_popup.is_none());
        assert_eq!(s.projects[1].setup_script.as_deref(), Some("echo hi"));
        assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(1)]));
    }

    #[test]
    fn edit_popup_esc_cancels() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.projects[0].setup_script = Some("orig".into());
        s.sidebar_selection = Some((0, None));
        let _ = submit_command(&mut s, "edit");
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(s.edit_popup.is_none());
        assert_eq!(s.projects[0].setup_script.as_deref(), Some("orig"));
    }

    #[test]
    fn cmd_set_theme_switches_palette_and_saves() {
        use crate::app::Command;
        use crate::theme::ThemeKind;
        let mut s = AppState::new();
        let cmds = submit_command(&mut s, "set theme=light");
        assert_eq!(s.theme.kind, ThemeKind::Light);
        assert!(cmds.iter().any(|c| matches!(c, Command::SaveGlobalConfig)));

        let cmds = submit_command(&mut s, "set theme=dark");
        assert_eq!(s.theme.kind, ThemeKind::Dark);
        assert!(cmds.iter().any(|c| matches!(c, Command::SaveGlobalConfig)));

        let _ = submit_command(&mut s, "set theme=neon");
        assert!(
            s.command_status
                .as_deref()
                .unwrap()
                .starts_with("invalid theme")
        );
    }

    #[test]
    fn cmd_set_unknown_key_sets_status() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "set nope=foo");
        assert_eq!(s.command_status.as_deref(), Some("unknown setting: nope"));
    }

    #[test]
    fn colon_clears_previous_status() {
        let mut s = AppState::new();
        s.command_status = Some("stale".into());
        let _ = reduce(&mut s, Action::Key(plain(':')));
        assert_eq!(s.mode, Mode::Command);
        assert!(s.command_status.is_none());
    }

    fn mouse_at(col: u16, row: u16, kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn left_down(col: u16, row: u16) -> MouseEvent {
        mouse_at(col, row, MouseEventKind::Down(MouseButton::Left))
    }

    #[test]
    fn click_on_sidebar_worktree_activates_it() {
        let mut s = mk_state_with_mock_projects();
        s.term_size = TermSize::new(40, 100);
        // Visible rows (sidebar inner starts at row 1):
        //   row 1: imbuia header
        //   row 2: imbuia/main
        //   row 3: imbuia/feat-x
        //   row 4: brick header
        //   row 5: brick/main
        let cmds = reduce(&mut s, Action::Mouse(left_down(2, 2)));
        assert_eq!(s.active_worktree, Some((0, 0)));
        assert_eq!(s.ui_focus, UiFocus::Sidebar);
        assert!(cmds.is_empty());
        assert!(s.projects[0].worktrees[0].sessions.is_empty());
    }

    #[test]
    fn click_on_sidebar_project_header_toggles_expand() {
        let mut s = mk_state_with_mock_projects();
        s.term_size = TermSize::new(40, 100);
        // scratch header position depends on imbuia (1 header + 2 worktrees)
        // + brick (1 header + 2 worktrees) = 6 rows before scratch.
        // scratch header is at visible index 6 → row = sidebar.y + 1 + 6 = 7.
        let cmds = reduce(&mut s, Action::Mouse(left_down(2, 7)));
        assert!(s.projects[2].expanded);
        // Expand-toggle persists.
        assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(2)]));
        assert_eq!(s.ui_focus, UiFocus::Sidebar);
    }

    #[test]
    fn click_on_tab_bar_switches_tab() {
        let mut s = mk_state_with_two_tabs();
        s.term_size = TermSize::new(40, 100);
        // Tab 0 spans cols 24..27 inside tab_bar (sidebar=24).
        // Tab bar row 0 = chrome.tab_bar.y. Click col 24 (start of tab 0).
        let _ = reduce(&mut s, Action::Mouse(left_down(24, 0)));
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
        assert_eq!(s.ui_focus, UiFocus::Terminal);
    }

    #[test]
    fn click_on_tab_bar_beyond_last_tab_is_noop() {
        let mut s = mk_state_with_two_tabs();
        s.term_size = TermSize::new(40, 100);
        // Two tabs occupy cols 24..31; col 40 is past them.
        let _ = reduce(&mut s, Action::Mouse(left_down(40, 0)));
        assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
    }

    #[test]
    fn left_click_in_terminal_auto_enters_terminal_mode() {
        let mut s = mk_state_with_two_tabs();
        s.term_size = TermSize::new(40, 100);
        assert_eq!(s.mode, Mode::Normal);
        let _ = reduce(&mut s, Action::Mouse(left_down(30, 5)));
        assert_eq!(s.mode, Mode::Terminal);
    }

    #[test]
    fn scroll_in_sidebar_shifts_offset_and_clamps() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(6, 100); // small viewport: ~5 visible rows
        s.projects = mock_projects();
        s.sidebar_selection = Some((0, None));

        // Scroll down twice — offset should grow but never exceed (total - visible).
        let down = mouse_at(2, 2, MouseEventKind::ScrollDown);
        let _ = reduce(&mut s, Action::Mouse(down));
        let after_one = s.sidebar_scroll;
        assert!(after_one > 0);
        let _ = reduce(&mut s, Action::Mouse(down));
        // Scroll up — should bring offset back to zero (saturating).
        let up = mouse_at(2, 2, MouseEventKind::ScrollUp);
        let _ = reduce(&mut s, Action::Mouse(up));
        let _ = reduce(&mut s, Action::Mouse(up));
        assert_eq!(s.sidebar_scroll, 0);
    }

    #[test]
    fn keyboard_nav_auto_scrolls_sidebar() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(4, 100); // 3 visible rows after title
        s.projects = mock_projects();
        s.sidebar_selection = Some((0, None));
        // Press j several times to walk past the bottom of the viewport.
        for _ in 0..5 {
            let _ = reduce(&mut s, Action::Key(plain('j')));
        }
        assert!(s.sidebar_scroll > 0);
    }

    #[test]
    fn scroll_in_terminal_doesnt_change_mode() {
        let mut s = mk_state_with_two_tabs();
        s.term_size = TermSize::new(40, 100);
        let scroll = mouse_at(30, 5, MouseEventKind::ScrollDown);
        let _ = reduce(&mut s, Action::Mouse(scroll));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn click_in_terminal_forwards_to_session() {
        let mut s = mk_state_with_two_tabs();
        s.term_size = TermSize::new(40, 100);
        // chrome: sidebar=24, tab_bar=2 rows, action_bar=1 row.
        // Terminal rect starts at col 24, row 2.
        let click = left_down(30, 5);
        let cmds = reduce(&mut s, Action::Mouse(click));
        assert_eq!(s.ui_focus, UiFocus::Terminal);
        match cmds.as_slice() {
            [Command::WriteMouse(102, ev)] => {
                assert_eq!(ev.column, 30 - 24);
                assert_eq!(ev.row, 5 - 2);
            }
            other => panic!("unexpected commands: {other:?}"),
        }
    }

    #[test]
    fn non_left_click_in_sidebar_only_changes_focus() {
        let mut s = mk_state_with_mock_projects();
        s.term_size = TermSize::new(40, 100);
        s.ui_focus = UiFocus::Terminal;
        let m = mouse_at(2, 2, MouseEventKind::ScrollDown);
        let cmds = reduce(&mut s, Action::Mouse(m));
        // Focus moved to sidebar; no activation happened.
        assert_eq!(s.ui_focus, UiFocus::Sidebar);
        assert_eq!(s.active_worktree, None);
        assert!(cmds.is_empty());
    }

    #[test]
    fn help_command_opens_popup() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "help");
        assert!(s.help_open);
    }

    #[test]
    fn help_alias_h_also_opens_popup() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "h");
        assert!(s.help_open);
    }

    #[test]
    fn esc_closes_help_popup() {
        let mut s = AppState::new();
        s.help_open = true;
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(!s.help_open);
    }

    #[test]
    fn j_scrolls_help_popup_without_closing() {
        let mut s = AppState::new();
        s.help_open = true;
        s.help_max_scroll.set(20);
        let _ = reduce(&mut s, Action::Key(plain('j')));
        assert!(s.help_open);
        assert_eq!(s.help_scroll, 1);
        let _ = reduce(&mut s, Action::Key(plain('k')));
        assert_eq!(s.help_scroll, 0);
    }

    #[test]
    fn help_scroll_clamps_to_max() {
        let mut s = AppState::new();
        s.help_open = true;
        s.help_max_scroll.set(5);
        for _ in 0..50 {
            let _ = reduce(&mut s, Action::Key(plain('j')));
        }
        assert_eq!(s.help_scroll, 5);
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        );
        assert_eq!(s.help_scroll, 0);
    }

    #[test]
    fn left_click_dismisses_help_popup() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.help_open = true;
        let _ = reduce(&mut s, Action::Mouse(left_down(50, 10)));
        assert!(!s.help_open);
    }

    #[test]
    fn colon_dismisses_help_popup() {
        let mut s = AppState::new();
        s.help_open = true;
        let _ = reduce(&mut s, Action::Key(plain(':')));
        assert!(!s.help_open);
        assert_eq!(s.mode, Mode::Command);
    }

    #[test]
    fn expand_user_path_handles_tilde() {
        let home = std::path::Path::new("/tmp/imbuia-test-home");
        assert_eq!(expand_user_path("~", Some(home)), PathBuf::from(home));
        assert_eq!(
            expand_user_path("~/projects/foo", Some(home)),
            PathBuf::from("/tmp/imbuia-test-home/projects/foo")
        );
        assert_eq!(
            expand_user_path("./rel", Some(home)),
            PathBuf::from("./rel")
        );
        assert_eq!(
            expand_user_path("/abs/x", Some(home)),
            PathBuf::from("/abs/x")
        );
        // With no HOME, `~` stays literal.
        assert_eq!(expand_user_path("~", None), PathBuf::from("~"));
    }

    #[test]
    fn cmd_open_with_arg_emits_open_command() {
        let mut s = AppState::new();
        let cmds = submit_command(&mut s, "open /tmp/some-repo");
        assert!(matches!(
            cmds.as_slice(),
            [Command::OpenProject { path, setup_script: None, .. }]
                if path.as_os_str() == "/tmp/some-repo"
        ));
        assert!(s.open_project_popup.is_none());
    }

    #[test]
    fn cmd_open_without_arg_opens_popup() {
        let mut s = AppState::new();
        let cmds = submit_command(&mut s, "open");
        assert!(cmds.is_empty());
        assert!(s.open_project_popup.is_some());
    }

    #[test]
    fn space_leader_o_opens_open_project_popup() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::Key(plain(' ')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(plain('o')));
        assert!(s.pending_chord.is_empty());
        assert!(s.open_project_popup.is_some());
    }

    #[test]
    fn space_leader_l_opens_launch_popup() {
        let mut s = mk_state_with_mock_projects();
        s.active_worktree = Some((0, 0));
        let _ = reduce(&mut s, Action::Key(plain(' ')));
        let _ = reduce(&mut s, Action::Key(plain('l')));
        assert!(s.launch_popup.is_some());
    }

    #[test]
    fn space_leader_q_quits() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::Key(plain(' ')));
        let _ = reduce(&mut s, Action::Key(plain('q')));
        assert!(!s.running);
    }

    #[test]
    fn space_leader_esc_cancels() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::Key(plain(' ')));
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(s.pending_chord.is_empty());
        assert!(s.open_project_popup.is_none());
        assert!(s.running);
    }

    #[test]
    fn update_checked_some_sets_available_update() {
        let mut s = AppState::new();
        s.update_status = crate::app::UpdateStatus::Checking;
        let info = crate::updater::UpdateInfo {
            latest_tag: "v9.9.9".into(),
            latest_version: semver::Version::parse("9.9.9").unwrap(),
        };
        let _ = reduce(&mut s, Action::UpdateChecked(Ok(Some(info))));
        assert_eq!(
            s.available_update.as_ref().map(|i| i.latest_tag.as_str()),
            Some("v9.9.9")
        );
        assert_eq!(s.update_status, crate::app::UpdateStatus::Idle);
    }

    #[test]
    fn update_checked_none_clears_available_update() {
        let mut s = AppState::new();
        s.available_update = Some(crate::updater::UpdateInfo {
            latest_tag: "v9.9.9".into(),
            latest_version: semver::Version::parse("9.9.9").unwrap(),
        });
        let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
        assert!(s.available_update.is_none());
    }

    #[test]
    fn update_installed_no_restart_shows_friendly_status() {
        let mut s = AppState::new();
        let outcome = crate::updater::InstallOutcome {
            installed_to: std::path::PathBuf::from("/tmp"),
            supervisor_restart_required: false,
            installed_tag: "v0.4.0".into(),
        };
        let _ = reduce(&mut s, Action::UpdateInstalled(Ok(outcome)));
        let status = s.command_status.as_deref().unwrap();
        assert!(status.contains("v0.4.0"));
        assert!(status.contains("relaunch"));
        assert!(!status.contains(":rs"));
        assert_eq!(
            s.update_status,
            crate::app::UpdateStatus::InstalledPendingRestart
        );
    }

    #[test]
    fn update_installed_with_protocol_bump_asks_for_rs() {
        let mut s = AppState::new();
        let outcome = crate::updater::InstallOutcome {
            installed_to: std::path::PathBuf::from("/tmp"),
            supervisor_restart_required: true,
            installed_tag: "v0.4.0".into(),
        };
        let _ = reduce(&mut s, Action::UpdateInstalled(Ok(outcome)));
        let status = s.command_status.as_deref().unwrap();
        assert!(status.contains(":rs"));
    }

    #[test]
    fn periodic_update_check_emits_command_when_idle() {
        let mut s = AppState::new();
        let cmds = reduce(&mut s, Action::PeriodicUpdateCheck);
        assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
        assert_eq!(s.update_status, crate::app::UpdateStatus::Checking);
    }

    #[test]
    fn periodic_update_check_is_inert_while_installing() {
        let mut s = AppState::new();
        s.update_status = crate::app::UpdateStatus::Installing;
        let cmds = reduce(&mut s, Action::PeriodicUpdateCheck);
        assert!(cmds.is_empty());
    }

    #[test]
    fn cmd_update_check_pushes_check_command() {
        let mut s = AppState::new();
        let cmds = submit_command(&mut s, "update check");
        assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
        assert_eq!(s.update_status, crate::app::UpdateStatus::Checking);
    }

    #[test]
    fn cmd_update_with_cached_info_installs_directly() {
        let mut s = AppState::new();
        s.available_update = Some(crate::updater::UpdateInfo {
            latest_tag: "v9.9.9".into(),
            latest_version: semver::Version::parse("9.9.9").unwrap(),
        });
        let cmds = submit_command(&mut s, "update");
        assert!(matches!(
            cmds.as_slice(),
            [Command::InstallUpdate { tag }] if tag == "v9.9.9"
        ));
        assert_eq!(s.update_status, crate::app::UpdateStatus::Installing);
    }

    #[test]
    fn cmd_update_without_cache_kicks_a_check() {
        let mut s = AppState::new();
        let cmds = submit_command(&mut s, "update");
        assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
        assert!(s.auto_install_after_check);
    }

    #[test]
    fn cmd_update_check_does_not_arm_auto_install() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "update check");
        assert!(!s.auto_install_after_check);
    }

    #[test]
    fn update_check_then_some_auto_installs_when_armed() {
        // Simulates: user types :update with no cached info, the check fires,
        // and a newer release is found — we should auto-install instead of
        // leaving the user staring at "checking for updates…".
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "update");
        assert!(s.auto_install_after_check);
        let info = crate::updater::UpdateInfo {
            latest_tag: "v9.9.9".into(),
            latest_version: semver::Version::parse("9.9.9").unwrap(),
        };
        let cmds = reduce(&mut s, Action::UpdateChecked(Ok(Some(info))));
        assert!(!s.auto_install_after_check);
        assert_eq!(s.update_status, crate::app::UpdateStatus::Installing);
        assert!(matches!(
            cmds.as_slice(),
            [Command::InstallUpdate { tag }] if tag == "v9.9.9"
        ));
    }

    #[test]
    fn update_check_then_none_when_armed_says_up_to_date() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "update");
        let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
        assert!(!s.auto_install_after_check);
        assert!(
            s.command_status
                .as_deref()
                .unwrap()
                .contains("already on the latest")
        );
    }

    #[test]
    fn background_check_does_not_set_command_status() {
        // Hourly tick → PeriodicUpdateCheck → CheckForUpdate. When the check
        // returns None, the status row should NOT chirp at the user.
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::PeriodicUpdateCheck);
        assert!(!s.auto_install_after_check);
        let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
        assert!(s.command_status.is_none());
    }

    #[test]
    fn open_popup_chars_and_enter_emit_open_command() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "open");
        let _ = reduce(&mut s, Action::Key(plain('/')));
        let _ = reduce(&mut s, Action::Key(plain('t')));
        let _ = reduce(&mut s, Action::Key(plain('m')));
        let _ = reduce(&mut s, Action::Key(plain('p')));
        assert_eq!(s.open_project_popup.as_ref().unwrap().path, "/tmp");
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(s.open_project_popup.is_none());
        assert!(matches!(
            cmds.as_slice(),
            [Command::OpenProject { path, setup_script: None, .. }]
                if path.as_os_str() == "/tmp"
        ));
    }

    #[test]
    fn open_popup_tab_then_script_then_ctrl_s_passes_script() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "open");
        let _ = reduce(&mut s, Action::Key(plain('/')));
        let _ = reduce(&mut s, Action::Key(plain('x')));
        // Tab to script, type a command, then Ctrl-S to submit.
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        let _ = reduce(&mut s, Action::Key(plain('e')));
        let _ = reduce(&mut s, Action::Key(plain('c')));
        let _ = reduce(&mut s, Action::Key(plain('h')));
        let _ = reduce(&mut s, Action::Key(plain('o')));
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        );
        assert!(s.open_project_popup.is_none());
        assert!(matches!(
            cmds.as_slice(),
            [Command::OpenProject { path, setup_script: Some(script), .. }]
                if path.as_os_str() == "/x" && script == "echo"
        ));
    }

    #[test]
    fn open_popup_esc_cancels_without_dispatch() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "open");
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(s.open_project_popup.is_none());
        assert!(cmds.is_empty());
    }

    #[test]
    fn worktree_command_requires_selected_project() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "worktree feat-x");
        assert!(
            s.command_status
                .as_deref()
                .unwrap()
                .contains("select a project")
        );
    }

    #[test]
    fn cmd_open_sets_pending_op() {
        let mut s = AppState::new();
        let _ = submit_command(&mut s, "open /tmp/x");
        assert!(s.pending_op.as_deref().unwrap().contains("Opening"));
    }

    #[test]
    fn project_opened_clears_pending_op() {
        let mut s = AppState::new();
        s.pending_op = Some("Opening …".into());
        let project = Project {
            slug: "x".into(),
            name: "x".into(),
            repo_path: PathBuf::from("/tmp/x"),
            worktrees: vec![],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        };
        let _ = reduce(
            &mut s,
            Action::ProjectOpened {
                project,
                import_existing: false,
            },
        );
        assert!(s.pending_op.is_none());
    }

    #[test]
    fn worktree_added_clears_pending_op() {
        let mut s = mk_state_with_mock_projects();
        s.pending_op = Some("Creating worktree…".into());
        let _ = reduce(
            &mut s,
            Action::WorktreeAdded {
                project_idx: 0,
                worktree: Worktree {
                    name: "x".into(),
                    path: PathBuf::from("/tmp/x"),
                    branch: Some("x".into()),
                    sessions: Vec::new(),
                    active_tab: None,
                },
            },
        );
        assert!(s.pending_op.is_none());
    }

    #[test]
    fn operation_failed_clears_pending_op() {
        let mut s = AppState::new();
        s.pending_op = Some("Opening…".into());
        let _ = reduce(&mut s, Action::OperationFailed("nope".into()));
        assert!(s.pending_op.is_none());
        assert_eq!(s.command_status.as_deref(), Some("nope"));
    }

    #[test]
    fn worktree_command_with_arg_emits_add_worktree() {
        let mut s = mk_state_with_mock_projects();
        s.sidebar_selection = Some((0, None));
        let cmds = submit_command(&mut s, "worktree feat-x");
        assert!(matches!(
            cmds.as_slice(),
            [Command::AddWorktree { project_idx: 0, branch, .. }] if branch == "feat-x"
        ));
    }

    #[test]
    fn project_opened_action_appends_and_saves() {
        let mut s = AppState::new();
        let project = Project {
            slug: "test".into(),
            name: "test".into(),
            repo_path: PathBuf::from("/tmp/test"),
            worktrees: vec![],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        };
        let cmds = reduce(
            &mut s,
            Action::ProjectOpened {
                project,
                import_existing: false,
            },
        );
        assert_eq!(s.projects.len(), 1);
        assert_eq!(s.projects[0].slug, "test");
        assert!(matches!(
            cmds.as_slice(),
            [Command::SaveGlobalConfig, Command::SaveProjectConfig(0)]
        ));
    }

    #[test]
    fn worktree_added_action_appends_and_saves() {
        let mut s = mk_state_with_mock_projects();
        let cmds = reduce(
            &mut s,
            Action::WorktreeAdded {
                project_idx: 0,
                worktree: Worktree {
                    name: "feat-y".into(),
                    path: PathBuf::from("/tmp/feat-y"),
                    branch: Some("feat-y".into()),
                    sessions: Vec::new(),
                    active_tab: None,
                },
            },
        );
        assert_eq!(s.projects[0].worktrees.len(), 3);
        assert_eq!(s.projects[0].worktrees.last().unwrap().name, "feat-y");
        // Two commands: persist the project + spawn a terminal in the new worktree.
        assert!(matches!(
            cmds.as_slice(),
            [
                Command::SaveProjectConfig(0),
                Command::SpawnInWorktree {
                    dest: (0, 2),
                    initial_command: None,
                    ..
                },
            ]
        ));
        assert_eq!(s.active_worktree, Some((0, 2)));
    }

    #[test]
    fn worktree_added_with_setup_script_passes_it_to_spawn() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.projects = mock_projects();
        s.projects[0].setup_script = Some("npm install".into());
        let cmds = reduce(
            &mut s,
            Action::WorktreeAdded {
                project_idx: 0,
                worktree: Worktree {
                    name: "feat-z".into(),
                    path: PathBuf::from("/tmp/feat-z"),
                    branch: Some("feat-z".into()),
                    sessions: Vec::new(),
                    active_tab: None,
                },
            },
        );
        let spawn = cmds
            .iter()
            .find_map(|c| match c {
                Command::SpawnInWorktree {
                    initial_command, ..
                } => Some(initial_command.clone()),
                _ => None,
            })
            .expect("spawn command emitted");
        assert_eq!(spawn.as_deref(), Some("npm install"));
    }

    #[test]
    fn worktree_remove_command_emits_remove_when_non_main() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        // mock_projects has imbuia with [main, feat-x]; give feat-x a path
        // that differs from repo_path so it doesn't trip the main-guard.
        s.projects[0].worktrees[1].path = PathBuf::from("/tmp/feat-x");
        s.sidebar_selection = Some((0, Some(1)));
        let cmds = submit_command(&mut s, "worktree-remove");
        // First step: a confirmation is staged, nothing is dispatched yet.
        assert!(cmds.is_empty());
        assert!(matches!(
            s.pending_confirm,
            Some(PendingConfirm::RemoveWorktree {
                project_idx: 0,
                worktree_idx: 1,
                ..
            })
        ));
        // Confirming with `y` emits the RemoveWorktree command and clears the
        // confirmation in favor of a pending_op for the action bar.
        let cmds = reduce(&mut s, Action::Key(plain('y')));
        assert!(s.pending_confirm.is_none());
        assert!(s.pending_op.is_some());
        assert!(matches!(
            cmds.as_slice(),
            [Command::RemoveWorktree {
                project_idx: 0,
                worktree_idx: 1,
                ..
            }]
        ));
    }

    #[test]
    fn worktree_remove_confirmation_cancelled_by_n() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.projects[0].worktrees[1].path = PathBuf::from("/tmp/feat-x");
        s.sidebar_selection = Some((0, Some(1)));
        let _ = submit_command(&mut s, "worktree-remove");
        assert!(s.pending_confirm.is_some());
        let cmds = reduce(&mut s, Action::Key(plain('n')));
        assert!(s.pending_confirm.is_none());
        assert!(s.pending_op.is_none());
        assert!(cmds.is_empty());
    }

    #[test]
    fn worktree_remove_refuses_main_worktree() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        // main worktree's path == repo_path; sidebar_selection on it.
        s.sidebar_selection = Some((0, Some(0)));
        let cmds = submit_command(&mut s, "worktree-remove");
        assert!(cmds.is_empty());
        assert_eq!(
            s.command_status.as_deref(),
            Some("can't remove the main worktree")
        );
    }

    #[test]
    fn worktree_removed_action_drops_entry_and_fixes_selection() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.active_worktree = Some((0, 1));
        s.sidebar_selection = Some((0, Some(1)));
        let _ = reduce(
            &mut s,
            Action::WorktreeRemoved {
                project_idx: 0,
                worktree_idx: 1,
            },
        );
        assert_eq!(s.projects[0].worktrees.len(), 1);
        assert_eq!(s.active_worktree, None);
        assert_eq!(s.sidebar_selection, Some((0, Some(0))));
    }

    #[test]
    fn operation_failed_sets_command_status() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::OperationFailed("bad path".into()));
        assert_eq!(s.command_status.as_deref(), Some("bad path"));
    }

    #[test]
    fn resize_broadcasts_terminal_pane_size() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 100);
        s.projects = mock_projects();
        let fake = FakeSession::new(3);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        let cmds = reduce(&mut s, Action::Resize(TermSize::new(30, 120)));
        // chrome: action_bar 1 row, tab_bar 2 rows → terminal rows = 27
        // sidebar 24 cols → terminal cols = 96
        match cmds.as_slice() {
            [Command::ResizePty(3, rows, cols)] => {
                assert_eq!(*rows, 27);
                assert_eq!(*cols, 96);
            }
            other => panic!("unexpected commands: {other:?}"),
        }
    }

    #[test]
    fn cmd_usage_opens_popup_and_subscribes() {
        let mut s = mk_state_with_mock_projects();
        let cmds = submit_command(&mut s, "usage");
        assert!(s.usage_popup.is_some());
        assert!(matches!(cmds.as_slice(), [Command::SubscribeUsage]));
    }

    #[test]
    fn usage_popup_esc_closes_and_unsubscribes() {
        let mut s = mk_state_with_mock_projects();
        s.usage_popup = Some(crate::app::UsagePopup::new());
        let cmds = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(s.usage_popup.is_none());
        assert!(matches!(cmds.as_slice(), [Command::UnsubscribeUsage]));
    }

    #[test]
    fn usage_received_populates_report() {
        use crate::ipc::{ProcessNode, SessionUsage, UsageReport};
        let mut s = mk_state_with_mock_projects();
        s.usage_popup = Some(crate::app::UsagePopup::new());
        let report = UsageReport {
            sessions: vec![SessionUsage {
                session_id: 7,
                project_slug: "imbuia".into(),
                worktree_name: "main".into(),
                root: ProcessNode {
                    pid: 1,
                    name: "zsh".into(),
                    rss_bytes: 0,
                    cpu_percent: 0.0,
                    children: vec![],
                },
            }],
            supervisor: ProcessNode {
                pid: 2,
                name: "imbuia".into(),
                rss_bytes: 0,
                cpu_percent: 0.0,
                children: vec![],
            },
            client: None,
            ts_ms: 1,
            cpu_count: 4,
        };
        let _ = reduce(&mut s, Action::UsageReceived(report));
        let popup = s.usage_popup.as_ref().unwrap();
        assert_eq!(popup.report.as_ref().unwrap().sessions.len(), 1);
    }

    #[test]
    fn usage_popup_enter_expands_selected_session() {
        use crate::ipc::{ProcessNode, SessionUsage, UsageReport};
        let mut s = mk_state_with_mock_projects();
        s.usage_popup = Some(crate::app::UsagePopup::new());
        let report = UsageReport {
            sessions: vec![SessionUsage {
                session_id: 42,
                project_slug: "imbuia".into(),
                worktree_name: "main".into(),
                root: ProcessNode {
                    pid: 1,
                    name: "zsh".into(),
                    rss_bytes: 0,
                    cpu_percent: 0.0,
                    children: vec![],
                },
            }],
            supervisor: ProcessNode {
                pid: 2,
                name: "imbuia".into(),
                rss_bytes: 0,
                cpu_percent: 0.0,
                children: vec![],
            },
            client: None,
            ts_ms: 0,
            cpu_count: 4,
        };
        let _ = reduce(&mut s, Action::UsageReceived(report));
        let _ = reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(s.usage_popup.as_ref().unwrap().expanded.contains(&42));
    }

    #[test]
    fn pr_statuses_fetched_inserts_by_worktree_idx() {
        use crate::app::PrStatus;
        let mut s = AppState::new();
        s.projects = mock_projects();
        // imbuia has 2 worktrees. Report Running for #1, nothing for #0.
        let cmds = reduce(
            &mut s,
            Action::PrStatusesFetched {
                project_idx: 0,
                statuses: vec![(0, None), (1, Some(PrStatus::Running))],
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(s.pr_statuses.get(&(0, 1)), Some(&PrStatus::Running));
        assert!(!s.pr_statuses.contains_key(&(0, 0)));
    }

    #[test]
    fn pr_statuses_fetched_clears_when_none() {
        use crate::app::PrStatus;
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.pr_statuses.insert((0, 1), PrStatus::Failed);
        let _ = reduce(
            &mut s,
            Action::PrStatusesFetched {
                project_idx: 0,
                statuses: vec![(1, None)],
            },
        );
        assert!(!s.pr_statuses.contains_key(&(0, 1)));
    }

    #[test]
    fn periodic_pr_check_emits_one_fetch_per_enabled_project() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.projects[0].github_enabled = true;
        s.projects[2].github_enabled = true;
        let cmds = reduce(&mut s, Action::PeriodicPrCheck);
        assert_eq!(cmds.len(), 2);
        let mut idxs: Vec<usize> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::FetchPrStatuses { project_idx, .. } => Some(*project_idx),
                _ => None,
            })
            .collect();
        idxs.sort();
        assert_eq!(idxs, vec![0, 2]);
    }

    #[test]
    fn worktree_removed_re_keys_pr_statuses() {
        use crate::app::PrStatus;
        let mut s = AppState::new();
        s.projects = mock_projects();
        // imbuia has [main, feat-x]; add a third so we can verify shift-down.
        s.projects[0].worktrees.push(Worktree {
            name: "extra".into(),
            path: PathBuf::from("/tmp/extra"),
            branch: Some("extra".into()),
            sessions: vec![],
            active_tab: None,
        });
        s.pr_statuses.insert((0, 0), PrStatus::Running);
        s.pr_statuses.insert((0, 1), PrStatus::Failed);
        s.pr_statuses.insert((0, 2), PrStatus::Merged);
        // Remove the middle worktree.
        let _ = reduce(
            &mut s,
            Action::WorktreeRemoved {
                project_idx: 0,
                worktree_idx: 1,
            },
        );
        assert_eq!(s.pr_statuses.get(&(0, 0)), Some(&PrStatus::Running));
        // (0,1) used to be "extra" / Merged after the shift.
        assert_eq!(s.pr_statuses.get(&(0, 1)), Some(&PrStatus::Merged));
        assert!(!s.pr_statuses.contains_key(&(0, 2)));
    }

    #[test]
    fn gh_disable_clears_pr_statuses_for_project() {
        use crate::app::PrStatus;
        let mut s = AppState::new();
        s.projects = mock_projects();
        s.projects[0].github_enabled = true;
        s.pr_statuses.insert((0, 0), PrStatus::Failed);
        s.pr_statuses.insert((1, 0), PrStatus::Merged);
        s.sidebar_selection = Some((0, None));
        let _ = submit_command(&mut s, "gh-disable");
        assert!(!s.projects[0].github_enabled);
        assert!(!s.pr_statuses.contains_key(&(0, 0)));
        // Other project's entry untouched.
        assert_eq!(s.pr_statuses.get(&(1, 0)), Some(&PrStatus::Merged));
    }

    #[test]
    fn g_alone_leaves_chord_pending_gt_clears_it() {
        let mut s = AppState::new();
        let _ = reduce(&mut s, Action::Key(plain('g')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(plain('t')));
        assert!(s.pending_chord.is_empty());
    }

    #[test]
    fn custom_binding_via_overlay_remaps_open_tab() {
        use crate::keybinds::{BindableAction, load_overlay};
        let mut overlay = std::collections::BTreeMap::new();
        overlay.insert("open_tab".into(), "<Space>t".into());
        let mut s = AppState::new();
        s.keymap = std::sync::Arc::new(load_overlay(&overlay));
        // Old default `o` should now do nothing.
        let _ = reduce(&mut s, Action::Key(plain('o')));
        assert!(s.pending_chord.is_empty());
        // New chord <Space>t fires OpenTab.
        let _ = reduce(&mut s, Action::Key(plain(' ')));
        assert_eq!(s.pending_chord.len(), 1);
        let _ = reduce(&mut s, Action::Key(plain('t')));
        assert!(s.pending_chord.is_empty());
        assert_eq!(
            s.keymap.binding_for(BindableAction::OpenTab).as_deref(),
            Some("<Space>t")
        );
    }

    #[test]
    fn worktrees_imported_appends_new_and_skips_dupes() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        let main_path = s.projects[0].worktrees[0].path.clone();
        let feat_path = s.projects[0].worktrees[1].path.clone();
        let entries = vec![
            Worktree {
                name: "main".into(),
                path: main_path,
                branch: Some("main".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
            Worktree {
                name: "feat-x".into(),
                path: feat_path,
                branch: Some("feat-x".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
            Worktree {
                name: "fresh-1".into(),
                path: PathBuf::from("/tmp/fresh-1"),
                branch: Some("fresh-1".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
            Worktree {
                name: "fresh-2".into(),
                path: PathBuf::from("/tmp/fresh-2"),
                branch: Some("fresh-2".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
        ];
        let cmds = reduce(
            &mut s,
            Action::WorktreesImported {
                project_idx: 0,
                entries,
            },
        );
        assert_eq!(s.projects[0].worktrees.len(), 4);
        assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(0)]));
        assert_eq!(s.command_status.as_deref(), Some("imported 2 worktrees"));
    }

    #[test]
    fn worktrees_imported_empty_set_says_nothing_to_import() {
        let mut s = AppState::new();
        s.projects = mock_projects();
        let cmds = reduce(
            &mut s,
            Action::WorktreesImported {
                project_idx: 0,
                entries: Vec::new(),
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(s.projects[0].worktrees.len(), 2);
        assert_eq!(
            s.command_status.as_deref(),
            Some("no new worktrees to import")
        );
    }

    #[test]
    fn terminal_chord_replay_forwards_buffered_keys_on_mismatch() {
        let mut s = AppState::new();
        s.term_size = TermSize::new(40, 200);
        s.mode = Mode::Terminal;
        let fake = FakeSession::new(7);
        let _ = reduce(
            &mut s,
            Action::SessionSpawned {
                session: fake,
                dest: (0, 0),
            },
        );
        s.projects = vec![Project {
            slug: "p".into(),
            name: "p".into(),
            repo_path: PathBuf::from("."),
            worktrees: vec![Worktree {
                name: "w".into(),
                path: PathBuf::from("."),
                branch: Some("w".into()),
                sessions: vec![7],
                active_tab: Some(0),
            }],
            expanded: true,
            setup_script: None,
            launchers: Vec::new(),
            github_enabled: false,
            gh_poll_interval_secs: None,
        }];
        s.active_worktree = Some((0, 0));

        // Ctrl-\ alone is a prefix — buffered, nothing reaches the PTY.
        let cmds = reduce(&mut s, Action::Key(ctrl('\\')));
        assert!(cmds.is_empty());
        assert_eq!(s.pending_chord.len(), 1);
        // Mismatch: replay both buffered keys to the PTY in order.
        let cmds = reduce(&mut s, Action::Key(plain('q')));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(
            cmds.as_slice(),
            [Command::WriteKey(7, _), Command::WriteKey(7, _)]
        ));
        assert!(s.pending_chord.is_empty());
        assert_eq!(s.mode, Mode::Terminal);
    }
}
