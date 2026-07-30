//! The review screen: a to-do list of differences.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{App, Item};
use crate::core::diff::{Row, Severity};

/// Fixed leading gutter: cursor, mark, and the spaces around them.
const GUTTER: usize = 5;
/// Two double-space column separators.
const SEPARATORS: usize = 4;
/// Never squeeze the action label below this; it is the part that says what will
/// happen, so it is the last thing that should be truncated.
const MIN_ACTION_W: usize = 24;

/// Column widths sized to the content, then shrunk to fit the terminal.
///
/// Fixed widths truncated real names like `sentry-setup-ai-monitoring` and
/// `marketplace claude-plugins-official` into uselessness, but a fully ragged
/// layout is unscannable — so the columns are uniform down the page and merely
/// computed rather than hardcoded.
fn columns(app: &App, total: usize) -> (usize, usize) {
    let visible: Vec<&Row> = app
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Row(i) => Some(&app.rows[*i]),
            Item::Header(_) => None,
        })
        .collect();

    let longest = |f: fn(&Row) -> &str| {
        visible
            .iter()
            .map(|r| f(r).chars().count())
            .max()
            .unwrap_or(0)
    };
    let mut name = longest(|r| r.name.as_str()).clamp(8, 34);
    let mut headline = longest(|r| r.headline.as_str()).clamp(8, 40);

    // Give the action label its floor by taking width back, headline first.
    let available = total.saturating_sub(GUTTER + SEPARATORS + MIN_ACTION_W);
    if name + headline > available {
        let overflow = name + headline - available;
        let from_headline = overflow.min(headline.saturating_sub(8));
        headline -= from_headline;
        name = name.saturating_sub(overflow - from_headline).max(8);
    }
    (name, headline)
}

pub fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(2), // header
        Constraint::Min(3),    // list
        Constraint::Length(3), // detail
        Constraint::Length(1), // footer
    ])
    .split(area);

    draw_header(app, frame, chunks[0]);
    draw_list(app, frame, chunks[1]);
    draw_detail(app, frame, chunks[2]);
    draw_footer(app, frame, chunks[3]);
}

