//! Client-side supervisor connection: probes/spawns the supervisor, attaches,
//! and exposes [`ProxySession`] (the `Session` trait impl that ships
//! keystroke/mouse/resize commands over the socket).

use crate::app::Action;
use crate::input;
use crate::ipc::{
    self, ClientMsg, HandshakeReq, HandshakeResp, OpOk, OpRequest, OpResult, PROTOCOL_VERSION,
    SessionId, SessionMeta, SupervisorMsg, WorktreeEntry,
};
use crate::session::Session;
use crate::{config, transport};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio_rustls::TlsConnector;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Boxed read/write halves of whatever transport the client attached over (a
/// local `UnixStream` or a `TlsStream<TcpStream>`). Both impl `AsyncRead` /
/// `AsyncWrite`, so the framing + handshake code is transport-agnostic.
type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Per-session local state held client-side. The vt100 parser is fed by
/// frames arriving from the supervisor (Dump on attach, Delta as live PTY
/// output flows).
pub(crate) struct ProxySession {
    /// Client-global id (unique across all supervisors); the key in
    /// `AppState.sessions` and `Worktree.sessions`. Returned by `Session::id`.
    global_id: SessionId,
    /// Per-supervisor wire id (what the owning supervisor knows this session
    /// as). Used on every outbound `ClientMsg`.
    local_id: SessionId,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Inner app's keyboard-input protocol, inferred from its PTY output (vt100
    /// doesn't track it). Fed by the reader thread, read by `write_key` to pick
    /// the encoding for modified functional keys (Shift+Enter, …).
    kbd: Arc<Mutex<input::KbdTracker>>,
    tx: ClientTx,
    notify: Arc<Notify>,
}

/// Channel-backed handle to the supervisor: enqueues `ClientMsg` onto an
/// unbounded queue drained by a dedicated async writer task. Crucially,
/// `UnboundedSender::send` is synchronous and non-blocking, so reducer/runtime
/// call sites stay unchanged even though the writer is now async.
type ClientTx = mpsc::UnboundedSender<ClientMsg>;

#[derive(Copy, Clone, Debug)]
struct PendingSpawn {
    dest: (usize, usize),
    rows: u16,
    cols: u16,
}

/// Continuation context for an in-flight `Op` request. The supervisor's
/// `OpResult` is opaque about *why* the op ran; this carries the indices/flags
/// needed to rebuild the same `Action` the old client-side threads built, so
/// the reducer is unchanged.
enum PendingOp {
    OpenProject {
        supervisor: crate::app::SupervisorId,
        setup_script: Option<String>,
        import_existing: bool,
    },
    ImportWorktrees {
        project_idx: usize,
    },
    AddWorktree {
        project_idx: usize,
    },
    RemoveWorktree {
        project_idx: usize,
        worktree_idx: usize,
    },
    FetchPr {
        project_idx: usize,
    },
    /// Open-project directory browser listing.
    ListDir,
}

/// Build a local `Worktree` from a wire entry, mirroring the name-fallback the
/// old `runtime` import path used (branch name, else the directory basename).
fn worktree_from_entry(e: WorktreeEntry) -> crate::app::Worktree {
    crate::app::Worktree {
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
    }
}

/// Turn an `OpResult` + its pending continuation into the `Action` to post.
/// Returns `None` when the response kind doesn't match the request (a bug, but
/// we log and drop rather than panic the reader thread).
fn op_result_to_action(pending: PendingOp, result: OpResult) -> Option<Action> {
    match result {
        Err(e) => Some(match pending {
            PendingOp::OpenProject { .. } => Action::OperationFailed(format!("open: {e}")),
            PendingOp::ImportWorktrees { .. } => Action::OperationFailed(format!("import: {e}")),
            PendingOp::AddWorktree { .. } => Action::OperationFailed(format!("worktree: {e}")),
            PendingOp::RemoveWorktree { .. } => {
                Action::OperationFailed(format!("remove worktree: {e}"))
            }
            PendingOp::FetchPr { project_idx } => Action::PrFetchFailed {
                project_idx,
                message: e,
            },
            PendingOp::ListDir => Action::OperationFailed(format!("list dir: {e}")),
        }),
        Ok(ok) => match (pending, ok) {
            (
                PendingOp::OpenProject {
                    supervisor,
                    setup_script,
                    import_existing,
                },
                OpOk::Validated(info),
            ) => Some(Action::ProjectValidated {
                supervisor,
                canonical_path: info.canonical_path,
                repo_name: info.repo_name,
                head_branch: info.head_branch,
                setup_script,
                import_existing,
            }),
            (
                PendingOp::ListDir,
                OpOk::DirListing {
                    dir,
                    parent,
                    entries,
                },
            ) => Some(Action::DirListed {
                dir,
                parent,
                entries,
            }),
            (PendingOp::ImportWorktrees { project_idx }, OpOk::Worktrees(entries)) => {
                Some(Action::WorktreesImported {
                    project_idx,
                    entries: entries.into_iter().map(worktree_from_entry).collect(),
                })
            }
            (PendingOp::AddWorktree { project_idx }, OpOk::WorktreeAdded(entry)) => {
                Some(Action::WorktreeAdded {
                    project_idx,
                    worktree: worktree_from_entry(entry),
                })
            }
            (
                PendingOp::RemoveWorktree {
                    project_idx,
                    worktree_idx,
                },
                OpOk::WorktreeRemoved,
            ) => Some(Action::WorktreeRemoved {
                project_idx,
                worktree_idx,
            }),
            (PendingOp::FetchPr { project_idx }, OpOk::PrStatuses(statuses)) => {
                Some(Action::PrStatusesFetched {
                    project_idx,
                    statuses,
                })
            }
            (_, _) => {
                tracing::warn!("OpResult kind did not match pending op; dropping");
                None
            }
        },
    }
}

impl ProxySession {
    fn bump_scrollback(&self, delta: i32) {
        let mut p = self.parser.lock().expect("parser poisoned");
        let cur = p.screen().scrollback() as i32;
        let next = (cur + delta).max(0) as usize;
        p.screen_mut().set_scrollback(next);
        drop(p);
        self.notify.notify_one();
    }

    fn send(&self, msg: ClientMsg) -> io::Result<()> {
        self.tx
            .send(msg)
            .map_err(|_| io::Error::other("supervisor disconnected"))
    }
}

impl Session for ProxySession {
    fn id(&self) -> SessionId {
        self.global_id
    }

    fn write_key(&self, key: KeyEvent) -> io::Result<()> {
        let app_cursor = {
            let p = self.parser.lock().expect("parser poisoned");
            p.screen().application_cursor()
        };
        let kbd = self.kbd.lock().expect("kbd tracker poisoned").encoding();
        let bytes = input::encode_key(key, app_cursor, kbd);
        if bytes.is_empty() {
            return Ok(());
        }
        self.send(ClientMsg::WriteBytes {
            id: self.local_id,
            bytes,
        })
    }

    fn write_paste(&self, text: &str) -> io::Result<()> {
        // Wrap in bracketed-paste markers only when the *inner* app has them
        // enabled (DECSET 2004); otherwise dumb apps would see literal
        // `\x1b[200~` garbage. crossterm already stripped the outer terminal's
        // markers, so this is purely the forward decision.
        let bracketed = {
            let p = self.parser.lock().expect("parser poisoned");
            p.screen().bracketed_paste()
        };

        // Split into chunks so neither (a) any single WriteBytes frame is huge
        // (the supervisor processes them on its command loop) nor (b) any
        // single PTY `write_all` parks the supervisor on kernel-buffer
        // back-pressure. The markers (if any) wrap the *whole* sequence, so the
        // receiving app still sees one paste.
        const CHUNK: usize = 16 * 1024;
        let body = text.as_bytes();
        let mut off = 0;
        let mut first = true;
        loop {
            let end = (off + CHUNK).min(body.len());
            let is_last = end >= body.len();
            let mut bytes = Vec::with_capacity((end - off) + 6);
            if first && bracketed {
                bytes.extend_from_slice(b"\x1b[200~");
            }
            bytes.extend_from_slice(&body[off..end]);
            if is_last && bracketed {
                bytes.extend_from_slice(b"\x1b[201~");
            }
            self.send(ClientMsg::WriteBytes {
                id: self.local_id,
                bytes,
            })?;
            first = false;
            off = end;
            if is_last {
                break;
            }
        }
        Ok(())
    }

    fn write_mouse(&self, ev: MouseEvent) -> io::Result<()> {
        let (mode, enc, app_cursor, alt_screen) = {
            let p = self.parser.lock().expect("parser poisoned");
            let s = p.screen();
            (
                s.mouse_protocol_mode(),
                s.mouse_protocol_encoding(),
                s.application_cursor(),
                s.alternate_screen(),
            )
        };
        let is_scroll = matches!(
            ev.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        );
        let shift_bypass = is_scroll && ev.modifiers.contains(KeyModifiers::SHIFT);

        if !shift_bypass {
            let bytes = encode_mouse(ev, mode, enc);
            if !bytes.is_empty() {
                return self.send(ClientMsg::WriteBytes {
                    id: self.local_id,
                    bytes,
                });
            }
        }
        if !shift_bypass && alt_screen && is_scroll {
            let arrow_bytes = scroll_as_arrows(ev.kind, app_cursor, 3);
            if !arrow_bytes.is_empty() {
                return self.send(ClientMsg::WriteBytes {
                    id: self.local_id,
                    bytes: arrow_bytes,
                });
            }
            return Ok(());
        }
        match ev.kind {
            MouseEventKind::ScrollUp => self.bump_scrollback(3),
            MouseEventKind::ScrollDown => self.bump_scrollback(-3),
            _ => {}
        }
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        // Poisoning the parser means another thread panicked while drawing
        // or feeding bytes — silently swallowing the resize would desync
        // the local vt100 view from the supervisor forever. Surface it.
        let mut p = self
            .parser
            .lock()
            .map_err(|_| anyhow!("local vt100 parser poisoned"))?;
        p.screen_mut().set_size(rows, cols);
        drop(p);
        self.send(ClientMsg::Resize {
            id: self.local_id,
            rows,
            cols,
        })?;
        Ok(())
    }

    fn kill(&self) {
        let _ = self.send(ClientMsg::Kill { id: self.local_id });
    }

    fn parser(&self) -> &Mutex<vt100::Parser> {
        &self.parser
    }
}

/// The client end of one supervisor connection. Owns the writer side of the
/// socket and a registry of [`ProxySession`]s (keyed by per-supervisor wire id)
/// for incoming frame routing.
pub struct SupervisorClient {
    /// Which supervisor this connection talks to (for tagging Actions so the
    /// reducer/usage popup can attribute output to the right host).
    sup_id: crate::app::SupervisorId,
    tx: ClientTx,
    /// Keyed by per-supervisor wire id (what frames carry).
    sessions: Arc<Mutex<HashMap<SessionId, Arc<ProxySession>>>>,
    /// Mints client-global session ids, shared across all supervisor
    /// connections so ids never collide between hosts.
    global_ids: Arc<AtomicU64>,
    pending_spawns: Arc<Mutex<HashMap<u64, PendingSpawn>>>,
    /// In-flight `Op` continuations, keyed by `request_id` (shares the
    /// `next_request` counter with `pending_spawns`).
    pending_ops: Arc<Mutex<HashMap<u64, PendingOp>>>,
    notify: Arc<Notify>,
    next_request: AtomicU64,
    /// Resumed sessions reported by the supervisor at handshake. Empty after
    /// `drain_initial_sessions` is called.
    initial_sessions: Mutex<Vec<SessionMeta>>,
}

impl SupervisorClient {
    /// Take the list of pre-existing sessions reported during the handshake,
    /// leaving the slot empty.
    pub fn drain_initial_sessions(&self) -> Vec<SessionMeta> {
        std::mem::take(&mut *self.initial_sessions.lock().unwrap())
    }

    /// Construct (and register) a `ProxySession` for an existing supervisor
    /// session — used on attach to re-bind resumed sessions to local tabs.
    pub fn adopt(&self, meta: &SessionMeta) -> Arc<ProxySession> {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(meta.rows, meta.cols, 10_000)));
        let global_id = self.global_ids.fetch_add(1, Ordering::Relaxed);
        let proxy = Arc::new(ProxySession {
            global_id,
            local_id: meta.id,
            parser,
            kbd: Arc::new(Mutex::new(input::KbdTracker::default())),
            tx: self.tx.clone(),
            notify: Arc::clone(&self.notify),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(meta.id, Arc::clone(&proxy));
        let _ = self.tx.send(ClientMsg::Attach { id: meta.id });
        proxy
    }

    /// This connection's supervisor id (for tagging Actions client-side).
    pub fn supervisor_id(&self) -> crate::app::SupervisorId {
        self.sup_id
    }

    /// Queue a spawn request. The reader task will post
    /// `Action::SessionSpawned` once the supervisor responds.
    #[allow(clippy::too_many_arguments)]
    pub fn request_spawn(
        &self,
        project_slug: String,
        worktree_name: String,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        initial_command: Option<String>,
        dest: (usize, usize),
    ) -> Result<()> {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.pending_spawns
            .lock()
            .unwrap()
            .insert(request_id, PendingSpawn { dest, rows, cols });
        let msg = ClientMsg::Spawn {
            request_id,
            project_slug,
            worktree_name,
            rows,
            cols,
            cwd,
            initial_command,
        };
        self.tx
            .send(msg)
            .map_err(|_| anyhow!("supervisor disconnected"))?;
        Ok(())
    }

    pub fn kill(&self, id: SessionId) {
        let _ = self.tx.send(ClientMsg::Kill { id });
    }

    pub fn shutdown_supervisor(&self) {
        let _ = self.tx.send(ClientMsg::Shutdown);
    }

    pub fn subscribe_usage(&self) {
        let _ = self.tx.send(ClientMsg::SubscribeUsage);
    }

    pub fn unsubscribe_usage(&self) {
        let _ = self.tx.send(ClientMsg::UnsubscribeUsage);
    }

    /// Allocate a `request_id`, record the continuation, and ship the `Op`.
    /// The reader thread turns the eventual `OpResult` back into an `Action`.
    fn send_op(&self, pending: PendingOp, req: OpRequest) -> Result<()> {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.pending_ops.lock().unwrap().insert(request_id, pending);
        self.tx
            .send(ClientMsg::Op { request_id, req })
            .map_err(|_| anyhow!("supervisor disconnected"))?;
        Ok(())
    }

    /// Validate a path as a git repo (supervisor-side). On success the reader
    /// posts `Action::ProjectValidated`; the reducer then derives the slug and
    /// persists the project (config logic stays client-side).
    pub fn request_open_project(
        &self,
        supervisor: crate::app::SupervisorId,
        repo_path: PathBuf,
        setup_script: Option<String>,
        import_existing: bool,
    ) -> Result<()> {
        self.send_op(
            PendingOp::OpenProject {
                supervisor,
                setup_script,
                import_existing,
            },
            OpRequest::Validate { repo_path },
        )
    }

    /// List a directory on this supervisor for the open-project browser.
    pub fn request_list_dir(&self, path: Option<PathBuf>) -> Result<()> {
        self.send_op(PendingOp::ListDir, OpRequest::ListDir { path })
    }

    pub fn request_import_worktrees(&self, project_idx: usize, repo_path: PathBuf) -> Result<()> {
        self.send_op(
            PendingOp::ImportWorktrees { project_idx },
            OpRequest::ListWorktrees { repo_path },
        )
    }

    pub fn request_add_worktree(
        &self,
        project_idx: usize,
        repo_path: PathBuf,
        branch: String,
    ) -> Result<()> {
        self.send_op(
            PendingOp::AddWorktree { project_idx },
            OpRequest::WorktreeAdd { repo_path, branch },
        )
    }

    pub fn request_remove_worktree(
        &self,
        project_idx: usize,
        worktree_idx: usize,
        repo_path: PathBuf,
        dest_path: PathBuf,
        branch: Option<String>,
    ) -> Result<()> {
        self.send_op(
            PendingOp::RemoveWorktree {
                project_idx,
                worktree_idx,
            },
            OpRequest::WorktreeRemove {
                repo_path,
                dest_path,
                branch,
            },
        )
    }

    pub fn request_fetch_pr(
        &self,
        project_idx: usize,
        repo_path: PathBuf,
        worktrees: Vec<(usize, PathBuf)>,
    ) -> Result<()> {
        self.send_op(
            PendingOp::FetchPr { project_idx },
            OpRequest::FetchPr {
                repo_path,
                worktrees,
            },
        )
    }
}

/// All live supervisor connections: the always-present local one plus every
/// configured remote (each `Some` when reachable, `None` when it failed to
/// connect at startup). The reducer-facing [`SupervisorDirectory`] is derived
/// from this.
pub struct Supervisors {
    entries: Vec<SupervisorEntry>,
    /// Mints client-global session ids, shared across every connection (so a
    /// late/background (re)connect keeps allocating from the same space).
    global_ids: Arc<AtomicU64>,
}

struct SupervisorEntry {
    id: crate::app::SupervisorId,
    name: String,
    /// Remote URL (`host:port`) to (re)dial; `None` for the local supervisor.
    url: Option<String>,
    client: Option<Arc<SupervisorClient>>,
}

impl Supervisors {
    /// The connected client for `id`, or `None` if unconfigured/unreachable.
    pub fn get(&self, id: crate::app::SupervisorId) -> Option<&Arc<SupervisorClient>> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.client.as_ref())
    }

    pub fn name_of(&self, id: crate::app::SupervisorId) -> &str {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.name.as_str())
            .unwrap_or("local")
    }

    pub fn is_connected(&self, id: crate::app::SupervisorId) -> bool {
        self.get(id).is_some()
    }

    /// The configured URL for a remote `id`, if any (`None` for local/unknown).
    pub fn url_of(&self, id: crate::app::SupervisorId) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.url.clone())
    }

    /// Shared session-id counter, for handing to a background (re)connect.
    pub fn global_ids(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.global_ids)
    }

    /// Install a freshly-connected client (after a background dial / reconnect).
    pub fn set_client(&mut self, id: crate::app::SupervisorId, client: Arc<SupervisorClient>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.client = Some(client);
        }
    }

    /// Drop a supervisor's connection (it died / detached); reconnect re-dials.
    pub fn mark_disconnected(&mut self, id: crate::app::SupervisorId) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.client = None;
        }
    }

    /// `(id, url)` for every configured remote not currently connected — the
    /// set the runtime dials in the background at startup and on `:reconnect`.
    pub fn disconnected_remotes(&self) -> Vec<(crate::app::SupervisorId, String)> {
        self.entries
            .iter()
            .filter(|e| e.client.is_none())
            .filter_map(|e| e.url.clone().map(|u| (e.id, u)))
            .collect()
    }

    /// Iterate every currently-connected client (for usage fan-out, rebind).
    pub fn connected(&self) -> impl Iterator<Item = &Arc<SupervisorClient>> {
        self.entries.iter().filter_map(|e| e.client.as_ref())
    }

    /// id↔name + connected-set projection for `AppState`.
    pub fn directory(&self) -> crate::app::SupervisorDirectory {
        crate::app::SupervisorDirectory {
            entries: self
                .entries
                .iter()
                .map(|e| (e.id, e.name.clone()))
                .collect(),
            connected: self
                .entries
                .iter()
                .filter(|e| e.client.is_some())
                .map(|e| e.id)
                .collect(),
        }
    }
}

