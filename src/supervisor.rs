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
    self, ClientMsg, DirEntry, HandshakeReq, HandshakeResp, OpOk, OpRequest, OpResult,
    PROTOCOL_VERSION, ProcessNode, ProjectInfo, SessionId, SessionMeta, SessionUsage,
    SupervisorMsg, UsageReport, WorktreeEntry,
};
use crate::{config, git, github, transport};
use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

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
use arc_swap::ArcSwapOption;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Active-client handle: a bounded queue of outgoing frames drained by a
/// dedicated async writer task, plus a [`CancellationToken`] used by
/// steal-on-attach to tear down the previous client's reader + writer tasks
/// (which drops both halves of its stream and closes the socket) even if the
/// writer is parked on a back-pressured socket.
///
/// `tx` is a tokio channel: the async command loop sends via `.send().await`,
/// while the off-runtime PTY/reaper/usage/op threads use
/// `.blocking_send()` / `.try_send()` (legal because those are plain
/// `std::thread`s, not runtime workers).
struct ClientChan {
    tx: mpsc::Sender<SupervisorMsg>,
    cancel: CancellationToken,
}

struct Supervised {
    meta: SessionMeta,
    /// PID of the PTY's direct child (the user's shell). Used as the root
    /// for descendant walks in the usage sampler.
    child_pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Bounded queue feeding a per-session writer thread. The command loop
    /// pushes here and returns immediately; the writer thread does the
    /// blocking `write_all` to the PTY. This keeps the command loop
    /// responsive when a big paste fills the PTY's kernel buffer.
    write_tx: SyncSender<Vec<u8>>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Bounded ring of every raw PTY byte we've seen. Replayed verbatim on
    /// attach so the client's local vt100 parser populates its own scrollback
    /// — `parser.screen().contents_formatted()` only covers the visible grid
    /// and would otherwise discard history across client restarts.
    output_log: Arc<Mutex<OutputLog>>,
    /// Inferred keyboard-input protocol the inner app negotiated (kitty /
    /// modifyOtherKeys). vt100 doesn't track it, so we scan PTY output into a
    /// `KbdTracker` and re-emit it in the attach prelude (see `send_dump`) so a
    /// reattaching client recovers the state even if the negotiation scrolled
    /// out of `output_log`.
    kbd: Arc<Mutex<crate::input::KbdTracker>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

struct Registry {
    sessions: HashMap<SessionId, Arc<Supervised>>,
    next_id: SessionId,
    /// Set when the active client subscribed to usage frames. The sampler
    /// thread polls this and exits when it goes false.
    usage_subscribed: bool,
    /// True while a usage-sampler thread is alive. Prevents double-spawn.
    usage_thread_alive: bool,
    /// PID of the currently-attached client (from handshake). Used by the
    /// usage sampler to include client RSS/CPU in the report.
    active_client_pid: Option<u32>,
}

/// Process-wide shared state. Hot path (PTY reader threads forwarding
/// `OutputDelta`) reads `active` + `active_generation` without touching the
/// registry mutex; writes to those fields are synchronized via the registry
/// mutex so attach/cleanup stays linearizable.
struct Shared {
    registry: Mutex<Registry>,
    /// Currently-attached client's frame queue + stream handle, or `None`
    /// when no client is connected. Updated under `registry` lock to
    /// serialize with cleanup.
    active: ArcSwapOption<ClientChan>,
    /// Generation counter; bumped when `active` changes. Per-session reader
    /// threads use it to know if their cached handle is stale.
    active_generation: AtomicU64,
    /// Inbox for the singleton `gh` worker. Every `FetchPr` op funnels through
    /// here so at most one `gh` runs at a time (auth-refresh / rate-limit
    /// races otherwise), with same-repo jobs coalesced to the newest. Unbounded
    /// so the command loop's `send` never blocks behind a slow `gh`.
    gh_tx: std::sync::mpsc::Sender<GhJob>,
}

impl Shared {
    fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                sessions: HashMap::new(),
                next_id: 1,
                usage_subscribed: false,
                usage_thread_alive: false,
                active_client_pid: None,
            }),
            active: ArcSwapOption::empty(),
            active_generation: AtomicU64::new(0),
            gh_tx: spawn_gh_worker(),
        }
    }
}

/// Entry point. Builds a tokio runtime and runs the accept loop(s) for the
/// process lifetime. `listen` (from `--listen host:port`) enables the optional
/// TCP+TLS acceptor in addition to the always-on local Unix socket.
pub fn run(listen: Option<String>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building supervisor runtime")?;
    rt.block_on(serve(listen))
}

