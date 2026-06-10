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
        let max_scroll = render_help_popup(frame, area, theme, state.help_scroll, state);
        state.help_max_scroll.set(max_scroll);
    } else if let Some(popup) = &state.popup {
        render_input_popup(frame, area, popup, theme);
    } else if let Some(open) = &state.open_project_popup {
        render_open_project_popup(frame, area, open, &state.supervisors, theme);
    } else if let Some(edit) = &state.edit_popup {
        render_edit_popup(frame, area, edit, theme);
    } else if let Some(usage) = &state.usage_popup {
        render_usage_popup(frame, area, usage, theme);
    } else if let Some(launch) = &state.launch_popup {
        render_launch_popup(frame, area, launch, theme);
    } else if let Some(palette) = &state.palette_popup {
        render_palette_popup(frame, area, palette, theme);
    }

    // Always last so it floats over whatever is below.
    if pending_chord_is_space(state) {
        render_space_leader_hint(frame, area, theme, state);
    }
}

/// True when exactly one chord — `<Space>` — is pending. Used to gate the
/// which-key popup so it only appears for the Space leader (not for `g` or
/// `<C-w>`).
fn pending_chord_is_space(state: &AppState) -> bool {
    state.pending_chord.len() == 1
        && state.pending_chord[0].code == ratatui::crossterm::event::KeyCode::Char(' ')
}

