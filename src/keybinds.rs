//! User-configurable keybindings.
//!
//! All Normal-mode keys and leader-chord targets resolve through a `KeyMap`
//! at runtime. Defaults are hardcoded here; the global config overlays
//! user-supplied bindings on top via a `[keybinds]` toml table.
//!
//! Wire format is vim-style: `<C-x>` (Ctrl), `<S-x>` (Shift, rarely needed),
//! `<A-x>` (Alt), `<Space>`, `<CR>`/`<Enter>`, `<Tab>`, `<F1>`–`<F12>`,
//! `<Esc>` (rejected). Multi-key chords concatenate: `gt`, `<Space>w`,
//! `<C-w>>`.
//!
//! Reserved sequences the parser refuses to bind:
//! - `<Esc>` — universal popup cancel.
//! - `<C-q>` — hard quit (short-circuited in `src/input.rs::map`).
//! - any binding that starts with `<C-\>` other than `leave_terminal`'s
//!   default — remapping the Terminal-mode escape to something a shell
//!   user would type would trap them in Terminal mode.

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

/// Every action a key can trigger. Variant name → snake_case in toml.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BindableAction {
    // Normal-mode single keys.
    FocusSidebar,
    FocusTerminal,
    SidebarUp,
    SidebarDown,
    ActivateSelection,
    OpenTab,
    CloseTab,
    EnterTerminalMode,
    EnterCommandMode,

    // Leader-chord targets.
    NextTab,
    PrevTab,
    SidebarGrow,
    SidebarShrink,
    SidebarReset,
    OpenProjectPopup,
    NewWorktree,
    RemoveWorktree,
    EditProject,
    LaunchPicker,
    UsagePopup,
    HelpPopup,
    Quit,

    // Terminal-mode allow-list.
    LeaveTerminal,
}

impl BindableAction {
    /// Stable snake_case name for the config toml.
    pub fn name(self) -> &'static str {
        match self {
            Self::FocusSidebar => "focus_sidebar",
            Self::FocusTerminal => "focus_terminal",
            Self::SidebarUp => "sidebar_up",
            Self::SidebarDown => "sidebar_down",
            Self::ActivateSelection => "activate_selection",
            Self::OpenTab => "open_tab",
            Self::CloseTab => "close_tab",
            Self::EnterTerminalMode => "enter_terminal_mode",
            Self::EnterCommandMode => "enter_command_mode",
            Self::NextTab => "next_tab",
            Self::PrevTab => "prev_tab",
            Self::SidebarGrow => "sidebar_grow",
            Self::SidebarShrink => "sidebar_shrink",
            Self::SidebarReset => "sidebar_reset",
            Self::OpenProjectPopup => "open_project_popup",
            Self::NewWorktree => "new_worktree",
            Self::RemoveWorktree => "remove_worktree",
            Self::EditProject => "edit_project",
            Self::LaunchPicker => "launch_picker",
            Self::UsagePopup => "usage_popup",
            Self::HelpPopup => "help_popup",
            Self::Quit => "quit",
            Self::LeaveTerminal => "leave_terminal",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        ALL.iter().copied().find(|a| a.name() == s)
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::FocusSidebar => "focus the sidebar",
            Self::FocusTerminal => "focus the terminal pane",
            Self::SidebarUp => "previous sidebar row",
            Self::SidebarDown => "next sidebar row",
            Self::ActivateSelection => "activate selected project / worktree",
            Self::OpenTab => "open a new tab in the active worktree",
            Self::CloseTab => "close the current tab",
            Self::EnterTerminalMode => "switch to Terminal mode",
            Self::EnterCommandMode => "open the :ex command line",
            Self::NextTab => "next tab",
            Self::PrevTab => "previous tab",
            Self::SidebarGrow => "grow the sidebar",
            Self::SidebarShrink => "shrink the sidebar",
            Self::SidebarReset => "reset sidebar width",
            Self::OpenProjectPopup => "open project popup",
            Self::NewWorktree => "new worktree",
            Self::RemoveWorktree => "remove worktree",
            Self::EditProject => "edit project setup script",
            Self::LaunchPicker => "launcher picker",
            Self::UsagePopup => "resource usage",
            Self::HelpPopup => "help",
            Self::Quit => "quit imbuia",
            Self::LeaveTerminal => "leave Terminal mode",
        }
    }

    pub fn scope(self) -> Scope {
        match self {
            Self::LeaveTerminal => Scope::Terminal,
            _ => Scope::Normal,
        }
    }
}

