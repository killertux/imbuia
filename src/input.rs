use crate::app::Action;
use crate::layout::TermSize;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Map a crossterm event to an Action. Returns None if not actionable.
pub fn map(event: Event) -> Option<Action> {
    match event {
        Event::Key(k) => {
            // We enable the kitty keyboard protocol on our host terminal (see
            // `main.rs`) so modified functional keys (Shift+Enter, …) arrive
            // with their modifiers intact. That can also surface key-release
            // events; we only act on presses/repeats so chords don't fire
            // twice.
            if k.kind == KeyEventKind::Release {
                return None;
            }
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

/// Which keyboard-input protocol the *inner* app has negotiated, inferred from
/// its PTY output by [`KbdTracker`]. Decides how `encode_key` represents
/// modifiers on functional keys (Enter, Tab, …) that the legacy raw-byte
/// encoding can't express.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KbdEncoding {
    /// No enhanced protocol active. Modifiers on functional keys are dropped
    /// (this is what every terminal did before kitty/modifyOtherKeys).
    Legacy,
    /// Kitty keyboard protocol, with the active progressive-enhancement flags
    /// (bit 0 = disambiguate, bit 3 = report-all-keys-as-escape-codes).
    Kitty(u8),
    /// xterm `modifyOtherKeys` at level >= 1.
    ModifyOtherKeys,
}

/// Kitty modifier bitmask per the protocol: shift=1, alt=2, ctrl=4, super=8.
/// The on-the-wire "modifier parameter" is this value + 1.
fn kitty_mod_bits(m: KeyModifiers) -> u8 {
    let mut v = 0u8;
    if m.contains(KeyModifiers::SHIFT) {
        v |= 1;
    }
    if m.contains(KeyModifiers::ALT) {
        v |= 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        v |= 4;
    }
    if m.contains(KeyModifiers::SUPER) {
        v |= 8;
    }
    v
}

/// Encode "functional" keys (Enter/Tab/Backspace/Esc) whose modifiers the
/// legacy encoding can't represent, using whatever enhanced protocol the inner
/// app negotiated. Returns `None` to fall through to the legacy path — which
/// happens for unmodified keys (unless the app asked for report-all) and for
/// every non-functional key. Printable `Char`s deliberately stay legacy:
/// kitty's base-layout keycodes can't be reconstructed from crossterm's
/// already-shifted char, so re-encoding them would corrupt normal typing.
fn encode_functional_enhanced(k: KeyEvent, kbd: KbdEncoding) -> Option<Vec<u8>> {
    let keycode: u32 = match k.code {
        KeyCode::Enter => 13,
        KeyCode::Tab => 9,
        KeyCode::Backspace => 127,
        KeyCode::Esc => 27,
        _ => return None,
    };
    let bits = kitty_mod_bits(k.modifiers);
    let report_all = matches!(kbd, KbdEncoding::Kitty(f) if f & 0b1000 != 0);
    // Unmodified functional keys only diverge from their legacy bytes when the
    // app explicitly asked for every key as an escape code (kitty flag 8).
    if bits == 0 && !report_all {
        return None;
    }
    let modparam = bits + 1;
    let s = match kbd {
        KbdEncoding::Kitty(_) if bits == 0 => format!("\x1b[{keycode}u"),
        KbdEncoding::Kitty(_) => format!("\x1b[{keycode};{modparam}u"),
        KbdEncoding::ModifyOtherKeys => format!("\x1b[27;{modparam};{keycode}~"),
        KbdEncoding::Legacy => return None,
    };
    Some(s.into_bytes())
}

/// Encode a key event for transmission to the PTY, honoring terminal modes.
pub fn encode_key(k: KeyEvent, app_cursor: bool, kbd: KbdEncoding) -> Vec<u8> {
    // Enhanced-keyboard passthrough for modified functional keys (Shift+Enter,
    // Ctrl+Enter, …). Returns None (falls through to legacy) when no protocol
    // is active, so we emit exactly the same legacy bytes as before.
    if let Some(bytes) = encode_functional_enhanced(k, kbd) {
        return bytes;
    }
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

/// Streaming scanner that infers the inner app's keyboard-input protocol from
/// its PTY *output*. vt100 0.16.2 tracks DECSET modes like application-cursor
/// and bracketed-paste for us, but has no API for the kitty keyboard protocol
/// or `modifyOtherKeys`, so we run this tiny CSI parser over the same byte
/// stream the vt100 parser sees and remember the result. `encode_key` then
/// asks [`Self::encoding`] how to represent modified functional keys.
///
/// Sequences recognised (all app → terminal):
/// - `CSI > flags u`        kitty push  (progressive enhancement)
/// - `CSI < n u`            kitty pop
/// - `CSI = flags ; mode u` kitty set/clear
/// - `CSI > 4 ; n m`        xterm modifyOtherKeys level n (n omitted = off)
///
/// Reattach caveat: on attach the supervisor replays a screen *dump*
/// (`contents_formatted`), not the inner app's mode-negotiation history, so an
/// app that enabled enhanced keys *before* the client attached starts out
/// looking Legacy until it re-negotiates (e.g. on the resize the client sends).
#[derive(Default)]
pub struct KbdTracker {
    /// Kitty progressive-enhancement push stack; top is the active flag set.
    kitty_stack: Vec<u8>,
    /// xterm modifyOtherKeys level (0 = off).
    modify_other_keys: u8,
    scan: Scan,
    prefix: u8,
    params: Vec<u32>,
    cur: Option<u32>,
    len: usize,
}

#[derive(Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl KbdTracker {
    /// Feed a chunk of PTY output. Maintains partial-sequence state across
    /// calls, so a CSI sequence split over two frames is still recognised.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.scan {
                Scan::Ground => {
                    if b == 0x1b {
                        self.scan = Scan::Esc;
                    }
                }
                Scan::Esc => {
                    if b == b'[' {
                        self.scan = Scan::Csi;
                        self.prefix = 0;
                        self.params.clear();
                        self.cur = None;
                        self.len = 0;
                    } else if b != 0x1b {
                        self.scan = Scan::Ground;
                    }
                }
                Scan::Csi => self.csi_byte(b),
            }
        }
    }

    fn csi_byte(&mut self, b: u8) {
        self.len += 1;
        // Bound a runaway/malformed CSI so a stray ESC[ can't grow unbounded.
        if self.len > 64 {
            self.scan = Scan::Ground;
            return;
        }
        match b {
            b'0'..=b'9' => self.cur = Some(self.cur.unwrap_or(0) * 10 + (b - b'0') as u32),
            b';' => self.params.push(self.cur.take().unwrap_or(0)),
            b'<' | b'=' | b'>' | b'?' if self.len == 1 => self.prefix = b,
            0x20..=0x3f => {} // other intermediates/params we don't use
            0x40..=0x7e => {
                if let Some(v) = self.cur.take() {
                    self.params.push(v);
                }
                self.dispatch(b);
                self.scan = Scan::Ground;
            }
            _ => self.scan = Scan::Ground,
        }
    }

    fn dispatch(&mut self, final_byte: u8) {
        match final_byte {
            b'u' => match self.prefix {
                b'>' => {
                    let flags = self.params.first().copied().unwrap_or(0) as u8;
                    self.kitty_stack.push(flags);
                }
                b'<' => {
                    let n = self.params.first().copied().unwrap_or(1).max(1);
                    for _ in 0..n {
                        self.kitty_stack.pop();
                    }
                }
                b'=' => {
                    let flags = self.params.first().copied().unwrap_or(0) as u8;
                    let mode = self.params.get(1).copied().unwrap_or(1);
                    let top = self.kitty_stack.last().copied().unwrap_or(0);
                    let next = match mode {
                        2 => top | flags,  // set given bits
                        3 => top & !flags, // clear given bits
                        _ => flags,        // 1 (default) = set all
                    };
                    if let Some(t) = self.kitty_stack.last_mut() {
                        *t = next;
                    } else {
                        self.kitty_stack.push(next);
                    }
                }
                _ => {} // '?' query/response — not our concern
            },
            b'm' if self.prefix == b'>' && self.params.first().copied() == Some(4) => {
                // CSI > 4 ; n m  (n absent on reset → level 0)
                self.modify_other_keys = self.params.get(1).copied().unwrap_or(0) as u8;
            }
            _ => {}
        }
    }

    /// The encoding `encode_key` should use for modified functional keys.
    pub fn encoding(&self) -> KbdEncoding {
        match self.kitty_stack.last().copied().unwrap_or(0) {
            0 if self.modify_other_keys >= 1 => KbdEncoding::ModifyOtherKeys,
            0 => KbdEncoding::Legacy,
            flags => KbdEncoding::Kitty(flags),
        }
    }

    /// Escape sequences that re-establish this protocol state in a fresh
    /// parser. Used by the supervisor's attach prelude so a reattaching
    /// client's own `KbdTracker` (fed the dump) lands on the same state the
    /// inner app negotiated before the client was around. Mirrors the
    /// DECSET-mode re-emission for mouse / cursor-key / bracketed-paste.
    pub fn prelude(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let flags = self.kitty_stack.last().copied().unwrap_or(0);
        if flags != 0 {
            out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
        }
        if self.modify_other_keys >= 1 {
            out.extend_from_slice(format!("\x1b[>4;{}m", self.modify_other_keys).as_bytes());
        }
        out
    }
}

