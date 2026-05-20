# imbuia

A Rust TUI terminal multiplexer for working on multiple git worktrees in
parallel — each worktree hosts a stack of terminal tabs, optionally running
your favourite agent harness (Claude Code, Codex CLI, aider) or a plain shell.

> ⚠️ Early-stage. The MVP works end-to-end (open project, create/remove
> worktrees, run a setup script on creation, multiple tabs per worktree,
> in-app TOML editing, mouse + scrollback, themes, detachable supervisor
> so sessions survive app restarts), but expect rough edges.

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
npm install && npm run dev
# … repeat for each new branch, then tile windows yourself
```

`imbuia` collapses that into a sidebar of projects and worktrees, a tab bar
per worktree, and a `:worktree feat-x` command that creates the directory,
opens a terminal in it, and runs your saved setup script automatically.

## Install

```sh
cargo build --release
./target/release/imbuia
```

No prebuilt binaries yet.

## Quick tour

| Key / command            | What it does                                                    |
|--------------------------|-----------------------------------------------------------------|
| `:open <path>`           | Add a project at `<path>` (must be inside a git repo).          |
| `:worktree <branch>`     | `git worktree add` for `<branch>`, opens a terminal, runs setup. |
| `:worktree-remove` / `:wr` | Removes the selected worktree (files + local branch).         |
| `:restart-supervisor` / `:rs` | Kill the PTY supervisor (and every session) and exit; respawned on next launch. |
| `:edit` / `:e`           | Open a multi-line popup to edit the selected project's setup script. |
| `:tabnew` / `o`          | New terminal tab in the active worktree.                        |
| `:tabclose` / `x`        | Close the focused terminal tab.                                 |
| `gt` / `gT`              | Next / previous tab.                                            |
| `j` / `k`                | Move sidebar selection.                                         |
| `h` / `l`                | Move focus between sidebar and terminal.                        |
| `Enter`                  | Activate worktree / toggle project expansion.                   |
| `i`                      | Enter Terminal mode (keys forwarded to the PTY).                |
| `Ctrl-\ Ctrl-N`          | Leave Terminal mode (Neovim-style).                             |
| `:set theme=dark|light`  | Switch palette (persisted).                                     |
| `:set sidebar.width=N`   | Resize sidebar (also via `Ctrl-W >` / `<` / `=`).               |
| `:help`                  | Command reference.                                              |
| `Ctrl-Q` / `:q`          | Quit.                                                           |
| **Scroll wheel**         | App-bound by default; **Shift+wheel** always scrolls terminal scrollback. |

## Setup scripts

Each project can store a multi-line bash script. Open `:edit`, paste your
script, **Ctrl-S** to save. The next `:worktree <branch>` will run it as the
shell's first command in the new worktree, so you can capture e.g.:

```sh
nvm use
pnpm install
pnpm dev &
```

## Config

Everything is persisted under `$XDG_CONFIG_HOME/imbuia/` (default
`~/.config/imbuia/`):

```
~/.config/imbuia/
├── config.toml           # global: sidebar_width, theme, project list
└── projects/
    ├── myrepo.toml       # per-project: name, path, expanded, setup_script, worktrees
    └── …
```

You can edit these by hand — or use `:edit` inside the app.

The supervisor's runtime files live under `$XDG_CACHE_HOME/imbuia/` (or
`~/.cache/imbuia/`):

```
~/.cache/imbuia/
├── sock              # Unix-domain socket the TUI client attaches to
├── supervisor.pid    # supervisor PID
├── supervisor.log    # supervisor tracing log
└── imbuia.log        # client tracing log
```

## License

TBD.