pub const ALL: &[BindableAction] = &[
    BindableAction::FocusSidebar,
    BindableAction::FocusTerminal,
    BindableAction::SidebarUp,
    BindableAction::SidebarDown,
    BindableAction::ActivateSelection,
    BindableAction::OpenTab,
    BindableAction::CloseTab,
    BindableAction::EnterTerminalMode,
    BindableAction::EnterCommandMode,
    BindableAction::NextTab,
    BindableAction::PrevTab,
    BindableAction::SidebarGrow,
    BindableAction::SidebarShrink,
    BindableAction::SidebarReset,
    BindableAction::OpenProjectPopup,
    BindableAction::NewWorktree,
    BindableAction::RemoveWorktree,
    BindableAction::EditProject,
    BindableAction::LaunchPicker,
    BindableAction::UsagePopup,
    BindableAction::HelpPopup,
    BindableAction::Quit,
    BindableAction::LeaveTerminal,
];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Normal,
    Terminal,
}

/// One keystroke = modifiers + keycode. Equality ignores irrelevant flags
/// crossterm may set (we already normalised at input layer).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: KeyModifiers,
    pub code: KeyCode,
}

impl Chord {
    pub fn from_event(k: &KeyEvent) -> Self {
        let (mods, code) = canonical(k.modifiers, k.code);
        Chord { mods, code }
    }
}

/// 1–4 chords. Anything longer is rejected at parse time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Binding(pub Vec<Chord>);

impl Binding {
    pub fn starts_with(&self, prefix: &[Chord]) -> bool {
        self.0.len() >= prefix.len() && self.0[..prefix.len()] == *prefix
    }
}

/// Resolved keymap. Each scope owns its `(Binding, Action)` table.
#[derive(Clone, Debug, Default)]
pub struct KeyMap {
    pub normal: Vec<(Binding, BindableAction)>,
    pub terminal: Vec<(Binding, BindableAction)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MatchResult<'a> {
    /// Sequence matched a binding exactly — fire this action.
    Exact(BindableAction),
    /// Sequence is a strict prefix of one or more bindings — keep waiting.
    Prefix(&'a [(Binding, BindableAction)]),
    /// No match and no prefix.
    None,
}

impl KeyMap {
    pub fn lookup(&self, scope: Scope, seq: &[Chord]) -> MatchResult<'_> {
        let table = match scope {
            Scope::Normal => &self.normal,
            Scope::Terminal => &self.terminal,
        };
        if seq.is_empty() {
            return MatchResult::None;
        }
        // Exact match first.
        for (b, a) in table {
            if b.0 == seq {
                return MatchResult::Exact(*a);
            }
        }
        // Strict prefix: every binding starting with `seq` and longer.
        let any_prefix = table
            .iter()
            .any(|(b, _)| b.0.len() > seq.len() && b.starts_with(seq));
        if any_prefix {
            MatchResult::Prefix(table.as_slice())
        } else {
            MatchResult::None
        }
    }

    /// Every binding in the given scope whose first chord equals `head`.
    /// Used by the which-key hint and `:help`.
    pub fn bindings_starting_with<'a>(
        &'a self,
        scope: Scope,
        head: &Chord,
    ) -> impl Iterator<Item = (&'a Binding, BindableAction)> + 'a {
        let table = match scope {
            Scope::Normal => &self.normal,
            Scope::Terminal => &self.terminal,
        };
        let head = *head;
        table
            .iter()
            .filter(move |(b, _)| b.0.first().is_some_and(|c| *c == head))
            .map(|(b, a)| (b, *a))
    }

    /// First binding registered for `action`, formatted for display.
    pub fn binding_for(&self, action: BindableAction) -> Option<String> {
        let table = match action.scope() {
            Scope::Normal => &self.normal,
            Scope::Terminal => &self.terminal,
        };
        table
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(b, _)| format_binding(b))
    }
}