/// Connect the local supervisor (auto-spawning if needed) and seed an entry for
/// every configured remote as **disconnected**. The local connection is the
/// only one required to start; remotes are dialed afterwards in the background
/// (see `runtime`) so an unreachable host never blocks or crashes startup —
/// each just stays disconnected (grayed in the sidebar) until it connects or
/// the user runs `:reconnect`.
pub async fn connect_all(
    global: &config::GlobalConfig,
    notify: Arc<Notify>,
    action_tx: mpsc::Sender<Action>,
) -> Result<Supervisors> {
    use crate::app::{LOCAL, SupervisorId};
    let global_ids = Arc::new(AtomicU64::new(1));

    // Local (required).
    let (rd, wr) = connect_local().await?;
    let local = handshake(
        rd,
        wr,
        LOCAL,
        Arc::clone(&global_ids),
        Arc::clone(&notify),
        action_tx.clone(),
        false, // local UDS: no compression
    )
    .await?;
    let mut entries = vec![SupervisorEntry {
        id: LOCAL,
        name: "local".into(),
        url: None,
        client: Some(local),
    }];

    // Remotes: seed disconnected entries (numbered in config order). The
    // runtime dials each in the background once the UI is up.
    for (i, (name, cfg)) in global.effective_remotes().into_iter().enumerate() {
        entries.push(SupervisorEntry {
            id: SupervisorId(i as u32 + 1),
            name,
            url: Some(cfg.url),
            client: None,
        });
    }

    Ok(Supervisors {
        entries,
        global_ids,
    })
}

