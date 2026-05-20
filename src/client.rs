//! Client-side supervisor connection: probes/spawns the supervisor, attaches,
//! and exposes [`ProxySession`] (the `Session` trait impl that ships
//! keystroke/mouse/resize commands over the socket).

use crate::app::Action;
use crate::input;
use crate::ipc::{
    self, ClientMsg, HandshakeReq, HandshakeResp, PROTOCOL_VERSION, SessionId, SessionMeta,
    SupervisorMsg,
};
use crate::session::Session;
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Per-session local state held client-side. The vt100 parser is fed by
/// frames arriving from the supervisor (Dump on attach, Delta as live PTY
/// output flows).
pub(crate) struct ProxySession {
    id: SessionId,
    parser: Arc<Mutex<vt100::Parser>>,
    tx: ClientTx,
    notify: Arc<Notify>,
}

/// Channel-backed handle to the supervisor: enqueues `ClientMsg` onto an
/// unbounded queue drained by a dedicated writer thread. Crucially, sending
/// never blocks the caller — even if the socket is back-pressured the
/// runtime task keeps making progress.
type ClientTx = std::sync::mpsc::Sender<ClientMsg>;

#[derive(Copy, Clone, Debug)]
struct PendingSpawn {
    dest: (usize, usize),
    rows: u16,
    cols: u16,
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
        self.id
    }

    fn write_key(&self, key: KeyEvent) -> io::Result<()> {
        let app_cursor = {
            let p = self.parser.lock().expect("parser poisoned");
            p.screen().application_cursor()
        };
        let bytes = input::encode_key(key, app_cursor);
        if bytes.is_empty() {
            return Ok(());
        }
        self.send(ClientMsg::WriteBytes { id: self.id, bytes })
    }

    fn write_paste(&self, text: &str) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        self.send(ClientMsg::WriteBytes { id: self.id, bytes })
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
                return self.send(ClientMsg::WriteBytes { id: self.id, bytes });
            }
        }
        if !shift_bypass && alt_screen && is_scroll {
            let arrow_bytes = scroll_as_arrows(ev.kind, app_cursor, 3);
            if !arrow_bytes.is_empty() {
                return self.send(ClientMsg::WriteBytes {
                    id: self.id,
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
            id: self.id,
            rows,
            cols,
        })?;
        Ok(())
    }

    fn parser(&self) -> &Mutex<vt100::Parser> {
        &self.parser
    }
}

/// The client end of the supervisor connection. Owns the writer side of the
/// socket and a registry of [`ProxySession`]s for incoming frame routing.
pub struct SupervisorClient {
    tx: ClientTx,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<ProxySession>>>>,
    pending_spawns: Arc<Mutex<HashMap<u64, PendingSpawn>>>,
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
        let proxy = Arc::new(ProxySession {
            id: meta.id,
            parser,
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
}

/// Attach to a running supervisor, or spawn one if none is alive, then
/// handshake. Returns the connected client + a handle to its reader thread.
pub fn connect_or_spawn(
    notify: Arc<Notify>,
    action_tx: mpsc::Sender<Action>,
) -> Result<Arc<SupervisorClient>> {
    let sock = ipc::resolve_socket_path();
    let stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::ConnectionRefused =>
        {
            spawn_supervisor()?;
            wait_for_socket(&sock, Duration::from_secs(2))?
        }
        Err(e) => return Err(anyhow!("connect supervisor socket: {e}")),
    };
    handshake(stream, notify, action_tx)
}

fn handshake(
    stream: UnixStream,
    notify: Arc<Notify>,
    action_tx: mpsc::Sender<Action>,
) -> Result<Arc<SupervisorClient>> {
    let mut reader = stream.try_clone().context("clone supervisor socket")?;
    let mut writer_stream = stream;

    // Bounded handshake: a half-dead supervisor that accepts but never
    // replies must not hang the TUI. Read/write timeouts are cleared after
    // the handshake completes — runtime IO is strictly blocking.
    let hs_timeout = Duration::from_secs(3);
    reader
        .set_read_timeout(Some(hs_timeout))
        .context("set handshake read timeout")?;
    writer_stream
        .set_write_timeout(Some(hs_timeout))
        .context("set handshake write timeout")?;
    let req = HandshakeReq {
        protocol: PROTOCOL_VERSION,
        client_pid: std::process::id(),
    };
    ipc::write_frame(&mut writer_stream, &req).context("handshake write")?;
    let resp: HandshakeResp =
        ipc::read_frame(&mut reader).context("handshake read (supervisor unresponsive?)")?;
    reader.set_read_timeout(None).ok();
    writer_stream.set_write_timeout(None).ok();
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

    // Spawn the writer thread: drains an mpsc queue and writes frames to
    // the socket. The runtime task only ever does `tx.send(msg)` which is
    // O(1) and never blocks — paste floods and back-pressured supervisor
    // writes can no longer freeze the TUI.
    let (tx, rx) = std::sync::mpsc::channel::<ClientMsg>();
    thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if let Err(e) = ipc::write_frame(&mut writer_stream, &msg) {
                tracing::warn!("supervisor write loop exiting: {e}");
                break;
            }
        }
    });

    let client = Arc::new(SupervisorClient {
        tx,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        pending_spawns: Arc::new(Mutex::new(HashMap::new())),
        notify,
        next_request: AtomicU64::new(1),
        initial_sessions: Mutex::new(sessions),
    });

    spawn_reader(Arc::clone(&client), reader, action_tx);
    Ok(client)
}

