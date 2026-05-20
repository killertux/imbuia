use crate::app::{AppState, Mode, UiFocus};
use crate::commands::COMMANDS;
use crate::layout::{ChromeRects, chrome};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let regions = chrome(area, state.sidebar_width);
    let theme = &state.theme;

    // Paint the whole screen with the theme background so empty regions
    // (action bar, tab bar gaps, etc.) inherit the palette.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );

    render_sidebar(frame, regions.sidebar, state);
    render_tab_bar(frame, regions.tab_bar, state);
    render_terminal(frame, regions.terminal, state);
    render_action_bar(frame, regions.action_bar, state, &regions);

    if matches!(state.mode, Mode::Command)
        && let Some(comp) = &state.command_completion
    {
        render_command_completion(frame, regions.action_bar, comp, theme);
    }

    if state.help_open {
        let max_scroll = render_help_popup(frame, area, theme, state.help_scroll);
        state.help_max_scroll.set(max_scroll);
    } else if let Some(popup) = &state.popup {
        render_input_popup(frame, area, popup, theme);
    } else if let Some(open) = &state.open_project_popup {
        render_open_project_popup(frame, area, open, theme);
    } else if let Some(edit) = &state.edit_popup {
        render_edit_popup(frame, area, edit, theme);
    } else if let Some(usage) = &state.usage_popup {
        render_usage_popup(frame, area, usage, theme);
    } else if let Some(launch) = &state.launch_popup {
        render_launch_popup(frame, area, launch, theme);
    }
}

