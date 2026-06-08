# imbuia — architecture for AI agents

Onboarding doc for AI coding agents (Claude Code, Cursor, etc.) editing this
codebase. Humans should read `README.md` first.

## Hard rules

- **Do not break the Elm architecture.** The reducer (`src/reducer.rs::reduce`)
  is pure: it mutates `AppState` and returns a `Commands` (SmallVec of side
  effects). Never call `tokio::spawn`, IO, or `Command::new` inside the
  reducer. New side effects are new `Command` variants executed by
  `src/runtime.rs::execute`.
- **Do not block the main task.** Tokio's main task in `runtime.rs` only
  orchestrates the event loop. IPC framing runs on dedicated tokio reader +
  writer tasks (`client.rs`); inbound frames come back as `Action`s via
  `mpsc::Sender<Action>`. Config writes are quick synchronous calls in
  `runtime::execute`. **Git/`gh` no longer run client-side at all** — they're
  shipped to the supervisor as `ClientMsg::Op` (see Async ops contract).
- **Disk/process work belongs in the supervisor.** Git and `gh` run inside
  the supervisor process so it can one day run remotely; the client keeps
  only config + rendering. Don't add `Command::new("git")`/`gh` calls to the
  client.
- **PTYs live in the supervisor, not the client.** The TUI process is a
  thin attach-client; PTY masters, vt100 parsers, and child processes are
  owned by a long-lived sibling process started via `imbuia --supervisor`.
  Sessions survive client restarts. See **Supervisor split** below.
- **The supervisor → screen path bypasses the reducer.** The client's
  reader task (`client::spawn_reader`) reads `SupervisorMsg::OutputDump`/
  `OutputDelta` frames, feeds bytes into the matching session's local
  vt100 `Parser`, then calls `notify_one()` on a `tokio::sync::Notify` so
  the render loop wakes up. Keep it that way — routing through the
  reducer would tank latency on a busy session.
- **`cargo add` for every new dep** (no editing `Cargo.toml` by hand for
  versions — the resolver will surface conflicts cleanly).
- **`cargo test` + `cargo clippy --all-targets` must stay green** before you
  hand control back to the user.

## Crate layout

| File              | Responsibility                                                   |
|-------------------|------------------------------------------------------------------|
| `main.rs`         | Entry: dispatch `--supervisor` vs client; raw mode, alt screen, mouse capture, tracing-to-file, panic hook. |
| `runtime.rs`      | (Client) Tokio event loop; attaches to the supervisor at startup; `execute()` dispatches `Command` → IO + threads. |
| `reducer.rs`      | Pure `reduce(state, action) -> Commands`. All key/mouse handling. |
| `app.rs`          | Plain types: `AppState`, `Project`, `Worktree`, `Action`, `Command`, popups. Also `SupervisorId` + `SupervisorDirectory` (id↔name projection of the live registry, snapshotted into `AppState`). |
| `commands.rs`     | Ex-style `:command` registry (`COMMANDS: &[CmdSpec]`) + handlers. |
| `config.rs`       | TOML schema, atomic write, slugging, XDG resolution. `GlobalConfig.remotes` (named remotes; legacy single `remote` folds in via `effective_remotes`), `ProjectConfig.supervisor` (name; absent = local). |
| `git.rs`          | `std::process::Command` wrappers (`validate_repo`, `head_branch`, `worktree_add`, `worktree_remove`). **Run inside the supervisor**, not the client (see Disk ops below). |
| `github.rs`       | `gh` CLI wrapper for PR status. Also **run inside the supervisor**. |
| `session.rs`      | `Session` trait + `FakeSession` for tests. The real impl is in `client.rs`. |
| `client.rs`       | `Supervisors` registry (local + N remotes), one `SupervisorClient` per connection; `connect_all` (eager, best-effort); `ProxySession` (client-side `Session` impl) with local↔global session-id remap; double-fork helpers, reader + writer tasks. |
| `supervisor.rs`   | `imbuia --supervisor` entry: owns a tokio runtime; PTY spawn/own (portable-pty + vt100) on blocking threads; UDS accept loop (always) + optional TCP+TLS acceptor (`--listen`); per-client async `handle_conn`. |
| `ipc.rs`          | Shared wire types (`ClientMsg`, `SupervisorMsg`, `Handshake*`, `OpRequest`/`OpOk` incl. `ListDir`/`DirListing`), framed read/write (`[u32 len][u8 codec][payload]`; codec 0=raw, 1=zstd — `write_frame_async(.., compress)` gates compression on size + remote-only, reader auto-detects; sync twins are test-only), socket path resolution. `PROTOCOL_VERSION = 2`. |
| `transport.rs`    | Optional remote transport: Ed25519 identity load/gen, SPKI fingerprints, rustls (ring) client/server configs with pinned-key verifiers. Both sides TOFU: client pins the supervisor in `known_hosts`; supervisor pins the first client into `authorized_keys` when empty. |
| `input.rs`        | crossterm `Event` → `Action`; `encode_key` with DECCKM handling + kitty/modifyOtherKeys passthrough; `KbdTracker` infers the inner app's keyboard protocol from its output. |
| `layout.rs`       | `chrome()` → sidebar/tab_bar/terminal/action_bar rects.          |
| `render.rs`       | ratatui rendering. Reads from vt100 `Screen` cell-by-cell.       |
| `theme.rs`        | `ThemeKind` (Dark / Light) + hardcoded palettes ported from rowdy. |

