//! Shared wire types + framed transport for the imbuia client/supervisor split.
//!
//! Framing is a 4-byte big-endian length followed by a bincode-serialized
//! payload. Bincode v2 with the `serde` feature is used so the wire types can
//! piggy-back on existing `Serialize` derives.

use crate::app::PrStatus;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME: u32 = 8 * 1024 * 1024;

pub type SessionId = u64;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionMeta {
    pub id: SessionId,
    pub project_slug: String,
    pub worktree_name: String,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HandshakeReq {
    pub protocol: u32,
    pub client_pid: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum HandshakeResp {
    Ok {
        supervisor_pid: u32,
        sessions: Vec<SessionMeta>,
    },
    VersionMismatch {
        supervisor_protocol: u32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ClientMsg {
    Spawn {
        request_id: u64,
        project_slug: String,
        worktree_name: String,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        initial_command: Option<String>,
    },
    /// Already-encoded keystroke / mouse / paste bytes to write to the PTY.
    WriteBytes {
        id: SessionId,
        bytes: Vec<u8>,
    },
    Resize {
        id: SessionId,
        rows: u16,
        cols: u16,
    },
    Kill {
        id: SessionId,
    },
    /// Re-request a fresh `OutputDump` for the given session (e.g. after attach).
    Attach {
        id: SessionId,
    },
    /// Kill all PTYs and exit the supervisor cleanly.
    Shutdown,
    /// Start emitting `Usage` frames at ~1 Hz. Idempotent.
    SubscribeUsage,
    /// Stop emitting `Usage` frames.
    UnsubscribeUsage,
    /// A disk/process-touching operation the supervisor runs on the client's
    /// behalf (git, `gh`). Asynchronous request/response correlated by
    /// `request_id` — the supervisor replies with [`SupervisorMsg::OpResult`].
    /// Moving these supervisor-side keeps all filesystem access on the host
    /// that owns the repos (so the supervisor can one day run remotely while
    /// the client only keeps config).
    Op {
        request_id: u64,
        req: OpRequest,
    },
}

/// A repo-touching operation. Every variant carries the `repo_path` (the
/// supervisor is stateless about projects — just like `Spawn` carries `cwd`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum OpRequest {
    /// Canonicalize + verify the path is a git work tree, read HEAD branch.
    Validate { repo_path: PathBuf },
    /// `git worktree list --porcelain`.
    ListWorktrees { repo_path: PathBuf },
    /// `git worktree add` (computing the destination path supervisor-side).
    WorktreeAdd { repo_path: PathBuf, branch: String },
    /// `git worktree remove --force` + optional `git branch -D`.
    WorktreeRemove {
        repo_path: PathBuf,
        dest_path: PathBuf,
        branch: Option<String>,
    },
    /// Per-worktree live HEAD resolution + `gh pr list`. `worktrees` is
    /// `(worktree_idx, worktree_cwd)`; the supervisor resolves each branch
    /// live so a `git switch` inside a worktree is picked up.
    FetchPr {
        repo_path: PathBuf,
        worktrees: Vec<(usize, PathBuf)>,
    },
}

/// Disk-derived facts the client needs to build a project, computed
/// supervisor-side. The slug is NOT here — it's derived client-side from the
/// other projects' slugs (config logic stays on the client).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectInfo {
    pub canonical_path: PathBuf,
    pub repo_name: String,
    pub head_branch: Option<String>,
}

/// Wire mirror of `git::WorktreeListEntry` (kept separate so `git.rs` stays
/// serde-free).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Successful payload of an [`OpRequest`], matched against the pending
/// request's kind client-side.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum OpOk {
    Validated(ProjectInfo),
    Worktrees(Vec<WorktreeEntry>),
    WorktreeAdded(WorktreeEntry),
    WorktreeRemoved,
    /// `(worktree_idx, classification)` — `None` means "no PR / detached".
    PrStatuses(Vec<(usize, Option<PrStatus>)>),
}

/// Result of an [`OpRequest`]. `Err` carries a formatted message the client
/// surfaces via `OperationFailed` (or `PrFetchFailed` for a fetch).
pub type OpResult = std::result::Result<OpOk, String>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SupervisorMsg {
    Spawned {
        request_id: u64,
        id: SessionId,
    },
    SpawnFailed {
        request_id: u64,
        error: String,
    },
    /// Snapshot of the current screen (vt100 `contents_formatted` plus a
    /// cursor reposition) — sent in response to `Attach` or right after a
    /// successful `Spawn`.
    OutputDump {
        id: SessionId,
        bytes: Vec<u8>,
    },
    /// Live PTY output bytes since the last frame.
    OutputDelta {
        id: SessionId,
        bytes: Vec<u8>,
    },
    Exited {
        id: SessionId,
    },
    /// Sent right before the supervisor closes the socket on this client
    /// (another client stole the slot, or shutdown was requested).
    Detached {
        reason: String,
    },
    /// Periodic snapshot of resource usage for every supervised session +
    /// the supervisor's own process. Emitted while subscribed.
    Usage(UsageReport),
    /// Reply to [`ClientMsg::Op`], correlated by `request_id`.
    OpResult {
        request_id: u64,
        result: OpResult,
    },
}

/// Snapshot of process resource usage across all supervised sessions.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UsageReport {
    pub sessions: Vec<SessionUsage>,
    /// Supervisor process itself (memory + cpu).
    pub supervisor: ProcessNode,
    /// Client (TUI) process — supplied at handshake. `None` if the
    /// supervisor couldn't sample the PID this tick.
    pub client: Option<ProcessNode>,
    pub ts_ms: u64,
    /// Number of logical CPU cores on the host. Lets the client present
    /// CPU% relative to total machine capacity if it wants to.
    pub cpu_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionUsage {
    pub session_id: SessionId,
    pub project_slug: String,
    pub worktree_name: String,
    /// Tree rooted at the PTY's direct child (the user's shell).
    pub root: ProcessNode,
}

/// One process in a session's descendant tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessNode {
    pub pid: u32,
    pub name: String,
    /// Resident set size in bytes (includes shared pages — OS limitation).
    pub rss_bytes: u64,
    /// % of a single CPU core (matches `top` / `htop` convention).
    pub cpu_percent: f32,
    pub children: Vec<ProcessNode>,
}