fn render_launch_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::LaunchPopup,
    theme: &Theme,
) {
    let max_label = popup
        .entries
        .iter()
        .map(|e| e.label.len())
        .max()
        .unwrap_or(20);
    let width = ((max_label + 8) as u16)
        .clamp(28, 80)
        .min(area.width.saturating_sub(4));
    let height = (popup.entries.len() as u16 + 4)
        .clamp(6, 24)
        .min(area.height.saturating_sub(4));
    let r = centered_rect(width, height, area);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(" Launch ".to_string())
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let mut lines = Vec::with_capacity(popup.entries.len() + 2);
    for (i, entry) in popup.entries.iter().enumerate() {
        let selected = i as u16 == popup.cursor;
        let prefix = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let detail = match &entry.command {
            Some(c) => format!("  {}", truncate(c, 44)),
            None => "  (plain shell)".to_string(),
        };
        let tag = match entry.source {
            crate::app::LaunchSource::Builtin => "",
            crate::app::LaunchSource::Project => "  [project]",
            crate::app::LaunchSource::Global => "  [global]",
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}{}", entry.label), style),
            Span::styled(detail, Style::default().fg(theme.fg_dim)),
            Span::styled(tag.to_string(), Style::default().fg(theme.fg_dim)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " j/k or ↑↓ · Enter to launch · Esc cancel",
        Style::default().fg(theme.fg_dim),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_open_project_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::OpenProjectPopup,
    theme: &Theme,
) {
    use crate::app::OpenProjectFocus;
    use ratatui::layout::{Constraint, Direction, Layout};

    let width = 70u16.min(area.width.saturating_sub(4));
    let height = 14u16.min(area.height.saturating_sub(4));
    let r = centered_rect(width, height, area);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(" Open project ".to_string())
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // path label + buffer
            Constraint::Length(1), // hint
            Constraint::Length(1), // setup-script label
            Constraint::Min(3),    // textarea
            Constraint::Length(1), // footer
        ])
        .split(inner);

    let path_focused = popup.focus == OpenProjectFocus::Path;
    let path_style = if path_focused {
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_dim)
    };
    let path_line = Line::from(vec![
        Span::styled(" path: ", path_style),
        Span::styled(popup.path.as_str(), Style::default().fg(theme.fg)),
    ]);
    frame.render_widget(Paragraph::new(path_line), chunks[0]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ~ expands to $HOME",
            Style::default().fg(theme.fg_dim),
        ))),
        chunks[1],
    );

    let script_focused = popup.focus == OpenProjectFocus::Script;
    let script_style = if script_focused {
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_dim)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " setup script (optional, runs in each new worktree):",
            script_style,
        ))),
        chunks[2],
    );
    frame.render_widget(&popup.script, chunks[3]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Tab switch · Enter or Ctrl-S confirm · Esc cancel",
            Style::default().fg(theme.fg_dim),
        ))),
        chunks[4],
    );

    if path_focused {
        let cursor_x = chunks[0].x + 7 + popup.path.chars().count() as u16;
        let cursor_y = chunks[0].y;
        if cursor_x < chunks[0].x + chunks[0].width {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

fn render_usage_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::UsagePopup,
    theme: &Theme,
) {
    let w = area.width.saturating_sub(6).clamp(60, 120);
    let h = area.height.saturating_sub(4).clamp(12, 30);
    let r = centered_rect(w, h, area);
    frame.render_widget(Clear, r);

    let title = match &popup.report {
        Some(_) => " Resource usage  (Enter/l expand · h collapse · j/k move · Esc close) ",
        None => " Resource usage  (waiting for first sample…) ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(title.to_string())
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let rows = crate::reducer::usage_visible_rows(popup);
    let report = match &popup.report {
        Some(r) => r,
        None => return,
    };

    // Header line + data lines + footer.
    let mut lines = Vec::with_capacity(rows.len() + 3);
    lines.push(Line::from(vec![Span::styled(
        format!("  {:<48}  {:>10}  {:>8}", "NAME", "RSS", "CPU%"),
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD),
    )]));

    let mut total_rss: u64 = 0;
    let mut total_cpu: f32 = 0.0;
    for (i, row) in rows.iter().enumerate() {
        let selected = i as u16 == popup.cursor;
        let style = if selected {
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let line = match row {
            crate::reducer::UsageRow::Session {
                usage, expanded, ..
            } => {
                let rss = usage.root.total_rss();
                let cpu = usage.root.total_cpu();
                total_rss += rss;
                total_cpu += cpu;
                let marker = if *expanded { "▾" } else { "▸" };
                let label = format!(
                    "{} {}/{}  (pid {})",
                    marker, usage.project_slug, usage.worktree_name, usage.root.pid
                );
                fmt_row(&label, rss, cpu, style)
            }
            crate::reducer::UsageRow::Process { depth, node } => {
                let indent = "  ".repeat(*depth as usize + 1);
                let label = format!("{}└ {}  (pid {})", indent, node.name, node.pid);
                fmt_row(&label, node.rss_bytes, node.cpu_percent, style)
            }
            crate::reducer::UsageRow::Supervisor(node) => fmt_row(
                &format!("◆ imbuia (supervisor pid {})", node.pid),
                node.rss_bytes,
                node.cpu_percent,
                style.fg(theme.fg_dim),
            ),
            crate::reducer::UsageRow::Client(node) => fmt_row(
                &format!("◆ imbuia (client pid {})", node.pid),
                node.rss_bytes,
                node.cpu_percent,
                style.fg(theme.fg_dim),
            ),
        };
        lines.push(line);
    }

    // Footer totals — sessions only, then everything (incl. imbuia itself).
    let app_rss = total_rss
        + report.supervisor.rss_bytes
        + report.client.as_ref().map(|c| c.rss_bytes).unwrap_or(0);
    let app_cpu = total_cpu
        + report.supervisor.cpu_percent
        + report.client.as_ref().map(|c| c.cpu_percent).unwrap_or(0.0);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  {:<48}  {:>10}  {:>7.1}",
            "Σ sessions",
            human_bytes(total_rss),
            total_cpu
        ),
        Style::default().fg(theme.header_fg),
    )]));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  {:<48}  {:>10}  {:>7.1}",
            format!("Σ everything  ({} cores available)", report.cpu_count),
            human_bytes(app_rss),
            app_cpu
        ),
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD),
    )]));

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn fmt_row(label: &str, rss: u64, cpu: f32, style: Style) -> Line<'static> {
    Line::from(vec![Span::styled(
        format!(
            "  {:<48}  {:>10}  {:>7.1}",
            truncate(label, 48),
            human_bytes(rss),
            cpu
        ),
        style,
    )])
}

fn truncate(s: &str, max: usize) -> String {
    // Count columns, not chars — wide chars (CJK, emoji) take 2 columns each
    // and would otherwise overflow the rect they're being truncated into.
    use unicode_width::UnicodeWidthChar;
    let mut width = 0usize;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            // Replace the last kept char(s) with '…' if we ran out of room.
            while width + 1 > max
                && let Some(last) = out.pop()
            {
                width -= UnicodeWidthChar::width(last).unwrap_or(0);
            }
            out.push('…');
            return out;
        }
        out.push(c);
        width += w;
    }
    out
}