async fn serve(listen: Option<String>) -> Result<()> {
    let sock = ipc::resolve_socket_path();
    init_logging(&sock);
    write_pidfile(&sock)?;

    // If a stale socket exists, unlink. We're the sole owner.
    let _ = std::fs::remove_file(&sock);
    let uds = UnixListener::bind(&sock).context("bind supervisor socket")?;
    tracing::info!(socket = %sock.display(), pid = std::process::id(), "supervisor up");

    let shared = Arc::new(Shared::new());

    // Always-on local UDS acceptor.
    let uds_task = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                match uds.accept().await {
                    Ok((stream, _addr)) => {
                        let (rd, wr) = tokio::io::split(stream);
                        let sh = Arc::clone(&shared);
                        tokio::spawn(async move {
                            // Local UDS: no compression.
                            if let Err(e) = handle_conn(sh, rd, wr, false).await {
                                tracing::warn!("uds client session ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("uds accept error: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        })
    };

    // Optional TCP + mutually-authenticated TLS acceptor.
    let tls_task = match listen {
        None => None,
        Some(addr) => {
            let dir = config::resolve_config_dir();
            let identity = transport::load_or_create_identity(&dir)?;
            tracing::info!(fingerprint = %identity.fingerprint, "supervisor TLS identity");
            let acceptor = TlsAcceptor::from(transport::server_config(&identity, &dir)?);
            let tcp = TcpListener::bind(&addr)
                .await
                .with_context(|| format!("binding TCP listener on {addr}"))?;
            tracing::info!(%addr, "supervisor listening for remote clients");
            let shared = Arc::clone(&shared);
            Some(tokio::spawn(async move {
                loop {
                    match tcp.accept().await {
                        Ok((stream, peer)) => {
                            // Disable Nagle so the supervisor's small writes
                            // (and the client's keystrokes) aren't held ~40ms.
                            if let Err(e) = stream.set_nodelay(true) {
                                tracing::warn!(%peer, "set_nodelay failed: {e}");
                            }
                            let acceptor = acceptor.clone();
                            let sh = Arc::clone(&shared);
                            tokio::spawn(async move {
                                // TLS handshake (incl. authorized_keys client
                                // auth) happens here; a rejected client never
                                // reaches `handle_conn`.
                                match acceptor.accept(stream).await {
                                    Ok(tls) => {
                                        let (rd, wr) = tokio::io::split(tls);
                                        // Remote link: compress large frames.
                                        if let Err(e) = handle_conn(sh, rd, wr, true).await {
                                            tracing::warn!("tcp client session ended: {e}");
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(%peer, "TLS handshake failed: {e}")
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!("tcp accept error: {e}");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }))
        }
    };

    // Acceptors loop forever (or until the process exits via `Shutdown`).
    if let Some(tls_task) = tls_task {
        let _ = tokio::try_join!(flatten(uds_task), flatten(tls_task));
    } else {
        let _ = flatten(uds_task).await;
    }
    Ok(())
}

/// Flatten a `JoinHandle<()>` into a `Result` so it composes with `try_join!`.
async fn flatten(handle: tokio::task::JoinHandle<()>) -> Result<()> {
    handle.await.context("acceptor task panicked")
}

/// Serve one attached client over the given async read/write halves (a UDS
/// stream or a TLS-over-TCP stream — `handle_conn` is transport-agnostic).
async fn handle_conn<R, W>(shared: Arc<Shared>, mut rd: R, mut wr: W, compress: bool) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Handshake against the bare halves before the writer task exists.
    let req: HandshakeReq = ipc::read_frame_async(&mut rd).await?;
    if req.protocol != PROTOCOL_VERSION {
        let resp = HandshakeResp::VersionMismatch {
            supervisor_protocol: PROTOCOL_VERSION,
        };
        let _ = ipc::write_frame_async(&mut wr, &resp, false).await;
        return Ok(());
    }

    // Snapshot existing sessions for the handshake reply *before* the steal —
    // the new client should see the session list as of now, not whatever
    // raced in during the swap.
    let sessions_snapshot: Vec<SessionMeta> = shared
        .registry
        .lock()
        .unwrap()
        .sessions
        .values()
        .map(|s| s.meta.clone())
        .collect();
    let resp = HandshakeResp::Ok {
        supervisor_pid: std::process::id(),
        sessions: sessions_snapshot.clone(),
    };
    ipc::write_frame_async(&mut wr, &resp, false).await?;

    // From here on every outgoing frame goes through `tx`, drained by the
    // writer task. `cancel` tears both tasks down on steal-on-attach.
    let (tx, mut rx) = mpsc::channel::<SupervisorMsg>(512);
    let cancel = CancellationToken::new();
    let writer = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = rx.recv() => {
                        let Some(msg) = msg else { break };
                        let is_detached = matches!(msg, SupervisorMsg::Detached { .. });
                        // Race the write against cancel: a steal must not wait
                        // on a wedged socket. A half-written frame is fine — the
                        // connection is being abandoned anyway.
                        let wrote = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break,
                            res = ipc::write_frame_async(&mut wr, &msg, compress) => res,
                        };
                        if wrote.is_err() || is_detached {
                            break;
                        }
                    }
                }
            }
            // `wr` drops here, closing the write half.
        })
    };
    let chan = Arc::new(ClientChan {
        tx: tx.clone(),
        cancel: cancel.clone(),
    });

    // Steal the active slot under the registry mutex so cleanup paths observe a
    // consistent (chan, pid) pair.
    let (generation, old) = {
        let mut reg = shared.registry.lock().unwrap();
        let old = shared.active.swap(Some(Arc::clone(&chan)));
        let generation = shared
            .active_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        reg.active_client_pid = Some(req.client_pid);
        (generation, old)
    };
    if let Some(old) = old {
        // try_send so we never block on a wedged old client; cancel() is what
        // actually frees its reader + writer tasks.
        let _ = old.tx.try_send(SupervisorMsg::Detached {
            reason: "new client attached".into(),
        });
        old.cancel.cancel();
    }

    // Push a dump for every existing session so the client restores its view.
    for meta in &sessions_snapshot {
        let sess = shared
            .registry
            .lock()
            .unwrap()
            .sessions
            .get(&meta.id)
            .cloned();
        if let Some(sess) = sess {
            let _ = tx.send(build_dump(meta.id, &sess)).await;
        }
    }

    let result = command_loop(&shared, &mut rd, &tx, &cancel).await;

    // Clean up if we're still the active client (attach updates `active` under
    // the registry mutex, so the generation check is consistent).
    {
        let mut reg = shared.registry.lock().unwrap();
        if shared.active_generation.load(Ordering::Acquire) == generation {
            shared.active.store(None);
            reg.usage_subscribed = false;
            reg.active_client_pid = None;
        }
    }
    // Stop the writer task and drop our senders; `rd` drops on return.
    cancel.cancel();
    drop(tx);
    drop(chan);
    let _ = writer.await;
    result
}

/// The per-client command loop: read `ClientMsg`s and dispatch. Returns when
/// the client disconnects (EOF) or the connection is cancelled (steal).
async fn command_loop<R>(
    shared: &Arc<Shared>,
    rd: &mut R,
    tx: &mpsc::Sender<SupervisorMsg>,
    cancel: &CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let msg: ClientMsg = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            r = ipc::read_frame_async(rd) => r?,
        };
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
                    shared,
                    project_slug,
                    worktree_name,
                    rows,
                    cols,
                    cwd,
                    initial_command,
                );
                match result {
                    Ok(id) => {
                        // Wire-protocol invariant: `Spawned` MUST precede
                        // `OutputDump` for the same id on the same channel. The
                        // writer drains FIFO, so sending Spawned first preserves
                        // order.
                        tx.send(SupervisorMsg::Spawned { request_id, id })
                            .await
                            .map_err(|_| anyhow::anyhow!("writer task closed"))?;
                        let sess = shared.registry.lock().unwrap().sessions.get(&id).cloned();
                        if let Some(sess) = sess {
                            let _ = tx.send(build_dump(id, &sess)).await;
                        }
                    }
                    Err(e) => {
                        tx.send(SupervisorMsg::SpawnFailed {
                            request_id,
                            error: format!("{e:#}"),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("writer task closed"))?;
                    }
                }
            }
            ClientMsg::WriteBytes { id, bytes } => {
                let sess = shared.registry.lock().unwrap().sessions.get(&id).cloned();
                if let Some(sess) = sess {
                    // The per-session PTY writer thread has its own bounded
                    // queue; this `send` is non-blocking unless that queue is
                    // full, in which case it paces this client's intake.
                    let _ = sess.write_tx.send(bytes);
                }
            }
            ClientMsg::Resize { id, rows, cols } => {
                if let Some(sess) = shared.registry.lock().unwrap().sessions.get(&id).cloned() {
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
                kill_session(shared, id);
            }
            ClientMsg::Attach { id } => {
                let sess = shared.registry.lock().unwrap().sessions.get(&id).cloned();
                if let Some(sess) = sess {
                    let _ = tx.send(build_dump(id, &sess)).await;
                }
            }
            ClientMsg::SubscribeUsage => {
                let mut reg = shared.registry.lock().unwrap();
                reg.usage_subscribed = true;
                if !reg.usage_thread_alive {
                    reg.usage_thread_alive = true;
                    drop(reg);
                    spawn_usage_sampler(Arc::clone(shared));
                }
            }
            ClientMsg::UnsubscribeUsage => {
                shared.registry.lock().unwrap().usage_subscribed = false;
            }
            ClientMsg::Op { request_id, req } => {
                // Run off the command loop — `worktree add` can take minutes and
                // would otherwise freeze WriteBytes/Resize. Reply goes to the
                // active client's writer.
                match shared.active.load_full() {
                    None => {
                        tracing::warn!(request_id, "Op with no active client; dropping")
                    }
                    Some(chan) => match req {
                        OpRequest::FetchPr {
                            repo_path,
                            worktrees,
                        } => {
                            if let Err(e) = shared.gh_tx.send(GhJob {
                                request_id,
                                repo_path,
                                worktrees,
                                chan,
                            }) {
                                tracing::warn!("gh worker channel closed: {e}");
                            }
                        }
                        git_op => {
                            thread::spawn(move || {
                                // Drop guard: a panic in `run_git_op` still
                                // sends an Err so the client's `pending_op`
                                // never wedges.
                                let guard = OpReplyGuard::new(chan, request_id);
                                let result = run_git_op(git_op);
                                guard.complete(result);
                            });
                        }
                    },
                }
            }
            ClientMsg::Shutdown => {
                tracing::info!("shutdown requested by client");
                shutdown_all(shared);
                // Unlink the socket *before* exiting so a racing client probe
                // gets ENOENT and respawns instead of attaching to a half-dead
                // supervisor.
                let sock = ipc::resolve_socket_path();
                let _ = std::fs::remove_file(&sock);
                std::process::exit(0);
            }
        }
    }
}

fn spawn_session(
    shared: &Arc<Shared>,
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

    // Per-session writer thread. Bounded queue so a runaway producer can't
    // grow memory unboundedly; the command loop uses a *blocking* `send`, so a
    // full queue back-pressures that client's command loop rather than dropping
    // bytes. Sized to buffer a handful of 16KB paste chunks before throttling.
    let (write_tx, write_rx) = sync_channel::<Vec<u8>>(256);
    thread::spawn(move || {
        while let Ok(bytes) = write_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
    let output_log = Arc::new(Mutex::new(OutputLog::default()));
    let kbd = Arc::new(Mutex::new(crate::input::KbdTracker::default()));

    // Allocate id & register before we start the reader thread so any early
    // output can find the active client.
    let id = {
        let mut reg = shared.registry.lock().unwrap();
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
            write_tx,
            parser: Arc::clone(&parser),
            output_log: Arc::clone(&output_log),
            kbd: Arc::clone(&kbd),
            killer: Mutex::new(killer),
        });
        reg.sessions.insert(id, sess);
        id
    };

    // PTY reader → parser + forward as OutputDelta.
    {
        let shared = Arc::clone(shared);
        let parser = Arc::clone(&parser);
        let output_log = Arc::clone(&output_log);
        let kbd = Arc::clone(&kbd);
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Cached `(active_generation, chan)` so the hot per-byte path
            // never touches the registry mutex. ArcSwap reads are lock-free;
            // we only reload when the generation counter moves (steal-on-
            // attach changed the active slot).
            let mut cached: Option<(u64, Arc<ClientChan>)> = None;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser.lock() {
                            p.process(&buf[..n]);
                        }
                        if let Ok(mut k) = kbd.lock() {
                            k.feed(&buf[..n]);
                        }
                        if let Ok(mut log) = output_log.lock() {
                            push_output_log(&mut log, &buf[..n]);
                        }
                        let cur_gen = shared.active_generation.load(Ordering::Acquire);
                        if cached.as_ref().map(|(g, _)| *g) != Some(cur_gen) {
                            cached = shared.active.load_full().map(|c| (cur_gen, c));
                        }
                        if let Some((_, chan)) = &cached {
                            // Bounded blocking_send: a slow client back-pressures
                            // the PTY reader (capacity 512). If the client's
                            // socket dies, the writer task drops the receiver and
                            // this returns Err — we clear the cache so the next
                            // iteration sees None (or a newly-attached client).
                            // Legal off-runtime: this is a plain std thread.
                            let res = chan.tx.blocking_send(SupervisorMsg::OutputDelta {
                                id,
                                bytes: buf[..n].to_vec(),
                            });
                            if res.is_err() {
                                cached = None;
                            }
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
        let shared = Arc::clone(shared);
        thread::spawn(move || {
            let _ = child.wait();
            // Single critical section: remove the session *and* snapshot the
            // active writer under the registry mutex. Attach also updates
            // `active_writer` under this mutex, so the snapshot is
            // consistent — an attaching client either lands fully before us
            // (no missed Exited) or fully after (sees no session, no Exited).
            let active = {
                let mut reg = shared.registry.lock().unwrap();
                reg.sessions.remove(&id);
                shared.active.load_full()
            };
            if let Some(chan) = active {
                let _ = chan.tx.blocking_send(SupervisorMsg::Exited { id });
            }
        });
    }
    Ok(id)
}

fn kill_session(shared: &Arc<Shared>, id: SessionId) {
    let sess = shared.registry.lock().unwrap().sessions.get(&id).cloned();
    if let Some(sess) = sess {
        let _ = sess.killer.lock().unwrap().kill();
    }
}

/// One queued PR-status fetch for the singleton gh worker. Carries the
/// `chan` to reply on so the worker isn't coupled to whichever client is
/// active when it finishes.
struct GhJob {
    request_id: u64,
    repo_path: PathBuf,
    worktrees: Vec<(usize, PathBuf)>,
    chan: Arc<ClientChan>,
}

/// Drop guard mirroring `runtime::OpGuard`: guarantees the client gets *some*
/// `OpResult` for `request_id` even if the op thread panics, so the client's
/// `pending_op` status bar can't stick on "doing X…" forever.
struct OpReplyGuard {
    chan: Arc<ClientChan>,
    request_id: u64,
    armed: bool,
}

impl OpReplyGuard {
    fn new(chan: Arc<ClientChan>, request_id: u64) -> Self {
        Self {
            chan,
            request_id,
            armed: true,
        }
    }

    fn complete(mut self, result: OpResult) {
        let _ = self.chan.tx.blocking_send(SupervisorMsg::OpResult {
            request_id: self.request_id,
            result,
        });
        self.armed = false;
    }
}

impl Drop for OpReplyGuard {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(self.request_id, "op thread ended without a result");
            let _ = self.chan.tx.blocking_send(SupervisorMsg::OpResult {
                request_id: self.request_id,
                result: Err("operation ended without a result".into()),
            });
        }
    }
}

