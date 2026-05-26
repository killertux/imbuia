use super::*;
use crate::app::{Project, Worktree, mock_projects};
use crate::commands::expand_user_path;
use crate::layout::{MIN_SIDEBAR_WIDTH, TermSize};
use crate::session::FakeSession;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::path::PathBuf;

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn plain(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[test]
fn quit_stops_running_and_emits_shutdown() {
    let mut s = AppState::new();
    let cmds = reduce(&mut s, Action::Quit);
    assert!(!s.running);
    assert!(matches!(cmds.as_slice(), [Command::Shutdown]));
}

#[test]
fn key_without_active_session_is_noop() {
    let mut s = AppState::new();
    let cmds = reduce(&mut s, Action::Key(plain('x')));
    assert!(cmds.is_empty());
}

#[test]
fn ctrl_w_then_gt_widens_sidebar() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(plain('>')));
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH + 2);
    assert!(s.pending_chord.is_empty());
}

#[test]
fn ctrl_w_then_lt_narrows_sidebar() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    let _ = reduce(&mut s, Action::Key(plain('<')));
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH - 2);
}

#[test]
fn ctrl_w_then_eq_resets_sidebar() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.sidebar_width = 40;
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    let _ = reduce(&mut s, Action::Key(plain('=')));
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
}

#[test]
fn sidebar_resize_clamps_to_min() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.sidebar_width = MIN_SIDEBAR_WIDTH;
    let mut cmds = Commands::new();
    apply_sidebar_resize(&mut s, -50, &mut cmds);
    assert_eq!(s.sidebar_width, MIN_SIDEBAR_WIDTH);
}

#[test]
fn sidebar_resize_clamps_to_half_term() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.sidebar_width = 20;
    let mut cmds = Commands::new();
    apply_sidebar_resize(&mut s, 200, &mut cmds);
    assert_eq!(s.sidebar_width, 50); // half of 100
}

#[test]
fn sidebar_resize_broadcasts_to_sessions() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.projects = mock_projects();
    let fake = FakeSession::new(7);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    let cmds = reduce(&mut s, Action::Key(plain('>')));
    // Width change broadcasts a ResizePty and autosaves global config.
    assert!(matches!(
        cmds.as_slice(),
        [Command::ResizePty(7, _, _), Command::SaveGlobalConfig]
    ));
}

#[test]
fn ctrl_w_then_unknown_clears_leader_silently() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    let cmds = reduce(&mut s, Action::Key(plain('x')));
    assert!(cmds.is_empty());
    assert!(s.pending_chord.is_empty());
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
}

#[test]
fn session_exit_does_not_quit() {
    let mut s = AppState::new();
    let fake = FakeSession::new(1);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    let cmds = reduce(&mut s, Action::SessionExited(1));
    assert!(s.running);
    assert!(cmds.is_empty());
}

#[test]
fn mode_starts_normal() {
    let s = AppState::new();
    assert_eq!(s.mode, Mode::Normal);
}

#[test]
fn i_enters_terminal_mode() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::Key(plain('i')));
    assert_eq!(s.mode, Mode::Terminal);
}

#[test]
fn ctrl_4_aliases_ctrl_backslash() {
    // crossterm delivers 0x1c as Ctrl-4; this is the user-visible chord.
    let mut s = AppState::new();
    s.mode = Mode::Terminal;
    let _ = reduce(&mut s, Action::Key(ctrl('4')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(ctrl('n')));
    assert_eq!(s.mode, Mode::Normal);
}

#[test]
fn ctrl_backslash_then_ctrl_n_exits_terminal_mode() {
    let mut s = AppState::new();
    s.mode = Mode::Terminal;
    let _ = reduce(&mut s, Action::Key(ctrl('\\')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(ctrl('n')));
    assert_eq!(s.mode, Mode::Normal);
    assert!(s.pending_chord.is_empty());
}

#[test]
fn ctrl_backslash_then_other_passes_through_to_pty() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.mode = Mode::Terminal;
    let fake = FakeSession::new(9);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    s.projects = vec![Project {
        slug: "p".into(),
        name: "p".into(),
        repo_path: PathBuf::from("."),
        worktrees: vec![Worktree {
            name: "w".into(),
            path: PathBuf::from("."),
            branch: Some("w".into()),
            sessions: vec![9],
            active_tab: Some(0),
        }],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    }];
    s.active_worktree = Some((0, 0));

    let _ = reduce(&mut s, Action::Key(ctrl('\\')));
    let cmds = reduce(&mut s, Action::Key(plain('a')));
    // Both Ctrl-\ and 'a' should be forwarded.
    assert!(matches!(
        cmds.as_slice(),
        [Command::WriteKey(9, _), Command::WriteKey(9, _)]
    ));
    assert_eq!(s.mode, Mode::Terminal);
}

#[test]
fn colon_enters_command_mode_and_clears_buffer() {
    let mut s = AppState::new();
    s.command = "stale".into();
    let _ = reduce(&mut s, Action::Key(plain(':')));
    assert_eq!(s.mode, Mode::Command);
    assert!(s.command.is_empty());
}

#[test]
fn esc_cancels_command_mode() {
    let mut s = AppState::new();
    s.mode = Mode::Command;
    s.command = "tabnew".into();
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert_eq!(s.mode, Mode::Normal);
    assert!(s.command.is_empty());
}

#[test]
fn command_chars_and_backspace_edit_buffer() {
    let mut s = AppState::new();
    s.mode = Mode::Command;
    let _ = reduce(&mut s, Action::Key(plain('q')));
    let _ = reduce(&mut s, Action::Key(plain('!')));
    assert_eq!(s.command, "q!");
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
    );
    assert_eq!(s.command, "q");
}

#[test]
fn command_submit_q_quits() {
    let mut s = AppState::new();
    s.mode = Mode::Command;
    s.command = "q".into();
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(!s.running);
    assert_eq!(s.mode, Mode::Normal);
    assert!(cmds.is_empty());
}

#[test]
fn ctrl_w_chord_inert_in_terminal_mode() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.mode = Mode::Terminal;
    // No session → key just goes nowhere; the point is the chord must NOT trigger.
    let _ = reduce(&mut s, Action::Key(ctrl('w')));
    assert!(s.pending_chord.is_empty());
    let cmds = reduce(&mut s, Action::Key(plain('>')));
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    // No writes (no active session in this test).
    assert!(cmds.is_empty());
}

#[test]
fn key_in_normal_mode_does_not_forward_to_session() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let fake = FakeSession::new(5);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    s.projects = vec![Project {
        slug: "p".into(),
        name: "p".into(),
        repo_path: PathBuf::from("."),
        worktrees: vec![Worktree {
            name: "w".into(),
            path: PathBuf::from("."),
            branch: Some("w".into()),
            sessions: vec![5],
            active_tab: Some(0),
        }],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    }];
    s.active_worktree = Some((0, 0));
    let cmds = reduce(&mut s, Action::Key(plain('a')));
    assert!(cmds.is_empty());
}