fn render_space_leader_hint(frame: &mut Frame, area: Rect, theme: &Theme, state: &AppState) {
    use crate::keybinds::{Binding, Chord, Scope, format_binding};
    let space = Chord {
        mods: ratatui::crossterm::event::KeyModifiers::NONE,
        code: ratatui::crossterm::event::KeyCode::Char(' '),
    };
    // Build the hint table from the live keymap: every Normal binding that
    // starts with `<Space>`. Display just the remainder after `<Space>`.
    let hints: Vec<(String, &'static str)> = state
        .keymap
        .bindings_starting_with(Scope::Normal, &space)
        .filter_map(|(b, a)| {
            if b.0.len() < 2 {
                return None;
            }
            let tail = Binding(b.0[1..].to_vec());
            Some((format_binding(&tail), a.description()))
        })
        .collect();
    if hints.is_empty() {
        return;
    }
    let inner_w = hints
        .iter()
        .map(|(k, d)| k.chars().count() + 3 + d.chars().count())
        .max()
        .unwrap_or(16) as u16
        + 4;
    let width = inner_w.min(area.width.saturating_sub(2));
    let height = (hints.len() as u16 + 3).min(area.height.saturating_sub(2));

    // Anchor to bottom-right so it doesn't cover the terminal viewport.
    let x = area.x + area.width.saturating_sub(width).saturating_sub(1);
    let y = area.y + area.height.saturating_sub(height).saturating_sub(2);
    let r = Rect::new(x, y, width, height);
    frame.render_widget(Clear, r);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(" <Space> ")
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Left),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let mut lines = Vec::with_capacity(hints.len() + 1);
    for (key, desc) in &hints {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<2}", key),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".to_string(), Style::default()),
            Span::styled((*desc).to_string(), Style::default().fg(theme.fg_dim)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        " Esc cancel".to_string(),
        Style::default().fg(theme.fg_dim),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
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

fn render_palette_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::PalettePopup,
    theme: &Theme,
) {
    let width = 72u16.min(area.width.saturating_sub(4));
    // Anchor the top edge at a fixed Y (~1/6 down the screen) instead of
    // centering: as typing narrows the list the popup shrinks, and a centered
    // rect would make the query line jump every keystroke. With a fixed top
    // only the bottom edge moves.
    let y = area.y + area.height / 6;
    let avail = area.height.saturating_sub(area.height / 6 + 2);
    // query line + list + hint line + borders.
    let height = (popup.filtered.len() as u16 + 5).clamp(7, 24).min(avail);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let r = Rect::new(x, y, width, height);
    frame.render_widget(Clear, r);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .title(
            Line::from(" Commands ".to_string())
                .style(
                    Style::default()
                        .fg(theme.header_fg)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
        );
    let inner = block.inner(r);
    frame.render_widget(block, r);

    let mut lines = Vec::with_capacity(inner.height as usize);
    lines.push(Line::from(vec![
        Span::styled(" > ", Style::default().fg(theme.header_fg)),
        Span::styled(popup.query.clone(), Style::default().fg(theme.fg)),
        Span::styled("█", Style::default().fg(theme.fg_dim)),
    ]));

    // List window: keep the cursor visible (query + hint rows eat 2 lines).
    let visible = inner.height.saturating_sub(2) as usize;
    let offset = (popup.cursor as usize + 1).saturating_sub(visible);
    let name_w = 18usize;
    for (row, &idx) in popup.filtered.iter().enumerate().skip(offset).take(visible) {
        let entry = &popup.entries[idx];
        let selected = row as u16 == popup.cursor;
        let prefix = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let keybind = match &entry.keybind {
            Some(b) => format!("  {b}"),
            None => String::new(),
        };
        let desc_room = (width as usize)
            .saturating_sub(2 + name_w + 1 + keybind.len() + 2)
            .max(8);
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}{:<name_w$}", entry.name), style),
            Span::styled(
                format!(" {}", truncate(entry.description, desc_room)),
                Style::default().fg(theme.fg_dim),
            ),
            Span::styled(keybind, Style::default().fg(theme.fg_dim)),
        ]));
    }
    if popup.filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching commands",
            Style::default().fg(theme.fg_dim),
        )));
    }
    lines.push(Line::from(Span::styled(
        " type to filter · ↑↓/C-p C-n move · Enter run · Esc cancel",
        Style::default().fg(theme.fg_dim),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_open_project_popup(
    frame: &mut Frame,
    area: Rect,
    popup: &crate::app::OpenProjectPopup,
    supervisors: &crate::app::SupervisorDirectory,
    theme: &Theme,
) {
    use crate::app::OpenProjectFocus;
    use ratatui::layout::{Constraint, Direction, Layout};

    let width = 70u16.min(area.width.saturating_sub(4));
    let height = 16u16.min(area.height.saturating_sub(4));
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
            Constraint::Length(1), // supervisor selector
            Constraint::Length(1), // current dir
            Constraint::Min(4),    // directory browser
            Constraint::Length(1), // setup-script label
            Constraint::Min(2),    // textarea
            Constraint::Length(1), // import toggle
            Constraint::Length(1), // footer
        ])
        .split(inner);

    // Supervisor selector.
    let sup_focused = popup.focus == OpenProjectFocus::Supervisor;
    let sup_style = if sup_focused {
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_dim)
    };
    let sup_name = supervisors.name_of(popup.supervisor);
    let sup_hint = if sup_focused {
        "  (←/→ change)"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" supervisor: ", sup_style),
            Span::styled(sup_name.to_string(), Style::default().fg(theme.fg)),
            Span::styled(sup_hint, Style::default().fg(theme.fg_dim)),
        ])),
        chunks[0],
    );

    // Current directory.
    let path_focused = popup.focus == OpenProjectFocus::Path;
    let dir_style = if path_focused {
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_dim)
    };
    let cur_dir = popup
        .browser
        .as_ref()
        .map(|b| b.dir.to_string_lossy().to_string())
        .unwrap_or_else(|| "(listing…)".into());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" dir: ", dir_style),
            Span::styled(cur_dir, Style::default().fg(theme.fg)),
        ])),
        chunks[1],
    );

    // Directory browser list.
    let mut rows: Vec<Line> = Vec::new();
    if let Some(browser) = popup.browser.as_ref() {
        if browser.parent.is_some() {
            rows.push(Line::from(Span::styled(
                "  ../",
                Style::default().fg(theme.fg_dim),
            )));
        }
        let view_h = chunks[2].height as usize;
        // Keep the cursor in view with a simple top-anchored window.
        let start = (browser.cursor as usize).saturating_sub(view_h.saturating_sub(2));
        for (i, e) in browser.entries.iter().enumerate().skip(start) {
            let selected = path_focused && i as u16 == browser.cursor;
            let marker = if e.is_repo { "◆ " } else { "  " };
            let label = format!("  {marker}{}/", e.name);
            let style = if selected {
                Style::default()
                    .bg(theme.selection_bg)
                    .fg(theme.selection_fg)
                    .add_modifier(Modifier::BOLD)
            } else if e.is_repo {
                Style::default().fg(theme.fg)
            } else {
                Style::default().fg(theme.fg_dim)
            };
            rows.push(Line::from(Span::styled(label, style)));
        }
    }
    frame.render_widget(Paragraph::new(rows), chunks[2]);

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
        chunks[3],
    );
    frame.render_widget(&popup.script, chunks[4]);

    let import_focused = popup.focus == OpenProjectFocus::Import;
    let import_style = if import_focused {
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_dim)
    };
    let checkbox = if popup.import_existing { "[x]" } else { "[ ]" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {checkbox} "), import_style),
            Span::styled(
                "import existing worktrees (git worktree list)",
                import_style,
            ),
        ])),
        chunks[5],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ move · Enter/→ open repo or descend · ←/Bksp up · Ctrl-S open dir · Tab cycle · Esc",
            Style::default().fg(theme.fg_dim),
        ))),
        chunks[6],
    );
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

    let title = if popup.reports.is_empty() && popup.client.is_none() {
        " Resource usage  (waiting for first sample…) "
    } else {
        " Resource usage  (Enter/l expand · h collapse · j/k move · Esc close) "
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
    if rows.is_empty() {
        return;
    }

    // Header line + data lines + footer.
    let mut lines = Vec::with_capacity(rows.len() + 4);
    lines.push(Line::from(vec![Span::styled(
        format!("  {:<48}  {:>10}  {:>8}", "NAME", "RSS", "CPU%"),
        Style::default()
            .fg(theme.header_fg)
            .add_modifier(Modifier::BOLD),
    )]));

    // Σ sessions across every supervisor; Σ everything also folds in each
    // supervisor's own process + the client.
    let mut session_rss: u64 = 0;
    let mut session_cpu: f32 = 0.0;
    let mut app_rss: u64 = 0;
    let mut app_cpu: f32 = 0.0;
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
            crate::reducer::UsageRow::SupervisorHeader { name } => Line::from(vec![Span::styled(
                format!("▏{name}"),
                Style::default()
                    .fg(theme.header_fg)
                    .add_modifier(Modifier::BOLD),
            )]),
            crate::reducer::UsageRow::Session {
                usage, expanded, ..
            } => {
                let rss = usage.root.total_rss();
                let cpu = usage.root.total_cpu();
                session_rss += rss;
                session_cpu += cpu;
                app_rss += rss;
                app_cpu += cpu;
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
            crate::reducer::UsageRow::Supervisor(node) => {
                app_rss += node.rss_bytes;
                app_cpu += node.cpu_percent;
                fmt_row(
                    &format!("  ◆ imbuia (supervisor pid {})", node.pid),
                    node.rss_bytes,
                    node.cpu_percent,
                    style.fg(theme.fg_dim),
                )
            }
            crate::reducer::UsageRow::Client(node) => {
                app_rss += node.rss_bytes;
                app_cpu += node.cpu_percent;
                fmt_row(
                    &format!("◆ imbuia (client pid {})", node.pid),
                    node.rss_bytes,
                    node.cpu_percent,
                    style.fg(theme.fg_dim),
                )
            }
        };
        lines.push(line);
    }

    let cores = popup
        .reports
        .values()
        .map(|r| r.cpu_count)
        .max()
        .unwrap_or(0);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  {:<48}  {:>10}  {:>7.1}",
            "Σ sessions",
            human_bytes(session_rss),
            session_cpu
        ),
        Style::default().fg(theme.header_fg),
    )]));
    lines.push(Line::from(vec![Span::styled(
        format!(
            "  {:<48}  {:>10}  {:>7.1}",
            format!("Σ everything  ({cores} cores available)"),
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

fn render_help_popup(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    scroll: u16,
    state: &AppState,
) -> u16 {
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

    let lines = help_lines(theme, state);
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

/// Full help content. Sources key strings from the live keymap so the popup
/// reflects the user's actual bindings.
fn help_lines(theme: &Theme, state: &AppState) -> Vec<Line<'static>> {
    use crate::keybinds::BindableAction as A;
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
    // Resolve a bindable action's key string from the live keymap. Falls
    // back to "(unbound)" if the user removed the default and didn't add a
    // replacement — `:help` still shows them what's unbound.
    let key = |a: A| -> String {
        state
            .keymap
            .binding_for(a)
            .unwrap_or_else(|| "(unbound)".into())
    };
    let kr = |a: A| -> Line<'static> { row(&key(a), a.description()) };

    lines.push(header("MODES".into()));
    lines.push(kr(A::EnterTerminalMode));
    lines.push(kr(A::LeaveTerminal));
    lines.push(kr(A::EnterCommandMode));
    lines.push(row("<Esc>", "Command → Normal · close popups"));
    lines.push(Line::from(""));

    lines.push(header("SIDEBAR & FOCUS (Normal mode)".into()));
    lines.push(kr(A::FocusSidebar));
    lines.push(kr(A::FocusTerminal));
    lines.push(kr(A::SidebarDown));
    lines.push(kr(A::SidebarUp));
    lines.push(kr(A::ActivateSelection));
    lines.push(Line::from(""));

    lines.push(header("TABS".into()));
    lines.push(kr(A::OpenTab));
    lines.push(kr(A::CloseTab));
    lines.push(kr(A::NextTab));
    lines.push(kr(A::PrevTab));
    lines.push(Line::from(""));

    lines.push(header("SIDEBAR WIDTH".into()));
    lines.push(kr(A::SidebarGrow));
    lines.push(kr(A::SidebarShrink));
    lines.push(kr(A::SidebarReset));
    lines.push(Line::from(""));

    lines.push(header("LEADER".into()));
    lines.push(kr(A::OpenProjectPopup));
    lines.push(kr(A::NewWorktree));
    lines.push(kr(A::RemoveWorktree));
    lines.push(kr(A::LaunchPicker));
    lines.push(kr(A::EditProject));
    lines.push(kr(A::UsagePopup));
    lines.push(kr(A::HelpPopup));
    lines.push(kr(A::Quit));
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
        // Tag projects hosted on a remote supervisor with its name; local
        // (the default) stays unadorned. A disconnected remote is marked
        // `[name ✗]` and the whole row is dimmed (see `dim` below).
        let connected = state.supervisors.is_connected(project.supervisor);
        let header = match state.supervisors.config_name(project.supervisor) {
            Some(sup) if connected => format!("{marker} {}  [{sup}]", project.name),
            Some(sup) => format!("{marker} {}  [{sup} ✗]", project.name),
            None => format!("{marker} {}", project.name),
        };
        // Dim only remote projects that are currently offline.
        let dim = !connected && project.supervisor != crate::app::LOCAL;
        lines.push(styled_line(
            truncate_to_width(&header, max_w),
            is_selected_header,
            focused,
            true,
            dim,
            theme,
        ));
        if project.expanded {
            for (wi, wt) in project.worktrees.iter().enumerate() {
                let active = state.active_worktree == Some((pi, wi));
                let dot = if active { "●" } else { "○" };
                let selected = state.sidebar_selection == Some((pi, Some(wi)));
                lines.push(worktree_line(
                    pi,
                    wi,
                    wt.name.as_str(),
                    dot,
                    selected,
                    focused,
                    max_w,
                    state,
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

/// Sidebar row for a single worktree. Renders a leading colored `▌` reflecting
/// the GitHub PR status (dim when no PR / integration disabled), then the
/// usual `● name` / `○ name` content. Two spans so the bar can have its own
/// color independent of selection styling.
#[allow(clippy::too_many_arguments)]
fn worktree_line(
    pi: usize,
    wi: usize,
    name: &str,
    dot: &str,
    selected: bool,
    focused: bool,
    max_w: usize,
    state: &AppState,
) -> Line<'static> {
    use crate::app::PrStatus;
    let theme = &state.theme;
    let bar_color = match state.pr_statuses.get(&(pi, wi)) {
        Some(PrStatus::Merged) => theme.status_changes,
        Some(PrStatus::ChangesRequested) | Some(PrStatus::Failed) => theme.status_error,
        Some(PrStatus::Running) => theme.status_running,
        Some(PrStatus::Approved) => theme.status_approved,
        Some(PrStatus::Open) => theme.status_ok,
        None => theme.fg_dim,
    };
    // The bar consumes 2 visible columns ("▌ "); the rest of the row uses
    // the existing format, capped to whatever's left of `max_w`.
    let bar_text = "▌ ";
    let rest_budget = max_w.saturating_sub(2);
    let rest_text = truncate_to_width(&format!("{dot} {name}"), rest_budget);

    let mut row_style = Style::default().fg(theme.fg).bg(theme.bg);
    if selected {
        if focused {
            row_style = row_style
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .add_modifier(Modifier::BOLD);
        } else {
            row_style = row_style.bg(theme.selection_bg).fg(theme.fg_dim);
        }
    }
    // The bar keeps its color regardless of selection; only the bg follows
    // the row so the selection highlight stays continuous.
    let bar_style = Style::default()
        .fg(bar_color)
        .bg(row_style.bg.unwrap_or(theme.bg));

    Line::from(vec![
        Span::styled(bar_text.to_string(), bar_style),
        Span::styled(rest_text, row_style),
    ])
}

fn styled_line(
    text: String,
    selected: bool,
    focused: bool,
    bold: bool,
    dim: bool,
    theme: &Theme,
) -> Line<'static> {
    let base_fg = if dim { theme.fg_dim } else { theme.fg };
    let mut style = Style::default().fg(base_fg).bg(theme.bg);
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
    if let Some(prompt) = confirm_prompt_text(state) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            prompt,
            Style::default()
                .fg(theme.status_error)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(busy) = &state.pending_op {
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
    // Auto-update banner: only when no busy-op and no error message are
    // already shouting in the same row.
    if state.pending_op.is_none()
        && state.command_status.is_none()
        && state.pending_confirm.is_none()
        && let Some(banner) = update_banner_text(state)
    {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(banner, Style::default().fg(theme.fg_dim)));
    }
    Line::from(spans)
}

fn confirm_prompt_text(state: &AppState) -> Option<String> {
    use crate::app::PendingConfirm;
    match state.pending_confirm.as_ref()? {
        PendingConfirm::RemoveWorktree { name, .. } => {
            Some(format!("delete worktree '{name}'? [y/N]"))
        }
    }
}

fn update_banner_text(state: &AppState) -> Option<String> {
    use crate::app::UpdateStatus;
    match state.update_status {
        UpdateStatus::Installing => Some("updating…".into()),
        UpdateStatus::Checking => None,
        _ => state
            .available_update
            .as_ref()
            .map(|info| format!("{} available · :update to install", info.latest_tag)),
    }
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
