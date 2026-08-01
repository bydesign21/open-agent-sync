//! The run screen (plan gate) and the result screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::App;
use crate::core::apply::Outcome;
use crate::core::plan::{FsOp, Plan, Step};
use crate::domains::World;
use crate::hosts::runner;
use crate::paths;

pub fn draw_plan(app: &App, frame: &mut Frame) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                "Run ".bold(),
                Span::styled(
                    format!("{}", app.plan.steps.len()),
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                " changes?".bold(),
            ]),
            Line::from("Nothing has been modified yet.".dim()),
        ]),
        chunks[0],
    );

    let mut lines: Vec<Line> = Vec::new();
    for step in &app.plan.steps {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(step.label.clone()),
        ]));
        if let Some(effect) = effect_of(&app.world, &step.step) {
            lines.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(effect, Style::new().fg(Color::DarkGray)),
            ]));
        }
    }
    if !app.plan.notes.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  NOTES",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]));
        for note in &app.plan.notes {
            lines.push(Line::from(vec![
                Span::raw("  \u{2022} "),
                Span::styled(note.clone(), Style::new().fg(Color::Yellow)),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    draw_keys(
        app,
        frame,
        chunks[2],
        &[("y", "run"), ("n", "back"), ("c", "copy as shell script")],
    );
}

/// Live state while the plan is executing.
///
/// Held locally by the runner rather than on `App`, so the worker thread can
/// borrow the host list while this is being mutated for drawing.
pub struct RunState {
    pub total: usize,
    pub done: Vec<crate::core::apply::StepResult>,
    /// The step currently in flight, if any.
    pub current: Option<(usize, String)>,
    /// Advances once per repaint to animate the spinner.
    pub frame: usize,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl RunState {
    pub fn new(total: usize) -> Self {
        RunState {
            total,
            done: Vec::new(),
            current: None,
            frame: 0,
        }
    }

    pub fn spinner(&self) -> &'static str {
        SPINNER[self.frame % SPINNER.len()]
    }
}

/// The in-progress screen: completed steps with their marks, the step currently
/// running, and a count. Without this the UI stops repainting until the
/// whole plan finishes, which looks the same as a freeze.
pub fn draw_running(state: &RunState, frame: &mut Frame) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let finished = state.done.len();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(state.spinner(), Style::new().fg(Color::Cyan)),
                Span::raw(" "),
                "Running".bold(),
                Span::raw(format!("   {finished} of {} steps", state.total)),
            ]),
            Line::from(match &state.current {
                Some((_, label)) => Span::styled(
                    format!("  {label}"),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                None => Span::raw(""),
            }),
        ]),
        chunks[0],
    );

    // Show the tail, so the newest lines stay visible on a long plan.
    let height = chunks[1].height.saturating_sub(1) as usize;
    let skip = state.done.len().saturating_sub(height);
    let mut lines: Vec<Line> = Vec::new();
    for result in state.done.iter().skip(skip) {
        let (mark, style) = mark_for(result.outcome);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(mark, style),
            Span::raw(" "),
            Span::raw(result.label.clone()),
        ]));
    }
    if let Some((_, label)) = &state.current {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(state.spinner(), Style::new().fg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(label.clone(), Style::new().fg(Color::Cyan)),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(" keys are ignored while running".dim())),
        chunks[2],
    );
}

fn mark_for(outcome: Outcome) -> (&'static str, Style) {
    match outcome {
        Outcome::Done => ("\u{2713}", Style::new().fg(Color::Green)),
        Outcome::Failed => ("\u{2717}", Style::new().fg(Color::Red)),
        Outcome::Skipped => ("\u{2013}", Style::new().fg(Color::DarkGray)),
    }
}

pub fn draw_result(app: &App, frame: &mut Frame) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let Some(report) = &app.report else {
        frame.render_widget(Paragraph::new("no report"), chunks[0]);
        return;
    };

    let summary_style = if report.any_failed() {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
    };
    let mut header = vec![Line::from(vec![
        "Done: ".bold(),
        Span::styled(report.summary(), summary_style),
    ])];
    if report.manifest_written {
        header.push(Line::from(
            format!(
                "manifest written to {}",
                paths::contract(&app.manifest_path)
            )
            .dim(),
        ));
    } else if let Some(e) = &report.manifest_error {
        header.push(Line::from(vec![Span::styled(
            format!("manifest NOT written: {e}"),
            Style::new().fg(Color::Red),
        )]));
    }
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let mut lines: Vec<Line> = Vec::new();
    for result in &report.results {
        let (mark, style) = mark_for(result.outcome);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(mark, style),
            Span::raw(" "),
            Span::raw(result.label.clone()),
        ]));
        // Only show detail where it changes what you would do next.
        if result.outcome != Outcome::Done && !result.message.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(result.message.clone(), style),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        chunks[1],
    );

    draw_keys(
        app,
        frame,
        chunks[2],
        &[("\u{23ce}", "rescan"), ("q", "quit")],
    );
}