/// The project picker.
///
/// Repos are discovered from the manifest and from each host's per-repo config.
/// A repo that has no agent configuration yet cannot be discovered, so it is
/// added with `agentsync --repo <path>` rather than typed in here — a text field
/// would be the only place in the UI that takes free input.
pub fn draw_projects(app: &App, frame: &mut Frame) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec!["Focus a project".bold()]),
            Line::from("Per-repo rows are limited to the choice; global ones always show.".dim()),
        ]),
        chunks[0],
    );

    let mut lines = vec![entry_line("all projects", app.project_cursor == 0, None)];
    for (index, repo) in app.projects.iter().enumerate() {
        let count = app
            .rows
            .iter()
            .filter(|r| {
                r.key
                    .host_scopes
                    .iter()
                    .any(|s| s.repo() == Some(repo.as_str()))
            })
            .count();
        lines.push(entry_line(
            &crate::core::model::short_repo(repo),
            app.project_cursor == index + 1,
            Some(count),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    let mut spans = vec![Span::raw(" ")];
    for (key, what) in [("\u{23ce}", "focus"), ("q", "back")] {
        spans.push(Span::styled(key, Style::new().fg(Color::Cyan)));
        spans.push(Span::raw(format!(" {what}   ")));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[2]);
}

fn entry_line<'a>(label: &str, selected: bool, count: Option<usize>) -> Line<'a> {
    let cursor = if selected { "\u{25b8}" } else { " " };
    let mut spans = vec![
        Span::raw(format!(" {cursor} ")),
        Span::raw(label.to_string()),
    ];
    if let Some(count) = count {
        spans.push(Span::raw("   "));
        spans.push(format!("{count} per-repo row(s)").dim());
    }
    let line = Line::from(spans);
    if selected {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

fn draw_header(app: &App, frame: &mut Frame, area: Rect) {
    let accepted = app.accepted_count();
    let todo = app.todo_count();

    let mut hosts: Vec<Span> = vec!["hosts: ".dim()];
    for (host, _) in app.world.detected() {
        hosts.push(Span::raw(host.name().to_string()));
        hosts.push(Span::styled(" \u{25cf}  ", Style::new().fg(Color::Green)));
    }
    for host in app.world.missing_hosts() {
        hosts.push(host.name().to_string().dim());
        hosts.push(" \u{25cb}  ".dim());
    }
    if let Some(project) = &app.project_filter {
        hosts.push("   project: ".dim());
        hosts.push(Span::styled(
            crate::core::model::short_repo(project),
            Style::new().fg(Color::Magenta),
        ));
    }

    let counts = if accepted > 0 {
        Line::from(vec![
            "agentsync".bold(),
            Span::raw("     "),
            Span::raw(format!("{todo} to review")),
            "   \u{b7}   ".dim(),
            Span::styled(
                format!("{accepted} accepted"),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            "agentsync".bold(),
            Span::raw("     "),
            Span::raw(format!("{todo} to review")),
        ])
    };

    frame.render_widget(Paragraph::new(vec![counts, Line::from(hosts)]), area);
}

fn draw_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let height = area.height as usize;
    let (name_w, headline_w) = columns(app, area.width as usize);

    // Keep the cursor in view.
    if app.cursor < app.offset {
        app.offset = app.cursor;
    } else if app.cursor >= app.offset + height {
        app.offset = app.cursor + 1 - height;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (index, item) in app.items.iter().enumerate().skip(app.offset).take(height) {
        match item {
            Item::Header(domain) => {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        domain.title(),
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            Item::Row(i) => {
                let row = &app.rows[*i];
                let selected = index == app.cursor;

                let cursor = if selected { "\u{25b8}" } else { " " };
                let mark = row.severity.mark(row.accepted);
                let mark_style = if row.accepted {
                    Style::new().fg(Color::Green)
                } else {
                    match row.severity {
                        Severity::Warn => Style::new().fg(Color::Yellow),
                        Severity::Blocked => Style::new().fg(Color::DarkGray),
                        _ => Style::new(),
                    }
                };

                let name_style = match row.severity {
                    Severity::Blocked => Style::new().fg(Color::DarkGray),
                    Severity::Synced => Style::new().fg(Color::DarkGray),
                    _ => Style::new(),
                };

                let action_style = if row.accepted {
                    Style::new().fg(Color::Green)
                } else if row.actionable() {
                    Style::new().fg(Color::Blue)
                } else {
                    Style::new().fg(Color::DarkGray)
                };

                // Truncate the action label rather than letting the terminal
                // clip it: a hard cut mid-word reads as a rendering bug, and on
                // this column it would hide what is about to happen.
                let action_w =
                    (area.width as usize).saturating_sub(GUTTER + SEPARATORS + name_w + headline_w);
                let line = Line::from(vec![
                    Span::raw(format!(" {cursor} ")),
                    Span::styled(mark.to_string(), mark_style),
                    Span::raw(" "),
                    Span::styled(pad(&row.name, name_w), name_style),
                    Span::raw("  "),
                    Span::styled(pad(&row.headline, headline_w), Style::new().fg(Color::Gray)),
                    Span::raw("  "),
                    Span::styled(pad(&row.action().label, action_w), action_style),
                ]);

                let mut line = line;
                if selected {
                    line = line.style(Style::new().add_modifier(Modifier::REVERSED));
                }
                lines.push(line);
            }
        }
    }

    if app.items.is_empty() {
        lines.push(Line::from(vec![Span::raw("  ")]));
        lines.push(Line::from(vec![
            "  Everything is in sync. ".into(),
            "Press v to see it, q to quit.".dim(),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_detail(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(Color::DarkGray));

    let lines = match app.selected() {
        None => vec![Line::from("")],
        Some(i) => {
            let row = &app.rows[i];
            let mut first = vec![row.name.clone().bold()];
            if !row.detail.is_empty() {
                first.push(Span::raw("  \u{2014}  "));
                first.push(Span::styled(
                    row.detail.clone(),
                    Style::new().fg(Color::Gray),
                ));
            }

            // The alternatives live here rather than on the row, where they
            // would push the action label off the right edge.
            let mut second = Vec::new();
            if row.actions.len() > 1 {
                second.push(Span::styled("e", Style::new().fg(Color::Cyan)));
                second.push(" cycles to: ".dim());
                let others: Vec<String> = row
                    .actions
                    .iter()
                    .enumerate()
                    .filter(|(n, _)| *n != row.chosen)
                    .map(|(_, a)| a.label.clone())
                    .collect();
                second.push(Span::styled(
                    others.join("  \u{b7}  "),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            vec![Line::from(first), Line::from(second)]
        }
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: true }),
        area,
    );
}

fn draw_footer(app: &App, frame: &mut Frame, area: Rect) {
    if let Some(flash) = &app.flash {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(flash.clone(), Style::new().fg(Color::Yellow)),
            ])),
            area,
        );
        return;
    }

    let keys: [(&str, &str); 8] = [
        ("space", "accept"),
        ("e", "change"),
        ("A", "accept section"),
        ("d", "remove"),
        ("v", "show synced"),
        ("p", "project"),
        ("r", "rescan"),
        ("\u{23ce}", "run"),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(key, Style::new().fg(Color::Cyan)));
        spans.push(Span::raw(format!(" {what}   ")));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Pad or ellipsize to an exact display width, counting characters rather than
/// bytes so names with non-ASCII do not break the columns.
pub fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len == width {
        return text.to_string();
    }
    if len < width {
        return format!("{text}{}", " ".repeat(width - len));
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_to_exact_width() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad("abcde", 5), "abcde");
    }

    #[test]
    fn ellipsizes_by_characters_not_bytes() {
        let padded = pad("upskillai-knowledge-server", 10);
        assert_eq!(padded.chars().count(), 10);
        assert!(padded.ends_with('\u{2026}'));

        let unicode = pad("caf\u{e9}-server-name", 6);
        assert_eq!(unicode.chars().count(), 6);
    }
}