fn spawn_reader(
    client: Arc<SupervisorClient>,
    mut reader: UnixStream,
    action_tx: mpsc::Sender<Action>,
) {
    thread::spawn(move || {
        loop {
            let msg: SupervisorMsg = match ipc::read_frame(&mut reader) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("supervisor read ended: {e}");
                    let _ = action_tx.blocking_send(Action::SupervisorLost(format!("{e}")));
                    return;
                }
            };
            match msg {
                SupervisorMsg::Spawned { request_id, id } => {
                    if let Some(pending) = client.pending_spawns.lock().unwrap().remove(&request_id)
                    {
                        let parser = Arc::new(Mutex::new(vt100::Parser::new(
                            pending.rows,
                            pending.cols,
                            10_000,
                        )));
                        let proxy = Arc::new(ProxySession {
                            id,
                            parser,
                            tx: client.tx.clone(),
                            notify: Arc::clone(&client.notify),
                        });
                        client
                            .sessions
                            .lock()
                            .unwrap()
                            .insert(id, Arc::clone(&proxy));
                        let _ = action_tx.blocking_send(Action::SessionSpawned {
                            session: proxy as Arc<dyn Session>,
                            dest: pending.dest,
                        });
                    }
                }
                SupervisorMsg::SpawnFailed { request_id, error } => {
                    client.pending_spawns.lock().unwrap().remove(&request_id);
                    let _ =
                        action_tx.blocking_send(Action::OperationFailed(format!("spawn: {error}")));
                }
                SupervisorMsg::OutputDump { id, bytes }
                | SupervisorMsg::OutputDelta { id, bytes } => {
                    let sess = client.sessions.lock().unwrap().get(&id).cloned();
                    if let Some(sess) = sess {
                        if let Ok(mut p) = sess.parser.lock() {
                            p.process(&bytes);
                        }
                        client.notify.notify_one();
                    }
                }
                SupervisorMsg::Exited { id } => {
                    client.sessions.lock().unwrap().remove(&id);
                    let _ = action_tx.blocking_send(Action::SessionExited(id));
                }
                SupervisorMsg::Detached { reason } => {
                    let _ = action_tx.blocking_send(Action::SupervisorLost(reason));
                    return;
                }
                SupervisorMsg::Usage(report) => {
                    let _ = action_tx.blocking_send(Action::UsageReceived(report));
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

fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> Result<UnixStream> {
    let start = Instant::now();
    loop {
        if let Ok(s) = UnixStream::connect(path) {
            return Ok(s);
        }
        if start.elapsed() > timeout {
            bail!("timeout waiting for supervisor socket {}", path.display());
        }
        thread::sleep(Duration::from_millis(50));
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