## Data flow

```
client process (TUI)                                           supervisor process
═══════════════════════════════                                ═══════════════════════════
crossterm Event ─► input::map ─► Action ─┐
                                         ▼
              ┌──────────────► reduce(state, action) ──► Commands ──► execute()
              │                  (pure)                              │
              │                                                      ├─► WriteKey / WriteMouse → ProxySession encodes
              │                                                      │       bytes locally, sends ClientMsg::WriteBytes ──┐
              │                                                      ├─► ResizePty → ClientMsg::Resize ───────────────────┤
              │                                                      ├─► SpawnInWorktree → SupervisorClient::request_spawn┤
              │                                                      │                       (ClientMsg::Spawn) ──────────┤
              │                                                      ├─► KillSession / RestartSupervisor (ClientMsg) ─────┤   UDS frames
              │                                                      ├─► OpenProject / AddWorktree / RemoveWorktree /     │   ◄────────────
              │                                                      │   ImportWorktrees / FetchPrStatuses               │
              │                                                      │   → SupervisorClient::request_* (ClientMsg::Op) ──┤       │
              │                                                      └─► SaveGlobalConfig / SaveProjectConfig             │       ▼
              │                                                          (sync, atomic write — config stays client-side)  │
              ▼                                                                                                            │   ┌─────────────────────┐
         AppState ── render ──► ratatui Frame                                                                              │   │ accept loop         │
              ▲                                                                                                            │   │  - handshake        │
              │   (supervisor → screen bypasses reducer)                                                                   │   │  - steal-on-attach  │
              │                                                                                                            │   │  - dispatch ClientMsg
   client::spawn_reader thread                                                                                             │   └─────────────────────┘
        ▲                                                                                                                  │       │
        │  SupervisorMsg::OutputDump / OutputDelta / Spawned / Exited / Detached / OpResult                                │       ▼
        └────────────────────────────────────────────────────────────────────────────────────────────────────────────────►│   per-session reader thread
                                                                                                                            │     PTY → parser.process(bytes)
                                                                                                                            │     bytes → SupervisorMsg::OutputDelta ──► active client writer
                                                                                                                            │
                                                                                                                            └──── frame ──►
   ProxySession's vt100 parser.process(bytes) ──► Notify ──► redraw_at = now + FRAME
```

## State ownership cheat-sheet

- `AppState.projects[*].worktrees[*].sessions: Vec<SessionId>` is the only
  index into `AppState.sessions: HashMap<SessionId, Arc<dyn Session>>`.
  Always update both in lockstep — leaking either causes silent UI bugs.
  **These `SessionId`s are client-global** (minted by `client::Supervisors`),
  not the per-supervisor wire ids — each `ProxySession` maps global↔local so
  ids from different supervisors never collide. `Project.supervisor:
  SupervisorId` says which connection a project (and its sessions) lives on;
  session-targeted commands route implicitly via the session's own
  `ProxySession`, project/spawn/op commands resolve the client from the
  registry (`execute` in `runtime.rs`).
- `active_worktree: Option<(pi, wi)>` points at the tab bar's source.
- `sidebar_selection: Option<(pi, Option<wi>)>` — `None` worktree means the
  cursor is on the project header row. These two can disagree (e.g. the
  user navigates in the sidebar without activating).
