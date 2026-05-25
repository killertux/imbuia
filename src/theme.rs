//! Color palette. Two bundled variants — `Dark` and `Light` — copied from the
//! rowdy project's `themes/dark.toml` and `themes/light.toml`. Persisted in
//! the global config as a plain string ("dark" / "light").

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    #[default]
    Dark,
    Light,
}

impl ThemeKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)] // status_idle reserved for future status-bar idle decoration
pub struct Theme {
    pub kind: ThemeKind,
    pub bg: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub status_idle: Color,
    pub status_running: Color,
    pub status_ok: Color,
    pub status_error: Color,
    /// Used for "review requested changes" — distinct from `status_error`
    /// (CI failure) so the two PR states are visually separable.
    pub status_changes: Color,
    /// Used for "PR approved, ready to merge" (sidebar bar). Blue.
    pub status_approved: Color,
    pub header_fg: Color,
}

impl Theme {
    pub const fn for_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Dark => DARK,
            ThemeKind::Light => LIGHT,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        DARK
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

const DARK: Theme = Theme {
    kind: ThemeKind::Dark,
    bg: rgb(0x1E, 0x1E, 0x2E),
    fg: rgb(0xCD, 0xD6, 0xF4),
    fg_dim: rgb(0x9A, 0xA0, 0xB6),
    border: rgb(0x45, 0x47, 0x5A),
    border_focus: rgb(0x89, 0xB4, 0xFA),
    selection_bg: rgb(0x58, 0x5B, 0x70),
    selection_fg: rgb(0xF5, 0xF7, 0xFA),
    status_idle: rgb(0x9A, 0xA0, 0xB6),
    status_running: rgb(0xF9, 0xE2, 0xAF),
    status_ok: rgb(0xA6, 0xE3, 0xA1),
    status_error: rgb(0xF3, 0x8B, 0xA8),
    status_changes: rgb(0xCB, 0xA6, 0xF7),
    status_approved: rgb(0x89, 0xB4, 0xFA),
    header_fg: rgb(0xF5, 0xC2, 0xE7),
};

const LIGHT: Theme = Theme {
    kind: ThemeKind::Light,
    bg: rgb(0xEF, 0xF1, 0xF5),
    fg: rgb(0x2C, 0x2F, 0x44),
    fg_dim: rgb(0x6C, 0x6F, 0x85),
    border: rgb(0xBC, 0xC0, 0xCC),
    border_focus: rgb(0x1E, 0x66, 0xF5),
    selection_bg: rgb(0xCC, 0xD0, 0xDA),
    selection_fg: rgb(0x1F, 0x22, 0x36),
    status_idle: rgb(0x6C, 0x6F, 0x85),
    status_running: rgb(0xBE, 0x6A, 0x00),
    status_ok: rgb(0x2D, 0x80, 0x1F),
    status_error: rgb(0xC4, 0x0E, 0x33),
    status_changes: rgb(0x88, 0x39, 0xEF),
    status_approved: rgb(0x1E, 0x66, 0xF5),
    header_fg: rgb(0xC8, 0x44, 0xA9),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trip() {
        assert_eq!(ThemeKind::parse("dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("Light"), Some(ThemeKind::Light));
        assert_eq!(ThemeKind::parse("  DARK  "), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("solarized"), None);
        assert_eq!(ThemeKind::Dark.as_str(), "dark");
        assert_eq!(ThemeKind::Light.as_str(), "light");
    }

    #[test]
    fn for_kind_returns_palette() {
        let dark = Theme::for_kind(ThemeKind::Dark);
        assert_eq!(dark.bg, Color::Rgb(0x1E, 0x1E, 0x2E));
        let light = Theme::for_kind(ThemeKind::Light);
        assert_eq!(light.bg, Color::Rgb(0xEF, 0xF1, 0xF5));
    }
}