fn human_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.0} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

fn render_edit_popup(frame: &mut Frame, area: Rect, edit: &crate::app::EditPopup, theme: &Theme) {
    let w = area.width.saturating_sub(8).clamp(40, 100);
    let h = area.height.saturating_sub(4).clamp(8, 24);
    let r = centered_rect(w, h, area);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(format!(" {} ", edit.title))
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);
    frame.render_widget(&edit.textarea, inner);
}

fn render_input_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::InputPopup,
    theme: &Theme,
) {
    let width = (popup
        .title
        .len()
        .max(popup.prompt.len() + popup.buffer.len() + 4)
        + 6)
    .clamp(40, area.width.saturating_sub(4) as usize) as u16;
    let height: u16 = 5; // borders + title + 1 input row + footer
    let r = centered_rect(width, height, area);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(format!(" {} ", popup.title))
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    // Two rows: input line, then footer.
    let input_line = Line::from(vec![
        Span::styled(
            format!(" {}: ", popup.prompt),
            Style::default().fg(theme.fg_dim),
        ),
        Span::styled(popup.buffer.as_str(), Style::default().fg(theme.fg)),
    ]);
    let footer = Line::from(Span::styled(
        " Enter to confirm · Esc to cancel",
        Style::default().fg(theme.fg_dim),
    ));
    frame.render_widget(
        Paragraph::new(vec![input_line, Line::from(""), footer])
            .style(Style::default().bg(theme.bg)),
        inner,
    );

    // Cursor at end of input.
    let cursor_x =
        inner.x + 1 + popup.prompt.chars().count() as u16 + 2 + popup.buffer.chars().count() as u16;
    let cursor_y = inner.y;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_help_popup(frame: &mut Frame, area: Rect, theme: &Theme, scroll: u16) -> u16 {
    let content_width = 78u16.min(area.width.saturating_sub(4));
    let content_height = area.height.saturating_sub(4).clamp(12, 36);

    let popup = centered_rect(content_width, content_height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(" Help ")
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = help_lines(theme);
    let viewport_h = inner.height.saturating_sub(1); // reserve 1 row for footer
    let max_scroll = (lines.len() as u16).saturating_sub(viewport_h);
    let clipped = scroll.min(max_scroll);

    // Body: viewport into the help lines.
    let body_rect = Rect::new(inner.x, inner.y, inner.width, viewport_h);
    let para = Paragraph::new(lines).style(Style::default().bg(theme.bg));
    frame.render_widget(para.scroll((clipped, 0)), body_rect);

    // Footer: scroll position + key hint.
    let footer = if max_scroll == 0 {
        " j/k or ↑↓ or wheel · Esc close".to_string()
    } else {
        format!(
            " j/k or ↑↓ or wheel · {}/{} · Esc close",
            clipped + 1,
            max_scroll + 1
        )
    };
    let footer_rect = Rect::new(
        inner.x,
        inner.y + viewport_h,
        inner.width,
        inner.height.saturating_sub(viewport_h),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.fg_dim),
        ))),
        footer_rect,
    );

    max_scroll
}