#[test]
fn key_in_terminal_mode_forwards_to_focused_session() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.mode = Mode::Terminal;
    let fake = FakeSession::new(8);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    s.projects = vec![Project {
        slug: "p".into(),
        name: "p".into(),
        repo_path: PathBuf::from("."),
        worktrees: vec![Worktree {
            name: "w".into(),
            path: PathBuf::from("."),
            branch: Some("w".into()),
            sessions: vec![8],
            active_tab: Some(0),
        }],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    }];
    s.active_worktree = Some((0, 0));
    let cmds = reduce(&mut s, Action::Key(plain('a')));
    assert!(matches!(cmds.as_slice(), [Command::WriteKey(8, _)]));
}

fn mk_state_with_mock_projects() -> AppState {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.projects = mock_projects();
    s.sidebar_selection = Some((0, None)); // first project header
    s
}

#[test]
fn launch_with_no_arg_opens_popup_with_terminal_first() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.projects[0].launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude".into(),
    }];
    let cmds = submit_command(&mut s, "launch");
    assert!(cmds.is_empty());
    let popup = s.launch_popup.as_ref().expect("launch popup open");
    assert_eq!(popup.entries.len(), 2);
    assert_eq!(popup.entries[0].label, "Terminal");
    assert!(popup.entries[0].command.is_none());
    assert_eq!(popup.entries[1].label, "claude");
    assert_eq!(popup.entries[1].command.as_deref(), Some("claude"));
}

#[test]
fn launch_with_named_arg_spawns_directly() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.projects[0].launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude --resume".into(),
    }];
    let cmds = submit_command(&mut s, "launch claude");
    assert!(s.launch_popup.is_none());
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { initial_command: Some(c), .. }]
            if c == "claude --resume"
    ));
}

#[test]
fn launch_terminal_spawns_plain_shell() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    let cmds = submit_command(&mut s, "launch terminal");
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree {
            initial_command: None,
            ..
        }]
    ));
}

#[test]
fn launch_named_falls_back_to_global() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.global_launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude --global".into(),
    }];
    let cmds = submit_command(&mut s, "launch claude");
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { initial_command: Some(c), .. }]
            if c == "claude --global"
    ));
}

#[test]
fn launch_project_shadows_global_with_same_name() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.projects[0].launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude --project".into(),
    }];
    s.global_launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude --global".into(),
    }];
    let cmds = submit_command(&mut s, "launch claude");
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { initial_command: Some(c), .. }]
            if c == "claude --project"
    ));
}

#[test]
fn launch_popup_includes_globals_with_dedup() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.projects[0].launchers = vec![crate::app::Launcher {
        name: "dev".into(),
        command: "pnpm dev".into(),
    }];
    s.global_launchers = vec![
        crate::app::Launcher {
            name: "dev".into(),
            command: "ignored".into(),
        },
        crate::app::Launcher {
            name: "repl".into(),
            command: "node".into(),
        },
    ];
    let _ = submit_command(&mut s, "launch");
    let popup = s.launch_popup.as_ref().unwrap();
    let labels: Vec<_> = popup.entries.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(labels, vec!["Terminal", "dev", "repl"]);
    assert_eq!(popup.entries[1].source, crate::app::LaunchSource::Project);
    assert_eq!(popup.entries[2].source, crate::app::LaunchSource::Global);
}

#[test]
fn launch_unknown_name_sets_status() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    let cmds = submit_command(&mut s, "launch nope");
    assert!(cmds.is_empty());
    assert!(s.command_status.as_deref().unwrap().contains("nope"));
}

#[test]
fn launch_popup_enter_spawns_pinned_to_open_worktree() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    s.projects[0].launchers = vec![crate::app::Launcher {
        name: "claude".into(),
        command: "claude".into(),
    }];
    let _ = submit_command(&mut s, "launch");
    let _ = reduce(&mut s, Action::Key(plain('j'))); // move to claude
    // Swap active worktree away to verify the popup remembers its target.
    s.active_worktree = Some((0, 1));
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(s.launch_popup.is_none());
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { dest, initial_command: Some(c), .. }]
            if *dest == (0, 0) && c == "claude"
    ));
}

#[test]
fn h_and_l_switch_ui_focus() {
    let mut s = AppState::new();
    s.ui_focus = UiFocus::Sidebar;
    let _ = reduce(&mut s, Action::Key(plain('l')));
    assert_eq!(s.ui_focus, UiFocus::Terminal);
    let _ = reduce(&mut s, Action::Key(plain('h')));
    assert_eq!(s.ui_focus, UiFocus::Sidebar);
}

#[test]
fn j_walks_visible_rows() {
    let mut s = mk_state_with_mock_projects();
    // imbuia (header) → imbuia/main → imbuia/feat-x → brick (header) → ...
    let _ = reduce(&mut s, Action::Key(plain('j')));
    assert_eq!(s.sidebar_selection, Some((0, Some(0))));
    let _ = reduce(&mut s, Action::Key(plain('j')));
    assert_eq!(s.sidebar_selection, Some((0, Some(1))));
    let _ = reduce(&mut s, Action::Key(plain('j')));
    assert_eq!(s.sidebar_selection, Some((1, None)));
}

#[test]
fn j_clamps_at_last_row() {
    let mut s = mk_state_with_mock_projects();
    for _ in 0..50 {
        let _ = reduce(&mut s, Action::Key(plain('j')));
    }
    // scratch is collapsed, so last visible row is the scratch header.
    assert_eq!(s.sidebar_selection, Some((2, None)));
}

#[test]
fn k_clamps_at_first_row() {
    let mut s = mk_state_with_mock_projects();
    for _ in 0..5 {
        let _ = reduce(&mut s, Action::Key(plain('k')));
    }
    assert_eq!(s.sidebar_selection, Some((0, None)));
}

#[test]
fn jk_inert_when_terminal_focused() {
    let mut s = mk_state_with_mock_projects();
    s.ui_focus = UiFocus::Terminal;
    let initial = s.sidebar_selection;
    let _ = reduce(&mut s, Action::Key(plain('j')));
    assert_eq!(s.sidebar_selection, initial);
}

#[test]
fn enter_on_project_header_toggles_expand() {
    let mut s = mk_state_with_mock_projects();
    s.sidebar_selection = Some((2, None)); // scratch (collapsed)
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(s.projects[2].expanded);
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(!s.projects[2].expanded);
}

#[test]
fn enter_on_empty_worktree_activates_without_spawning() {
    let mut s = mk_state_with_mock_projects();
    s.sidebar_selection = Some((0, Some(0)));
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert_eq!(s.active_worktree, Some((0, 0)));
    // Focus stays on sidebar; no PTY auto-spawn.
    assert_eq!(s.ui_focus, UiFocus::Sidebar);
    assert!(cmds.is_empty());
    assert!(s.projects[0].worktrees[0].sessions.is_empty());
}

#[test]
fn enter_on_worktree_with_sessions_doesnt_respawn() {
    let mut s = mk_state_with_mock_projects();
    let fake = FakeSession::new(11);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    s.sidebar_selection = Some((0, Some(0)));
    s.projects[0].worktrees[0].active_tab = None; // pretend no active tab
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert_eq!(s.active_worktree, Some((0, 0)));
    assert!(cmds.is_empty());
    // Pre-existing sessions: active_tab restored to 0.
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
}

