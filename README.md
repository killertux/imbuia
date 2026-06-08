# imbuia

A Rust TUI terminal multiplexer for working on multiple git worktrees in
parallel — each worktree hosts a stack of terminal tabs, optionally launching
your favourite agent harness (Claude Code, Codex CLI, aider) or a plain shell.

> ⚠️ Early-stage. The MVP works end-to-end (open projects, create/remove
> worktrees, run a setup script on creation, multiple tabs per worktree,
> named launchers, in-app TOML editing, mouse + scrollback, themes, live
> resource usage, detachable supervisor so sessions survive app restarts),
> but expect rough edges.

## Sessions survive restarts

PTYs are owned by a separate, long-lived **supervisor** process, not the
TUI. Quit imbuia (`:q` / `Ctrl-Q`), pull, rebuild, relaunch — your shells,
Claude Code, REPLs, dev-servers all keep running and reattach to their
tabs. The supervisor is auto-spawned the first time you launch and stays
alive in the background; `:restart-supervisor` (`:rs`) explicitly kills it
and everything it owns.

## Why

If you juggle many feature branches or agent sessions, the usual flow is:

```sh
git worktree add ../myrepo-feat-x feat-x
cd ../myrepo-feat-x
pnpm install && pnpm dev
# … repeat for each new branch, then tile windows yourself
```

`imbuia` collapses that into a sidebar of projects and worktrees, a tab bar
per worktree, and a `:worktree feat-x` command that creates the directory,
opens a terminal in it, and runs your saved setup script automatically. Add
named launchers (e.g. `claude`, `dev`, `repl`) and a single keystroke spawns
a new tab running that command in the current worktree.

## Install & updates

Imbuia auto-checks GitHub for new releases on startup and once an hour;
when one's available, a `vX.Y.Z available · :update to install` hint
appears in the action bar. Running `:update` installs in place
(in-process pipes the install script into `sh`). If the IPC protocol
hasn't changed, your existing supervisor and sessions survive the
relaunch — otherwise `:rs` is needed.

