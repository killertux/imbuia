use crate::app::{Action, AppState, Command, Project};
use crate::client::{self, SupervisorClient, Supervisors};
use crate::config;
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

    // Attach to the local supervisor (auto-spawning if needed) plus every
    // configured remote, before we start polling input — resumed sessions need
    // to be wired into AppState first so output frames arriving immediately
    // after handshake can find their parsers.
    let supervisors =
        client::connect_all(&config_dir, &global, Arc::clone(&notify), action_tx.clone()).await?;

    spawn_input_thread(action_tx.clone());

    let mut state = AppState::new();
    state.term_size = term_size;
    state.config_dir = config_dir.clone();
    state.supervisors = supervisors.directory();
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
    let dir = supervisors.directory();
    state.projects = project_cfgs
        .into_iter()
        .map(|cfg| {
            let sup = dir.resolve(cfg.supervisor.as_deref());
            Project::from_config(cfg, sup)
        })
        .collect();
    if !state.projects.is_empty() {
        state.sidebar_selection = Some((0, None));
    }

    // Re-bind sessions every connected supervisor has from a previous run.
    rebind_resumed_sessions(&mut state, &supervisors);
    // Host terminal may have resized since the supervisor last saw a client;
    // push the current dimensions to every resumed PTY before the first
    // render so the screen isn't drawn against stale rows/cols.
    if !state.sessions.is_empty() {
        let cmds = reduce(&mut state, Action::Resize(term_size));
        for cmd in cmds {
            execute(cmd, &state, &action_tx, &notify, &supervisors);
        }
    }

    // sysinfo handle for sampling THIS client's own process while the usage
    // popup is open (supervisors no longer report the client pid).
    let mut usage_sys = sysinfo::System::new();
    let mut usage_tick = tokio::time::interval(Duration::from_secs(1));
    usage_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                handle_action(&mut state, action, &action_tx, &notify, &supervisors);
                while let Ok(action) = action_rx.try_recv() {
                    handle_action(&mut state, action, &action_tx, &notify, &supervisors);
                }
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = notify.notified() => {
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = update_tick.tick() => {
                handle_action(&mut state, Action::PeriodicUpdateCheck, &action_tx, &notify, &supervisors);
                redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
            }
            _ = pr_tick.tick() => {
                if maybe_emit_pr_poll(&mut state, &mut pr_poll_last) {
                    handle_action(&mut state, Action::PeriodicPrCheck, &action_tx, &notify, &supervisors);
                    redraw_at.get_or_insert_with(|| Instant::now() + FRAME);
                }
            }
            _ = usage_tick.tick() => {
                // While the usage popup is open, sample our own process and feed
                // it back so the popup can show a "Client" row (the supervisors
                // can't see this pid, especially remote ones).
                if state.usage_popup.is_some()
                    && let Some(node) = sample_client_node(&mut usage_sys)
                {
                    handle_action(&mut state, Action::LocalUsageSampled(node), &action_tx, &notify, &supervisors);
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

fn rebind_resumed_sessions(state: &mut AppState, supervisors: &Supervisors) {
    // Each connected supervisor reports its own resumed sessions; bind them to
    // the matching project/worktree, but only if that project is actually
    // hosted on this supervisor (else it's an orphan to be killed).
    for client in supervisors.connected() {
        let sup_id = client.supervisor_id();
        for meta in client.drain_initial_sessions() {
            let located = locate(state, &meta.project_slug, &meta.worktree_name)
                .filter(|&(pi, _)| state.projects[pi].supervisor == sup_id);
            let Some((pi, wi)) = located else {
                tracing::warn!(
                    slug = %meta.project_slug,
                    worktree = %meta.worktree_name,
                    id = meta.id,
                    "supervisor reported session for unknown/mismatched project; dropping"
                );
                client.kill(meta.id);
                continue;
            };
            let proxy = client.adopt(&meta);
            let global = proxy.id();
            let wt = &mut state.projects[pi].worktrees[wi];
            wt.sessions.push(global);
            if wt.active_tab.is_none() {
                wt.active_tab = Some(wt.sessions.len() - 1);
            }
            state.sessions.insert(global, proxy as Arc<dyn Session>);
            if state.active_worktree.is_none() {
                state.active_worktree = Some((pi, wi));
            }
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
    supervisors: &Supervisors,
) {
    let cmds = reduce(state, action);
    for cmd in cmds {
        execute(cmd, state, action_tx, notify, supervisors);
    }
}

/// Resolve a project-indexed command's target supervisor, or `None` (logging)
/// if the project is gone or its supervisor is unreachable.
fn project_client<'a>(
    state: &AppState,
    supervisors: &'a Supervisors,
    project_idx: usize,
    action_tx: &mpsc::Sender<Action>,
) -> Option<&'a Arc<SupervisorClient>> {
    let sup = state.projects.get(project_idx)?.supervisor;
    match supervisors.get(sup) {
        Some(c) => Some(c),
        None => {
            let _ = action_tx.blocking_send(Action::OperationFailed(format!(
                "supervisor '{}' is not connected",
                supervisors.name_of(sup)
            )));
            None
        }
    }
}

fn execute(
    cmd: Command,
    state: &AppState,
    action_tx: &mpsc::Sender<Action>,
    _notify: &Arc<Notify>,
    supervisors: &Supervisors,
) {
    use crate::app::{LOCAL, SupervisorId};
    // Resolve a directly-targeted supervisor, posting an error if unreachable.
    let targeted = |sup: SupervisorId| -> Option<&Arc<SupervisorClient>> {
        match supervisors.get(sup) {
            Some(c) => Some(c),
            None => {
                let _ = action_tx.blocking_send(Action::OperationFailed(format!(
                    "supervisor '{}' is not connected",
                    supervisors.name_of(sup)
                )));
                None
            }
        }
    };
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
            supervisor,
            rows,
            cols,
            cwd,
            dest,
            initial_command,
            project_slug,
            worktree_name,
        } => {
            if let Some(client) = targeted(supervisor)
                && let Err(e) = client.request_spawn(
                    project_slug,
                    worktree_name,
                    rows,
                    cols,
                    cwd,
                    initial_command,
                    dest,
                )
            {
                tracing::warn!("supervisor spawn request failed: {e}");
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("spawn: {e}")));
            }
        }
        Command::KillSession(id) => {
            // Routes implicitly via the session's own connection.
            if let Some(sess) = state.sessions.get(&id) {
                sess.kill();
            }
        }
        Command::RestartSupervisor => {
            // `:rs` targets the local supervisor (the auto-spawned one).
            if let Some(client) = supervisors.get(LOCAL) {
                client.shutdown_supervisor();
            }
        }
        Command::SubscribeUsage => {
            for client in supervisors.connected() {
                client.subscribe_usage();
            }
        }
        Command::UnsubscribeUsage => {
            for client in supervisors.connected() {
                client.unsubscribe_usage();
            }
        }
        Command::OpenProject {
            supervisor,
            path,
            setup_script,
            import_existing,
        } => {
            if let Some(client) = targeted(supervisor)
                && let Err(e) =
                    client.request_open_project(supervisor, path, setup_script, import_existing)
            {
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("open: {e}")));
            }
        }
        Command::ListDir { supervisor, path } => {
            if let Some(client) = targeted(supervisor)
                && let Err(e) = client.request_list_dir(path)
            {
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("list dir: {e}")));
            }
        }
        Command::ImportWorktrees {
            project_idx,
            repo_path,
        } => {
            if let Some(client) = project_client(state, supervisors, project_idx, action_tx)
                && let Err(e) = client.request_import_worktrees(project_idx, repo_path)
            {
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("import: {e}")));
            }
        }
        Command::AddWorktree {
            project_idx,
            repo_path,
            branch,
        } => {
            if let Some(client) = project_client(state, supervisors, project_idx, action_tx)
                && let Err(e) = client.request_add_worktree(project_idx, repo_path, branch)
            {
                let _ = action_tx.blocking_send(Action::OperationFailed(format!("worktree: {e}")));
            }
        }
        Command::RemoveWorktree {
            project_idx,
            worktree_idx,
            repo_path,
            dest_path,
            branch,
        } => {
            if let Some(client) = project_client(state, supervisors, project_idx, action_tx)
                && let Err(e) = client.request_remove_worktree(
                    project_idx,
                    worktree_idx,
                    repo_path,
                    dest_path,
                    branch,
                )
            {
                let _ = action_tx
                    .blocking_send(Action::OperationFailed(format!("remove worktree: {e}")));
            }
        }
        Command::SaveGlobalConfig => {
            let disk = config::load_global(&state.config_dir).ok();
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
                // `remotes`/`remote` are hand-edited config the UI never owns;
                // preserve whatever is on disk so a save doesn't wipe them.
                remotes: disk.as_ref().map(|g| g.remotes.clone()).unwrap_or_default(),
                remote: disk.and_then(|g| g.remote),
            };
            if let Err(e) = config::save_global(&state.config_dir, &global) {
                tracing::warn!("save_global failed: {e}");
            }
        }
        Command::SaveProjectConfig(i) => {
            if let Some(p) = state.projects.get(i) {
                let cfg = p.to_config(state.supervisors.config_name(p.supervisor));
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
            // The supervisor's gh worker serialises and coalesces; the reader
            // posts PrStatusesFetched / PrFetchFailed back.
            if let Some(client) = project_client(state, supervisors, project_idx, action_tx)
                && let Err(e) = client.request_fetch_pr(project_idx, repo_path, worktrees)
            {
                tracing::warn!("gh fetch request failed: {e}");
            }
        }
        Command::CheckForUpdate => spawn_update_check(action_tx),
        Command::InstallUpdate { tag } => spawn_update_install(tag, action_tx),
        Command::Shutdown => {}
    }
}

/// Sample this client process's own resource usage (no children — the TUI
/// doesn't fork). `sys` is kept across ticks so `cpu_usage()` is a delta over
/// the ~1s sampling interval. Returns `None` if our own pid vanished (it won't).
fn sample_client_node(sys: &mut sysinfo::System) -> Option<crate::ipc::ProcessNode> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let p = sys.process(pid)?;
    Some(crate::ipc::ProcessNode {
        pid: pid.as_u32(),
        name: p.name().to_string_lossy().to_string(),
        rss_bytes: p.memory(),
        cpu_percent: p.cpu_usage(),
        children: Vec::new(),
    })
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

fn spawn_update_check(action_tx: &mpsc::Sender<Action>) {
    let tx = action_tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = crate::updater::check_for_update().map_err(|e| format!("{e:#}"));
        let _ = tx.blocking_send(Action::UpdateChecked(result));
    });
}

fn spawn_update_install(tag: String, action_tx: &mpsc::Sender<Action>) {
    let tx = action_tx.clone();
    tokio::task::spawn_blocking(move || {
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