#[test]
fn session_spawned_appends_to_worktree_and_focuses_tab() {
    let mut s = mk_state_with_mock_projects();
    let fake = FakeSession::new(42);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (1, 0),
        },
    );
    assert_eq!(s.projects[1].worktrees[0].sessions, vec![42]);
    assert_eq!(s.projects[1].worktrees[0].active_tab, Some(0));
    assert!(s.sessions.contains_key(&42));
}

fn mk_state_with_two_tabs() -> AppState {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: FakeSession::new(101),
            dest: (0, 0),
        },
    );
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: FakeSession::new(102),
            dest: (0, 0),
        },
    );
    // active_tab is now Some(1) (last spawned).
    s
}

#[test]
fn gt_advances_tab_with_wrap() {
    let mut s = mk_state_with_two_tabs();
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
    let _ = reduce(&mut s, Action::Key(plain('g')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(plain('t')));
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
    assert!(s.pending_chord.is_empty());
}

#[test]
fn shift_g_t_retreats_tab_with_wrap() {
    let mut s = mk_state_with_two_tabs();
    s.projects[0].worktrees[0].active_tab = Some(0);
    let _ = reduce(&mut s, Action::Key(plain('g')));
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)),
    );
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
}

#[test]
fn g_then_unknown_clears_leader() {
    let mut s = mk_state_with_two_tabs();
    let _ = reduce(&mut s, Action::Key(plain('g')));
    let _ = reduce(&mut s, Action::Key(plain('z')));
    assert!(s.pending_chord.is_empty());
    // active_tab unchanged
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
}

#[test]
fn o_emits_spawn_for_active_worktree() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((1, 0));
    let cmds = reduce(&mut s, Action::Key(plain('o')));
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { dest: (1, 0), .. }]
    ));
}

#[test]
fn o_inert_without_active_worktree() {
    let mut s = mk_state_with_mock_projects();
    let cmds = reduce(&mut s, Action::Key(plain('o')));
    assert!(cmds.is_empty());
}

#[test]
fn x_closes_current_tab_and_drops_session() {
    let mut s = mk_state_with_two_tabs();
    let _ = reduce(&mut s, Action::Key(plain('x')));
    // We removed the active tab (index 1). 101 remains.
    assert_eq!(s.projects[0].worktrees[0].sessions, vec![101]);
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
    assert!(!s.sessions.contains_key(&102));
    assert!(s.sessions.contains_key(&101));
}

#[test]
fn x_on_last_tab_leaves_active_worktree_with_no_tabs() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: FakeSession::new(33),
            dest: (0, 0),
        },
    );
    let _ = reduce(&mut s, Action::Key(plain('x')));
    assert_eq!(s.active_worktree, Some((0, 0)));
    assert!(s.projects[0].worktrees[0].sessions.is_empty());
    assert_eq!(s.projects[0].worktrees[0].active_tab, None);
    assert!(s.sessions.is_empty());
}

#[test]
fn tab_keys_inert_in_terminal_mode() {
    let mut s = mk_state_with_two_tabs();
    s.mode = Mode::Terminal;
    // 'g' should be forwarded to the PTY (it's just a letter in Terminal mode).
    let cmds = reduce(&mut s, Action::Key(plain('g')));
    assert!(s.pending_chord.is_empty());
    assert!(matches!(cmds.as_slice(), [Command::WriteKey(_, _)]));
}

fn submit_command(s: &mut AppState, line: &str) -> Commands {
    s.mode = Mode::Command;
    s.command = line.into();
    reduce(
        s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
}

#[test]
fn cmd_tabnew_emits_spawn_for_active_worktree() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((2, 0));
    let cmds = submit_command(&mut s, "tabnew");
    assert!(matches!(
        cmds.as_slice(),
        [Command::SpawnInWorktree { dest: (2, 0), .. }]
    ));
}

#[test]
fn cmd_tabclose_closes_current_tab() {
    let mut s = mk_state_with_two_tabs();
    let _ = submit_command(&mut s, "tabclose");
    assert_eq!(s.projects[0].worktrees[0].sessions, vec![101]);
    assert!(!s.sessions.contains_key(&102));
}

#[test]
fn cmd_set_sidebar_width_space_separated() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = submit_command(&mut s, "set sidebar.width 30");
    assert_eq!(s.sidebar_width, 30);
    assert!(s.command_status.is_none());
}

#[test]
fn cmd_set_sidebar_width_equals_form() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = submit_command(&mut s, "set sidebar.width=40");
    assert_eq!(s.sidebar_width, 40);
    assert!(s.command_status.is_none());
}

#[test]
fn cmd_set_invalid_width_sets_status() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    let _ = submit_command(&mut s, "set sidebar.width=abc");
    assert_eq!(s.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    assert_eq!(s.command_status.as_deref(), Some("invalid width: abc"));
}

#[test]
fn cmd_unknown_sets_status() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "frobnicate");
    assert!(
        s.command_status
            .as_deref()
            .unwrap()
            .starts_with("not a command")
    );
}

#[test]
fn edit_command_opens_popup_with_existing_script() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.projects[0].setup_script = Some("npm install\nnpm run dev".into());
    s.sidebar_selection = Some((0, None));
    let _ = submit_command(&mut s, "edit");
    let popup = s.edit_popup.as_ref().expect("popup opened");
    assert_eq!(popup.project_idx, 0);
    assert_eq!(popup.textarea.lines(), &["npm install", "npm run dev"]);
}

#[test]
fn edit_popup_ctrl_s_saves_and_closes() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.sidebar_selection = Some((1, None));
    let _ = submit_command(&mut s, "edit");
    // Type a script via direct mutation (would otherwise need full key sim).
    s.edit_popup
        .as_mut()
        .unwrap()
        .textarea
        .insert_str("echo hi");
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
    );
    assert!(s.edit_popup.is_none());
    assert_eq!(s.projects[1].setup_script.as_deref(), Some("echo hi"));
    assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(1)]));
}

#[test]
fn edit_popup_esc_cancels() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.projects[0].setup_script = Some("orig".into());
    s.sidebar_selection = Some((0, None));
    let _ = submit_command(&mut s, "edit");
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(s.edit_popup.is_none());
    assert_eq!(s.projects[0].setup_script.as_deref(), Some("orig"));
}

#[test]
fn cmd_set_theme_switches_palette_and_saves() {
    use crate::app::Command;
    use crate::theme::ThemeKind;
    let mut s = AppState::new();
    let cmds = submit_command(&mut s, "set theme=light");
    assert_eq!(s.theme.kind, ThemeKind::Light);
    assert!(cmds.iter().any(|c| matches!(c, Command::SaveGlobalConfig)));

    let cmds = submit_command(&mut s, "set theme=dark");
    assert_eq!(s.theme.kind, ThemeKind::Dark);
    assert!(cmds.iter().any(|c| matches!(c, Command::SaveGlobalConfig)));

    let _ = submit_command(&mut s, "set theme=neon");
    assert!(
        s.command_status
            .as_deref()
            .unwrap()
            .starts_with("invalid theme")
    );
}

#[test]
fn cmd_set_unknown_key_sets_status() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "set nope=foo");
    assert_eq!(s.command_status.as_deref(), Some("unknown setting: nope"));
}