For the first install, one-liner that fetches the latest release for
your platform and drops the binary in `$HOME/.local/bin`:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/killertux/imbuia/main/install.sh | sh
```

Supported targets: Linux x86_64, Linux aarch64, macOS aarch64. The script
warns if the install directory isn't on your `$PATH`.

Override the defaults with env vars:

- `IMBUIA_INSTALL_DIR=/usr/local/bin` — install somewhere else
- `IMBUIA_VERSION=v0.2.0` — pin to a specific release tag (default: latest)

### From source

```sh
cargo install --path .
# or
cargo build --release
./target/release/imbuia
```

## Quick tour

### Normal-mode keys

| Key                       | What it does                                                    |
|---------------------------|-----------------------------------------------------------------|
| `h` / `←`, `l` / `→`      | Move focus between sidebar and terminal.                        |
| `j` / `↓`, `k` / `↑`      | Move sidebar selection (when sidebar is focused).               |
| `Enter`                   | Activate worktree / toggle project expansion.                   |
| `o`                       | New terminal tab in the active worktree.                        |
| `x`                       | Close the focused tab.                                          |
| `gt` / `gT`               | Next / previous tab.                                            |
| `<Space>`                 | Open the leader menu (which-key style overlay; see below).      |
| `Ctrl-W >` / `<` / `=`    | Grow / shrink / reset sidebar width.                            |
| `i`                       | Enter Terminal mode (keys forwarded to the PTY).                |
| `Ctrl-\ Ctrl-N`           | Leave Terminal mode (Neovim-style).                             |
| `:`                       | Enter Command mode.                                             |
| `Ctrl-Q` / `:q`           | Quit.                                                           |
| **Shift+wheel**           | Always scrolls terminal scrollback (bypasses TUI app).          |

### `<Space>` leader

Pressing `<Space>` in Normal mode opens a small hint overlay listing the
available follow-up keys. The current bindings:

| Chord          | What it does                                  |
|----------------|-----------------------------------------------|
| `<Space> o`    | Open project popup (path + setup script).     |
| `<Space> w`    | New worktree popup.                           |
| `<Space> W`    | Remove the selected worktree.                 |
| `<Space> l`    | Launcher picker.                              |
| `<Space> e`    | Edit the selected project's setup script.    |
| `<Space> u`    | Live resource-usage popup.                    |
| `<Space> ?`    | Open this help popup.                         |
| `<Space> q`    | Quit.                                         |

`Esc` (or any key not bound under the leader) cancels.

### Commands

| Command                       | What it does                                                  |
|-------------------------------|---------------------------------------------------------------|
| `:open [path]`                | Add a project. With no arg, opens a popup for path + setup-script. |
| `:worktree <branch>` / `:wt`  | `git worktree add` for `<branch>`, opens a terminal, runs setup. |
| `:worktree-remove` / `:wr`    | Removes the selected worktree (files + local branch).         |
| `:edit` / `:e`                | Multi-line popup to edit the selected project's setup script. |
| `:launch [name]` / `:l`       | Launch a named command in a new tab; no arg → picker popup.   |
| `:tabnew` / `:tabclose`       | New terminal / close current terminal.                        |
| `:usage` / `:u`               | Live memory + CPU per session and its descendants.            |
| `:restart-supervisor` / `:rs` | Kill PTY supervisor (and every session) and exit.             |
| `:set theme=dark\|light`      | Switch palette (persisted).                                   |
| `:set sidebar.width=N`        | Resize sidebar.                                               |
| `:update`                     | Install the latest release. With `check`, just re-runs the version check. |

## Setup scripts

Each project can store a multi-line bash script. Either fill it in when
opening the project (`Shift-O` popup), or open `:edit` later, paste your
script, **Ctrl-S** to save. The next `:worktree <branch>` will run it as
the shell's first command in the new worktree:

```sh
nvm use
pnpm install
pnpm dev &
```

## Launchers

A launcher is a named command pinned to a project. `:launch claude` opens a
new tab in the active worktree and runs `claude` as its first input.
`Shift-L` (or `:launch` with no arg) opens a picker showing the project's
launchers plus a always-present `Terminal` entry for a plain shell.

Launchers can live in two places. Per-project in
`~/.config/imbuia/projects/<slug>.toml`:

```toml
[[launchers]]
name = "claude"
command = "claude --resume"

[[launchers]]
name = "dev"
command = "pnpm dev"
```

Or globally in `~/.config/imbuia/config.toml`, where they're available
across every project:

```toml
[[launchers]]
name = "claude"
command = "claude"

[[launchers]]
name = "repl"
command = "node"
```

The picker shows both lists with a `[project]` / `[global]` tag. When the
same name appears in both, the project entry wins (closer scope beats wider
scope). No in-app editor yet — edit the TOML by hand.

## Config

Everything is persisted under `$XDG_CONFIG_HOME/imbuia/` (default
`~/.config/imbuia/`):

```
~/.config/imbuia/
├── config.toml           # global: sidebar_width, theme, project list
└── projects/
    ├── myrepo.toml       # per-project: name, path, expanded,
    │                     #              setup_script, worktrees, launchers
    └── …
