use crate::app::{Action, AppState, Command, Project, Worktree};
use crate::client::{self, SupervisorClient};
use crate::config;
use crate::git;
use crate::input;
use crate::layout::TermSize;
use crate::reducer::reduce;
use crate::render;
use crate::session::Session;
use crate::theme::Theme;
use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant, sleep_until};

const FRAME: Duration = Duration::from_millis(16);

pub async fn run() -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let area = terminal.size()?;
    let term_size = TermSize::new(area.height, area.width);

    let config_dir = config::resolve_config_dir();
    let (global, project_cfgs) = config::load_or_default(&config_dir);

    let notify = Arc::new(Notify::new());
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(256);

    // Attach to (or spawn) the supervisor before we start polling input —
    // resumed sessions need to be wired into AppState first so output frames
    // arriving immediately after handshake can find their parsers.
    let supervisor = client::connect_or_spawn(Arc::clone(&notify), action_tx.clone())?;

    spawn_input_thread(action_tx.clone());

    let mut state = AppState::new();
    state.term_size = term_size;
    state.config_dir = config_dir.clone();
    state.sidebar_width = global.sidebar_width;
    state.theme = Theme::for_kind(global.theme);
    state.projects = project_cfgs.into_iter().map(Project::from_config).collect();
    if !state.projects.is_empty() {
        state.sidebar_selection = Some((0, None));
    }

    // Re-bind any sessions the supervisor has from a previous run.
    rebind_resumed_sessions(&mut state, &supervisor);
    // Host terminal may have resized since the supervisor last saw a client;
    // push the current dimensions to every resumed PTY before the first
    // render so the screen isn't drawn against stale rows/cols.
    if !state.sessions.is_empty() {
        let cmds = reduce(&mut state, Action::Resize(term_size));
        for cmd in cmds {
            execute(cmd, &state, &action_tx, &notify, &supervisor);
        }
    }

    let mut redraw_at: Option<Instant> = Some(Instant::now());

    while state.running {
        if redraw_at.is_some_and(|d| Instant::now() >= d) {
            terminal.draw(|f| render::render(f, &state))?;
            redraw_at = None;
        }

        tokio::select! {
            biased;
            maybe_action = action_rx.recv() => {
                let Some(action) = maybe_action else { break };
                handle_action(&mut state, action, &action_tx, &notify, &supervisor);
                while let Ok(action) = action_rx.try_recv() {
                    handle_action(&mut state, action, &action_tx, &notify, &supervisor);
                }
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = notify.notified() => {
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = async {
                match redraw_at {
                    Some(d) => sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }

    Ok(())
}

fn rebind_resumed_sessions(state: &mut AppState, supervisor: &Arc<SupervisorClient>) {
    let resumed = supervisor.drain_initial_sessions();
    for meta in resumed {
        let Some((pi, wi)) = locate(state, &meta.project_slug, &meta.worktree_name) else {
            tracing::warn!(
                slug = %meta.project_slug,
                worktree = %meta.worktree_name,
                id = meta.id,
                "supervisor reported session for unknown project/worktree; dropping"
            );
            supervisor.kill(meta.id);
            continue;
        };
        let proxy = supervisor.adopt(&meta);
        let wt = &mut state.projects[pi].worktrees[wi];
        wt.sessions.push(meta.id);
        if wt.active_tab.is_none() {
            wt.active_tab = Some(wt.sessions.len() - 1);
        }
        state.sessions.insert(meta.id, proxy as Arc<dyn Session>);
        if state.active_worktree.is_none() {
            state.active_worktree = Some((pi, wi));
        }
    }
}

fn locate(state: &AppState, slug: &str, wt_name: &str) -> Option<(usize, usize)> {
    let pi = state.projects.iter().position(|p| p.slug == slug)?;
    let wi = state.projects[pi]
        .worktrees
        .iter()
        .position(|w| w.name == wt_name)?;
    Some((pi, wi))
}

fn handle_action(
    state: &mut AppState,
    action: Action,
    action_tx: &mpsc::Sender<Action>,
    notify: &Arc<Notify>,
    supervisor: &Arc<SupervisorClient>,
) {
    let cmds = reduce(state, action);
    for cmd in cmds {
        execute(cmd, state, action_tx, notify, supervisor);
    }
}

fn execute(
    cmd: Command,
    state: &AppState,
    action_tx: &mpsc::Sender<Action>,
    _notify: &Arc<Notify>,
    supervisor: &Arc<SupervisorClient>,
) {
    match cmd {
        Command::WriteKey(id, key) => {
            if let Some(sess) = state.sessions.get(&id)
                && let Err(e) = sess.write_key(key)
            {
                tracing::warn!(session = id, "write_key failed: {e}");
            }
        }
        Command::WritePaste(id, text) => {
            if let Some(sess) = state.sessions.get(&id)
                && let Err(e) = sess.write_paste(&text)
            {
                tracing::warn!(session = id, "write_paste failed: {e}");
            }
        }
        Command::WriteMouse(id, ev) => {
            if let Some(sess) = state.sessions.get(&id)
                && let Err(e) = sess.write_mouse(ev)
            {
                tracing::warn!(session = id, "write_mouse failed: {e}");
            }
        }
        Command::ResizePty(id, rows, cols) => {
            if let Some(sess) = state.sessions.get(&id)
                && let Err(e) = sess.resize(rows, cols)
            {
                tracing::warn!(session = id, "resize failed: {e}");
            }
        }
        Command::SpawnInWorktree {
            rows,
            cols,
            cwd,
            dest,
            initial_command,
            project_slug,
            worktree_name,
        } => {
            if let Err(e) = supervisor.request_spawn(
                project_slug,
                worktree_name,
                rows,
                cols,
                cwd,
                initial_command,
                dest,
            ) {
                tracing::warn!("supervisor spawn request failed: {e}");
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("spawn: {e}")));
            }
        }
        Command::KillSession(id) => {
            supervisor.kill(id);
        }
        Command::RestartSupervisor => {
            supervisor.shutdown_supervisor();
        }
        Command::SubscribeUsage => supervisor.subscribe_usage(),
        Command::UnsubscribeUsage => supervisor.unsubscribe_usage(),
        Command::OpenProject { path, setup_script } => {
            spawn_open_project(
                path,
                setup_script,
                state.config_dir.clone(),
                state_slugs(state),
                action_tx,
            );
        }
        Command::AddWorktree {
            project_idx,
            repo_path,
            branch,
        } => {
            spawn_add_worktree(project_idx, repo_path, branch, action_tx);
        }
        Command::RemoveWorktree {
            project_idx,
            worktree_idx,
            repo_path,
            dest_path,
            branch,
        } => {
            spawn_remove_worktree(
                project_idx,
                worktree_idx,
                repo_path,
                dest_path,
                branch,
                action_tx,
            );
        }
        Command::SaveGlobalConfig => {
            let global = config::GlobalConfig {
                sidebar_width: state.sidebar_width,
                theme: state.theme.kind,
                projects: state_slugs(state),
            };
            if let Err(e) = config::save_global(&state.config_dir, &global) {
                tracing::warn!("save_global failed: {e}");
            }
        }
        Command::SaveProjectConfig(i) => {
            if let Some(p) = state.projects.get(i) {
                let cfg = p.to_config();
                if let Err(e) = config::save_project(&state.config_dir, &cfg) {
                    tracing::warn!("save_project failed: {e}");
                }
            }
        }
        Command::Shutdown => {}
    }
}

fn state_slugs(state: &AppState) -> Vec<String> {
    state.projects.iter().map(|p| p.slug.clone()).collect()
}

fn spawn_open_project(
    path: PathBuf,
    setup_script: Option<String>,
    config_dir: PathBuf,
    existing_slugs: Vec<String>,
    action_tx: &mpsc::Sender<Action>,
) {
    let tx = action_tx.clone();
    std::thread::spawn(move || {
        match do_open_project(&path, setup_script, &config_dir, &existing_slugs) {
            Ok(cfg) => {
                tracing::info!(slug = %cfg.slug, "project opened");
                let _ = tx.blocking_send(Action::ProjectOpened(Project::from_config(cfg)));
            }
            Err(e) => {
                let _ = tx.blocking_send(Action::OperationFailed(format!("open: {e}")));
            }
        }
    });
}

fn do_open_project(
    path: &std::path::Path,
    setup_script: Option<String>,
    config_dir: &std::path::Path,
    existing_slugs: &[String],
) -> Result<config::ProjectConfig> {
    let absolute = std::fs::canonicalize(path)?;
    git::validate_repo(&absolute)?;
    let head = git::head_branch(&absolute)?;
    let name = absolute
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let slug = config::compute_slug(&name, existing_slugs);
    let main = config::WorktreeConfig {
        name: head.clone().unwrap_or_else(|| "main".into()),
        path: absolute.clone(),
        branch: head,
    };
    let cfg = config::ProjectConfig {
        slug,
        name,
        path: absolute,
        expanded: true,
        setup_script,
        worktrees: vec![main],
        launchers: Vec::new(),
    };
    config::save_project(config_dir, &cfg)?;
    Ok(cfg)
}

fn spawn_add_worktree(
    project_idx: usize,
    repo_path: PathBuf,
    branch: String,
    action_tx: &mpsc::Sender<Action>,
) {
    let tx = action_tx.clone();
    std::thread::spawn(move || match do_add_worktree(&repo_path, &branch) {
        Ok(worktree) => {
            tracing::info!(branch = %branch, "worktree added");
            let _ = tx.blocking_send(Action::WorktreeAdded {
                project_idx,
                worktree,
            });
        }
        Err(e) => {
            let _ = tx.blocking_send(Action::OperationFailed(format!("worktree: {e}")));
        }
    });
}

fn do_add_worktree(repo: &std::path::Path, branch: &str) -> Result<Worktree> {
    let dest = worktree_dest(repo, branch);
    git::worktree_add(repo, &dest, branch)?;
    Ok(Worktree {
        name: branch.to_string(),
        path: dest,
        branch: Some(branch.to_string()),
        sessions: Vec::new(),
        active_tab: None,
    })
}

fn spawn_remove_worktree(
    project_idx: usize,
    worktree_idx: usize,
    repo_path: PathBuf,
    dest_path: PathBuf,
    branch: Option<String>,
    action_tx: &mpsc::Sender<Action>,
) {
    let tx = action_tx.clone();
    std::thread::spawn(move || {
        match git::worktree_remove(&repo_path, &dest_path, branch.as_deref()) {
            Ok(()) => {
                tracing::info!(branch = ?branch, "worktree removed");
                let _ = tx.blocking_send(Action::WorktreeRemoved {
                    project_idx,
                    worktree_idx,
                });
            }
            Err(e) => {
                let _ = tx.blocking_send(Action::OperationFailed(format!("remove worktree: {e}")));
            }
        }
    });
}

fn worktree_dest(repo: &std::path::Path, branch: &str) -> PathBuf {
    let parent = repo.parent().unwrap_or(repo);
    let base = repo
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    parent.join(format!("{base}-worktrees")).join(branch)
}

fn spawn_input_thread(tx: mpsc::Sender<Action>) {
    std::thread::spawn(move || {
        loop {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if let Some(action) = input::map(ev)
                            && tx.blocking_send(action).is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("crossterm read error: {e}");
                        break;
                    }
                },
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!("crossterm poll error: {e}");
                    break;
                }
            }
        }
    });
}