#[test]
fn colon_clears_previous_status() {
    let mut s = AppState::new();
    s.command_status = Some("stale".into());
    let _ = reduce(&mut s, Action::Key(plain(':')));
    assert_eq!(s.mode, Mode::Command);
    assert!(s.command_status.is_none());
}

fn mouse_at(col: u16, row: u16, kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn left_down(col: u16, row: u16) -> MouseEvent {
    mouse_at(col, row, MouseEventKind::Down(MouseButton::Left))
}

#[test]
fn click_on_sidebar_worktree_activates_it() {
    let mut s = mk_state_with_mock_projects();
    s.term_size = TermSize::new(40, 100);
    // Visible rows (sidebar inner starts at row 1):
    //   row 1: imbuia header
    //   row 2: imbuia/main
    //   row 3: imbuia/feat-x
    //   row 4: brick header
    //   row 5: brick/main
    let cmds = reduce(&mut s, Action::Mouse(left_down(2, 2)));
    assert_eq!(s.active_worktree, Some((0, 0)));
    assert_eq!(s.ui_focus, UiFocus::Sidebar);
    assert!(cmds.is_empty());
    assert!(s.projects[0].worktrees[0].sessions.is_empty());
}

#[test]
fn click_on_sidebar_project_header_toggles_expand() {
    let mut s = mk_state_with_mock_projects();
    s.term_size = TermSize::new(40, 100);
    // scratch header position depends on imbuia (1 header + 2 worktrees)
    // + brick (1 header + 2 worktrees) = 6 rows before scratch.
    // scratch header is at visible index 6 → row = sidebar.y + 1 + 6 = 7.
    let cmds = reduce(&mut s, Action::Mouse(left_down(2, 7)));
    assert!(s.projects[2].expanded);
    // Expand-toggle persists.
    assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(2)]));
    assert_eq!(s.ui_focus, UiFocus::Sidebar);
}

#[test]
fn click_on_tab_bar_switches_tab() {
    let mut s = mk_state_with_two_tabs();
    s.term_size = TermSize::new(40, 100);
    // Tab 0 spans cols 24..27 inside tab_bar (sidebar=24).
    // Tab bar row 0 = chrome.tab_bar.y. Click col 24 (start of tab 0).
    let _ = reduce(&mut s, Action::Mouse(left_down(24, 0)));
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(0));
    assert_eq!(s.ui_focus, UiFocus::Terminal);
}

#[test]
fn click_on_tab_bar_beyond_last_tab_is_noop() {
    let mut s = mk_state_with_two_tabs();
    s.term_size = TermSize::new(40, 100);
    // Two tabs occupy cols 24..31; col 40 is past them.
    let _ = reduce(&mut s, Action::Mouse(left_down(40, 0)));
    assert_eq!(s.projects[0].worktrees[0].active_tab, Some(1));
}

#[test]
fn left_click_in_terminal_auto_enters_terminal_mode() {
    let mut s = mk_state_with_two_tabs();
    s.term_size = TermSize::new(40, 100);
    assert_eq!(s.mode, Mode::Normal);
    let _ = reduce(&mut s, Action::Mouse(left_down(30, 5)));
    assert_eq!(s.mode, Mode::Terminal);
}

#[test]
fn scroll_in_sidebar_shifts_offset_and_clamps() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(6, 100); // small viewport: ~5 visible rows
    s.projects = mock_projects();
    s.sidebar_selection = Some((0, None));

    // Scroll down twice — offset should grow but never exceed (total - visible).
    let down = mouse_at(2, 2, MouseEventKind::ScrollDown);
    let _ = reduce(&mut s, Action::Mouse(down));
    let after_one = s.sidebar_scroll;
    assert!(after_one > 0);
    let _ = reduce(&mut s, Action::Mouse(down));
    // Scroll up — should bring offset back to zero (saturating).
    let up = mouse_at(2, 2, MouseEventKind::ScrollUp);
    let _ = reduce(&mut s, Action::Mouse(up));
    let _ = reduce(&mut s, Action::Mouse(up));
    assert_eq!(s.sidebar_scroll, 0);
}

#[test]
fn keyboard_nav_auto_scrolls_sidebar() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(4, 100); // 3 visible rows after title
    s.projects = mock_projects();
    s.sidebar_selection = Some((0, None));
    // Press j several times to walk past the bottom of the viewport.
    for _ in 0..5 {
        let _ = reduce(&mut s, Action::Key(plain('j')));
    }
    assert!(s.sidebar_scroll > 0);
}

#[test]
fn scroll_in_terminal_doesnt_change_mode() {
    let mut s = mk_state_with_two_tabs();
    s.term_size = TermSize::new(40, 100);
    let scroll = mouse_at(30, 5, MouseEventKind::ScrollDown);
    let _ = reduce(&mut s, Action::Mouse(scroll));
    assert_eq!(s.mode, Mode::Normal);
}

#[test]
fn click_in_terminal_forwards_to_session() {
    let mut s = mk_state_with_two_tabs();
    s.term_size = TermSize::new(40, 100);
    // chrome: sidebar=24, tab_bar=2 rows, action_bar=1 row.
    // Terminal rect starts at col 24, row 2.
    let click = left_down(30, 5);
    let cmds = reduce(&mut s, Action::Mouse(click));
    assert_eq!(s.ui_focus, UiFocus::Terminal);
    match cmds.as_slice() {
        [Command::WriteMouse(102, ev)] => {
            assert_eq!(ev.column, 30 - 24);
            assert_eq!(ev.row, 5 - 2);
        }
        other => panic!("unexpected commands: {other:?}"),
    }
}

#[test]
fn non_left_click_in_sidebar_only_changes_focus() {
    let mut s = mk_state_with_mock_projects();
    s.term_size = TermSize::new(40, 100);
    s.ui_focus = UiFocus::Terminal;
    let m = mouse_at(2, 2, MouseEventKind::ScrollDown);
    let cmds = reduce(&mut s, Action::Mouse(m));
    // Focus moved to sidebar; no activation happened.
    assert_eq!(s.ui_focus, UiFocus::Sidebar);
    assert_eq!(s.active_worktree, None);
    assert!(cmds.is_empty());
}

#[test]
fn help_command_opens_popup() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "help");
    assert!(s.help_open);
}

#[test]
fn help_alias_h_also_opens_popup() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "h");
    assert!(s.help_open);
}

#[test]
fn esc_closes_help_popup() {
    let mut s = AppState::new();
    s.help_open = true;
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(!s.help_open);
}

#[test]
fn q_closes_help_popup() {
    let mut s = AppState::new();
    s.help_open = true;
    let _ = reduce(&mut s, Action::Key(plain('q')));
    assert!(!s.help_open);
}

#[test]
fn j_scrolls_help_popup_without_closing() {
    let mut s = AppState::new();
    s.help_open = true;
    s.help_max_scroll.set(20);
    let _ = reduce(&mut s, Action::Key(plain('j')));
    assert!(s.help_open);
    assert_eq!(s.help_scroll, 1);
    let _ = reduce(&mut s, Action::Key(plain('k')));
    assert_eq!(s.help_scroll, 0);
}

