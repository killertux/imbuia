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

    // Long-lived worker that serialises every `gh` invocation so a 2-min
    // background poll never races with a foreground `:gh-refresh` (or another
    // tick that arrived while gh was still chewing). Drops requests that
    // arrive while one is already in flight for the *same project* — the
    // newer request would observe the same upstream state anyway.
    let gh_tx = spawn_gh_worker(action_tx.clone());

    let mut state = AppState::new();
    state.term_size = term_size;
    state.config_dir = config_dir.clone();
    state.sidebar_width = global.sidebar_width;
    state.theme = Theme::for_kind(global.theme);
    state.global_launchers = global
        .launchers
        .into_iter()
        .map(|l| crate::app::Launcher {
            name: l.name,
            command: l.command,
        })
        .collect();
    state.gh_poll_interval_secs = global.gh_poll_interval_secs;
    state.keybinds_config = global.keybinds.clone();
    state.keymap = std::sync::Arc::new(crate::keybinds::load_overlay(&global.keybinds));
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
            execute(cmd, &state, &action_tx, &notify, &supervisor, &gh_tx);
        }
    }

    // Hourly auto-update check. Tokio's `interval` fires immediately on
    // first poll, which is what we want — kicks off the startup check.
    let mut update_tick = tokio::time::interval(Duration::from_secs(3600));
    update_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // PR status poll. Ticks every 30s; the reducer fans this out into one
    // `FetchPrStatuses` per gh-enabled project, and the runtime gates per
    // (project, last-fetched-at) so each project only refreshes at its own
    // cadence — see `pr_poll_last` below.
    let mut pr_tick = tokio::time::interval(Duration::from_secs(30));
    pr_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Per-project last-poll instant, keyed by slug so it survives index
    // shuffles. Built lazily; missing entry => "never polled, fetch now".
    let mut pr_poll_last: std::collections::HashMap<String, Instant> =
        std::collections::HashMap::new();

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
                handle_action(&mut state, action, &action_tx, &notify, &supervisor, &gh_tx);
                while let Ok(action) = action_rx.try_recv() {
                    handle_action(&mut state, action, &action_tx, &notify, &supervisor, &gh_tx);
                }
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = notify.notified() => {
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = update_tick.tick() => {
                handle_action(&mut state, Action::PeriodicUpdateCheck, &action_tx, &notify, &supervisor, &gh_tx);
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = pr_tick.tick() => {
                if maybe_emit_pr_poll(&mut state, &mut pr_poll_last) {
                    handle_action(&mut state, Action::PeriodicPrCheck, &action_tx, &notify, &supervisor, &gh_tx);
                    redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
                }
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
    gh_tx: &std::sync::mpsc::Sender<GhRequest>,
) {
    let cmds = reduce(state, action);
    for cmd in cmds {
        execute(cmd, state, action_tx, notify, supervisor, gh_tx);
    }
}

fn execute(
    cmd: Command,
    state: &AppState,
    action_tx: &mpsc::Sender<Action>,
    _notify: &Arc<Notify>,
    supervisor: &Arc<SupervisorClient>,
    gh_tx: &std::sync::mpsc::Sender<GhRequest>,
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
        Command::OpenProject {
            path,
            setup_script,
            import_existing,
        } => {
            spawn_open_project(
                path,
                setup_script,
                import_existing,
                state.config_dir.clone(),
                state_slugs(state),
                action_tx,
            );
        }
        Command::ImportWorktrees {
            project_idx,
            repo_path,
        } => spawn_import_worktrees(project_idx, repo_path, action_tx),
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
                launchers: state
                    .global_launchers
                    .iter()
                    .map(|l| config::LauncherConfig {
                        name: l.name.clone(),
                        command: l.command.clone(),
                    })
                    .collect(),
                gh_poll_interval_secs: state.gh_poll_interval_secs,
                keybinds: state.keybinds_config.clone(),
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
        Command::FetchPrStatuses {
            project_idx,
            repo_path,
            worktrees,
        } => {
            // Fire-and-forget: the worker thread serialises and forwards
            // results via the same action_tx channel.
            if let Err(e) = gh_tx.send(GhRequest {
                project_idx,
                repo_path,
                worktrees,
            }) {
                tracing::warn!("gh worker channel closed: {e}");
            }
        }
        Command::CheckForUpdate => spawn_update_check(action_tx),
        Command::InstallUpdate { tag } => spawn_update_install(tag, action_tx),
        Command::Shutdown => {}
    }
}

/// Returns `true` if at least one project is due for a refresh, after also
/// updating `last` with `now` for every project that just got queued. The
/// reducer still walks `state.projects` to emit the actual commands; this
/// helper just decides whether the tick should fire at all.
fn maybe_emit_pr_poll(
    state: &mut AppState,
    last: &mut std::collections::HashMap<String, Instant>,
) -> bool {
    if !crate::github::gh_available() {
        return false;
    }
    let now = Instant::now();
    let global_default = Duration::from_secs(state.gh_poll_interval_secs.unwrap_or(120));
    let mut any = false;
    // Garbage-collect entries for projects that no longer exist.
    let live_slugs: std::collections::HashSet<String> =
        state.projects.iter().map(|p| p.slug.clone()).collect();
    last.retain(|s, _| live_slugs.contains(s));
    for p in &state.projects {
        if !p.github_enabled {
            continue;
        }
        let interval = p
            .gh_poll_interval_secs
            .map(Duration::from_secs)
            .unwrap_or(global_default);
        let due = match last.get(&p.slug) {
            Some(t) => now.duration_since(*t) >= interval,
            None => true,
        };
        if due {
            last.insert(p.slug.clone(), now);
            any = true;
        }
    }
    any
}

/// Message handed to the gh worker. One request = one project's worth of
/// branch lookups.
struct GhRequest {
    project_idx: usize,
    repo_path: PathBuf,
    worktrees: Vec<(usize, PathBuf)>,
}

/// Spawn the singleton worker. Returns its inbox sender.
///
/// The thread drains its queue serially: while it's processing project A,
/// other requests pile up in the channel and run after. Coalescing: before
/// starting a request, we drain any *additional* pending requests for the
/// same project and keep only the newest — they'd just observe the same
/// upstream state anyway.
fn spawn_gh_worker(action_tx: mpsc::Sender<Action>) -> std::sync::mpsc::Sender<GhRequest> {
    let (tx, rx) = std::sync::mpsc::channel::<GhRequest>();
    std::thread::spawn(move || {
        while let Ok(mut req) = rx.recv() {
            // Drain anything else already queued; collapse same-project
            // requests down to the newest one.
            while let Ok(next) = rx.try_recv() {
                if next.project_idx == req.project_idx {
                    req = next;
                } else {
                    // Different project — process the current one first, then
                    // re-queue `next` at the front by handling it after this
                    // iteration. Simplest: handle req now, then handle next.
                    do_fetch(req, &action_tx);
                    req = next;
                }
            }
            do_fetch(req, &action_tx);
        }
        tracing::info!("gh worker exiting (channel closed)");
    });
    tx
}

fn do_fetch(req: GhRequest, action_tx: &mpsc::Sender<Action>) {
    let GhRequest {
        project_idx,
        repo_path,
        worktrees,
    } = req;
    let tx = action_tx.clone();
    // Inline body — the function used to spawn its own thread; now we run
    // synchronously on the worker so requests are serialised.
    let work = move || {
        tracing::info!(
            project_idx,
            n = worktrees.len(),
            repo = %repo_path.display(),
            "gh: fetching per-worktree PR status"
        );
        let mut statuses: Vec<(usize, Option<crate::app::PrStatus>)> =
            Vec::with_capacity(worktrees.len());
        let mut last_err: Option<String> = None;
        let mut any_ok = false;
        for (wi, wt_path) in worktrees {
            // Live HEAD resolution — picks up `git switch` inside the worktree.
            let branch = match crate::git::head_branch(&wt_path) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    tracing::info!(project_idx, wi, path = %wt_path.display(), "gh: detached HEAD, skipping");
                    statuses.push((wi, None));
                    any_ok = true;
                    continue;
                }
                Err(e) => {
                    let msg = format!("git symbolic-ref failed: {e}");
                    tracing::warn!(project_idx, wi, path = %wt_path.display(), "{msg}");
                    last_err = Some(msg);
                    continue;
                }
            };
            match crate::github::fetch_pr_by_branch(&repo_path, &branch) {
                Ok(s) => {
                    any_ok = true;
                    tracing::info!(project_idx, wi, %branch, status = ?s, "gh: branch status");
                    statuses.push((wi, s));
                }
                Err(e) => {
                    let msg = format!("{e}");
                    tracing::warn!(project_idx, wi, %branch, "gh: branch fetch failed: {msg}");
                    last_err = Some(msg);
                }
            }
        }
        if !any_ok && let Some(msg) = last_err {
            let _ = tx.blocking_send(Action::PrFetchFailed {
                project_idx,
                message: msg,
            });
            return;
        }
        let _ = tx.blocking_send(Action::PrStatusesFetched {
            project_idx,
            statuses,
        });
    };
    work();
}