/// Full help content. Built once per frame; cheap (a few dozen lines).
/// Returning owned `Line<'static>` keeps the renderer free of borrow gymnastics.
fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let header = |text: String| -> Line<'static> {
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(theme.header_fg)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let row = |keys: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {:<22}", keys),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(desc.to_string(), Style::default().fg(theme.fg)),
        ])
    };
    let dim = |text: &str| -> Line<'static> {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(theme.fg_dim),
        ))
    };

    lines.push(header("MODES".into()));
    lines.push(row(
        "i",
        "Normal → Terminal (keys are forwarded to the PTY)",
    ));
    lines.push(row("Ctrl-\\ Ctrl-N", "Terminal → Normal"));
    lines.push(row(":", "Normal → Command (ex-line at the bottom)"));
    lines.push(row("Esc", "Command → Normal · close popups"));
    lines.push(Line::from(""));

    lines.push(header("SIDEBAR & FOCUS (Normal mode)".into()));
    lines.push(row("h  ←", "focus sidebar"));
    lines.push(row("l  →", "focus terminal"));
    lines.push(row("j  ↓", "next sidebar row (when focused)"));
    lines.push(row("k  ↑", "previous sidebar row (when focused)"));
    lines.push(row("Enter", "activate selected project/worktree"));
    lines.push(Line::from(""));

    lines.push(header("TABS".into()));
    lines.push(row("o", "open new tab in the active worktree"));
    lines.push(row("x", "close the current tab"));
    lines.push(row("gt", "next tab"));
    lines.push(row("gT", "previous tab"));
    lines.push(Line::from(""));

    lines.push(header("SIDEBAR WIDTH".into()));
    lines.push(row("Ctrl-W >", "grow sidebar by 2 columns"));
    lines.push(row("Ctrl-W <", "shrink sidebar by 2 columns"));
    lines.push(row("Ctrl-W =", "reset sidebar to default width"));
    lines.push(Line::from(""));

    lines.push(header("PROJECTS & WORKTREES".into()));
    lines.push(row("Shift-O", "open project popup (path + setup script)"));
    lines.push(row("Shift-W", "new worktree popup"));
    lines.push(row("Shift-L", "launcher picker (`:launch [name]`)"));
    lines.push(Line::from(""));

    lines.push(header("POPUPS".into()));
    lines.push(row("Esc", "cancel / close"));
    lines.push(row("Enter", "confirm single-line input"));
    lines.push(row("Tab", "switch focus (open project)"));
    lines.push(row("Ctrl-S", "save (edit · open project)"));
    lines.push(row("j/k · wheel", "scroll (help · usage)"));
    lines.push(Line::from(""));

    lines.push(header("TERMINAL".into()));
    lines.push(row(
        "Shift+wheel",
        "scroll local scrollback (bypasses TUI app)",
    ));
    lines.push(row("wheel (alt-screen)", "synthesised ↑/↓ for vim/less"));
    lines.push(row("Option+drag (iTerm)", "native text selection"));
    lines.push(Line::from(""));

    lines.push(header("MISC".into()));
    lines.push(row("Ctrl-Q", "quit immediately (no confirmation)"));
    lines.push(row(":help", "open this popup"));
    lines.push(Line::from(""));

    lines.push(header("COMMANDS".into()));
    let usage_col = COMMANDS.iter().map(|c| c.usage.len()).max().unwrap_or(20) + 2;
    for spec in COMMANDS {
        let pad = usage_col.saturating_sub(spec.usage.len());
        let usage = format!("  {}{}", spec.usage, " ".repeat(pad));
        lines.push(Line::from(vec![
            Span::styled(
                usage,
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(spec.description.to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(dim(
        "  (this list is the source of truth — see /commands.rs)",
    ));

    lines
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn render_sidebar(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from("PROJECTS")
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Left),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let focused = state.ui_focus == UiFocus::Sidebar;
    let max_w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (pi, project) in state.projects.iter().enumerate() {
        let marker = if project.expanded { "▼" } else { "▶" };
        let is_selected_header = state.sidebar_selection == Some((pi, None));
        lines.push(styled_line(
            truncate_to_width(&format!("{marker} {}", project.name), max_w),
            is_selected_header,
            focused,
            true,
            theme,
        ));
        if project.expanded {
            for (wi, wt) in project.worktrees.iter().enumerate() {
                let active = state.active_worktree == Some((pi, wi));
                let dot = if active { "●" } else { "○" };
                let selected = state.sidebar_selection == Some((pi, Some(wi)));
                lines.push(styled_line(
                    truncate_to_width(&format!("  {dot} {}", wt.name), max_w),
                    selected,
                    focused,
                    false,
                    theme,
                ));
            }
        }
    }

    let para = Paragraph::new(lines)
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .scroll((state.sidebar_scroll, 0));
    frame.render_widget(para, inner);
}

/// Truncate `s` to fit within `max_chars` columns. If truncated, the last
/// visible char is replaced with `…`.
fn truncate_to_width(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

fn styled_line(
    text: String,
    selected: bool,
    focused: bool,
    bold: bool,
    theme: &Theme,
) -> Line<'static> {
    let mut style = Style::default().fg(theme.fg).bg(theme.bg);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if selected {
        if focused {
            style = style
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD);
        } else {
            style = style.bg(theme.selection_bg).fg(theme.fg_dim);
        }
    }
    Line::from(Span::styled(text, style))
}

fn render_tab_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some((pi, wi)) = state.active_worktree else {
        return;
    };
    let Some(wt) = state.projects.get(pi).and_then(|p| p.worktrees.get(wi)) else {
        return;
    };
    if wt.sessions.is_empty() {
        return;
    }

    let mut spans: Vec<Span> = Vec::new();
    for (i, _sid) in wt.sessions.iter().enumerate() {
        let active = wt.active_tab == Some(i);
        let label = format!(" {} ", i + 1);
        let style = if active {
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.bg).fg(theme.fg_dim)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        inner,
    );
}

fn render_terminal(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let id = match (state.active_worktree, state.focused_session_id()) {
        (Some(_), Some(id)) => id,
        (Some(_), None) => {
            render_centered_message(
                frame,
                area,
                "No terminal open — press 'o' or :tabnew to open one.",
                theme,
            );
            return;
        }
        (None, _) => {
            if state.projects.is_empty() {
                render_centered_lines(
                    frame,
                    area,
                    &[
                        "No projects yet.",
                        "",
                        "Press O or run :open <path> to add one.",
                        "Type :help to see all commands.",
                    ],
                    theme,
                );
            } else {
                render_centered_message(
                    frame,
                    area,
                    "No worktree selected — press Enter on a worktree in the sidebar.",
                    theme,
                );
            }
            return;
        }
    };
    let Some(sess) = state.sessions.get(&id) else {
        return;
    };

    // Snapshot the visible region under the parser lock, then release it
    // before drawing into ratatui's buffer. The supervisor reader thread
    // also locks this parser on every PTY chunk; holding it across the full
    // cell loop ping-pongs us under load. Drawing into `buf` doesn't need
    // the lock — it's owned by the main render task.
    struct Snap {
        cells: Vec<(u16, u16, String, Style)>,
        cursor: Option<(u16, u16)>,
    }
    let snap = {
        let parser = sess.parser().lock().expect("parser poisoned");
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let h = area.height.min(rows);
        let w = area.width.min(cols);
        let mut cells = Vec::with_capacity((h as usize) * (w as usize));
        for r in 0..h {
            for c in 0..w {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                let contents = cell.contents();
                let symbol = if contents.is_empty() {
                    " ".to_string()
                } else {
                    contents.to_string()
                };
                let mut style = Style::default()
                    .fg(convert_color(cell.fgcolor(), theme.fg))
                    .bg(convert_color(cell.bgcolor(), theme.bg));
                let mut mods = Modifier::empty();
                if cell.bold() {
                    mods |= Modifier::BOLD;
                }
                if cell.italic() {
                    mods |= Modifier::ITALIC;
                }
                if cell.underline() {
                    mods |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    mods |= Modifier::REVERSED;
                }
                style = style.add_modifier(mods);
                cells.push((c, r, symbol, style));
            }
        }
        let cursor = if screen.hide_cursor() {
            None
        } else {
            let (cy, cx) = screen.cursor_position();
            Some((cx, cy))
        };
        Snap { cells, cursor }
    };

    let buf = frame.buffer_mut();
    for (c, r, symbol, style) in snap.cells {
        let bcell = &mut buf[(area.x + c, area.y + r)];
        if symbol == " " {
            bcell.set_char(' ');
        } else {
            bcell.set_symbol(&symbol);
        }
        bcell.set_style(style);
    }
    if let Some((cx, cy)) = snap.cursor {
        let x = area.x + cx.min(area.width.saturating_sub(1));
        let y = area.y + cy.min(area.height.saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
}

fn render_action_bar(frame: &mut Frame, area: Rect, state: &AppState, regions: &ChromeRects) {
    let theme = &state.theme;
    let middle = match state.active_worktree {
        Some((pi, wi)) => state
            .projects
            .get(pi)
            .and_then(|p| {
                p.worktrees.get(wi).map(|w| {
                    format!(
                        "{} · {} · {}×{}",
                        p.name, w.name, regions.terminal.height, regions.terminal.width
                    )
                })
            })
            .unwrap_or_default(),
        None => {
            if state.projects.is_empty() {
                "no projects — :open <path> or press O".to_string()
            } else {
                "no worktree".to_string()
            }
        }
    };

    let line = match state.mode {
        Mode::Normal => normal_or_terminal_line(
            state,
            &middle,
            " -- NORMAL -- ",
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Mode::Terminal => normal_or_terminal_line(
            state,
            &middle,
            " -- TERMINAL -- ",
            Style::default()
                .fg(theme.bg)
                .bg(theme.status_ok)
                .add_modifier(Modifier::BOLD),
        ),
        Mode::Command => Line::from(vec![Span::styled(
            format!(":{}", state.command),
            Style::default().fg(theme.fg),
        )]),
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );

    if matches!(state.mode, Mode::Command) {
        // `:` + buffer; place cursor at the next column.
        let len = state.command.chars().count() as u16 + 1;
        let x = (area.x + len).min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((x, area.y));
    }
}

fn normal_or_terminal_line<'a>(
    state: &'a AppState,
    middle: &'a str,
    label: &'static str,
    label_style: Style,
) -> Line<'a> {
    let theme = &state.theme;
    let mut spans: Vec<Span<'a>> = vec![
        Span::styled(label, label_style),
        Span::raw("  "),
        Span::styled(middle, Style::default().fg(theme.fg_dim)),
    ];
    if let Some(busy) = &state.pending_op {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            busy.as_str(),
            Style::default()
                .fg(theme.status_running)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(status) = &state.command_status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            status.as_str(),
            Style::default().fg(theme.status_error),
        ));
    }
    Line::from(spans)
}

fn render_command_completion(
    frame: &mut Frame,
    action_bar: Rect,
    comp: &crate::app::CommandCompletion,
    _theme: &Theme,
) {
    if comp.matches.is_empty() {
        return;
    }
    // Popover uses a fixed dark palette regardless of the active theme — black
    // bg, white fg — so it stands out as a floating menu in light mode too.
    let popover_bg = Color::Rgb(0x14, 0x14, 0x18);
    let popover_fg = Color::Rgb(0xF5, 0xF5, 0xF5);
    let popover_dim = Color::Rgb(0xA8, 0xA8, 0xB0);
    let selected_bg = Color::Rgb(0x3A, 0x3A, 0x4A);

    let name_w = comp.matches.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let desc_w = comp.matches.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
    // 1 (left pad) + name + 2 (gap) + desc + 1 (right pad).
    let want_w = (name_w + desc_w + 4) as u16;
    let w = want_w.min(action_bar.width).max(20);
    let h = (comp.matches.len() as u16).min(8);
    if h == 0 || action_bar.y == 0 {
        return;
    }
    let y = action_bar.y.saturating_sub(h);
    let r = Rect::new(action_bar.x, y, w, h);
    frame.render_widget(Clear, r);

    let mut lines: Vec<Line> = Vec::with_capacity(comp.matches.len());
    for (i, (name, desc)) in comp.matches.iter().enumerate() {
        let selected = comp.selected == Some(i);
        let row_bg = if selected { selected_bg } else { popover_bg };
        let name_style = Style::default()
            .bg(row_bg)
            .fg(popover_fg)
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let desc_style = Style::default().bg(row_bg).fg(popover_dim);
        let padded_name = format!(" :{name:<width$}  ", width = name_w);
        let desc_text = format!("{desc} ");
        lines.push(Line::from(vec![
            Span::styled(padded_name, name_style),
            Span::styled(desc_text, desc_style),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(popover_bg).fg(popover_fg)),
        r,
    );
}

fn render_centered_message(frame: &mut Frame, area: Rect, msg: &str, theme: &Theme) {
    let para = Paragraph::new(msg)
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme.fg_dim).bg(theme.bg));
    frame.render_widget(para, vertical_center(area, 1));
}

fn render_centered_lines(frame: &mut Frame, area: Rect, lines: &[&str], theme: &Theme) {
    let body: Vec<Line> = lines
        .iter()
        .map(|s| Line::from(Span::raw(*s)).alignment(Alignment::Center))
        .collect();
    let h = body.len() as u16;
    let para = Paragraph::new(body).style(Style::default().fg(theme.fg_dim).bg(theme.bg));
    frame.render_widget(para, vertical_center(area, h));
}

fn vertical_center(area: Rect, lines: u16) -> Rect {
    let top = area.y + area.height.saturating_sub(lines) / 2;
    Rect::new(area.x, top, area.width, lines.min(area.height))
}

fn convert_color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