#[test]
fn help_scroll_clamps_to_max() {
    let mut s = AppState::new();
    s.help_open = true;
    s.help_max_scroll.set(5);
    for _ in 0..50 {
        let _ = reduce(&mut s, Action::Key(plain('j')));
    }
    assert_eq!(s.help_scroll, 5);
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
    );
    assert_eq!(s.help_scroll, 0);
}

#[test]
fn left_click_dismisses_help_popup() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.help_open = true;
    let _ = reduce(&mut s, Action::Mouse(left_down(50, 10)));
    assert!(!s.help_open);
}

#[test]
fn colon_dismisses_help_popup() {
    let mut s = AppState::new();
    s.help_open = true;
    let _ = reduce(&mut s, Action::Key(plain(':')));
    assert!(!s.help_open);
    assert_eq!(s.mode, Mode::Command);
}

#[test]
fn expand_user_path_handles_tilde() {
    let home = std::path::Path::new("/tmp/imbuia-test-home");
    assert_eq!(expand_user_path("~", Some(home)), PathBuf::from(home));
    assert_eq!(
        expand_user_path("~/projects/foo", Some(home)),
        PathBuf::from("/tmp/imbuia-test-home/projects/foo")
    );
    assert_eq!(
        expand_user_path("./rel", Some(home)),
        PathBuf::from("./rel")
    );
    assert_eq!(
        expand_user_path("/abs/x", Some(home)),
        PathBuf::from("/abs/x")
    );
    // With no HOME, `~` stays literal.
    assert_eq!(expand_user_path("~", None), PathBuf::from("~"));
}

#[test]
fn cmd_open_with_arg_emits_open_command() {
    let mut s = AppState::new();
    let cmds = submit_command(&mut s, "open /tmp/some-repo");
    assert!(matches!(
        cmds.as_slice(),
        [Command::OpenProject { path, setup_script: None, .. }]
            if path.as_os_str() == "/tmp/some-repo"
    ));
    assert!(s.open_project_popup.is_none());
}

#[test]
fn cmd_open_without_arg_opens_popup() {
    let mut s = AppState::new();
    let cmds = submit_command(&mut s, "open");
    assert!(cmds.is_empty());
    assert!(s.open_project_popup.is_some());
}

#[test]
fn space_leader_o_opens_open_project_popup() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::Key(plain(' ')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(plain('o')));
    assert!(s.pending_chord.is_empty());
    assert!(s.open_project_popup.is_some());
}

#[test]
fn space_leader_l_opens_launch_popup() {
    let mut s = mk_state_with_mock_projects();
    s.active_worktree = Some((0, 0));
    let _ = reduce(&mut s, Action::Key(plain(' ')));
    let _ = reduce(&mut s, Action::Key(plain('l')));
    assert!(s.launch_popup.is_some());
}

#[test]
fn space_leader_q_quits() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::Key(plain(' ')));
    let _ = reduce(&mut s, Action::Key(plain('q')));
    assert!(!s.running);
}

#[test]
fn space_leader_esc_cancels() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::Key(plain(' ')));
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(s.pending_chord.is_empty());
    assert!(s.open_project_popup.is_none());
    assert!(s.running);
}

#[test]
fn update_checked_some_sets_available_update() {
    let mut s = AppState::new();
    s.update_status = crate::app::UpdateStatus::Checking;
    let info = crate::updater::UpdateInfo {
        latest_tag: "v9.9.9".into(),
        latest_version: semver::Version::parse("9.9.9").unwrap(),
    };
    let _ = reduce(&mut s, Action::UpdateChecked(Ok(Some(info))));
    assert_eq!(
        s.available_update.as_ref().map(|i| i.latest_tag.as_str()),
        Some("v9.9.9")
    );
    assert_eq!(s.update_status, crate::app::UpdateStatus::Idle);
}

#[test]
fn update_checked_none_clears_available_update() {
    let mut s = AppState::new();
    s.available_update = Some(crate::updater::UpdateInfo {
        latest_tag: "v9.9.9".into(),
        latest_version: semver::Version::parse("9.9.9").unwrap(),
    });
    let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
    assert!(s.available_update.is_none());
}

#[test]
fn update_installed_no_restart_shows_friendly_status() {
    let mut s = AppState::new();
    let outcome = crate::updater::InstallOutcome {
        installed_to: std::path::PathBuf::from("/tmp"),
        supervisor_restart_required: false,
        installed_tag: "v0.4.0".into(),
    };
    let _ = reduce(&mut s, Action::UpdateInstalled(Ok(outcome)));
    let status = s.command_status.as_deref().unwrap();
    assert!(status.contains("v0.4.0"));
    assert!(status.contains("relaunch"));
    assert!(!status.contains(":rs"));
    assert_eq!(
        s.update_status,
        crate::app::UpdateStatus::InstalledPendingRestart
    );
}

#[test]
fn update_installed_with_protocol_bump_asks_for_rs() {
    let mut s = AppState::new();
    let outcome = crate::updater::InstallOutcome {
        installed_to: std::path::PathBuf::from("/tmp"),
        supervisor_restart_required: true,
        installed_tag: "v0.4.0".into(),
    };
    let _ = reduce(&mut s, Action::UpdateInstalled(Ok(outcome)));
    let status = s.command_status.as_deref().unwrap();
    assert!(status.contains(":rs"));
}

#[test]
fn periodic_update_check_emits_command_when_idle() {
    let mut s = AppState::new();
    let cmds = reduce(&mut s, Action::PeriodicUpdateCheck);
    assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
    assert_eq!(s.update_status, crate::app::UpdateStatus::Checking);
}

#[test]
fn periodic_update_check_is_inert_while_installing() {
    let mut s = AppState::new();
    s.update_status = crate::app::UpdateStatus::Installing;
    let cmds = reduce(&mut s, Action::PeriodicUpdateCheck);
    assert!(cmds.is_empty());
}

#[test]
fn cmd_update_check_pushes_check_command() {
    let mut s = AppState::new();
    let cmds = submit_command(&mut s, "update check");
    assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
    assert_eq!(s.update_status, crate::app::UpdateStatus::Checking);
}

#[test]
fn cmd_update_with_cached_info_installs_directly() {
    let mut s = AppState::new();
    s.available_update = Some(crate::updater::UpdateInfo {
        latest_tag: "v9.9.9".into(),
        latest_version: semver::Version::parse("9.9.9").unwrap(),
    });
    let cmds = submit_command(&mut s, "update");
    assert!(matches!(
        cmds.as_slice(),
        [Command::InstallUpdate { tag }] if tag == "v9.9.9"
    ));
    assert_eq!(s.update_status, crate::app::UpdateStatus::Installing);
}

#[test]
fn cmd_update_without_cache_kicks_a_check() {
    let mut s = AppState::new();
    let cmds = submit_command(&mut s, "update");
    assert!(matches!(cmds.as_slice(), [Command::CheckForUpdate]));
    assert!(s.auto_install_after_check);
}

#[test]
fn cmd_update_check_does_not_arm_auto_install() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "update check");
    assert!(!s.auto_install_after_check);
}