fn draw_keys(app: &App, frame: &mut Frame, area: Rect, keys: &[(&str, &str)]) {
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
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(*key, Style::new().fg(Color::Cyan)));
        spans.push(Span::raw(format!(" {what}   ")));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One line describing exactly what a step will do.
pub fn effect_of(world: &World, step: &Step) -> Option<String> {
    match step {
        Step::Host { host, argv, cwd } => {
            let bin = world
                .host(host)
                .map(|h| h.descriptor.detect.bin.clone())
                .unwrap_or_else(|| host.clone());
            let line = runner::shell_line(&bin, argv);
            Some(match cwd {
                Some(dir) => format!("{line}    (in {})", paths::contract(dir)),
                None => line,
            })
        }
        Step::Fs(FsOp::Link { target, link }) => Some(format!(
            "ln -sfn {} {}",
            paths::contract(target),
            paths::contract(link)
        )),
        Step::Fs(FsOp::Unlink(path)) => Some(format!("rm {}", paths::contract(path))),
        Step::Fs(FsOp::MoveIntoCanonical { from, to }) => Some(format!(
            "mv {} {}    (backed up first)",
            paths::contract(from),
            paths::contract(to)
        )),
        Step::Fs(FsOp::RemoveTree(path)) => Some(format!(
            "rm -r {}    (backed up first)",
            paths::contract(path)
        )),
        Step::Manual(text) => Some(format!("you must: {text}")),
        Step::Manifest(op) => Some(format!("manifest: {}", op.describe())),
    }
}

/// Render the plan as a runnable shell script.
///
/// Only the host commands and filesystem operations can be scripted. Manifest
/// edits and manual steps are emitted as comments, so the script never pretends
/// to be a complete substitute for running the tool.
pub fn as_shell_script(world: &World, plan: &Plan) -> String {
    let mut out = String::from(
        "#!/usr/bin/env bash\n\
         # Generated by agentsync. Review before running.\n\
         #\n\
         # Manifest edits are shown as comments only: agentsync writes the manifest\n\
         # itself, in one atomic step, after validating it for literal credentials.\n\
         set -euo pipefail\n\n",
    );

    for step in &plan.steps {
        out.push_str(&format!("# {}\n", step.label));
        match &step.step {
            Step::Manifest(_) => out.push_str("#   (manifest edit \u{2014} run agentsync)\n"),
            Step::Manual(text) => out.push_str(&format!("#   TODO by hand: {text}\n")),
            other => match effect_of(world, other) {
                Some(line) => out.push_str(&format!("{line}\n")),
                None => out.push_str("#   (nothing to run)\n"),
            },
        }
        out.push('\n');
    }

    for note in &plan.notes {
        out.push_str(&format!("# note: {note}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use std::path::PathBuf;

    fn world() -> World {
        World {
            manifest: Manifest::default(),
            manifest_path: PathBuf::from("/tmp/m.toml"),
            hosts: Vec::new(),
            snapshots: Vec::new(),
            repos: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn script_quotes_host_commands_and_comments_manifest_edits() {
        let mut plan = Plan::default();
        plan.push(
            "add kicad to codex",
            Step::Host {
                host: "codex".into(),
                argv: vec!["mcp".into(), "add".into(), "kicad".into()],
                cwd: None,
            },
        );
        plan.push(
            "adopt kicad",
            Step::Manifest(crate::core::plan::ManifestOp::RemoveMcp("kicad".into())),
        );
        let script = as_shell_script(&world(), &plan);

        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains("codex mcp add kicad"));
        assert!(script.contains("#   (manifest edit"));
        // The manifest removal must not appear as an executable line.
        assert!(!script.contains("\nremove mcp"));
    }

    #[test]
    fn script_marks_manual_steps_as_todo() {
        let mut plan = Plan::default();
        plan.push("set the token", Step::Manual("export TOK=...".into()));
        let script = as_shell_script(&world(), &plan);
        assert!(script.contains("TODO by hand: export TOK=..."));
    }

    #[test]
    fn notes_survive_into_the_script() {
        let mut plan = Plan::default();
        plan.note("skipped codex \u{2014} unsupported: headers");
        let script = as_shell_script(&world(), &plan);
        assert!(script.contains("# note: skipped codex"));
    }
}