/// Connect + handshake one remote, with an overall timeout so a dead host
/// doesn't stall the dial. Used by the runtime's background startup connects
/// and by `:reconnect`.
pub(crate) async fn connect_remote_handshake(
    config_dir: &Path,
    url: &str,
    sup_id: crate::app::SupervisorId,
    global_ids: Arc<AtomicU64>,
    notify: Arc<Notify>,
    action_tx: mpsc::Sender<Action>,
) -> Result<Arc<SupervisorClient>> {
    let (rd, wr) = tokio::time::timeout(Duration::from_secs(5), connect_remote(config_dir, url))
        .await
        .context("remote connect timed out")??;
    // Remote link: compress large outbound frames (big pastes, etc.).
    handshake(rd, wr, sup_id, global_ids, notify, action_tx, true).await
}

/// Connect to (or spawn + wait for) the local Unix-socket supervisor.
async fn connect_local() -> Result<(BoxRead, BoxWrite)> {
    let sock = ipc::resolve_socket_path();
    let stream = match UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::ConnectionRefused =>
        {
            spawn_supervisor()?;
            wait_for_socket(&sock, Duration::from_secs(2)).await?
        }
        Err(e) => return Err(anyhow!("connect supervisor socket: {e}")),
    };
    let (rd, wr) = tokio::io::split(stream);
    Ok((Box::new(rd), Box::new(wr)))
}