/// Hardcoded default bindings. Order here is the order rendered by `:help`.
pub fn defaults() -> KeyMap {
    let mut normal: Vec<(Binding, BindableAction)> = Vec::new();
    let mut terminal: Vec<(Binding, BindableAction)> = Vec::new();
    let add_n = |v: &mut Vec<(Binding, BindableAction)>, s: &str, a: BindableAction| {
        v.push((parse_unchecked(s), a));
    };
    add_n(&mut normal, "h", BindableAction::FocusSidebar);
    add_n(&mut normal, "l", BindableAction::FocusTerminal);
    add_n(&mut normal, "j", BindableAction::SidebarDown);
    add_n(&mut normal, "k", BindableAction::SidebarUp);
    add_n(&mut normal, "<CR>", BindableAction::ActivateSelection);
    add_n(&mut normal, "o", BindableAction::OpenTab);
    add_n(&mut normal, "x", BindableAction::CloseTab);
    add_n(&mut normal, "i", BindableAction::EnterTerminalMode);
    add_n(&mut normal, ":", BindableAction::EnterCommandMode);
    add_n(&mut normal, "gt", BindableAction::NextTab);
    add_n(&mut normal, "gT", BindableAction::PrevTab);
    add_n(&mut normal, "<C-w>>", BindableAction::SidebarGrow);
    add_n(&mut normal, "<C-w><", BindableAction::SidebarShrink);
    add_n(&mut normal, "<C-w>=", BindableAction::SidebarReset);
    add_n(&mut normal, "<Space>o", BindableAction::OpenProjectPopup);
    add_n(&mut normal, "<Space>w", BindableAction::NewWorktree);
    add_n(&mut normal, "<Space>W", BindableAction::RemoveWorktree);
    add_n(&mut normal, "<Space>e", BindableAction::EditProject);
    add_n(&mut normal, "<Space>l", BindableAction::LaunchPicker);
    add_n(&mut normal, "<Space>u", BindableAction::UsagePopup);
    add_n(&mut normal, "<Space>?", BindableAction::HelpPopup);
    add_n(&mut normal, "<Space>q", BindableAction::Quit);

    terminal.push((
        parse_unchecked("<C-\\><C-n>"),
        BindableAction::LeaveTerminal,
    ));

    KeyMap { normal, terminal }
}

/// Build a `KeyMap` from a config toml map, overlaying user bindings on top
/// of `defaults()`. Unknown actions and unparseable bindings log via
/// `tracing::warn!` and fall back to their compile-time default.
pub fn load_overlay(user: &BTreeMap<String, String>) -> KeyMap {
    let mut km = defaults();
    for (name, binding_str) in user {
        let Some(action) = BindableAction::from_name(name) else {
            tracing::warn!(name, "unknown keybind action — ignored");
            continue;
        };
        let parsed = match parse(binding_str) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(name, binding = %binding_str, "invalid keybind: {e} — using default");
                continue;
            }
        };
        if is_reserved(&parsed, action) {
            tracing::warn!(name, binding = %binding_str, "reserved key sequence — ignored");
            continue;
        }
        let table = match action.scope() {
            Scope::Normal => &mut km.normal,
            Scope::Terminal => &mut km.terminal,
        };
        // Drop the default binding for this action (if any), then insert the
        // user's. We keep `action`'s default for *other* actions that may
        // share a prefix — the user takes responsibility for collisions.
        table.retain(|(_, a)| *a != action);
        table.push((parsed, action));
    }
    km
}

/// Render the full default keymap as a flat `BTreeMap<String, String>` for
/// first-launch population of the config toml.
pub fn defaults_as_config() -> BTreeMap<String, String> {
    let km = defaults();
    let mut out = BTreeMap::new();
    for (b, a) in km.normal.iter().chain(km.terminal.iter()) {
        out.insert(a.name().to_string(), format_binding(b));
    }
    out
}

fn is_reserved(b: &Binding, action: BindableAction) -> bool {
    // <Esc> bare is always reserved.
    if b.0.len() == 1
        && b.0[0]
            == (Chord {
                mods: KeyModifiers::NONE,
                code: KeyCode::Esc,
            })
    {
        return true;
    }
    // Ctrl-Q is short-circuited before reaching the reducer.
    if b.0.len() == 1
        && b.0[0]
            == (Chord {
                mods: KeyModifiers::CONTROL,
                code: KeyCode::Char('q'),
            })
    {
        return true;
    }
    // Only `leave_terminal` may live in the Terminal scope. Refuse anything
    // else that tries to land there.
    if action.scope() == Scope::Terminal && action != BindableAction::LeaveTerminal {
        return true;
    }
    false
}