impl ProcessNode {
    /// Sum rss_bytes across this node + every descendant.
    pub fn total_rss(&self) -> u64 {
        self.rss_bytes + self.children.iter().map(Self::total_rss).sum::<u64>()
    }
    /// Sum cpu_percent across this node + every descendant.
    pub fn total_cpu(&self) -> f32 {
        self.cpu_percent + self.children.iter().map(Self::total_cpu).sum::<f32>()
    }
}

fn config() -> bincode::config::Configuration {
    bincode::config::standard()
}

/// Serialize and write one length-delimited message. The live transport is
/// async ([`write_frame_async`]); this sync twin backs the round-trip tests.
#[cfg(test)]
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(value, config())?;
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("frame too large"))?;
    if len > MAX_FRAME {
        bail!("frame too large: {len} bytes");
    }
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Read one length-delimited message, blocking until a full frame arrives.
/// Sync twin of [`read_frame_async`]; used by the round-trip tests.
#[cfg(test)]
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    let (value, _) = bincode::serde::decode_from_slice(&buf, config())?;
    Ok(value)
}

/// Async twin of [`write_frame`] for tokio streams (UDS or TLS-over-TCP).
pub async fn write_frame_async<W, T>(w: &mut W, value: &T) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    use tokio::io::AsyncWriteExt;
    let bytes = bincode::serde::encode_to_vec(value, config())?;
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| anyhow::anyhow!("frame too large"))?;
    if len > MAX_FRAME {
        bail!("frame too large: {len} bytes");
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Async twin of [`read_frame`]; awaits a full length-delimited frame.
pub async fn read_frame_async<R, T>(r: &mut R) -> Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let (value, _) = bincode::serde::decode_from_slice(&buf, config())?;
    Ok(value)
}