/// Run a non-`gh` repo op synchronously. Called on a throwaway thread so the
/// command loop stays responsive. `FetchPr` is handled by the gh worker, not
/// here.
fn run_git_op(req: OpRequest) -> OpResult {
    match req {
        OpRequest::Validate { repo_path } => {
            // Expand `~` against the *supervisor's* HOME (the client no longer
            // expands — its home differs from a remote supervisor's).
            let expanded = expand_user(&repo_path);
            let absolute = std::fs::canonicalize(&expanded).map_err(|e| format!("{e:#}"))?;
            git::validate_repo(&absolute).map_err(|e| format!("{e:#}"))?;
            let head = git::head_branch(&absolute).map_err(|e| format!("{e:#}"))?;
            let repo_name = absolute
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            tracing::info!(repo = %absolute.display(), branch = ?head, "validated project");
            Ok(OpOk::Validated(ProjectInfo {
                canonical_path: absolute,
                repo_name,
                head_branch: head,
            }))
        }
        OpRequest::ListWorktrees { repo_path } => {
            let entries = git::list_worktrees(&repo_path).map_err(|e| format!("{e:#}"))?;
            tracing::info!(repo = %repo_path.display(), count = entries.len(), "listed worktrees");
            Ok(OpOk::Worktrees(
                entries
                    .into_iter()
                    .map(|e| WorktreeEntry {
                        path: e.path,
                        branch: e.branch,
                    })
                    .collect(),
            ))
        }
        OpRequest::WorktreeAdd { repo_path, branch } => {
            let dest = worktree_dest(&repo_path, &branch);
            git::worktree_add(&repo_path, &dest, &branch).map_err(|e| format!("{e:#}"))?;
            tracing::info!(repo = %repo_path.display(), %branch, dest = %dest.display(), "worktree added");
            Ok(OpOk::WorktreeAdded(WorktreeEntry {
                path: dest,
                branch: Some(branch),
            }))
        }
        OpRequest::WorktreeRemove {
            repo_path,
            dest_path,
            branch,
        } => {
            git::worktree_remove(&repo_path, &dest_path, branch.as_deref())
                .map_err(|e| format!("{e:#}"))?;
            tracing::info!(repo = %repo_path.display(), branch = ?branch, "worktree removed");
            Ok(OpOk::WorktreeRemoved)
        }
        OpRequest::ListDir { path } => list_dir(path),
        OpRequest::FetchPr { .. } => {
            // Routed to the gh worker in the command loop; never reached.
            Err("FetchPr must go through the gh worker".into())
        }
    }
}