fn spawn_update_check(action_tx: &mpsc::Sender<Action>) {
    let tx = action_tx.clone();
    std::thread::spawn(move || {
        let result = crate::updater::check_for_update().map_err(|e| format!("{e:#}"));
        let _ = tx.blocking_send(Action::UpdateChecked(result));
    });
}

fn spawn_update_install(tag: String, action_tx: &mpsc::Sender<Action>) {
    let tx = action_tx.clone();
    std::thread::spawn(move || {
        // Re-derive UpdateInfo from the tag; semver is cheap to parse and we
        // don't want to thread the full struct through the command queue.
        let version_str = tag.strip_prefix('v').unwrap_or(&tag).to_string();
        let info_result = semver::Version::parse(&version_str)
            .map_err(|e| format!("bad tag {tag:?}: {e}"))
            .map(|version| crate::updater::UpdateInfo {
                latest_tag: tag.clone(),
                latest_version: version,
            });
        let result = match info_result {
            Ok(info) => crate::updater::install_update(&info).map_err(|e| format!("{e:#}")),
            Err(e) => Err(e),
        };
        let _ = tx.blocking_send(Action::UpdateInstalled(result));
    });
}

fn state_slugs(state: &AppState) -> Vec<String> {
    state.projects.iter().map(|p| p.slug.clone()).collect()
}

