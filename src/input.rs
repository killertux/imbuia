use crate::app::Action;
use crate::layout::TermSize;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Map a crossterm event to an Action. Returns None if not actionable.
pub fn map(event: Event) -> Option<Action> {
    match event {
        Event::Key(k) => {
            // Ctrl-Q is an immediate quit — bypasses `:q` and any confirmation
            // pattern. Intentional: matches the gut-feel "kill the TUI"
            // shortcut and predates the command bar. Document if you ever
            // add a "confirm on unsaved work" flow.
            if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('q')) {
                return Some(Action::Quit);
            }
            Some(Action::Key(k))
        }
        Event::Mouse(m) => Some(Action::Mouse(m)),
        Event::Paste(s) => Some(Action::Paste(s)),
        Event::Resize(cols, rows) => Some(Action::Resize(TermSize::new(rows, cols))),
        _ => None,
    }
}

/// Encode a key event for transmission to the PTY, honoring terminal modes.
pub fn encode_key(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut buf: Vec<u8> = Vec::with_capacity(8);
    if alt {
        buf.push(0x1b);
    }
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = match c {
                    '@' | ' ' => 0x00,
                    'a'..='z' => (c as u8) - b'a' + 1,
                    'A'..='Z' => (c as u8) - b'A' + 1,
                    '[' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '7' => 0x1f,
                    '?' => 0x7f,
                    _ => c as u8,
                };
                buf.push(b);
            } else {
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
        KeyCode::Enter => buf.push(b'\r'),
        KeyCode::Tab => buf.push(b'\t'),
        KeyCode::Backspace => buf.push(0x7f),
        KeyCode::Esc => buf.push(0x1b),
        KeyCode::Left => buf.extend_from_slice(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        KeyCode::Right => buf.extend_from_slice(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        KeyCode::Up => buf.extend_from_slice(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        KeyCode::Down => buf.extend_from_slice(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
        KeyCode::Home => buf.extend_from_slice(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        KeyCode::End => buf.extend_from_slice(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        KeyCode::PageUp => buf.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => buf.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => buf.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => buf.extend_from_slice(b"\x1b[2~"),
        KeyCode::BackTab => buf.extend_from_slice(b"\x1b[Z"),
        KeyCode::F(n) => {
            let s: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => return Vec::new(),
            };
            buf.extend_from_slice(s);
        }
        _ => return Vec::new(),
    }
    buf
}