#[test]
fn update_check_then_some_auto_installs_when_armed() {
    // Simulates: user types :update with no cached info, the check fires,
    // and a newer release is found — we should auto-install instead of
    // leaving the user staring at "checking for updates…".
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "update");
    assert!(s.auto_install_after_check);
    let info = crate::updater::UpdateInfo {
        latest_tag: "v9.9.9".into(),
        latest_version: semver::Version::parse("9.9.9").unwrap(),
    };
    let cmds = reduce(&mut s, Action::UpdateChecked(Ok(Some(info))));
    assert!(!s.auto_install_after_check);
    assert_eq!(s.update_status, crate::app::UpdateStatus::Installing);
    assert!(matches!(
        cmds.as_slice(),
        [Command::InstallUpdate { tag }] if tag == "v9.9.9"
    ));
}

#[test]
fn update_check_then_none_when_armed_says_up_to_date() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "update");
    let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
    assert!(!s.auto_install_after_check);
    assert!(
        s.command_status
            .as_deref()
            .unwrap()
            .contains("already on the latest")
    );
}

#[test]
fn background_check_does_not_set_command_status() {
    // Hourly tick → PeriodicUpdateCheck → CheckForUpdate. When the check
    // returns None, the status row should NOT chirp at the user.
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::PeriodicUpdateCheck);
    assert!(!s.auto_install_after_check);
    let _ = reduce(&mut s, Action::UpdateChecked(Ok(None)));
    assert!(s.command_status.is_none());
}

#[test]
fn open_popup_chars_and_enter_emit_open_command() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "open");
    let _ = reduce(&mut s, Action::Key(plain('/')));
    let _ = reduce(&mut s, Action::Key(plain('t')));
    let _ = reduce(&mut s, Action::Key(plain('m')));
    let _ = reduce(&mut s, Action::Key(plain('p')));
    assert_eq!(s.open_project_popup.as_ref().unwrap().path, "/tmp");
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(s.open_project_popup.is_none());
    assert!(matches!(
        cmds.as_slice(),
        [Command::OpenProject { path, setup_script: None, .. }]
            if path.as_os_str() == "/tmp"
    ));
}

#[test]
fn open_popup_tab_then_script_then_ctrl_s_passes_script() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "open");
    let _ = reduce(&mut s, Action::Key(plain('/')));
    let _ = reduce(&mut s, Action::Key(plain('x')));
    // Tab to script, type a command, then Ctrl-S to submit.
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    );
    let _ = reduce(&mut s, Action::Key(plain('e')));
    let _ = reduce(&mut s, Action::Key(plain('c')));
    let _ = reduce(&mut s, Action::Key(plain('h')));
    let _ = reduce(&mut s, Action::Key(plain('o')));
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
    );
    assert!(s.open_project_popup.is_none());
    assert!(matches!(
        cmds.as_slice(),
        [Command::OpenProject { path, setup_script: Some(script), .. }]
            if path.as_os_str() == "/x" && script == "echo"
    ));
}

#[test]
fn open_popup_esc_cancels_without_dispatch() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "open");
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(s.open_project_popup.is_none());
    assert!(cmds.is_empty());
}

#[test]
fn worktree_command_requires_selected_project() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "worktree feat-x");
    assert!(
        s.command_status
            .as_deref()
            .unwrap()
            .contains("select a project")
    );
}

#[test]
fn cmd_open_sets_pending_op() {
    let mut s = AppState::new();
    let _ = submit_command(&mut s, "open /tmp/x");
    assert!(s.pending_op.as_deref().unwrap().contains("Opening"));
}

#[test]
fn project_opened_clears_pending_op() {
    let mut s = AppState::new();
    s.pending_op = Some("Opening …".into());
    let project = Project {
        slug: "x".into(),
        name: "x".into(),
        repo_path: PathBuf::from("/tmp/x"),
        worktrees: vec![],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    };
    let _ = reduce(
        &mut s,
        Action::ProjectOpened {
            project,
            import_existing: false,
        },
    );
    assert!(s.pending_op.is_none());
}

#[test]
fn worktree_added_clears_pending_op() {
    let mut s = mk_state_with_mock_projects();
    s.pending_op = Some("Creating worktree…".into());
    let _ = reduce(
        &mut s,
        Action::WorktreeAdded {
            project_idx: 0,
            worktree: Worktree {
                name: "x".into(),
                path: PathBuf::from("/tmp/x"),
                branch: Some("x".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
        },
    );
    assert!(s.pending_op.is_none());
}

#[test]
fn operation_failed_clears_pending_op() {
    let mut s = AppState::new();
    s.pending_op = Some("Opening…".into());
    let _ = reduce(&mut s, Action::OperationFailed("nope".into()));
    assert!(s.pending_op.is_none());
    assert_eq!(s.command_status.as_deref(), Some("nope"));
}

#[test]
fn worktree_command_with_arg_emits_add_worktree() {
    let mut s = mk_state_with_mock_projects();
    s.sidebar_selection = Some((0, None));
    let cmds = submit_command(&mut s, "worktree feat-x");
    assert!(matches!(
        cmds.as_slice(),
        [Command::AddWorktree { project_idx: 0, branch, .. }] if branch == "feat-x"
    ));
}

#[test]
fn project_opened_action_appends_and_saves() {
    let mut s = AppState::new();
    let project = Project {
        slug: "test".into(),
        name: "test".into(),
        repo_path: PathBuf::from("/tmp/test"),
        worktrees: vec![],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    };
    let cmds = reduce(
        &mut s,
        Action::ProjectOpened {
            project,
            import_existing: false,
        },
    );
    assert_eq!(s.projects.len(), 1);
    assert_eq!(s.projects[0].slug, "test");
    assert!(matches!(
        cmds.as_slice(),
        [Command::SaveGlobalConfig, Command::SaveProjectConfig(0)]
    ));
}

#[test]
fn worktree_added_action_appends_and_saves() {
    let mut s = mk_state_with_mock_projects();
    let cmds = reduce(
        &mut s,
        Action::WorktreeAdded {
            project_idx: 0,
            worktree: Worktree {
                name: "feat-y".into(),
                path: PathBuf::from("/tmp/feat-y"),
                branch: Some("feat-y".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
        },
    );
    assert_eq!(s.projects[0].worktrees.len(), 3);
    assert_eq!(s.projects[0].worktrees.last().unwrap().name, "feat-y");
    // Two commands: persist the project + spawn a terminal in the new worktree.
    assert!(matches!(
        cmds.as_slice(),
        [
            Command::SaveProjectConfig(0),
            Command::SpawnInWorktree {
                dest: (0, 2),
                initial_command: None,
                ..
            },
        ]
    ));
    assert_eq!(s.active_worktree, Some((0, 2)));
}

#[test]
fn worktree_added_with_setup_script_passes_it_to_spawn() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.projects = mock_projects();
    s.projects[0].setup_script = Some("npm install".into());
    let cmds = reduce(
        &mut s,
        Action::WorktreeAdded {
            project_idx: 0,
            worktree: Worktree {
                name: "feat-z".into(),
                path: PathBuf::from("/tmp/feat-z"),
                branch: Some("feat-z".into()),
                sessions: Vec::new(),
                active_tab: None,
            },
        },
    );
    let spawn = cmds
        .iter()
        .find_map(|c| match c {
            Command::SpawnInWorktree {
                initial_command, ..
            } => Some(initial_command.clone()),
            _ => None,
        })
        .expect("spawn command emitted");
    assert_eq!(spawn.as_deref(), Some("npm install"));
}

#[test]
fn worktree_remove_command_emits_remove_when_non_main() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    // mock_projects has imbuia with [main, feat-x]; give feat-x a path
    // that differs from repo_path so it doesn't trip the main-guard.
    s.projects[0].worktrees[1].path = PathBuf::from("/tmp/feat-x");
    s.sidebar_selection = Some((0, Some(1)));
    let cmds = submit_command(&mut s, "worktree-remove");
    // First step: a confirmation is staged, nothing is dispatched yet.
    assert!(cmds.is_empty());
    assert!(matches!(
        s.pending_confirm,
        Some(PendingConfirm::RemoveWorktree {
            project_idx: 0,
            worktree_idx: 1,
            ..
        })
    ));
    // Confirming with `y` emits the RemoveWorktree command and clears the
    // confirmation in favor of a pending_op for the action bar.
    let cmds = reduce(&mut s, Action::Key(plain('y')));
    assert!(s.pending_confirm.is_none());
    assert!(s.pending_op.is_some());
    assert!(matches!(
        cmds.as_slice(),
        [Command::RemoveWorktree {
            project_idx: 0,
            worktree_idx: 1,
            ..
        }]
    ));
}

#[test]
fn worktree_remove_confirmation_cancelled_by_n() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.projects[0].worktrees[1].path = PathBuf::from("/tmp/feat-x");
    s.sidebar_selection = Some((0, Some(1)));
    let _ = submit_command(&mut s, "worktree-remove");
    assert!(s.pending_confirm.is_some());
    let cmds = reduce(&mut s, Action::Key(plain('n')));
    assert!(s.pending_confirm.is_none());
    assert!(s.pending_op.is_none());
    assert!(cmds.is_empty());
}

#[test]
fn worktree_remove_refuses_main_worktree() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    // main worktree's path == repo_path; sidebar_selection on it.
    s.sidebar_selection = Some((0, Some(0)));
    let cmds = submit_command(&mut s, "worktree-remove");
    assert!(cmds.is_empty());
    assert_eq!(
        s.command_status.as_deref(),
        Some("can't remove the main worktree")
    );
}