/// Connect to a remote supervisor over TCP wrapped in mutually-authenticated
/// TLS. The supervisor's key is pinned TOFU in `known_hosts`; our identity cert
/// must be in its `authorized_keys` or the handshake is rejected.
async fn connect_remote(config_dir: &Path, url: &str) -> Result<(BoxRead, BoxWrite)> {
    let (host, _port) = transport::split_host_port(url)?;
    let identity = transport::load_or_create_identity(config_dir)?;
    tracing::info!(fingerprint = %identity.fingerprint, %url, "client TLS identity");
    let connector = TlsConnector::from(transport::client_config(&identity, host, config_dir)?);
    let tcp = TcpStream::connect(url)
        .await
        .with_context(|| format!("connecting to remote supervisor {url}"))?;
    // Disable Nagle: interactive keystrokes are tiny writes, and Nagle would
    // hold them up to ~40ms waiting to coalesce — the #1 remote typing lag.
    if let Err(e) = tcp.set_nodelay(true) {
        tracing::warn!("set_nodelay on remote supervisor socket failed: {e}");
    }
    let server_name = transport::server_name(host)?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake with remote supervisor")?;
    let (rd, wr) = tokio::io::split(tls);
    Ok((Box::new(rd), Box::new(wr)))
}

async fn handshake(
    mut rd: BoxRead,
    mut wr: BoxWrite,
    sup_id: crate::app::SupervisorId,
    global_ids: Arc<AtomicU64>,
    notify: Arc<Notify>,
    action_tx: mpsc::Sender<Action>,
    // Compress large outbound frames — `true` for remote (TCP) connections,
    // `false` for the local UDS (pointless there).
    compress: bool,
) -> Result<Arc<SupervisorClient>> {
    // Bounded handshake: a half-dead supervisor that accepts but never replies
    // must not hang the TUI.
    let hs = Duration::from_secs(5);
    let req = HandshakeReq {
        protocol: PROTOCOL_VERSION,
        client_pid: std::process::id(),
    };
    tokio::time::timeout(hs, ipc::write_frame_async(&mut wr, &req, false))
        .await
        .context("handshake write timeout")??;
    let resp: HandshakeResp = tokio::time::timeout(hs, ipc::read_frame_async(&mut rd))
        .await
        .context("handshake read timeout (supervisor unresponsive?)")??;
    let sessions = match resp {
        HandshakeResp::Ok {
            supervisor_pid: _,
            sessions,
        } => sessions,
        HandshakeResp::VersionMismatch {
            supervisor_protocol,
        } => {
            bail!(
                "supervisor protocol v{supervisor_protocol}, client v{PROTOCOL_VERSION} — \
                 run `:restart-supervisor` from another imbuia"
            );
        }
    };

    // Writer task: drains an unbounded queue and writes frames. Runtime code
    // only ever does `tx.send(msg)` (O(1), non-blocking), so paste floods and a
    // back-pressured supervisor can't freeze the TUI.
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = ipc::write_frame_async(&mut wr, &msg, compress).await {
                tracing::warn!("supervisor write loop exiting: {e}");
                break;
            }
        }
    });

    let client = Arc::new(SupervisorClient {
        sup_id,
        tx,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        global_ids,
        pending_spawns: Arc::new(Mutex::new(HashMap::new())),
        pending_ops: Arc::new(Mutex::new(HashMap::new())),
        notify,
        next_request: AtomicU64::new(1),
        initial_sessions: Mutex::new(sessions),
    });

    spawn_reader(Arc::clone(&client), rd, action_tx);
    Ok(client)
}