- `mode: Normal | Terminal | Command` is vim semantics. Keys are forwarded
  to the PTY only in `Terminal`. `Command` is the `:` ex-line.
- Popups (`popup: Option<InputPopup>`, `edit_popup: Option<EditPopup>`,
  `help_open: bool`) are mutually-modal: only one is shown at a time, and
  every popup short-circuits the regular key path at the top of
  `handle_key`.

## Supervisor split (don't tweak without reading this)

- **Single binary, two roles.** `imbuia` (no args) is the client; `imbuia
  --supervisor` is the daemon. `main.rs` branches on the flag before any
  TTY setup so the supervisor never touches raw mode. Both roles call
  `transport::init()` to install the ring rustls provider.
- **Async transport, two backends.** Both the client and supervisor speak
  the framed protocol over a tokio `AsyncRead + AsyncWrite` stream, split
  with `tokio::io::split` into a reader task + a writer task. The backend is
  either a local `UnixStream` (default) or a `tokio_rustls` TLS stream over
  TCP (remote). The framing (`*_async`) and the per-client `handle_conn` /
  reader loop are transport-agnostic. The supervisor's PTY/reaper/usage/op
  work stays on blocking `std::thread`s and pushes frames via the tokio
  channel using `blocking_send`/`try_send` (legal off-runtime).
- **Remote latency/throughput.** On the TCP path both sides set
  `TCP_NODELAY` (disable Nagle — keystrokes are tiny writes and would
  otherwise stall ~40ms). The writers for remote connections pass
  `compress = true`, so large frames (`OutputDump`, bulk `OutputDelta`, big
  pastes) are zstd-compressed; small/interactive frames stay raw. The local
  UDS path sets neither (no Nagle on Unix sockets; compression pointless).
- **Socket layout.** `$XDG_RUNTIME_DIR/imbuia/sock` (preferred on Linux),
  else `$XDG_CACHE_HOME/imbuia/sock`, else `~/.cache/imbuia/sock`.
  Sibling files: `supervisor.pid`, `supervisor.log`. See
  `ipc::resolve_socket_path`.
- **Auto-spawn (local only).** `client::connect_or_spawn` probes the socket;
  if absent it `fork()`s twice (daemon trick), `setsid()`s, redirects
  stdin/stdout/stderr to `/dev/null` + `supervisor.log`, then `execv`s
  itself with `--supervisor`. The parent waitpid()s the intermediate, then
  connects. A configured *remote* never auto-spawns — it's a hard connect.
- **Remote supervisor (opt-in).** Set `remote.url = "host:port"` in the
  global config to make the client connect over TCP+TLS instead of the local
  UDS. Start the remote daemon with `imbuia --supervisor --listen host:port`
  (it still binds its local UDS too). Trust is SSH-style pinned keys, not
  CA/PKI: each side has an Ed25519 `identity.key` in its config dir; the
  fingerprint is `sha256(SPKI)` (stable across cert regen). Both directions
  are **TOFU**: the client pins the supervisor's key in `known_hosts` on first
  connect; the supervisor, when `authorized_keys` is empty, pins the *first*
  client that connects and admits only listed fingerprints thereafter (so a
  fresh remote needs no manual key exchange — add more clients by hand). All
  in `transport.rs`. The local UDS path is unauthenticated (filesystem perms
  are the boundary).
- **Single client at a time.** The supervisor's accept loop hands the new
  client an exclusive slot. The old client (if any) gets
  `SupervisorMsg::Detached`, then its `CancellationToken` is fired to tear
  down its reader + writer tasks (dropping both stream halves closes the
  socket). The TUI handles `Detached` by posting `OperationFailed` and
  exiting.
- **State sync = "dump on attach + raw byte forward".** On attach the
  supervisor sends `OutputDump { bytes: parser.screen().contents_formatted() }`
  per session — escape sequences that restore the visible screen and
  cursor. Subsequent live output ships as `OutputDelta { bytes }` —
  unparsed PTY bytes. The client runs its own vt100 parser to render.
  Two parsers exist, but only the supervisor's is the source-of-truth for
  reattach. Scrollback older than the current screen isn't replayed
  (known limitation).