#[test]
fn worktree_removed_action_drops_entry_and_fixes_selection() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.active_worktree = Some((0, 1));
    s.sidebar_selection = Some((0, Some(1)));
    let _ = reduce(
        &mut s,
        Action::WorktreeRemoved {
            project_idx: 0,
            worktree_idx: 1,
        },
    );
    assert_eq!(s.projects[0].worktrees.len(), 1);
    assert_eq!(s.active_worktree, None);
    assert_eq!(s.sidebar_selection, Some((0, Some(0))));
}

#[test]
fn operation_failed_sets_command_status() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::OperationFailed("bad path".into()));
    assert_eq!(s.command_status.as_deref(), Some("bad path"));
}

#[test]
fn resize_broadcasts_terminal_pane_size() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 100);
    s.projects = mock_projects();
    let fake = FakeSession::new(3);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    let cmds = reduce(&mut s, Action::Resize(TermSize::new(30, 120)));
    // chrome: action_bar 1 row, tab_bar 2 rows → terminal rows = 27
    // sidebar 24 cols → terminal cols = 96
    match cmds.as_slice() {
        [Command::ResizePty(3, rows, cols)] => {
            assert_eq!(*rows, 27);
            assert_eq!(*cols, 96);
        }
        other => panic!("unexpected commands: {other:?}"),
    }
}

#[test]
fn cmd_usage_opens_popup_and_subscribes() {
    let mut s = mk_state_with_mock_projects();
    let cmds = submit_command(&mut s, "usage");
    assert!(s.usage_popup.is_some());
    assert!(matches!(cmds.as_slice(), [Command::SubscribeUsage]));
}

#[test]
fn usage_popup_esc_closes_and_unsubscribes() {
    let mut s = mk_state_with_mock_projects();
    s.usage_popup = Some(crate::app::UsagePopup::new());
    let cmds = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(s.usage_popup.is_none());
    assert!(matches!(cmds.as_slice(), [Command::UnsubscribeUsage]));
}

#[test]
fn usage_received_populates_report() {
    use crate::ipc::{ProcessNode, SessionUsage, UsageReport};
    let mut s = mk_state_with_mock_projects();
    s.usage_popup = Some(crate::app::UsagePopup::new());
    let report = UsageReport {
        sessions: vec![SessionUsage {
            session_id: 7,
            project_slug: "imbuia".into(),
            worktree_name: "main".into(),
            root: ProcessNode {
                pid: 1,
                name: "zsh".into(),
                rss_bytes: 0,
                cpu_percent: 0.0,
                children: vec![],
            },
        }],
        supervisor: ProcessNode {
            pid: 2,
            name: "imbuia".into(),
            rss_bytes: 0,
            cpu_percent: 0.0,
            children: vec![],
        },
        client: None,
        ts_ms: 1,
        cpu_count: 4,
    };
    let _ = reduce(&mut s, Action::UsageReceived(report));
    let popup = s.usage_popup.as_ref().unwrap();
    assert_eq!(popup.report.as_ref().unwrap().sessions.len(), 1);
}

#[test]
fn usage_popup_enter_expands_selected_session() {
    use crate::ipc::{ProcessNode, SessionUsage, UsageReport};
    let mut s = mk_state_with_mock_projects();
    s.usage_popup = Some(crate::app::UsagePopup::new());
    let report = UsageReport {
        sessions: vec![SessionUsage {
            session_id: 42,
            project_slug: "imbuia".into(),
            worktree_name: "main".into(),
            root: ProcessNode {
                pid: 1,
                name: "zsh".into(),
                rss_bytes: 0,
                cpu_percent: 0.0,
                children: vec![],
            },
        }],
        supervisor: ProcessNode {
            pid: 2,
            name: "imbuia".into(),
            rss_bytes: 0,
            cpu_percent: 0.0,
            children: vec![],
        },
        client: None,
        ts_ms: 0,
        cpu_count: 4,
    };
    let _ = reduce(&mut s, Action::UsageReceived(report));
    let _ = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(s.usage_popup.as_ref().unwrap().expanded.contains(&42));
}

#[test]
fn pr_statuses_fetched_inserts_by_worktree_idx() {
    use crate::app::PrStatus;
    let mut s = AppState::new();
    s.projects = mock_projects();
    // imbuia has 2 worktrees. Report Running for #1, nothing for #0.
    let cmds = reduce(
        &mut s,
        Action::PrStatusesFetched {
            project_idx: 0,
            statuses: vec![(0, None), (1, Some(PrStatus::Running))],
        },
    );
    assert!(cmds.is_empty());
    assert_eq!(s.pr_statuses.get(&(0, 1)), Some(&PrStatus::Running));
    assert!(!s.pr_statuses.contains_key(&(0, 0)));
}

#[test]
fn pr_statuses_fetched_clears_when_none() {
    use crate::app::PrStatus;
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.pr_statuses.insert((0, 1), PrStatus::Failed);
    let _ = reduce(
        &mut s,
        Action::PrStatusesFetched {
            project_idx: 0,
            statuses: vec![(1, None)],
        },
    );
    assert!(!s.pr_statuses.contains_key(&(0, 1)));
}