fn spawn_reader(client: Arc<SupervisorClient>, mut rd: BoxRead, action_tx: mpsc::Sender<Action>) {
    tokio::spawn(async move {
        loop {
            let msg: SupervisorMsg = match ipc::read_frame_async(&mut rd).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("supervisor read ended: {e}");
                    let _ = action_tx
                        .send(Action::SupervisorLost(client.sup_id, format!("{e}")))
                        .await;
                    return;
                }
            };
            match msg {
                SupervisorMsg::Spawned { request_id, id } => {
                    // Bind in a statement so the MutexGuard drops before the
                    // later `.await` (a held std guard isn't Send).
                    let pending = client.pending_spawns.lock().unwrap().remove(&request_id);
                    if let Some(pending) = pending {
                        let parser = Arc::new(Mutex::new(vt100::Parser::new(
                            pending.rows,
                            pending.cols,
                            10_000,
                        )));
                        // Mint a client-global id so this session never collides
                        // with another supervisor's id space.
                        let global_id = client.global_ids.fetch_add(1, Ordering::Relaxed);
                        let proxy = Arc::new(ProxySession {
                            global_id,
                            local_id: id,
                            parser,
                            kbd: Arc::new(Mutex::new(input::KbdTracker::default())),
                            tx: client.tx.clone(),
                            notify: Arc::clone(&client.notify),
                        });
                        client
                            .sessions
                            .lock()
                            .unwrap()
                            .insert(id, Arc::clone(&proxy));
                        let _ = action_tx
                            .send(Action::SessionSpawned {
                                session: proxy as Arc<dyn Session>,
                                dest: pending.dest,
                            })
                            .await;
                    }
                }
                SupervisorMsg::SpawnFailed { request_id, error } => {
                    client.pending_spawns.lock().unwrap().remove(&request_id);
                    let _ = action_tx
                        .send(Action::OperationFailed(format!("spawn: {error}")))
                        .await;
                }
                SupervisorMsg::OutputDump { id, bytes }
                | SupervisorMsg::OutputDelta { id, bytes } => {
                    // Routed by per-supervisor wire id (the map's key).
                    let sess = client.sessions.lock().unwrap().get(&id).cloned();
                    if let Some(sess) = sess {
                        if let Ok(mut p) = sess.parser.lock() {
                            p.process(&bytes);
                        }
                        // Sniff the same bytes for keyboard-protocol negotiation
                        // so write_key knows how to encode modified keys.
                        if let Ok(mut k) = sess.kbd.lock() {
                            k.feed(&bytes);
                        }
                        client.notify.notify_one();
                    }
                }
                SupervisorMsg::Exited { id } => {
                    // Translate wire id → client-global id for AppState.
                    let global = client
                        .sessions
                        .lock()
                        .unwrap()
                        .remove(&id)
                        .map(|p| p.global_id);
                    if let Some(global) = global {
                        let _ = action_tx.send(Action::SessionExited(global)).await;
                    }
                }
                SupervisorMsg::Detached { reason } => {
                    let _ = action_tx
                        .send(Action::SupervisorLost(client.sup_id, reason))
                        .await;
                    return;
                }
                SupervisorMsg::Usage(report) => {
                    let _ = action_tx
                        .send(Action::UsageReceived(client.sup_id, report))
                        .await;
                }
                SupervisorMsg::OpResult { request_id, result } => {
                    let pending = client.pending_ops.lock().unwrap().remove(&request_id);
                    let Some(pending) = pending else {
                        tracing::warn!(request_id, "OpResult for unknown request; ignoring");
                        continue;
                    };
                    if let Some(action) = op_result_to_action(pending, result) {
                        let _ = action_tx.send(action).await;
                    }
                }
            }
        }
    });
}