/// Vim-style parser. Returns 1–4 chords.
pub fn parse(s: &str) -> Result<Binding> {
    let mut chords = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find matching `>`. If absent: only accept `<` as literal when
            // it's the final char in the binding (so `<C-w><` parses as
            // Ctrl-W then `<`). Anything else is an unclosed token.
            if let Some(end) = (i + 1..bytes.len()).find(|j| bytes[*j] == b'>') {
                let token = &s[i + 1..end];
                chords.push(parse_token(token)?);
                i = end + 1;
                continue;
            }
            if i + 1 != bytes.len() {
                return Err(anyhow!("unclosed `<` in {s:?}"));
            }
            chords.push(chord_for_char('<'));
            i += 1;
        } else {
            let c = s[i..].chars().next().unwrap();
            chords.push(chord_for_char(c));
            i += c.len_utf8();
        }
    }
    if chords.is_empty() {
        return Err(anyhow!("empty binding"));
    }
    if chords.len() > 4 {
        return Err(anyhow!("binding too long ({} chords, max 4)", chords.len()));
    }
    Ok(Binding(chords))
}

/// `parse(s).expect()` for the hardcoded defaults table. Crashes the binary
/// at startup if the defaults are bad — that's a programmer error.
fn parse_unchecked(s: &str) -> Binding {
    parse(s).unwrap_or_else(|e| panic!("default binding {s:?} failed to parse: {e}"))
}

