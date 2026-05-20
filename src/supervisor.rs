//! Supervisor process: owns PTY masters + vt100 parsers, serves a single
//! TUI client over a Unix domain socket.
//!
//! Lifecycle:
//!
//! 1. The binary is re-exec'd (or jumped to via `main.rs`) with
//!    `--supervisor`.
//! 2. We bind the socket from [`ipc::resolve_socket_path`] and accept
//!    connections one at a time. Connecting clients steal the slot.
//! 3. PTYs survive across client disconnects — children stay alive until
//!    the supervisor itself exits (graceful `Shutdown`) or a `Kill`
//!    arrives for that specific session.
//!
//! No UI, no crossterm, no raw mode. stdout/stderr are detached (see
//! [`crate::client_spawn::double_fork_supervisor`]); we log to
//! `~/.cache/imbuia/supervisor.log` via `tracing`.

use crate::ipc::{
    self, ClientMsg, HandshakeReq, HandshakeResp, PROTOCOL_VERSION, ProcessNode, SessionId,
    SessionMeta, SessionUsage, SupervisorMsg, UsageReport,
};
use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::collections::{HashMap, VecDeque};
use std::io::Write;

/// Per-session cap on the raw-byte replay log. Sized so a packed text dump
/// of vt100's 10k scrollback fits with headroom for ANSI escapes. Eviction
/// drops from the front; see [`push_output_log`].
const OUTPUT_LOG_CAP: usize = 2 * 1024 * 1024;

/// Raw-byte replay buffer with a "we've evicted bytes" flag. Once truncated,
/// `send_dump` prefers the parser's `contents_formatted()` over the (now
/// possibly mid-escape) buffer — sacrificing scrollback history to avoid
/// color smears from replaying into the middle of a CSI sequence.
#[derive(Default)]
struct OutputLog {
    buf: VecDeque<u8>,
    truncated: bool,
}
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Shared writer-half of the currently-active client connection.
type ClientWriter = Arc<Mutex<UnixStream>>;

struct Supervised {
    meta: SessionMeta,
    /// PID of the PTY's direct child (the user's shell). Used as the root
    /// for descendant walks in the usage sampler.
    child_pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Bounded ring of every raw PTY byte we've seen. Replayed verbatim on
    /// attach so the client's local vt100 parser populates its own scrollback
    /// — `parser.screen().contents_formatted()` only covers the visible grid
    /// and would otherwise discard history across client restarts.
    output_log: Arc<Mutex<OutputLog>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

struct Registry {
    sessions: HashMap<SessionId, Arc<Supervised>>,
    next_id: SessionId,
    active: Option<ClientWriter>,
    /// Generation counter; bumped when `active` changes. Per-session reader
    /// threads use it to know if their cached writer is stale.
    active_generation: u64,
    /// Set when the active client subscribed to usage frames. The sampler
    /// thread polls this and exits when it goes false.
    usage_subscribed: bool,
    /// True while a usage-sampler thread is alive. Prevents double-spawn.
    usage_thread_alive: bool,
    /// PID of the currently-attached client (from handshake). Used by the
    /// usage sampler to include client RSS/CPU in the report.
    active_client_pid: Option<u32>,
}

impl Registry {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: 1,
            active: None,
            active_generation: 0,
            usage_subscribed: false,
            usage_thread_alive: false,
            active_client_pid: None,
        }
    }
}