/// The supervisor's home directory (`$HOME`), or `/` as a last resort.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~` / `~/…` against the supervisor's `$HOME`.
fn expand_user(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

/// List a directory for the open-project browser. `None` lists `$HOME`. Returns
/// canonicalized `dir`, its parent, and child directories (dirs only), each
/// flagged for whether it looks like a git repo.
fn list_dir(path: Option<PathBuf>) -> OpResult {
    let target = path.map(|p| expand_user(&p)).unwrap_or_else(home_dir);
    let dir = std::fs::canonicalize(&target).map_err(|e| format!("{e:#}"))?;
    let read = std::fs::read_dir(&dir).map_err(|e| format!("{e:#}"))?;
    let mut entries: Vec<DirEntry> = Vec::new();
    for ent in read.flatten() {
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // hide dotdirs (incl. nested .git) from the browser
        }
        let is_repo = ent.path().join(".git").exists();
        entries.push(DirEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    let parent = dir.parent().map(|p| p.to_path_buf());
    tracing::info!(dir = %dir.display(), count = entries.len(), "listed directory");
    Ok(OpOk::DirListing {
        dir,
        parent,
        entries,
    })
}

/// Destination path for a new worktree: `<repo-parent>/<repo>-worktrees/<branch>`.
/// Computed supervisor-side because it's a path on the supervisor's filesystem.
fn worktree_dest(repo: &std::path::Path, branch: &str) -> PathBuf {
    let parent = repo.parent().unwrap_or(repo);
    let base = repo
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    parent.join(format!("{base}-worktrees")).join(branch)
}

/// Spawn the singleton `gh` worker. Drains its queue serially so only one
/// `gh` runs at a time; coalesces queued jobs for the *same repo* down to the
/// newest (an older poll would observe the same upstream state anyway). The
/// stateless analog of runtime's old per-`project_idx` coalescing.
fn spawn_gh_worker() -> std::sync::mpsc::Sender<GhJob> {
    let (tx, rx) = std::sync::mpsc::channel::<GhJob>();
    thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut job = first;
            // Older same-repo jobs we coalesce away: we still ACK each one
            // (with the newest job's result) so the client clears its
            // `pending_ops` entry instead of leaking it.
            let mut superseded: Vec<(u64, Arc<ClientChan>)> = Vec::new();
            while let Ok(next) = rx.try_recv() {
                if next.repo_path == job.repo_path {
                    superseded.push((job.request_id, Arc::clone(&job.chan)));
                    job = next;
                } else {
                    run_fetch_pr(job, std::mem::take(&mut superseded));
                    job = next;
                }
            }
            run_fetch_pr(job, superseded);
        }
        tracing::info!("gh worker exiting (channel closed)");
    });
    tx
}