fn spawn_open_project(
    path: PathBuf,
    setup_script: Option<String>,
    import_existing: bool,
    config_dir: PathBuf,
    existing_slugs: Vec<String>,
    action_tx: &mpsc::Sender<Action>,
) {
    let tx = action_tx.clone();
    std::thread::spawn(move || {
        match do_open_project(&path, setup_script, &config_dir, &existing_slugs) {
            Ok(cfg) => {
                tracing::info!(slug = %cfg.slug, "project opened");
                let _ = tx.blocking_send(Action::ProjectOpened {
                    project: Project::from_config(cfg),
                    import_existing,
                });
            }
            Err(e) => {
                let _ = tx.blocking_send(Action::OperationFailed(format!("open: {e}")));
            }
        }
    });
}

fn spawn_import_worktrees(
    project_idx: usize,
    repo_path: PathBuf,
    action_tx: &mpsc::Sender<Action>,
) {
    let tx = action_tx.clone();
    std::thread::spawn(move || match git::list_worktrees(&repo_path) {
        Ok(entries) => {
            tracing::info!(
                project_idx,
                count = entries.len(),
                "import: git worktree list"
            );
            let imported: Vec<crate::app::Worktree> = entries
                .into_iter()
                .map(|e| crate::app::Worktree {
                    name: e.branch.clone().unwrap_or_else(|| {
                        e.path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "worktree".into())
                    }),
                    path: e.path,
                    branch: e.branch,
                    sessions: Vec::new(),
                    active_tab: None,
                })
                .collect();
            let _ = tx.blocking_send(Action::WorktreesImported {
                project_idx,
                entries: imported,
            });
        }
        Err(e) => {
            tracing::warn!(project_idx, "git worktree list failed: {e}");
            let _ = tx.blocking_send(Action::OperationFailed(format!("import: {e}")));
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
        github_enabled: true,
        gh_poll_interval_secs: None,
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