/// Sniffs OSC 52 clipboard-*copy* sequences out of a PTY byte stream so the
/// client can forward them to the real outer terminal (see
/// `Command::SetClipboard`). imbuia re-renders inner output cell-by-cell via
/// vt100, so escape sequences with no on-screen effect — OSC 52 among them —
/// are otherwise swallowed by the parser and never reach the emulator that
/// owns the system clipboard. Mirrors [`KbdTracker`]: fed the raw stream, it
/// keeps partial-sequence state across frames.
///
/// Only copy requests (`ESC ] 52 ; <ty> ; <base64> ST`) are surfaced; paste
/// queries (`… ; ?`) carry no data and are ignored.
#[derive(Default)]
pub struct ClipboardSniffer {
    scan: OscScan,
    /// Bytes accumulated between `ESC ]` and the string terminator.
    buf: Vec<u8>,
}

#[derive(Default, PartialEq, Eq)]
enum OscScan {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// Cap on a single buffered OSC payload; a copy larger than this is dropped
/// rather than buffered unbounded. 1 MiB of base64 ≈ 768 KiB of clipboard.
const MAX_OSC: usize = 1 << 20;

impl ClipboardSniffer {
    /// Feed a chunk of PTY output; returns the payload (`<ty>;<base64>`) of any
    /// completed OSC 52 copy sequences, ready to splice into
    /// `ESC ] 52 ; <payload> BEL`.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.scan {
                OscScan::Ground => {
                    if b == 0x1b {
                        self.scan = OscScan::Esc;
                    }
                }
                OscScan::Esc => match b {
                    b']' => {
                        self.scan = OscScan::Osc;
                        self.buf.clear();
                    }
                    0x1b => {} // stay: ESC ESC
                    _ => self.scan = OscScan::Ground,
                },
                OscScan::Osc => match b {
                    0x07 => {
                        // BEL terminator.
                        if let Some(p) = self.take_copy() {
                            out.push(p);
                        }
                        self.scan = OscScan::Ground;
                    }
                    0x1b => self.scan = OscScan::OscEsc,
                    _ => {
                        self.buf.push(b);
                        if self.buf.len() > MAX_OSC {
                            self.buf.clear();
                            self.scan = OscScan::Ground;
                        }
                    }
                },
                OscScan::OscEsc => {
                    // The string terminator is `ESC \`; anything else aborts.
                    if b == b'\\'
                        && let Some(p) = self.take_copy()
                    {
                        out.push(p);
                    }
                    self.buf.clear();
                    self.scan = if b == 0x1b { OscScan::Esc } else { OscScan::Ground };
                }
            }
        }
        out
    }

    /// If the buffered OSC is a `52` *copy* (not a `?` paste query), return its
    /// `<ty>;<base64>` payload.
    fn take_copy(&self) -> Option<String> {
        let s = std::str::from_utf8(&self.buf).ok()?;
        let payload = s.strip_prefix("52;")?;
        // Paste queries end in `;?` (or are a bare `?`) and carry no data.
        if payload.rsplit(';').next()? == "?" {
            return None;
        }
        Some(payload.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn legacy_enter_is_carriage_return_regardless_of_shift() {
        // No protocol negotiated: Shift+Enter degrades to bare CR (unchanged
        // pre-fix behaviour — we simply can't do better without a protocol).
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
            KbdEncoding::Legacy,
        );
        assert_eq!(out, b"\r");
    }

    #[test]
    fn kitty_shift_enter_is_csi_u() {
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
            KbdEncoding::Kitty(1),
        );
        assert_eq!(out, b"\x1b[13;2u");
    }

    #[test]
    fn kitty_unmodified_enter_stays_legacy_without_report_all() {
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::NONE),
            false,
            KbdEncoding::Kitty(1),
        );
        assert_eq!(out, b"\r");
    }

    #[test]
    fn kitty_report_all_encodes_unmodified_enter() {
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::NONE),
            false,
            KbdEncoding::Kitty(0b1001),
        );
        assert_eq!(out, b"\x1b[13u");
    }

    #[test]
    fn modify_other_keys_shift_enter() {
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
            KbdEncoding::ModifyOtherKeys,
        );
        assert_eq!(out, b"\x1b[27;2;13~");
    }

    #[test]
    fn ctrl_alt_enter_modifier_math() {
        // ctrl(4) + alt(2) = 6, +1 = 7.
        let out = encode_key(
            ev(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::ALT),
            false,
            KbdEncoding::Kitty(1),
        );
        assert_eq!(out, b"\x1b[13;7u");
    }

    #[test]
    fn printable_chars_never_take_enhanced_path() {
        // 'A' must stay 'A' even in kitty mode — we don't touch Char keys.
        let out = encode_key(
            ev(KeyCode::Char('A'), KeyModifiers::SHIFT),
            false,
            KbdEncoding::Kitty(1),
        );
        assert_eq!(out, b"A");
    }

    #[test]
    fn tracker_detects_kitty_push_and_pop() {
        let mut t = KbdTracker::default();
        assert_eq!(t.encoding(), KbdEncoding::Legacy);
        t.feed(b"\x1b[>1u");
        assert_eq!(t.encoding(), KbdEncoding::Kitty(1));
        t.feed(b"\x1b[<u");
        assert_eq!(t.encoding(), KbdEncoding::Legacy);
    }

    #[test]
    fn tracker_handles_sequence_split_across_frames() {
        let mut t = KbdTracker::default();
        t.feed(b"\x1b[>");
        t.feed(b"5");
        t.feed(b"u");
        assert_eq!(t.encoding(), KbdEncoding::Kitty(5));
    }

    #[test]
    fn tracker_kitty_set_all_and_clear() {
        let mut t = KbdTracker::default();
        t.feed(b"\x1b[=15;1u"); // set all → 15
        assert_eq!(t.encoding(), KbdEncoding::Kitty(15));
        t.feed(b"\x1b[=8;3u"); // clear bit 3 (value 8) → 7
        assert_eq!(t.encoding(), KbdEncoding::Kitty(7));
    }

    #[test]
    fn tracker_modify_other_keys_on_off() {
        let mut t = KbdTracker::default();
        t.feed(b"\x1b[>4;2m");
        assert_eq!(t.encoding(), KbdEncoding::ModifyOtherKeys);
        t.feed(b"\x1b[>4m"); // reset → off
        assert_eq!(t.encoding(), KbdEncoding::Legacy);
    }

    #[test]
    fn prelude_round_trips_through_a_fresh_tracker() {
        // Whatever state the supervisor's tracker is in, replaying its prelude
        // into a fresh client tracker must reproduce the same encoding.
        for seq in [&b"\x1b[>5u"[..], &b"\x1b[>4;2m"[..], &b"\x1b[=15;1u"[..]] {
            let mut src = KbdTracker::default();
            src.feed(seq);
            let mut dst = KbdTracker::default();
            dst.feed(&src.prelude());
            assert_eq!(src.encoding(), dst.encoding(), "seq {seq:?}");
        }
    }

    #[test]
    fn prelude_is_empty_when_legacy() {
        assert!(KbdTracker::default().prelude().is_empty());
    }

    #[test]
    fn tracker_ignores_unrelated_csi() {
        let mut t = KbdTracker::default();
        t.feed(b"\x1b[2J\x1b[1;1H\x1b[?1049h hello \x1b[0m");
        assert_eq!(t.encoding(), KbdEncoding::Legacy);
    }

    #[test]
    fn sniffs_osc52_copy_bel_terminated() {
        let mut s = ClipboardSniffer::default();
        let out = s.feed(b"before\x1b]52;c;aGVsbG8=\x07after");
        assert_eq!(out, vec!["c;aGVsbG8=".to_string()]);
    }

    #[test]
    fn sniffs_osc52_copy_st_terminated() {
        let mut s = ClipboardSniffer::default();
        let out = s.feed(b"\x1b]52;p;ZGF0YQ==\x1b\\");
        assert_eq!(out, vec!["p;ZGF0YQ==".to_string()]);
    }

    #[test]
    fn sniffs_osc52_split_across_frames() {
        let mut s = ClipboardSniffer::default();
        assert!(s.feed(b"\x1b]52;c;aGVs").is_empty());
        let out = s.feed(b"bG8=\x07");
        assert_eq!(out, vec!["c;aGVsbG8=".to_string()]);
    }

    #[test]
    fn ignores_osc52_paste_query() {
        let mut s = ClipboardSniffer::default();
        assert!(s.feed(b"\x1b]52;c;?\x07").is_empty());
    }

    #[test]
    fn ignores_non_52_osc() {
        let mut s = ClipboardSniffer::default();
        // Window-title (OSC 0) must not be mistaken for a clipboard copy.
        assert!(s.feed(b"\x1b]0;my title\x07").is_empty());
    }

    #[test]
    fn sniffs_multiple_copies_in_one_chunk() {
        let mut s = ClipboardSniffer::default();
        let out = s.feed(b"\x1b]52;c;YQ==\x07\x1b]52;c;Yg==\x07");
        assert_eq!(out, vec!["c;YQ==".to_string(), "c;Yg==".to_string()]);
    }
}