```

You can edit these by hand — or use `:edit` inside the app.

The supervisor's runtime files live under `$XDG_RUNTIME_DIR/imbuia/` (Linux)
or `$XDG_CACHE_HOME/imbuia/` / `~/.cache/imbuia/` (macOS):

```
~/.cache/imbuia/
├── sock              # Unix-domain socket the TUI client attaches to
├── supervisor.pid    # supervisor PID
├── supervisor.log    # supervisor tracing log
└── imbuia.log        # client tracing log
```

## Remote supervisor

By default the supervisor runs on the same machine as the TUI, over a local
Unix socket. You can instead run supervisors on **one or more remote hosts** and
attach to them from your laptop over TCP — sessions then live on (and survive
reboots of the *client* machine on) the remote.

The client always keeps its **local** supervisor and connects to every
configured remote as well. Each **project is pinned to one supervisor** (chosen
when you open it); all of that project's worktrees and sessions live there.

The link is **encrypted and mutually authenticated** with TLS. Trust is
SSH-style pinned public keys, not certificate authorities: each side keeps a
long-lived Ed25519 identity (`identity.key`, auto-generated on first use) and
recognises the other by its fingerprint (`sha256` of the public key). No manual
key exchange is needed for the common single-client case — both sides
**trust-on-first-connect** (TOFU).

### 1. Start the supervisor on the remote host

Install `imbuia` there and run it with `--listen`:

```sh
imbuia --supervisor --listen 0.0.0.0:7777
```

- Use `0.0.0.0:7777` for any interface, or `127.0.0.1:7777` to expose it only
  to loopback (e.g. when reaching it through an SSH tunnel).
- It still serves its local Unix socket too, and logs its own key fingerprint
  to `supervisor.log` on startup.
- There's no auto-restart — run it under systemd/launchd, `tmux`, or `nohup`
  if you want it to outlive your shell.

### 2. Point the client at it

Add a `[remotes.<name>]` table per remote to the client's
`~/.config/imbuia/config.toml` (the `<name>` is how it shows up in the UI):

```toml
[remotes.gpu-box]
url = "your.remote.host:7777"   # host:port, no scheme

[remotes.ci]
url = "10.0.0.9:7777"
```

Launch `imbuia` as usual. On the first connection to each remote:

- the client generates its identity and **pins the supervisor's key** in
  `~/.config/imbuia/known_hosts`;
- the supervisor, if its `authorized_keys` is empty, **pins your client** and
  lets it in.

A remote that's unreachable at startup is non-fatal — it's just unavailable
until you relaunch (projects pinned to it report an error when used).

> Older configs with a single `[remote]` table still work — it's treated as a
> remote named `remote`.

### 3. Open a project on a supervisor

Run `:open` (no argument) to get the open-project popup:

- **Tab** cycles the fields. The **supervisor** row (←/→) picks which supervisor
  to create the project on — `local` or any configured remote.
- The **directory browser** lists the *selected supervisor's* filesystem, so you
  can navigate to the repo even on a remote whose paths differ from yours.
  ↑/↓ move, →/Enter descends (or opens a highlighted git repo, marked `◆`),
  ←/Backspace goes up, **Ctrl-S** opens the current directory as the project.

The project's supervisor is saved to its `projects/<slug>.toml` as
`supervisor = "<name>"` (absent = local), so it reattaches there next launch.

### Resource usage across supervisors

`:usage` shows one section per supervisor (its sessions + its own process), a
single **Client** row for the TUI itself (sampled locally), and a grand total.
A remote dropping out just removes its section — the app keeps running.

### Trust files & security notes

Three files in each side's config dir (`~/.config/imbuia/`) govern trust:

| File              | Side       | Contents                                            |
|-------------------|------------|-----------------------------------------------------|
| `identity.key`    | both       | this host's Ed25519 private key (mode `0600`)       |
| `known_hosts`     | client     | `host:port <fingerprint>` pins (TOFU)               |
| `authorized_keys` | supervisor | allowed client `<fingerprint>` lines                |

- **The first connection is the trust moment.** Only bootstrap on a network
  where nobody can race you to the port. To avoid TOFU entirely, exchange
  fingerprints by hand first: add `host:port <fp>` to the client's
  `known_hosts`, and the client's fingerprint to the remote's
  `authorized_keys`. Each process logs its own fingerprint at startup
  (`supervisor.log` / `imbuia.log`).
- **More than one client:** the supervisor only auto-pins while
  `authorized_keys` is empty. Add further clients by appending their
  fingerprints (one per line) by hand.
- **Key changed?** If you regenerate the supervisor's `identity.key`, the
  client refuses to connect (host-key-changed) — delete the stale line from
  `known_hosts` to re-trust.
- The local Unix-socket path is unauthenticated; filesystem permissions are
  the boundary there.

## License

[MIT](LICENSE)