fn spawn_supervisor() -> Result<()> {
    use nix::sys::wait::waitpid;
    use nix::unistd::{ForkResult, fork, setsid};

    let exe = std::env::current_exe().context("current_exe")?;

    // SAFETY: standard double-fork daemon trick. We only call async-signal-safe
    // syscalls in the children; no allocations between fork and exec.
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            waitpid(child, None).ok();
            Ok(())
        }
        ForkResult::Child => {
            // Detach from controlling terminal.
            let _ = setsid();
            match unsafe { fork() }? {
                ForkResult::Parent { .. } => std::process::exit(0),
                ForkResult::Child => {
                    redirect_std_streams();
                    let err = std::process::Command::new(exe)
                        .arg("--supervisor")
                        .exec_replacement();
                    eprintln!("imbuia: failed to exec supervisor: {err}");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn redirect_std_streams() {
    use std::os::fd::AsRawFd;
    let sock = ipc::resolve_socket_path();
    let log = ipc::supervisor_log_path(&sock);
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let dev_null = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .ok();
    let logf = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log)
        .ok();
    unsafe {
        if let Some(f) = &dev_null {
            libc::dup2(f.as_raw_fd(), libc::STDIN_FILENO);
        }
        if let Some(f) = &logf {
            libc::dup2(f.as_raw_fd(), libc::STDOUT_FILENO);
            libc::dup2(f.as_raw_fd(), libc::STDERR_FILENO);
        }
    }
}

trait CommandExt {
    fn exec_replacement(&mut self) -> std::io::Error;
}

impl CommandExt for std::process::Command {
    fn exec_replacement(&mut self) -> std::io::Error {
        use std::os::unix::process::CommandExt as _;
        self.exec()
    }
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let start = Instant::now();
    loop {
        if let Ok(s) = UnixStream::connect(path).await {
            return Ok(s);
        }
        if start.elapsed() > timeout {
            bail!("timeout waiting for supervisor socket {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// --- mouse / scroll encoding (copied from session.rs; sole owner now that
//      `PtySession` is gone) -----------------------------------------------

fn scroll_as_arrows(kind: MouseEventKind, app_cursor: bool, lines: usize) -> Vec<u8> {
    let seq: &[u8] = match (kind, app_cursor) {
        (MouseEventKind::ScrollUp, false) => b"\x1b[A",
        (MouseEventKind::ScrollDown, false) => b"\x1b[B",
        (MouseEventKind::ScrollUp, true) => b"\x1bOA",
        (MouseEventKind::ScrollDown, true) => b"\x1bOB",
        _ => return Vec::new(),
    };
    seq.repeat(lines)
}

fn encode_mouse(ev: MouseEvent, mode: MouseProtocolMode, enc: MouseProtocolEncoding) -> Vec<u8> {
    use crossterm::event::MouseButton;
    if matches!(mode, MouseProtocolMode::None) {
        return Vec::new();
    }
    let mouse_button_code = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut button, drag, release) = match ev.kind {
        MouseEventKind::Down(b) => (mouse_button_code(b), false, false),
        MouseEventKind::Up(b) => (mouse_button_code(b), false, true),
        MouseEventKind::Drag(b) => (mouse_button_code(b), true, false),
        MouseEventKind::Moved => (3, true, false),
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
    };
    let allowed = match (mode, &ev.kind) {
        (MouseProtocolMode::Press, MouseEventKind::Down(_)) => true,
        (
            MouseProtocolMode::Press,
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight,
        ) => true,
        (MouseProtocolMode::PressRelease, k) => {
            !matches!(k, MouseEventKind::Drag(_) | MouseEventKind::Moved)
        }
        (MouseProtocolMode::ButtonMotion, k) => !matches!(k, MouseEventKind::Moved),
        (MouseProtocolMode::AnyMotion, _) => true,
        _ => false,
    };
    if !allowed {
        return Vec::new();
    }
    if drag {
        button += 32;
    }
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        button += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        button += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        button += 16;
    }
    let col = ev.column as u32 + 1;
    let row = ev.row as u32 + 1;
    match enc {
        MouseProtocolEncoding::Sgr => {
            let term = if release { 'm' } else { 'M' };
            format!("\x1b[<{};{};{}{}", button, col, row, term).into_bytes()
        }
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            let cb = if release { 3 } else { button };
            let cb_byte = (cb + 32).min(255) as u8;
            let cx_byte = (col + 32).min(255) as u8;
            let cy_byte = (row + 32).min(255) as u8;
            vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
        }
    }
}