- **Session metadata is opaque to the supervisor.** `ClientMsg::Spawn`
  carries `project_slug` + `worktree_name` strings; the supervisor stores
  them in `SessionMeta` and echoes them back on `HandshakeResp::Ok`. The
  client uses them to re-bind resumed sessions to the right tab via
  `runtime::rebind_resumed_sessions`. If a project/worktree no longer
  exists locally, the orphan session is killed (see `runtime::locate`).
- **Spawned session sizing.** `client::request_spawn` stores `(dest, rows,
  cols)` in `pending_spawns` keyed by `request_id`. When the supervisor
  responds with `Spawned { request_id, id }`, the client builds the local
  vt100 parser with the correct dimensions. Don't hardcode 24×80 here —
  there was a bug.
- **`:restart-supervisor` (`:rs`) kills everything.** The handler sends
  `ClientMsg::Shutdown`, the supervisor kills all children + unlinks the
  socket, and the client exits. Re-launch to get a fresh supervisor. No
  in-place reconnect today; the reader task terminates when the socket
  closes.

## vt100 + scrollback (don't tweak without reading this)

- We allocate `vt100::Parser::new(rows, cols, 10_000)` — 10k rows of
  scrollback. **Both** the supervisor and the client parser are sized
  this way; on `Spawned`, the client uses the spawn-request dimensions
  (not 24×80) so the freshly-spawned tab renders at the correct size.
- `vt100::Screen::set_scrollback(n)` shifts the *view* into history. The
  renderer reads `screen.cell()` which respects that offset automatically.
- vt100 keeps the view anchored when new output arrives (see
  `vt100/grid.rs::scroll_up`: if `scrollback_offset > 0`, it increments).
  **Never `set_scrollback(0)` from the reader thread** — it forces snap to
  bottom and yanks users out of scrollback mid-stream.
- Wheel routing (`client::ProxySession::write_mouse`):
  1. App enabled SGR mouse → forward encoded bytes (unless Shift bypass).
  2. Alt screen + no mouse + plain wheel → synthesise arrow keys (less/vim).
  3. Else (main screen, plain wheel, or **Shift+wheel from anywhere**) →
     `bump_scrollback` (local vt100 view).

## Async ops contract

**Anything that touches the supervisor's disk or shells out (git, `gh`) runs
in the supervisor**, never the client. The client only ever does config
(load/save TOML, slug computation) and rendering. This is the boundary that
lets the supervisor one day run on a remote host. Git ops (`OpenProject`,
`ImportWorktrees`, `AddWorktree`, `RemoveWorktree`) and the PR fetch
(`FetchPrStatuses`) all go over the socket; the convention is:

1. Reducer pushes the `Command` and sets `state.pending_op = Some("…")`.
2. `runtime::execute` calls `SupervisorClient::request_*`, which allocates a
   `request_id`, records a `PendingOp` continuation, and ships a
   `ClientMsg::Op { request_id, req: OpRequest }`. (`next_request` is shared
   with `pending_spawns` — one id space, two maps.)
3. The supervisor runs the op **off its command loop** (a throwaway thread per
   git op; a singleton serial `gh` worker for `FetchPr` — `worktree add` can
   take minutes and must not freeze WriteBytes/Resize). It replies with
   `SupervisorMsg::OpResult { request_id, result }`. An `OpReplyGuard`
   guarantees a reply even on panic, so `pending_op` never wedges.
4. `client::spawn_reader` matches `OpResult` to its `PendingOp` and posts the
   *same* `Action` the old local threads posted (`WorktreeAdded`,
   `ProjectValidated`, `PrStatusesFetched`, …) — so the **reducer is
   unchanged**. Errors become `OperationFailed` (or `PrFetchFailed`), clearing
   `pending_op`.

The supervisor stays **stateless about projects**: every `OpRequest` carries
the `repo_path` (like `Spawn` carries `cwd`). It never resolves slugs or holds
a project model — that's all client/config side.