/// Resolve each worktree's live HEAD then query `gh pr list`, replying with an
/// `OpResult` to `job` and to every `superseded` (coalesced) request_id.
/// Mirrors the old client-side `runtime::do_fetch`: `Ok` if any worktree
/// resolved, `Err(last_err)` only if every one failed.
fn run_fetch_pr(job: GhJob, superseded: Vec<(u64, Arc<ClientChan>)>) {
    let GhJob {
        request_id,
        repo_path,
        worktrees,
        chan,
    } = job;
    tracing::info!(request_id, n = worktrees.len(), repo = %repo_path.display(), "gh: fetching per-worktree PR status");
    let mut statuses = Vec::with_capacity(worktrees.len());
    let mut last_err: Option<String> = None;
    let mut any_ok = false;
    for (wi, wt_path) in worktrees {
        // Live HEAD resolution — picks up `git switch` inside the worktree.
        let branch = match git::head_branch(&wt_path) {
            Ok(Some(b)) => b,
            Ok(None) => {
                statuses.push((wi, None));
                any_ok = true;
                continue;
            }
            Err(e) => {
                last_err = Some(format!("git symbolic-ref failed: {e}"));
                continue;
            }
        };
        match github::fetch_pr_by_branch(&repo_path, &branch) {
            Ok(s) => {
                any_ok = true;
                statuses.push((wi, s));
            }
            Err(e) => last_err = Some(format!("{e}")),
        }
    }
    let result: OpResult = if !any_ok && let Some(msg) = last_err {
        Err(msg)
    } else {
        Ok(OpOk::PrStatuses(statuses))
    };
    // ACK coalesced requests first (cloning the shared result), then the live one.
    for (rid, sc) in superseded {
        let _ = sc.tx.blocking_send(SupervisorMsg::OpResult {
            request_id: rid,
            result: result.clone(),
        });
    }
    let _ = chan
        .tx
        .blocking_send(SupervisorMsg::OpResult { request_id, result });
}