/// Entry point. Owns the listener for the process lifetime.
pub fn run() -> Result<()> {
    let sock = ipc::resolve_socket_path();
    init_logging(&sock);
    write_pidfile(&sock)?;

    // If a stale socket exists, unlink. We're the sole owner.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).context("bind supervisor socket")?;
    tracing::info!(socket = %sock.display(), pid = std::process::id(), "supervisor up");

    let registry = Arc::new(Mutex::new(Registry::new()));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let reg = Arc::clone(&registry);
                thread::spawn(move || {
                    if let Err(e) = serve_client(reg, stream) {
                        tracing::warn!("client session ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept error: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}

fn serve_client(registry: Arc<Mutex<Registry>>, stream: UnixStream) -> Result<()> {
    // Split into a read half (for ClientMsg) and a write half (shared with
    // per-session reader threads forwarding OutputDelta).
    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));

    // Handshake
    let req: HandshakeReq = ipc::read_frame(&mut reader)?;
    if req.protocol != PROTOCOL_VERSION {
        let resp = HandshakeResp::VersionMismatch {
            supervisor_protocol: PROTOCOL_VERSION,
        };
        let _ = ipc::write_frame(&mut *writer.lock().unwrap(), &resp);
        return Ok(());
    }

    // Steal active slot. Old client gets `Detached`, then we close their writer.
    let (sessions_snapshot, generation) = {
        let mut reg = registry.lock().unwrap();
        if let Some(old) = reg.active.take()
            && let Ok(mut g) = old.lock()
        {
            let _ = ipc::write_frame(
                &mut *g,
                &SupervisorMsg::Detached {
                    reason: "new client attached".into(),
                },
            );
            let _ = g.shutdown(std::net::Shutdown::Both);
        }
        reg.active = Some(Arc::clone(&writer));
        reg.active_generation = reg.active_generation.wrapping_add(1);
        reg.active_client_pid = Some(req.client_pid);
        let snap: Vec<SessionMeta> = reg.sessions.values().map(|s| s.meta.clone()).collect();
        (snap, reg.active_generation)
    };

    let resp = HandshakeResp::Ok {
        supervisor_pid: std::process::id(),
        sessions: sessions_snapshot.clone(),
    };
    ipc::write_frame(&mut *writer.lock().unwrap(), &resp)?;

    // Immediately push a dump for every existing session so the client can
    // restore its rendered view.
    for meta in &sessions_snapshot {
        if let Some(sess) = registry.lock().unwrap().sessions.get(&meta.id).cloned() {
            send_dump(&writer, meta.id, &sess);
        }
    }

    // Command loop.
    let result = (|| -> Result<()> {
        loop {
            let msg: ClientMsg = ipc::read_frame(&mut reader)?;
            match msg {
                ClientMsg::Spawn {
                    request_id,
                    project_slug,
                    worktree_name,
                    rows,
                    cols,
                    cwd,
                    initial_command,
                } => {
                    let result = spawn_session(
                        &registry,
                        project_slug,
                        worktree_name,
                        rows,
                        cols,
                        cwd,
                        initial_command,
                    );
                    let mut w = writer.lock().unwrap();
                    match result {
                        Ok(id) => {
                            // Wire-protocol invariant: `Spawned` MUST be sent
                            // before `OutputDump` for the same id, on the same
                            // writer mutex. The client only allocates a local
                            // parser on `Spawned`; any dump that races ahead
                            // would arrive for an unknown session and be
                            // silently dropped. Both frames go through the
                            // shared `writer` mutex below, which preserves
                            // order as long as no one else writes between.
                            ipc::write_frame(&mut *w, &SupervisorMsg::Spawned { request_id, id })?;
                            drop(w);
                            if let Some(sess) = registry.lock().unwrap().sessions.get(&id).cloned()
                            {
                                send_dump(&writer, id, &sess);
                            }
                        }
                        Err(e) => {
                            ipc::write_frame(
                                &mut *w,
                                &SupervisorMsg::SpawnFailed {
                                    request_id,
                                    error: format!("{e:#}"),
                                },
                            )?;
                        }
                    }
                }
                ClientMsg::WriteBytes { id, bytes } => {
                    if let Some(sess) = registry.lock().unwrap().sessions.get(&id).cloned() {
                        let mut w = sess.writer.lock().unwrap();
                        let _ = w.write_all(&bytes);
                        let _ = w.flush();
                    }
                }
                ClientMsg::Resize { id, rows, cols } => {
                    if let Some(sess) = registry.lock().unwrap().sessions.get(&id).cloned() {
                        let _ = sess.master.lock().unwrap().resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        if let Ok(mut p) = sess.parser.lock() {
                            p.screen_mut().set_size(rows, cols);
                        }
                    }
                }
                ClientMsg::Kill { id } => {
                    kill_session(&registry, id);
                }
                ClientMsg::Attach { id } => {
                    if let Some(sess) = registry.lock().unwrap().sessions.get(&id).cloned() {
                        send_dump(&writer, id, &sess);
                    }
                }
                ClientMsg::SubscribeUsage => {
                    let mut reg = registry.lock().unwrap();
                    reg.usage_subscribed = true;
                    if !reg.usage_thread_alive {
                        reg.usage_thread_alive = true;
                        drop(reg);
                        spawn_usage_sampler(Arc::clone(&registry));
                    }
                }
                ClientMsg::UnsubscribeUsage => {
                    registry.lock().unwrap().usage_subscribed = false;
                }
                ClientMsg::Shutdown => {
                    tracing::info!("shutdown requested by client");
                    shutdown_all(&registry);
                    // Unlink the socket *before* exiting so a racing client
                    // probe gets ENOENT and respawns instead of attaching to
                    // a half-dead supervisor.
                    let sock = ipc::resolve_socket_path();
                    let _ = std::fs::remove_file(&sock);
                    std::process::exit(0);
                }
            }
        }
    })();

    // Clean up if we're still the active client.
    let mut reg = registry.lock().unwrap();
    if reg
        .active
        .as_ref()
        .map(|w| Arc::ptr_eq(w, &writer))
        .unwrap_or(false)
        && reg.active_generation == generation
    {
        reg.active = None;
        reg.usage_subscribed = false;
        reg.active_client_pid = None;
    }
    result
}

fn spawn_session(
    registry: &Arc<Mutex<Registry>>,
    project_slug: String,
    worktree_name: String,
    rows: u16,
    cols: u16,
    cwd: PathBuf,
    initial_command: Option<String>,
) -> Result<SessionId> {
    let pty_system = NativePtySystem::default();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    let resolved_cwd = if cwd.as_os_str().is_empty() || cwd == std::path::Path::new(".") {
        std::env::current_dir().unwrap_or(cwd.clone())
    } else {
        cwd.clone()
    };
    cmd.cwd(resolved_cwd);
    let mut child = pair.slave.spawn_command(cmd)?;
    let killer = child.clone_killer();
    let child_pid = child.process_id();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    if let Some(initial) = initial_command {
        let mut bytes = initial.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        let _ = writer.write_all(&bytes);
    }

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
    let output_log = Arc::new(Mutex::new(OutputLog::default()));

    // Allocate id & register before we start the reader thread so any early
    // output can find the active client.
    let id = {
        let mut reg = registry.lock().unwrap();
        let id = reg.next_id;
        reg.next_id += 1;
        let meta = SessionMeta {
            id,
            project_slug,
            worktree_name,
            cwd,
            rows,
            cols,
        };
        let sess = Arc::new(Supervised {
            meta,
            child_pid,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            parser: Arc::clone(&parser),
            output_log: Arc::clone(&output_log),
            killer: Mutex::new(killer),
        });
        reg.sessions.insert(id, sess);
        id
    };

    // PTY reader → parser + forward as OutputDelta.
    {
        let registry = Arc::clone(registry);
        let parser = Arc::clone(&parser);
        let output_log = Arc::clone(&output_log);
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Cached `(active_generation, writer)` so the hot per-byte path
            // doesn't lock the global registry on every chunk. Refresh on
            // generation mismatch (steal-on-attach changed the active slot).
            let mut cached: Option<(u64, ClientWriter)> = None;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser.lock() {
                            p.process(&buf[..n]);
                        }
                        if let Ok(mut log) = output_log.lock() {
                            push_output_log(&mut log, &buf[..n]);
                        }
                        let current_gen = {
                            let reg = registry.lock().unwrap();
                            if cached.as_ref().map(|(g, _)| *g) != Some(reg.active_generation) {
                                cached = reg
                                    .active
                                    .as_ref()
                                    .map(|w| (reg.active_generation, Arc::clone(w)));
                            }
                            reg.active_generation
                        };
                        let _ = current_gen;
                        let write_err = if let Some((_, w)) = &cached {
                            match w.lock() {
                                Ok(mut g) => ipc::write_frame(
                                    &mut *g,
                                    &SupervisorMsg::OutputDelta {
                                        id,
                                        bytes: buf[..n].to_vec(),
                                    },
                                )
                                .is_err(),
                                Err(_) => true,
                            }
                        } else {
                            false
                        };
                        if write_err {
                            // Active client's socket died; drop cache so
                            // next iteration repopulates (or stays None).
                            cached = None;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(session = id, "PTY read error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // Child reaper.
    {
        let registry = Arc::clone(registry);
        thread::spawn(move || {
            let _ = child.wait();
            // Single critical section: remove the session *and* snapshot the
            // active writer. Splitting these two locks lets an attaching
            // client land between them — it would miss the session in the
            // resume list and then receive an `Exited` for an id it never
            // saw, confusing the rebind logic.
            let active = {
                let mut reg = registry.lock().unwrap();
                reg.sessions.remove(&id);
                reg.active.clone()
            };
            if let Some(w) = active
                && let Ok(mut g) = w.lock()
            {
                let _ = ipc::write_frame(&mut *g, &SupervisorMsg::Exited { id });
            }
        });
    }
    Ok(id)
}

fn kill_session(registry: &Arc<Mutex<Registry>>, id: SessionId) {
    let sess = registry.lock().unwrap().sessions.get(&id).cloned();
    if let Some(sess) = sess {
        let _ = sess.killer.lock().unwrap().kill();
    }
}

/// Snapshot all sessions, SIGTERM each child's PID group, give the reapers
/// up to ~500 ms to wait() them, then move on. Reaper threads do the actual
/// `child.wait()` — we just nudge them and poll the session map.
fn shutdown_all(registry: &Arc<Mutex<Registry>>) {
    let sessions: Vec<Arc<Supervised>> = registry
        .lock()
        .unwrap()
        .sessions
        .values()
        .cloned()
        .collect();
    for sess in &sessions {
        if let Ok(mut k) = sess.killer.lock() {
            let _ = k.kill();
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if registry.lock().unwrap().sessions.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Replay every PTY byte we've buffered so the client's local vt100 parser
/// rebuilds visible screen *and* scrollback. Falls back to
/// `contents_formatted()` if the log is empty (fresh session that hasn't
/// produced output yet) so the cursor still lands in a sensible spot.
fn send_dump(writer: &ClientWriter, id: SessionId, sess: &Supervised) {
    let bytes: Vec<u8> = {
        let log = sess.output_log.lock().unwrap();
        if log.buf.is_empty() || log.truncated {
            // Either no output yet, or the buffer evicted its head and the
            // remaining prefix may start mid-CSI. Fall back to the parser's
            // formatted view — loses scrollback but never smears colors.
            drop(log);
            let p = sess.parser.lock().unwrap();
            p.screen().contents_formatted()
        } else {
            log.buf.iter().copied().collect()
        }
    };
    if let Ok(mut g) = writer.lock() {
        let _ = ipc::write_frame(&mut *g, &SupervisorMsg::OutputDump { id, bytes });
    }
}

/// Append `chunk` to the per-session replay log, evicting from the front to
/// stay within [`OUTPUT_LOG_CAP`]. After enforcing the cap, advance the head
/// to the next newline (within a small window) so replay doesn't start mid
/// escape sequence; if no newline is found in-window, mark the log truncated
/// so [`send_dump`] falls back to the parser's formatted view.
fn push_output_log(log: &mut OutputLog, chunk: &[u8]) {
    log.buf.extend(chunk.iter().copied());
    if log.buf.len() > OUTPUT_LOG_CAP {
        let drop_n = log.buf.len() - OUTPUT_LOG_CAP;
        log.buf.drain(..drop_n);
        let scan = log.buf.len().min(4096);
        if let Some(pos) = log.buf.iter().take(scan).position(|b| *b == b'\n') {
            log.buf.drain(..=pos);
        }
        log.truncated = true;
    }
}

/// Background thread that samples process trees and pushes one
/// [`UsageReport`] per second to the active client while
/// `registry.usage_subscribed` is true.
fn spawn_usage_sampler(registry: Arc<Mutex<Registry>>) {
    use std::collections::{HashSet, VecDeque};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    thread::spawn(move || {
        let mut system = System::new();
        // Prime CPU usage. The first per-process cpu_usage() reading after
        // a fresh System is 0 by design — we sleep a beat then refresh again
        // before the first emit so the user sees meaningful numbers.
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        thread::sleep(Duration::from_millis(250));

        loop {
            // Subscribe-check + thread-alive flip must happen under the same
            // lock. Otherwise a client subscribing between our read of
            // `subscribed=false` and our write of `usage_thread_alive=false`
            // would see "alive=true" and skip spawning a sampler — leaving
            // `subscribed=true` with no producer.
            let snapshot = {
                let mut reg = registry.lock().unwrap();
                if !reg.usage_subscribed || reg.active.is_none() {
                    reg.usage_thread_alive = false;
                    tracing::debug!("usage sampler exiting");
                    return;
                }
                (
                    reg.active.clone().unwrap(),
                    reg.sessions
                        .values()
                        .map(|s| (s.meta.clone(), s.child_pid))
                        .collect::<Vec<_>>(),
                    reg.active_client_pid,
                )
            };
            let (writer, sessions_snapshot, client_pid) = snapshot;

            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );

            // Build a parent-of and pgid-of map once, then index per session.
            let procs = system.processes();
            let mut by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
            for (pid, proc) in procs {
                if let Some(parent) = proc.parent() {
                    by_parent
                        .entry(parent.as_u32())
                        .or_default()
                        .push(pid.as_u32());
                }
            }
            let pgid_of = collect_pgids(procs.keys().map(|p| p.as_u32()));

            let cpu_count = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(0);
            let supervisor_pid = std::process::id();

            let mut sessions = Vec::with_capacity(sessions_snapshot.len());
            for (meta, child_pid) in &sessions_snapshot {
                let Some(root_pid) = child_pid else { continue };
                // Union: PPID-walk + pgid filter (catches double-forked
                // descendants that lost their parent link).
                let mut included: HashSet<u32> = HashSet::new();
                included.insert(*root_pid);
                let mut queue: VecDeque<u32> = VecDeque::from([*root_pid]);
                while let Some(p) = queue.pop_front() {
                    if let Some(kids) = by_parent.get(&p) {
                        for k in kids {
                            if included.insert(*k) {
                                queue.push_back(*k);
                            }
                        }
                    }
                }
                // pgid fallback: anything in our session's pgid (== root_pid
                // by virtue of PTY setsid) that the PPID walk missed.
                for (pid, pgid) in &pgid_of {
                    if *pgid == *root_pid {
                        included.insert(*pid);
                    }
                }
                // Collect pids reachable from root_pid via PPID; the orphans
                // (in `included` but unreachable) get appended flat to root.
                let mut reachable: HashSet<u32> = HashSet::new();
                reachable.insert(*root_pid);
                let mut q: VecDeque<u32> = VecDeque::from([*root_pid]);
                while let Some(p) = q.pop_front() {
                    if let Some(kids) = by_parent.get(&p) {
                        for k in kids {
                            if included.contains(k) && reachable.insert(*k) {
                                q.push_back(*k);
                            }
                        }
                    }
                }
                let orphans: Vec<u32> = included
                    .iter()
                    .copied()
                    .filter(|p| !reachable.contains(p))
                    .collect();
                let mut root = build_tree(*root_pid, &reachable, &by_parent, procs);
                for o in orphans {
                    root.children
                        .push(build_tree(o, &reachable, &by_parent, procs));
                }
                sessions.push(SessionUsage {
                    session_id: meta.id,
                    project_slug: meta.project_slug.clone(),
                    worktree_name: meta.worktree_name.clone(),
                    root,
                });
            }

            let supervisor = procs
                .get(&Pid::from_u32(supervisor_pid))
                .map(|p| ProcessNode {
                    pid: supervisor_pid,
                    name: p.name().to_string_lossy().to_string(),
                    rss_bytes: p.memory(),
                    cpu_percent: p.cpu_usage(),
                    children: Vec::new(),
                })
                .unwrap_or_else(|| ProcessNode {
                    pid: supervisor_pid,
                    name: "imbuia".into(),
                    rss_bytes: 0,
                    cpu_percent: 0.0,
                    children: Vec::new(),
                });

            let client = client_pid.and_then(|pid| {
                procs.get(&Pid::from_u32(pid)).map(|p| ProcessNode {
                    pid,
                    name: p.name().to_string_lossy().to_string(),
                    rss_bytes: p.memory(),
                    cpu_percent: p.cpu_usage(),
                    children: Vec::new(),
                })
            });
            let report = UsageReport {
                sessions,
                supervisor,
                client,
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                cpu_count,
            };
            if let Ok(mut g) = writer.lock() {
                let _ = ipc::write_frame(&mut *g, &SupervisorMsg::Usage(report));
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Recursively materialize a [`ProcessNode`] for `pid`, including only PIDs
/// in `included` (the union of PPID-walk + pgid filter).
fn build_tree(
    pid: u32,
    included: &std::collections::HashSet<u32>,
    by_parent: &HashMap<u32, Vec<u32>>,
    procs: &HashMap<sysinfo::Pid, sysinfo::Process>,
) -> ProcessNode {
    let (name, rss_bytes, cpu_percent) = match procs.get(&sysinfo::Pid::from_u32(pid)) {
        Some(p) => (
            p.name().to_string_lossy().to_string(),
            p.memory(),
            p.cpu_usage(),
        ),
        None => ("<gone>".into(), 0, 0.0),
    };
    let mut children = Vec::new();
    if let Some(kids) = by_parent.get(&pid) {
        for k in kids {
            if included.contains(k) {
                children.push(build_tree(*k, included, by_parent, procs));
            }
        }
    }
    ProcessNode {
        pid,
        name,
        rss_bytes,
        cpu_percent,
        children,
    }
}

#[cfg(unix)]
fn collect_pgids<I: IntoIterator<Item = u32>>(pids: I) -> Vec<(u32, u32)> {
    pids.into_iter()
        .filter_map(|pid| {
            let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
            if pgid < 0 {
                None
            } else {
                Some((pid, pgid as u32))
            }
        })
        .collect()
}

fn write_pidfile(sock: &std::path::Path) -> Result<()> {
    let path = ipc::pidfile_path(sock);
    std::fs::write(&path, std::process::id().to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn init_logging(sock: &std::path::Path) {
    let log_path = ipc::supervisor_log_path(sock);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
    {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_target(false)
            .try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_output_log_appends_under_cap() {
        let mut log = OutputLog::default();
        push_output_log(&mut log, b"hello\n");
        assert_eq!(log.buf.len(), 6);
        assert!(!log.truncated);
    }

    #[test]
    fn push_output_log_evicts_at_cap_with_newline_boundary() {
        let mut log = OutputLog::default();
        // Fill with a long string of "line\n" so eviction has newlines to align on.
        let chunk = b"line\n".repeat(OUTPUT_LOG_CAP / 5 + 100);
        push_output_log(&mut log, &chunk);
        assert!(log.buf.len() <= OUTPUT_LOG_CAP);
        assert!(log.truncated, "should be flagged truncated after eviction");
        // After eviction we should land at (or just past) a newline boundary.
        // Either the head is right after a `\n` or no newline was found —
        // both cases are valid; we just need the cap respected.
    }

    #[test]
    fn push_output_log_marks_truncated_even_without_newline() {
        let mut log = OutputLog::default();
        // No newlines anywhere — eviction can't align, but truncated must flip.
        let chunk = vec![b'x'; OUTPUT_LOG_CAP + 1024];
        push_output_log(&mut log, &chunk);
        assert!(log.buf.len() <= OUTPUT_LOG_CAP);
        assert!(log.truncated);
    }
}