`OpenProject` splits: the supervisor validates + canonicalizes + reads HEAD
(`OpRequest::Validate` → `ProjectInfo`); the reducer's `ProjectValidated` arm
then computes the slug (needs the other projects' slugs) and builds + persists
the `Project`. Slug/save are config logic and stay client-side.

When adding a new disk/process op: add an `OpRequest`/`OpOk` variant, a
`PendingOp` continuation + `request_*` method in `client.rs`, a supervisor
handler (thread or worker — never inline on the command loop), and map the
`OpResult` back to an existing `Action` in `spawn_reader`. Don't run git/`gh`
in `runtime.rs` and don't add tokio tasks there.

`Spawn`/`Spawned` and `Op`/`OpResult` are the only request/response pairs
(correlated via `request_id` against `pending_spawns` / `pending_ops`); the
rest of the `ClientMsg`s are fire-and-forget.

## Persistence

- `~/.config/imbuia/config.toml` — global state. Schema in
  `config::GlobalConfig`. Owns the *ordered list of project slugs*.
- `~/.config/imbuia/projects/<slug>.toml` — one file per project.
  `config::ProjectConfig` (the `slug` field is `#[serde(skip)]` — the
  filename is the slug).
- All writes are atomic via `.tmp` + `rename()` (`config::write_toml_atomic`).
- Mutations that should persist push `Command::SaveGlobalConfig` and/or
  `Command::SaveProjectConfig(idx)` from the reducer. **Never write files
  from the reducer directly.**
- Remote-transport trust files live in the same dir (`config.rs` helpers):
  `identity.key` (this host's Ed25519 key, mode 0600, auto-generated),
  `known_hosts` (client TOFU pins), `authorized_keys` (supervisor allow-list).
  `GlobalConfig.remote` is hand-edited and never owned by the UI, so
  `SaveGlobalConfig` preserves it by re-reading the on-disk value.

## Adding an ex-command

```rust
// in src/commands.rs
CmdSpec {
    names: &["frob"],
    usage: ":frob [thing]",
    description: "Frob the selected thing.",
    handler: cmd_frob,
},

fn cmd_frob(state: &mut AppState, args: &[&str], cmds: &mut Commands) {
    // mutate state, push commands, OR set state.command_status on error
}
```

`:help` reads `COMMANDS` directly, so new commands self-document.

## Adding a key chord

Multi-key chords use `state.pending_leader: Option<Leader>`. See
`Leader::CtrlW` / `G` / `CtrlBackslash` in `reducer.rs`. Single keys live
in `handle_normal_key` / `handle_terminal_key` / `handle_command_key`.

## Testing

- 90+ unit tests in `src/reducer.rs::tests` cover the reducer end-to-end
  using a `FakeSession` (no PTY, no git, no socket).
- IPC wire types have round-trip tests in `src/ipc.rs::tests` — keep them
  green when adding new `ClientMsg` / `SupervisorMsg` variants.
- Reducer tests should never touch disk, spawn processes, or open
  sockets — that logic belongs in `runtime.rs`, `git.rs`, `client.rs`,
  or `supervisor.rs`.
- `client.rs`, `supervisor.rs`, `git.rs`, and `runtime.rs` are
  integration-tested manually (see the verification steps in `README.md`).

## Logging

`tracing` writes to `$XDG_CACHE_HOME/imbuia/imbuia.log` for the client
(stdout/stderr are owned by the TUI) and to `supervisor.log` next to the
socket for the supervisor. Use `tracing::info!` / `warn!` liberally for
async ops; the files are the only debug channel.

## What's deliberately *not* done

- No removing projects (only worktrees).
- No closing a project / refreshing it from disk (use `:edit` to mutate).
- No concurrency safety on the project toml — single-user app.
- No per-tab scrollback view — vt100's 10k buffer is the only one.
- No structured agent harness integration yet — every tab is a plain shell.
- No tests for `runtime.rs` / `client.rs` / `supervisor.rs` (manual
  verification).
- No multi-client attach on the supervisor (a second `imbuia` steals the
  slot; concurrent rendering would need viewport negotiation).
- No automatic supervisor restart on protocol-version mismatch — the
  client tells the user to run `:rs` and relaunch.
- Sessions don't survive host reboot (would need a launchd / systemd
  unit). Scrollback older than the screen at attach time isn't replayed.
- Keyboard-protocol state (kitty / modifyOtherKeys) is tracked
  supervisor-side by a `input::KbdTracker` fed from PTY output (vt100
  doesn't model it) and re-emitted in the attach prelude — see
  `supervisor::send_dump` / `mode_prelude`, alongside the DECSET mode
  re-sync. A reattaching client's own `KbdTracker` (fed the dump) recovers
  the state. Caveat: only the active (top-of-stack) kitty flag set is
  re-emitted, so deeply-nested push/pop sequences whose lower frames
  scrolled out of `output_log` can drift by a flag bit after a later pop —
  harmless in practice (any nonzero flags still mean CSI-u for modified
  keys).
