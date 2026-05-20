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

## Install

One-liner that fetches the latest release for your platform and drops the
binary in `$HOME/.local/bin`:

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

## License

[MIT](LICENSE)