/// Resolve the Unix-domain-socket path used by the supervisor.
///
/// Prefers `$XDG_RUNTIME_DIR/imbuia/sock` (Linux convention), then
/// `$XDG_CACHE_HOME/imbuia/sock`, then `~/.cache/imbuia/sock`. The parent
/// directory is created with mode 0700 if missing.
pub fn resolve_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("imbuia");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&dir) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&dir, perms);
        }
    }
    dir.join("sock")
}

pub fn pidfile_path(sock: &Path) -> PathBuf {
    sock.with_file_name("supervisor.pid")
}

pub fn supervisor_log_path(sock: &Path) -> PathBuf {
    sock.with_file_name("supervisor.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn async_frame_round_trip() {
        // Write several frames into one half of a duplex pipe, read them back
        // from the other — exercising the length-prefix + bincode path that the
        // remote transport relies on.
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = vec![
            ClientMsg::Resize {
                id: 3,
                rows: 24,
                cols: 80,
            },
            ClientMsg::Shutdown,
        ];
        for m in &sent {
            write_frame_async(&mut a, m).await.unwrap();
        }
        drop(a);
        for expected in &sent {
            let got: ClientMsg = read_frame_async(&mut b).await.unwrap();
            // ClientMsg has no PartialEq; compare via canonical encoding.
            assert_eq!(
                bincode::serde::encode_to_vec(&got, config()).unwrap(),
                bincode::serde::encode_to_vec(expected, config()).unwrap(),
            );
        }
    }

    #[test]
    fn roundtrip_client_msgs() {
        let cases = vec![
            ClientMsg::Spawn {
                request_id: 7,
                project_slug: "imbuia".into(),
                worktree_name: "main".into(),
                rows: 24,
                cols: 80,
                cwd: PathBuf::from("/tmp"),
                initial_command: Some("echo hi".into()),
            },
            ClientMsg::WriteBytes {
                id: 42,
                bytes: b"hello".to_vec(),
            },
            ClientMsg::Resize {
                id: 1,
                rows: 30,
                cols: 100,
            },
            ClientMsg::Kill { id: 9 },
            ClientMsg::Attach { id: 9 },
            ClientMsg::Shutdown,
            ClientMsg::SubscribeUsage,
            ClientMsg::UnsubscribeUsage,
            ClientMsg::Op {
                request_id: 11,
                req: OpRequest::Validate {
                    repo_path: PathBuf::from("/repo"),
                },
            },
            ClientMsg::Op {
                request_id: 12,
                req: OpRequest::ListWorktrees {
                    repo_path: PathBuf::from("/repo"),
                },
            },
            ClientMsg::Op {
                request_id: 13,
                req: OpRequest::WorktreeAdd {
                    repo_path: PathBuf::from("/repo"),
                    branch: "feat".into(),
                },
            },
            ClientMsg::Op {
                request_id: 14,
                req: OpRequest::WorktreeRemove {
                    repo_path: PathBuf::from("/repo"),
                    dest_path: PathBuf::from("/repo-worktrees/feat"),
                    branch: Some("feat".into()),
                },
            },
            ClientMsg::Op {
                request_id: 15,
                req: OpRequest::FetchPr {
                    repo_path: PathBuf::from("/repo"),
                    worktrees: vec![(0, PathBuf::from("/repo")), (1, PathBuf::from("/wt"))],
                },
            },
        ];
        for msg in cases {
            let mut buf = Vec::new();
            write_frame(&mut buf, &msg).unwrap();
            let mut cur = Cursor::new(buf);
            let _: ClientMsg = read_frame(&mut cur).unwrap();
        }
    }

    #[test]
    fn roundtrip_supervisor_msgs() {
        let cases = vec![
            SupervisorMsg::Spawned {
                request_id: 1,
                id: 5,
            },
            SupervisorMsg::OutputDump {
                id: 5,
                bytes: vec![1, 2, 3],
            },
            SupervisorMsg::OutputDelta {
                id: 5,
                bytes: vec![],
            },
            SupervisorMsg::Exited { id: 5 },
            SupervisorMsg::Detached {
                reason: "stolen".into(),
            },
            SupervisorMsg::Usage(UsageReport {
                sessions: vec![SessionUsage {
                    session_id: 1,
                    project_slug: "imbuia".into(),
                    worktree_name: "main".into(),
                    root: ProcessNode {
                        pid: 100,
                        name: "zsh".into(),
                        rss_bytes: 1_000_000,
                        cpu_percent: 1.5,
                        children: vec![ProcessNode {
                            pid: 101,
                            name: "node".into(),
                            rss_bytes: 50_000_000,
                            cpu_percent: 80.0,
                            children: vec![],
                        }],
                    },
                }],
                supervisor: ProcessNode {
                    pid: 99,
                    name: "imbuia".into(),
                    rss_bytes: 2_000_000,
                    cpu_percent: 0.1,
                    children: vec![],
                },
                client: None,
                ts_ms: 1,
                cpu_count: 8,
            }),
            SupervisorMsg::OpResult {
                request_id: 1,
                result: Ok(OpOk::Validated(ProjectInfo {
                    canonical_path: PathBuf::from("/repo"),
                    repo_name: "repo".into(),
                    head_branch: Some("main".into()),
                })),
            },
            SupervisorMsg::OpResult {
                request_id: 2,
                result: Ok(OpOk::Worktrees(vec![WorktreeEntry {
                    path: PathBuf::from("/repo"),
                    branch: Some("main".into()),
                }])),
            },
            SupervisorMsg::OpResult {
                request_id: 3,
                result: Ok(OpOk::WorktreeAdded(WorktreeEntry {
                    path: PathBuf::from("/repo-worktrees/feat"),
                    branch: Some("feat".into()),
                })),
            },
            SupervisorMsg::OpResult {
                request_id: 4,
                result: Ok(OpOk::WorktreeRemoved),
            },
            SupervisorMsg::OpResult {
                request_id: 5,
                result: Ok(OpOk::PrStatuses(vec![(0, Some(PrStatus::Open)), (1, None)])),
            },
            SupervisorMsg::OpResult {
                request_id: 6,
                result: Err("boom".into()),
            },
        ];
        for msg in cases {
            let mut buf = Vec::new();
            write_frame(&mut buf, &msg).unwrap();
            let mut cur = Cursor::new(buf);
            let _: SupervisorMsg = read_frame(&mut cur).unwrap();
        }
    }

    #[test]
    fn handshake_roundtrip() {
        let req = HandshakeReq {
            protocol: PROTOCOL_VERSION,
            client_pid: 1234,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let mut cur = Cursor::new(buf);
        let decoded: HandshakeReq = read_frame(&mut cur).unwrap();
        assert_eq!(decoded.protocol, PROTOCOL_VERSION);
        assert_eq!(decoded.client_pid, 1234);

        let resp = HandshakeResp::Ok {
            supervisor_pid: 99,
            sessions: vec![SessionMeta {
                id: 1,
                project_slug: "x".into(),
                worktree_name: "y".into(),
                cwd: PathBuf::from("/"),
                rows: 24,
                cols: 80,
            }],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &resp).unwrap();
        let mut cur = Cursor::new(buf);
        let _: HandshakeResp = read_frame(&mut cur).unwrap();
    }

    #[test]
    fn read_frame_rejects_oversized_len() {
        let mut bad = Vec::new();
        bad.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        let mut cur = Cursor::new(bad);
        let result: Result<HandshakeReq> = read_frame(&mut cur);
        assert!(result.is_err(), "oversized frame should be rejected");
    }
}