/// Snapshot all sessions, SIGTERM each child's PID group, give the reapers
/// up to ~500 ms to wait() them, then move on. Reaper threads do the actual
/// `child.wait()` — we just nudge them and poll the session map.
fn shutdown_all(shared: &Arc<Shared>) {
    let sessions: Vec<Arc<Supervised>> = shared
        .registry
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
        if shared.registry.lock().unwrap().sessions.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Replay every PTY byte we've buffered so the client's local vt100 parser
/// rebuilds visible screen *and* scrollback. Always prefixed with a mode
/// prelude (alt-screen / mouse / DECCKM / bracketed paste) derived from the
/// parser's *current* state — `contents_formatted()` doesn't include those
/// toggles, and even the raw log can start after their original emission
/// once the buffer rolls over.
///
/// Falls back to `contents_formatted()` if the log is empty (fresh session)
/// or hard-truncated (no newline boundary inside the scan window, so replay
/// could land mid-CSI).
fn build_dump(id: SessionId, sess: &Supervised) -> SupervisorMsg {
    let mut bytes = mode_prelude(&sess.parser.lock().unwrap());
    // Keyboard-protocol state lives outside vt100, in our own tracker; re-emit
    // it alongside the DECSET modes so the client's tracker re-syncs.
    bytes.extend(sess.kbd.lock().unwrap().prelude());
    {
        let log = sess.output_log.lock().unwrap();
        if log.buf.is_empty() || log.truncated {
            drop(log);
            let p = sess.parser.lock().unwrap();
            bytes.extend(p.screen().contents_formatted());
        } else {
            bytes.extend(log.buf.iter().copied());
        }
    }
    SupervisorMsg::OutputDump { id, bytes }
}

/// Escape-sequence prelude that puts the receiving vt100 parser back into
/// the same alt-screen / mouse / cursor-key / bracketed-paste mode the
/// supervisor's parser is in. Without this, after a client restart the
/// local parser thinks we're on the main screen (and no mouse capture), so
/// wheel routing in `client::ProxySession::write_mouse` goes to local
/// scrollback even when the app under the PTY is in alt-screen and wants
/// the wheel forwarded as arrow keys.
fn mode_prelude(parser: &vt100::Parser) -> Vec<u8> {
    let s = parser.screen();
    let mut out = Vec::with_capacity(64);
    if s.alternate_screen() {
        // 1049 also clears + repositions, which is what xterm does on enter.
        out.extend_from_slice(b"\x1b[?1049h");
    }
    if s.application_cursor() {
        out.extend_from_slice(b"\x1b[?1h");
    }
    if s.application_keypad() {
        out.extend_from_slice(b"\x1b=");
    }
    if s.bracketed_paste() {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    use vt100::MouseProtocolMode as M;
    match s.mouse_protocol_mode() {
        M::None => {}
        M::Press => out.extend_from_slice(b"\x1b[?9h"),
        M::PressRelease => out.extend_from_slice(b"\x1b[?1000h"),
        M::ButtonMotion => out.extend_from_slice(b"\x1b[?1002h"),
        M::AnyMotion => out.extend_from_slice(b"\x1b[?1003h"),
    }
    use vt100::MouseProtocolEncoding as E;
    match s.mouse_protocol_encoding() {
        E::Default => {}
        E::Utf8 => out.extend_from_slice(b"\x1b[?1005h"),
        E::Sgr => out.extend_from_slice(b"\x1b[?1006h"),
    }
    out
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
            // Newline-aligned — replay is safe, keep using the log.
        } else {
            // No newline within scan window; the prefix could be in the
            // middle of an escape sequence. Tell `send_dump` to fall back
            // to `contents_formatted()` for this session.
            log.truncated = true;
        }
    }
}

