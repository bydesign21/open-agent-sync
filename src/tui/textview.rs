//! A scrollable rendering of a [`Report`].
//!
//! Scrolling is not a nicety here. The plan gate is the safety mechanism of the
//! whole tool. A plan of ninety steps rendered into a thirty-line terminal
//! silently hid most of what you were approving. Same for the result screen: a
//! failure below the fold reads as no failure.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::report::{Mark, Report};

pub struct TextView {
    pub title: String,
    /// Shown under the title, dimmed.
    pub subtitle: String,
    pub report: Report,
    pub offset: usize,
    /// Set while a background thread is still filling `report`.
    pub loading: Option<String>,
    pub frame: usize,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl TextView {
    pub fn new(title: impl Into<String>, subtitle: impl Into<String>) -> Self {
        TextView {
            title: title.into(),
            subtitle: subtitle.into(),
            report: Report::default(),
            offset: 0,
            loading: None,
            frame: 0,
        }
    }

    pub fn loading(title: impl Into<String>, what: impl Into<String>) -> Self {
        let mut view = TextView::new(title, "");
        view.loading = Some(what.into());
        view
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.frame % SPINNER.len()]
    }

    /// Flatten the report into displayable lines. Recomputed per draw, which is
    /// cheap and avoids a cache that can disagree with the report.
    fn lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        for (index, section) in self.report.sections.iter().enumerate() {
            if index > 0 {
                out.push(Line::from(""));
            }
            out.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    section.title.clone(),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ]));
            for line in &section.lines {
                let (glyph, style) = match line.mark {
                    Mark::Ok => ("\u{2713}", Style::new().fg(Color::Green)),
                    Mark::Problem => ("\u{2717}", Style::new().fg(Color::Red)),
                    Mark::Warn => ("!", Style::new().fg(Color::Yellow)),
                    Mark::Info => ("\u{2013}", Style::new().fg(Color::DarkGray)),
                    Mark::Plain => (" ", Style::new()),
                };
                let indent = "  ".repeat(line.indent as usize + 1);
                out.push(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(glyph, style),
                    Span::raw(" "),
                    if line.indent > 0 {
                        Span::styled(line.text.clone(), Style::new().fg(Color::DarkGray))
                    } else {
                        Span::raw(line.text.clone())
                    },
                ]));
            }
        }
        out
    }

    pub fn scroll(&mut self, delta: isize, viewport: usize) {
        let total = self.lines().len();
        let max = total.saturating_sub(viewport);
        let next = self.offset as isize + delta;
        self.offset = next.clamp(0, max as isize) as usize;
    }

    pub fn scroll_to_end(&mut self, viewport: usize) {
        self.offset = self.lines().len().saturating_sub(viewport);
    }

    pub fn draw(&mut self, frame: &mut Frame, keys: &[(&str, &str)]) {
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

        let viewport = chunks[1].height as usize;
        let all = self.lines();
        let total = all.len();

        // Keep the offset legal if the content shrank since the last draw.
        if self.offset > total.saturating_sub(viewport) {
            self.offset = total.saturating_sub(viewport);
        }

        let mut header = vec![Line::from(vec![self.title.clone().bold()])];
        header.push(match &self.loading {
            Some(what) => Line::from(vec![
                Span::styled(self.spinner(), Style::new().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(what.clone(), Style::new().fg(Color::Cyan)),
            ]),
            None => {
                let mut spans = vec![self.subtitle.clone().dim()];
                // Say when there is more than fits, so a truncated screen never
                // passes for a complete one.
                if total > viewport {
                    spans.push(
                        format!(
                            "   lines {}-{} of {total}",
                            self.offset + 1,
                            (self.offset + viewport).min(total)
                        )
                        .dim(),
                    );
                }
                Line::from(spans)
            }
        });
        frame.render_widget(Paragraph::new(header), chunks[0]);

        let visible: Vec<Line> = all.into_iter().skip(self.offset).take(viewport).collect();
        frame.render_widget(
            Paragraph::new(visible).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::new().fg(Color::DarkGray)),
            ),
            chunks[1],
        );

        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys {
            spans.push(Span::styled(*key, Style::new().fg(Color::Cyan)));
            spans.push(Span::raw(format!(" {what}   ")));
        }
        if total > viewport {
            spans.push("j/k scroll   ".dim());
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Line as RLine;

    fn view_with(n: usize) -> TextView {
        let mut view = TextView::new("t", "s");
        let mut report = Report::default();
        report.sections.push(crate::report::Section {
            title: "S".into(),
            lines: (0..n).map(|i| RLine::plain(format!("line {i}"))).collect(),
        });
        view.report = report;
        view
    }

    #[test]
    fn scrolling_is_clamped_at_both_ends() {
        // 1 title line + 20 content lines = 21 displayable.
        let mut view = view_with(20);
        view.scroll(-5, 10);
        assert_eq!(view.offset, 0, "cannot scroll above the top");
        view.scroll(1000, 10);
        assert_eq!(view.offset, 11, "cannot scroll past the last screenful");
    }

    #[test]
    fn short_content_never_scrolls() {
        let mut view = view_with(3);
        view.scroll(50, 40);
        assert_eq!(view.offset, 0);
    }

    #[test]
    fn scroll_to_end_shows_the_last_screenful() {
        let mut view = view_with(100);
        view.scroll_to_end(20);
        assert_eq!(view.offset, 101 - 20);
    }
}