fn parse_token(t: &str) -> Result<Chord> {
    let mut mods = KeyModifiers::NONE;
    let mut rest = t;
    loop {
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("c-") {
            mods |= KeyModifiers::CONTROL;
            rest = &rest[2..];
        } else if lower.starts_with("s-") {
            mods |= KeyModifiers::SHIFT;
            rest = &rest[2..];
        } else if lower.starts_with("a-") || lower.starts_with("m-") {
            mods |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else {
            break;
        }
    }
    let code = named_key(rest)
        .or_else(|| {
            let mut chars = rest.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(KeyCode::Char(c))
        })
        .ok_or_else(|| anyhow!("unrecognised key token `{t}`"))?;
    let (mods, code) = canonical(mods, code);
    Ok(Chord { mods, code })
}

fn named_key(s: &str) -> Option<KeyCode> {
    match s.to_ascii_lowercase().as_str() {
        "space" => Some(KeyCode::Char(' ')),
        "cr" | "enter" | "return" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "bs" | "backspace" => Some(KeyCode::Backspace),
        "esc" | "escape" => Some(KeyCode::Esc),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" | "pgup" => Some(KeyCode::PageUp),
        "pagedown" | "pgdn" => Some(KeyCode::PageDown),
        "lt" => Some(KeyCode::Char('<')),
        "gt" => Some(KeyCode::Char('>')),
        "bslash" | "bs2" => Some(KeyCode::Char('\\')),
        s if s.starts_with('f') => s[1..].parse::<u8>().ok().map(KeyCode::F),
        _ => None,
    }
}

fn chord_for_char(c: char) -> Chord {
    // SHIFT for an ASCII uppercase char is implied by the character itself;
    // we deliberately leave mods=NONE so this matches a KeyEvent regardless
    // of whether crossterm reports SHIFT. See `canonical` for the same
    // rule on the input path.
    Chord {
        mods: KeyModifiers::NONE,
        code: KeyCode::Char(c),
    }
}

/// Canonicalise a `(mods, code)` pair so equivalent keystrokes compare equal
/// regardless of how the active keyboard protocol reports them.
///
/// Two normalisations, both needed because crossterm's reporting depends on
/// whether the kitty keyboard protocol is active (see `main.rs`):
///
/// 1. **Ctrl + letter is case- and Shift-insensitive.** Every terminal
///    collapses Ctrl+n, Ctrl+N and Ctrl+Shift+n to the same control byte
///    (`0x0E`); under the kitty protocol crossterm instead reports them
///    distinctly (e.g. `Char('N')` + `CONTROL|SHIFT`). Fold the letter to
///    lowercase and drop Shift so the `<C-\><C-n>`-style chords keep matching.
///    (Alt is left alone — `M-n` vs `M-N` are traditionally distinct.)
/// 2. **Uppercase letter without Ctrl implies Shift.** `Shift+A` and `A` both
///    mean the `A` binding, so drop a redundant Shift there too. Lowercase vs
///    uppercase letters *without* Ctrl stay distinct (vim `g` vs `G`).
fn canonical(mut m: KeyModifiers, code: KeyCode) -> (KeyModifiers, KeyCode) {
    m &= KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT;
    if let KeyCode::Char(c) = code
        && c.is_ascii_alphabetic()
    {
        if m.contains(KeyModifiers::CONTROL) {
            m.remove(KeyModifiers::SHIFT);
            return (m, KeyCode::Char(c.to_ascii_lowercase()));
        }
        if c.is_ascii_uppercase() {
            m.remove(KeyModifiers::SHIFT);
        }
    }
    (m, code)
}

/// Format a binding back into vim-style. Round-trips with `parse`.
pub fn format_binding(b: &Binding) -> String {
    let mut out = String::new();
    for c in &b.0 {
        format_chord(c, &mut out);
    }
    out
}

fn format_chord(c: &Chord, out: &mut String) {
    let needs_brackets = !c.mods.is_empty()
        || matches!(
            c.code,
            KeyCode::Char(' ')
                | KeyCode::Enter
                | KeyCode::Tab
                | KeyCode::Esc
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Backspace
                | KeyCode::F(_)
        );
    if !needs_brackets && let KeyCode::Char(ch) = c.code {
        out.push(ch);
        return;
    }
    out.push('<');
    if c.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("C-");
    }
    if c.mods.contains(KeyModifiers::SHIFT) {
        out.push_str("S-");
    }
    if c.mods.contains(KeyModifiers::ALT) {
        out.push_str("A-");
    }
    match c.code {
        KeyCode::Char(' ') => out.push_str("Space"),
        KeyCode::Char(ch) => out.push(ch),
        KeyCode::Enter => out.push_str("CR"),
        KeyCode::Tab => out.push_str("Tab"),
        KeyCode::Esc => out.push_str("Esc"),
        KeyCode::Backspace => out.push_str("BS"),
        KeyCode::Up => out.push_str("Up"),
        KeyCode::Down => out.push_str("Down"),
        KeyCode::Left => out.push_str("Left"),
        KeyCode::Right => out.push_str("Right"),
        KeyCode::Home => out.push_str("Home"),
        KeyCode::End => out.push_str("End"),
        KeyCode::PageUp => out.push_str("PageUp"),
        KeyCode::PageDown => out.push_str("PageDown"),
        KeyCode::F(n) => {
            out.push('F');
            out.push_str(&n.to_string());
        }
        _ => out.push('?'),
    }
    out.push('>');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_char() {
        let b = parse("o").unwrap();
        assert_eq!(b.0.len(), 1);
        assert_eq!(
            b.0[0],
            Chord {
                mods: KeyModifiers::NONE,
                code: KeyCode::Char('o')
            }
        );
    }

    #[test]
    fn parse_uppercase_implies_shift_stripped() {
        // Round-trip: `T` should match a KeyEvent with Char('T') and no Shift.
        let b = parse("T").unwrap();
        let evt = KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT);
        assert_eq!(b.0[0], Chord::from_event(&evt));
    }

    #[test]
    fn ctrl_letter_is_case_and_shift_insensitive() {
        // The `<C-\><C-n>` leave-terminal chord must keep matching no matter how
        // the keyboard protocol reports Ctrl+n. Under kitty disambiguate,
        // Ctrl+Shift+n arrives as Char('N') + CONTROL|SHIFT; legacy gives
        // Char('n') + CONTROL. Both must canonicalise to the `<C-n>` chord.
        let want = parse("<C-n>").unwrap().0[0];
        for (code, mods) in [
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('N'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (
                KeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            assert_eq!(Chord::from_event(&KeyEvent::new(code, mods)), want);
        }
        // …but Shift still matters for a plain (non-Ctrl) letter: `G` != `g`.
        assert_ne!(
            Chord::from_event(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)),
            Chord::from_event(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)),
        );
    }

    #[test]
    fn parse_ctrl_w_gt() {
        let b = parse("<C-w>>").unwrap();
        assert_eq!(b.0.len(), 2);
        assert_eq!(b.0[0].mods, KeyModifiers::CONTROL);
        assert_eq!(b.0[0].code, KeyCode::Char('w'));
        assert_eq!(b.0[1].code, KeyCode::Char('>'));
    }

    #[test]
    fn parse_space_chord() {
        let b = parse("<Space>w").unwrap();
        assert_eq!(b.0.len(), 2);
        assert_eq!(b.0[0].code, KeyCode::Char(' '));
        assert_eq!(b.0[1].code, KeyCode::Char('w'));
    }

    #[test]
    fn parse_gt() {
        let b = parse("gt").unwrap();
        assert_eq!(b.0.len(), 2);
        assert_eq!(b.0[0].code, KeyCode::Char('g'));
        assert_eq!(b.0[1].code, KeyCode::Char('t'));
    }

    #[test]
    fn round_trip_all_defaults() {
        for (b, _) in defaults().normal.iter().chain(defaults().terminal.iter()) {
            let s = format_binding(b);
            let re = parse(&s).unwrap_or_else(|e| panic!("re-parse {s:?}: {e}"));
            assert_eq!(b, &re, "round-trip changed binding: {s:?}");
        }
    }

    #[test]
    fn rejects_too_long() {
        assert!(parse("abcde").is_err());
    }

    #[test]
    fn rejects_unclosed_bracket() {
        assert!(parse("<C-w").is_err());
    }

    #[test]
    fn lookup_exact_then_prefix() {
        let km = defaults();
        let g = Chord {
            mods: KeyModifiers::NONE,
            code: KeyCode::Char('g'),
        };
        assert!(matches!(
            km.lookup(Scope::Normal, &[g]),
            MatchResult::Prefix(_)
        ));
        let t = Chord {
            mods: KeyModifiers::NONE,
            code: KeyCode::Char('t'),
        };
        assert_eq!(
            km.lookup(Scope::Normal, &[g, t]),
            MatchResult::Exact(BindableAction::NextTab)
        );
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let km = defaults();
        let z = Chord {
            mods: KeyModifiers::NONE,
            code: KeyCode::Char('z'),
        };
        assert!(matches!(km.lookup(Scope::Normal, &[z]), MatchResult::None));
    }

    #[test]
    fn user_overlay_replaces_default_binding_for_action() {
        let mut user = BTreeMap::new();
        user.insert("open_tab".into(), "<Space>t".into());
        let km = load_overlay(&user);
        assert_eq!(
            km.binding_for(BindableAction::OpenTab).as_deref(),
            Some("<Space>t")
        );
    }

    #[test]
    fn user_overlay_skips_unknown_action() {
        let mut user = BTreeMap::new();
        user.insert("not_a_real_action".into(), "x".into());
        let km = load_overlay(&user);
        // Defaults intact for known actions.
        assert_eq!(
            km.binding_for(BindableAction::OpenTab).as_deref(),
            Some("o")
        );
    }

    #[test]
    fn user_overlay_skips_unparseable() {
        let mut user = BTreeMap::new();
        user.insert("open_tab".into(), "<NotAKey>".into());
        let km = load_overlay(&user);
        assert_eq!(
            km.binding_for(BindableAction::OpenTab).as_deref(),
            Some("o")
        );
    }

    #[test]
    fn reserved_esc_rejected() {
        let mut user = BTreeMap::new();
        user.insert("open_tab".into(), "<Esc>".into());
        let km = load_overlay(&user);
        assert_eq!(
            km.binding_for(BindableAction::OpenTab).as_deref(),
            Some("o")
        );
    }

    #[test]
    fn defaults_as_config_lists_every_action() {
        let cfg = defaults_as_config();
        for a in ALL {
            assert!(
                cfg.contains_key(a.name()),
                "missing default for {}",
                a.name()
            );
        }
    }
}