#[test]
fn periodic_pr_check_emits_one_fetch_per_enabled_project() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.projects[0].github_enabled = true;
    s.projects[2].github_enabled = true;
    let cmds = reduce(&mut s, Action::PeriodicPrCheck);
    assert_eq!(cmds.len(), 2);
    let mut idxs: Vec<usize> = cmds
        .iter()
        .filter_map(|c| match c {
            Command::FetchPrStatuses { project_idx, .. } => Some(*project_idx),
            _ => None,
        })
        .collect();
    idxs.sort();
    assert_eq!(idxs, vec![0, 2]);
}

#[test]
fn worktree_removed_re_keys_pr_statuses() {
    use crate::app::PrStatus;
    let mut s = AppState::new();
    s.projects = mock_projects();
    // imbuia has [main, feat-x]; add a third so we can verify shift-down.
    s.projects[0].worktrees.push(Worktree {
        name: "extra".into(),
        path: PathBuf::from("/tmp/extra"),
        branch: Some("extra".into()),
        sessions: vec![],
        active_tab: None,
    });
    s.pr_statuses.insert((0, 0), PrStatus::Running);
    s.pr_statuses.insert((0, 1), PrStatus::Failed);
    s.pr_statuses.insert((0, 2), PrStatus::Merged);
    // Remove the middle worktree.
    let _ = reduce(
        &mut s,
        Action::WorktreeRemoved {
            project_idx: 0,
            worktree_idx: 1,
        },
    );
    assert_eq!(s.pr_statuses.get(&(0, 0)), Some(&PrStatus::Running));
    // (0,1) used to be "extra" / Merged after the shift.
    assert_eq!(s.pr_statuses.get(&(0, 1)), Some(&PrStatus::Merged));
    assert!(!s.pr_statuses.contains_key(&(0, 2)));
}

#[test]
fn gh_disable_clears_pr_statuses_for_project() {
    use crate::app::PrStatus;
    let mut s = AppState::new();
    s.projects = mock_projects();
    s.projects[0].github_enabled = true;
    s.pr_statuses.insert((0, 0), PrStatus::Failed);
    s.pr_statuses.insert((1, 0), PrStatus::Merged);
    s.sidebar_selection = Some((0, None));
    let _ = submit_command(&mut s, "gh-disable");
    assert!(!s.projects[0].github_enabled);
    assert!(!s.pr_statuses.contains_key(&(0, 0)));
    // Other project's entry untouched.
    assert_eq!(s.pr_statuses.get(&(1, 0)), Some(&PrStatus::Merged));
}

#[test]
fn g_alone_leaves_chord_pending_gt_clears_it() {
    let mut s = AppState::new();
    let _ = reduce(&mut s, Action::Key(plain('g')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(plain('t')));
    assert!(s.pending_chord.is_empty());
}

#[test]
fn custom_binding_via_overlay_remaps_open_tab() {
    use crate::keybinds::{BindableAction, load_overlay};
    let mut overlay = std::collections::BTreeMap::new();
    overlay.insert("open_tab".into(), "<Space>t".into());
    let mut s = AppState::new();
    s.keymap = std::sync::Arc::new(load_overlay(&overlay));
    // Old default `o` should now do nothing.
    let _ = reduce(&mut s, Action::Key(plain('o')));
    assert!(s.pending_chord.is_empty());
    // New chord <Space>t fires OpenTab.
    let _ = reduce(&mut s, Action::Key(plain(' ')));
    assert_eq!(s.pending_chord.len(), 1);
    let _ = reduce(&mut s, Action::Key(plain('t')));
    assert!(s.pending_chord.is_empty());
    assert_eq!(
        s.keymap.binding_for(BindableAction::OpenTab).as_deref(),
        Some("<Space>t")
    );
}

#[test]
fn worktrees_imported_appends_new_and_skips_dupes() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    let main_path = s.projects[0].worktrees[0].path.clone();
    let feat_path = s.projects[0].worktrees[1].path.clone();
    let entries = vec![
        Worktree {
            name: "main".into(),
            path: main_path,
            branch: Some("main".into()),
            sessions: Vec::new(),
            active_tab: None,
        },
        Worktree {
            name: "feat-x".into(),
            path: feat_path,
            branch: Some("feat-x".into()),
            sessions: Vec::new(),
            active_tab: None,
        },
        Worktree {
            name: "fresh-1".into(),
            path: PathBuf::from("/tmp/fresh-1"),
            branch: Some("fresh-1".into()),
            sessions: Vec::new(),
            active_tab: None,
        },
        Worktree {
            name: "fresh-2".into(),
            path: PathBuf::from("/tmp/fresh-2"),
            branch: Some("fresh-2".into()),
            sessions: Vec::new(),
            active_tab: None,
        },
    ];
    let cmds = reduce(
        &mut s,
        Action::WorktreesImported {
            project_idx: 0,
            entries,
        },
    );
    assert_eq!(s.projects[0].worktrees.len(), 4);
    assert!(matches!(cmds.as_slice(), [Command::SaveProjectConfig(0)]));
    assert_eq!(s.command_status.as_deref(), Some("imported 2 worktrees"));
}

#[test]
fn worktrees_imported_empty_set_says_nothing_to_import() {
    let mut s = AppState::new();
    s.projects = mock_projects();
    let cmds = reduce(
        &mut s,
        Action::WorktreesImported {
            project_idx: 0,
            entries: Vec::new(),
        },
    );
    assert!(cmds.is_empty());
    assert_eq!(s.projects[0].worktrees.len(), 2);
    assert_eq!(
        s.command_status.as_deref(),
        Some("no new worktrees to import")
    );
}

#[test]
fn terminal_chord_replay_forwards_buffered_keys_on_mismatch() {
    let mut s = AppState::new();
    s.term_size = TermSize::new(40, 200);
    s.mode = Mode::Terminal;
    let fake = FakeSession::new(7);
    let _ = reduce(
        &mut s,
        Action::SessionSpawned {
            session: fake,
            dest: (0, 0),
        },
    );
    s.projects = vec![Project {
        slug: "p".into(),
        name: "p".into(),
        repo_path: PathBuf::from("."),
        worktrees: vec![Worktree {
            name: "w".into(),
            path: PathBuf::from("."),
            branch: Some("w".into()),
            sessions: vec![7],
            active_tab: Some(0),
        }],
        expanded: true,
        setup_script: None,
        launchers: Vec::new(),
        github_enabled: false,
        gh_poll_interval_secs: None,
    }];
    s.active_worktree = Some((0, 0));

    // Ctrl-\ alone is a prefix — buffered, nothing reaches the PTY.
    let cmds = reduce(&mut s, Action::Key(ctrl('\\')));
    assert!(cmds.is_empty());
    assert_eq!(s.pending_chord.len(), 1);
    // Mismatch: replay both buffered keys to the PTY in order.
    let cmds = reduce(&mut s, Action::Key(plain('q')));
    assert_eq!(cmds.len(), 2);
    assert!(matches!(
        cmds.as_slice(),
        [Command::WriteKey(7, _), Command::WriteKey(7, _)]
    ));
    assert!(s.pending_chord.is_empty());
    assert_eq!(s.mode, Mode::Terminal);
}
