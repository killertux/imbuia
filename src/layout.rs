use ratatui::layout::{Constraint, Layout as RatLayout, Rect};

pub const DEFAULT_SIDEBAR_WIDTH: u16 = 24;
pub const MIN_SIDEBAR_WIDTH: u16 = 10;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

impl TermSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }

    pub fn as_rect(self) -> Rect {
        Rect::new(0, 0, self.cols, self.rows)
    }
}

/// The four chrome regions that frame the app.
#[derive(Copy, Clone, Debug)]
pub struct ChromeRects {
    pub sidebar: Rect,
    pub tab_bar: Rect,
    pub terminal: Rect,
    pub action_bar: Rect,
}

/// Compute chrome regions given the full terminal area and the sidebar width.
/// `sidebar_width` is clamped so the right pane always has at least 10 cols.
pub fn chrome(area: Rect, sidebar_width: u16) -> ChromeRects {
    let max_sidebar = area.width.saturating_sub(MIN_SIDEBAR_WIDTH);
    let sidebar_w = sidebar_width.min(max_sidebar).max(MIN_SIDEBAR_WIDTH);

    let vert = RatLayout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let main = vert[0];
    let action_bar = vert[1];

    let horiz =
        RatLayout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(1)]).split(main);
    let sidebar = horiz[0];
    let right = horiz[1];

    let right_vert = RatLayout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(right);
    let tab_bar = right_vert[0];
    let terminal = right_vert[1];

    ChromeRects {
        sidebar,
        tab_bar,
        terminal,
        action_bar,
    }
}

/// Clamp a sidebar width proposal to `[MIN_SIDEBAR_WIDTH, max(MIN, cols/2)]`.
pub fn clamp_sidebar_width(width: u16, term_cols: u16) -> u16 {
    let max = (term_cols / 2).max(MIN_SIDEBAR_WIDTH);
    width.clamp(MIN_SIDEBAR_WIDTH, max)
}