/// Background thread that samples process trees and pushes one
/// [`UsageReport`] per second to the active client while
/// `registry.usage_subscribed` is true.
fn spawn_usage_sampler(shared: Arc<Shared>) {
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
                let mut reg = shared.registry.lock().unwrap();
                let active = shared.active.load_full();
                if !reg.usage_subscribed || active.is_none() {
                    reg.usage_thread_alive = false;
                    tracing::debug!("usage sampler exiting");
                    return;
                }
                (
                    active.unwrap(),
                    reg.sessions
                        .values()
                        .map(|s| (s.meta.clone(), s.child_pid))
                        .collect::<Vec<_>>(),
                    reg.active_client_pid,
                )
            };
            let (chan, sessions_snapshot, _client_pid) = snapshot;

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

            let report = UsageReport {
                sessions,
                supervisor,
                // The client process lives on a different host for remote
                // supervisors; the client samples its own pid locally now.
                client: None,
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                cpu_count,
            };
            // try_send: usage frames are best-effort. If the client can't
            // keep up with even periodic samples, skipping is the right call.
            let _ = chan.tx.try_send(SupervisorMsg::Usage(report));
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
        // Newline-aligned eviction keeps the log safe to replay, so the
        // truncated flag stays false.
        assert!(
            !log.truncated,
            "newline-aligned eviction should not flip truncated"
        );
        // Head should now be at the start of a fresh `line` token.
        assert_eq!(
            &log.buf.iter().copied().take(4).collect::<Vec<_>>(),
            b"line"
        );
    }

    #[test]
    fn push_output_log_marks_truncated_without_newline() {
        let mut log = OutputLog::default();
        // No newlines anywhere — eviction can't align, so truncated must flip.
        let chunk = vec![b'x'; OUTPUT_LOG_CAP + 1024];
        push_output_log(&mut log, &chunk);
        assert!(log.buf.len() <= OUTPUT_LOG_CAP);
        assert!(log.truncated);
    }

    #[test]
    fn mode_prelude_emits_alt_screen_and_sgr_mouse() {
        // Synthesise a parser that's been told to enter alt-screen with SGR
        // mouse capture; the prelude should reflect both.
        let mut p = vt100::Parser::new(24, 80, 100);
        p.process(b"\x1b[?1049h\x1b[?1006h\x1b[?1000h");
        let prelude = mode_prelude(&p);
        assert!(
            prelude
                .windows(b"\x1b[?1049h".len())
                .any(|w| w == b"\x1b[?1049h"),
            "missing alt-screen toggle: {prelude:?}"
        );
        assert!(
            prelude
                .windows(b"\x1b[?1006h".len())
                .any(|w| w == b"\x1b[?1006h"),
            "missing SGR mouse encoding: {prelude:?}"
        );
        assert!(
            prelude
                .windows(b"\x1b[?1000h".len())
                .any(|w| w == b"\x1b[?1000h"),
            "missing press/release mouse mode: {prelude:?}"
        );
    }

    #[test]
    fn mode_prelude_empty_when_no_modes_active() {
        let p = vt100::Parser::new(24, 80, 100);
        assert!(mode_prelude(&p).is_empty());
    }

    #[test]
    fn list_dir_lists_subdirs_and_flags_repos() {
        let base = std::env::temp_dir().join(format!("imbuia-listdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("plain")).unwrap();
        std::fs::create_dir_all(base.join(".hidden")).unwrap();
        std::fs::write(base.join("file.txt"), b"x").unwrap();

        let ok = list_dir(Some(base.clone())).unwrap();
        let OpOk::DirListing { entries, .. } = ok else {
            panic!("expected DirListing");
        };
        // Dirs only, dotdirs hidden, sorted.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["plain", "repo"]);
        let repo = entries.iter().find(|e| e.name == "repo").unwrap();
        assert!(repo.is_repo);
        assert!(!entries.iter().find(|e| e.name == "plain").unwrap().is_repo);

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn expand_user_resolves_tilde() {
        // Uses the process HOME; just assert the prefix is replaced (not literal ~).
        let expanded = expand_user(std::path::Path::new("~/sub"));
        assert!(!expanded.to_string_lossy().starts_with('~'));
        assert!(expanded.ends_with("sub"));
        // Non-tilde paths pass through unchanged.
        assert_eq!(
            expand_user(std::path::Path::new("/abs/x")),
            std::path::PathBuf::from("/abs/x")
        );
    }
}
