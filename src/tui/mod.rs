//! The review TUI.
//!
//! Two screens, deliberately:
//!
//! * **Review** — a to-do list of differences. Rows that are in sync are hidden,
//!   because a screen that is dense when nothing is wrong teaches you to ignore
//!   it. Each row is a sentence plus an already-chosen action.
//! * **Run** — the plan gate. Keys in the review screen only *stage* decisions;
//!   nothing mutates until you see the exact commands and confirm. `c` writes the
//!   plan out as a shell script, so you can always run it yourself instead.

mod review;
mod run;

use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::core::apply::{self, Report};
use crate::core::diff::{ActionKind, Domain, Row, Severity};
use crate::core::plan::Plan;
use crate::domains::World;
use crate::paths;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Review,
    /// Confirming a plan.
    Run,
    /// Executing. Entered by the key handler, acted on by the event loop, which
    /// is what keeps `handle_key` free of terminal plumbing and testable.
    Running,
    /// Showing what happened.
    Result,
}

/// One line of the review list: either a section heading or a row.
#[derive(Clone, Copy)]
enum Item {
    Header(Domain),
    Row(usize),
}

pub struct App {
    world: World,
    rows: Vec<Row>,
    items: Vec<Item>,
    /// Index into `items`. Always points at an `Item::Row` when one exists.
    cursor: usize,
    /// Scroll offset into `items`.
    offset: usize,
    show_synced: bool,
    screen: Screen,
    plan: Plan,
    report: Option<Report>,
    /// Transient message shown in the footer.
    flash: Option<String>,
    manifest_path: PathBuf,
    repos: Vec<String>,
}

/// Execute a plan on a worker thread while repainting a live progress screen.
///
/// A synchronous run inside the event loop cannot repaint, so a plan containing
/// anything slow — a plugin install clones a repository — looks like a freeze.
/// The work runs in a scoped thread and reports through a channel; this loop
/// drains it, redraws, and discards keystrokes so nothing typed during the run is
/// replayed into the review screen afterwards.
fn run_with_progress(
    plan: &Plan,
    mut manifest: crate::manifest::Manifest,
    manifest_path: &std::path::Path,
    hosts: &[crate::hosts::Host],
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<Report> {
    use std::sync::mpsc;
    use std::time::Duration;

    enum Message {
        Started(usize, String),
        Finished(apply::StepResult),
    }

    let (tx, rx) = mpsc::channel::<Message>();
    let mut state = run::RunState::new(plan.steps.len());

    std::thread::scope(|scope| -> Result<Report> {
        let worker = scope.spawn(move || {
            apply::run(plan, &mut manifest, manifest_path, hosts, |progress| {
                let message = match progress {
                    apply::Progress::Started { index, label } => {
                        Message::Started(index, label.to_string())
                    }
                    apply::Progress::Finished(result) => Message::Finished(result.clone()),
                };
                // A send failure means the UI went away; the work still finishes.
                let _ = tx.send(message);
            })
        });

        loop {
            let mut drained_any = false;
            while let Ok(message) = rx.try_recv() {
                drained_any = true;
                match message {
                    Message::Started(index, label) => state.current = Some((index, label)),
                    Message::Finished(result) => {
                        state.done.push(result);
                        state.current = None;
                    }
                }
            }

            state.frame += 1;
            terminal.draw(|frame| run::draw_running(&state, frame))?;

            // Swallow input so a keypress during the run does not act on the
            // review screen the moment it reappears.
            while event::poll(Duration::ZERO)? {
                let _ = event::read()?;
            }

            if worker.is_finished() && !drained_any && rx.try_recv().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }

        worker
            .join()
            .map_err(|_| anyhow::anyhow!("the apply worker panicked"))
    })
}

pub fn run(world: World, rows: Vec<Row>) -> Result<()> {
    let mut app = App::new(world, rows);
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    fn new(world: World, rows: Vec<Row>) -> Self {
        let manifest_path = world.manifest_path.clone();
        let repos = world.repos.clone();
        let mut app = App {
            world,
            rows,
            items: Vec::new(),
            cursor: 0,
            offset: 0,
            show_synced: false,
            screen: Screen::Review,
            plan: Plan::default(),
            report: None,
            flash: None,
            manifest_path,
            repos,
        };
        app.rebuild_items();
        app
    }

    /// Recompute the visible line list, keeping the cursor on a row.
    fn rebuild_items(&mut self) {
        self.items.clear();
        for domain in Domain::ALL {
            let visible: Vec<usize> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    r.domain == domain && (self.show_synced || r.severity != Severity::Synced)
                })
                .map(|(i, _)| i)
                .collect();
            if visible.is_empty() {
                continue;
            }
            self.items.push(Item::Header(domain));
            self.items.extend(visible.into_iter().map(Item::Row));
        }
        if !matches!(self.items.get(self.cursor), Some(Item::Row(_))) {
            self.cursor = self
                .items
                .iter()
                .position(|i| matches!(i, Item::Row(_)))
                .unwrap_or(0);
        }
    }

    fn selected(&self) -> Option<usize> {
        match self.items.get(self.cursor) {
            Some(Item::Row(i)) => Some(*i),
            _ => None,
        }
    }

    fn accepted_count(&self) -> usize {
        self.rows.iter().filter(|r| r.accepted).count()
    }

    fn todo_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.severity != Severity::Synced)
            .count()
    }

    // -----------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------

    fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let mut idx = self.cursor as isize;
        loop {
            idx += delta;
            if idx < 0 || idx >= self.items.len() as isize {
                return;
            }
            if matches!(self.items[idx as usize], Item::Row(_)) {
                self.cursor = idx as usize;
                return;
            }
        }
    }

    /// The domain whose section the cursor is in.
    fn current_domain(&self) -> Option<Domain> {
        self.selected().map(|i| self.rows[i].domain)
    }

    // -----------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------

    fn toggle_selected(&mut self) {
        if let Some(i) = self.selected() {
            self.rows[i].toggle();
        }
    }

    fn cycle_selected(&mut self) {
        if let Some(i) = self.selected() {
            self.rows[i].cycle();
        }
    }

    fn accept_section(&mut self) {
        let Some(domain) = self.current_domain() else {
            return;
        };
        let mut n = 0;
        for row in self.rows.iter_mut() {
            if row.domain == domain && row.severity != Severity::Synced && row.actionable() {
                row.accepted = true;
                n += 1;
            }
        }
        self.flash = Some(format!("accepted {n} in {}", domain.title().to_lowercase()));
    }

    /// Jump the selected row to its delete action, if it has one.
    fn choose_delete(&mut self) {
        let Some(i) = self.selected() else { return };
        let row = &mut self.rows[i];
        match row
            .actions
            .iter()
            .position(|a| matches!(a.kind, ActionKind::Delete { .. }))
        {
            Some(pos) => {
                row.chosen = pos;
                row.accepted = true;
                self.flash = Some(format!("{}: {}", row.name, row.action().label));
            }
            None => self.flash = Some(format!("{} has no delete action", row.name)),
        }
    }

    fn toggle_synced(&mut self) {
        self.show_synced = !self.show_synced;
        self.rebuild_items();
        self.flash = Some(if self.show_synced {
            "showing rows that are in sync".into()
        } else {
            "hiding rows that are in sync".into()
        });
    }

    /// Re-read everything from disk, preserving nothing: after a run, stale rows
    /// would describe a world that no longer exists.
    fn rescan(&mut self) -> Result<()> {
        self.world = World::load(&self.manifest_path, &self.repos)?;
        self.rows = self.world.rows();
        self.report = None;
        self.rebuild_items();
        self.flash = Some(format!(
            "rescanned \u{2014} {} to review",
            self.todo_count()
        ));
        Ok(())
    }

    fn build_plan(&mut self) {
        self.plan = self.world.plan(&self.rows);
    }

    fn write_script(&mut self) {
        match self.script_path() {
            Ok(path) => self.flash = Some(format!("wrote {}", paths::contract(&path))),
            Err(e) => self.flash = Some(format!("could not write script: {e:#}")),
        }
    }

    fn script_path(&self) -> Result<PathBuf> {
        let path = paths::config_dir().join("plan.sh");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, run::as_shell_script(&self.world, &self.plan))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    fn execute(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let manifest = self.world.manifest.clone();
        let path = self.world.manifest_path.clone();
        // Immutable reborrows; the runner never touches `self`, which is what
        // lets the worker thread hold the host list while we redraw.
        let report = run_with_progress(&self.plan, manifest, &path, &self.world.hosts, terminal)?;
        self.report = Some(report);
        self.screen = Screen::Result;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Event loop
    // -----------------------------------------------------------------

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            if self.screen == Screen::Running {
                self.execute(terminal)?;
                continue;
            }

            terminal.draw(|frame| match self.screen {
                Screen::Review => review::draw(self, frame),
                Screen::Run => run::draw_plan(self, frame),
                Screen::Result | Screen::Running => run::draw_result(self, frame),
            })?;

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if self.handle_key(key)? {
                return Ok(());
            }
        }
    }

    /// Returns true when the app should exit.
    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        // Ctrl-C always quits, on every screen.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        self.flash = None;

        match self.screen {
            Screen::Review => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
                KeyCode::Char(' ') => self.toggle_selected(),
                KeyCode::Char('e') => self.cycle_selected(),
                KeyCode::Char('A') => self.accept_section(),
                KeyCode::Char('d') => self.choose_delete(),
                KeyCode::Char('v') => self.toggle_synced(),
                KeyCode::Char('r') => self.rescan()?,
                KeyCode::Enter => {
                    if self.accepted_count() == 0 {
                        self.flash =
                            Some("nothing accepted yet \u{2014} press space on a row".into());
                    } else {
                        self.build_plan();
                        self.screen = Screen::Run;
                    }
                }
                _ => {}
            },

            Screen::Run => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('n') => {
                    self.screen = Screen::Review
                }
                KeyCode::Char('c') => self.write_script(),
                KeyCode::Char('y') => self.screen = Screen::Running,
                _ => {}
            },

            // The event loop drives this screen; no keys are read while it is up.
            Screen::Running => {}

            Screen::Result => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Enter | KeyCode::Char('r') => {
                    self.rescan()?;
                    self.screen = Screen::Review;
                }
                _ => {}
            },
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::diff::{Action, RowKey};

    fn row(domain: Domain, name: &str, severity: Severity) -> Row {
        Row {
            domain,
            name: name.into(),
            headline: "only in codex".into(),
            detail: String::new(),
            severity,
            actions: vec![
                Action::new(
                    "adopt",
                    ActionKind::Adopt {
                        push: true,
                        promote: false,
                    },
                ),
                Action::new(
                    "delete everywhere",
                    ActionKind::Delete {
                        hosts: vec![],
                        from_manifest: false,
                        purge: false,
                    },
                ),
            ],
            chosen: 0,
            accepted: false,
            key: RowKey::default(),
        }
    }

    fn app_with(rows: Vec<Row>) -> App {
        // A World with no hosts is enough to exercise navigation and staging.
        let world = World {
            manifest: Default::default(),
            manifest_path: PathBuf::from("/tmp/agentsync-test-manifest.toml"),
            hosts: Vec::new(),
            snapshots: Vec::new(),
            repos: Vec::new(),
            warnings: Vec::new(),
        };
        App::new(world, rows)
    }

    #[test]
    fn synced_rows_are_hidden_until_asked_for() {
        let mut app = app_with(vec![
            row(Domain::Mcp, "visible", Severity::Normal),
            row(Domain::Mcp, "quiet", Severity::Synced),
        ]);
        let row_count = |a: &App| a.items.iter().filter(|i| matches!(i, Item::Row(_))).count();
        assert_eq!(row_count(&app), 1);
        app.toggle_synced();
        assert_eq!(row_count(&app), 2);
    }

    #[test]
    fn the_cursor_skips_section_headers() {
        let mut app = app_with(vec![
            row(Domain::Mcp, "a", Severity::Normal),
            row(Domain::Skills, "b", Severity::Normal),
        ]);
        // Headers exist for both sections, so a naive +1 would land on one.
        assert!(matches!(app.items[app.cursor], Item::Row(_)));
        app.move_cursor(1);
        assert!(matches!(app.items[app.cursor], Item::Row(_)));
        assert_eq!(
            app.selected().map(|i| app.rows[i].name.clone()),
            Some("b".into())
        );
    }

    #[test]
    fn accept_section_only_touches_its_own_domain() {
        let mut app = app_with(vec![
            row(Domain::Mcp, "a", Severity::Normal),
            row(Domain::Mcp, "b", Severity::Normal),
            row(Domain::Skills, "c", Severity::Normal),
        ]);
        app.accept_section();
        assert_eq!(app.accepted_count(), 2);
        assert!(!app.rows[2].accepted);
    }

    #[test]
    fn d_selects_the_delete_action_and_accepts_it() {
        let mut app = app_with(vec![row(Domain::Mcp, "a", Severity::Normal)]);
        app.choose_delete();
        assert!(app.rows[0].accepted);
        assert!(matches!(
            app.rows[0].action().kind,
            ActionKind::Delete { .. }
        ));
    }

    #[test]
    fn enter_refuses_to_open_the_run_screen_with_nothing_accepted() {
        let mut app = app_with(vec![row(Domain::Mcp, "a", Severity::Normal)]);
        app.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(matches!(app.screen, Screen::Review));
        assert!(app.flash.as_deref().unwrap().contains("nothing accepted"));

        app.toggle_selected();
        app.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(matches!(app.screen, Screen::Run));
    }

    #[test]
    fn y_hands_execution_to_the_event_loop_rather_than_running_inline() {
        // The key handler must not execute the plan itself: it has no terminal to
        // repaint with, which is exactly what made the UI look frozen.
        let mut app = app_with(vec![row(Domain::Mcp, "a", Severity::Normal)]);
        app.screen = Screen::Run;
        let quit = app.handle_key(KeyEvent::from(KeyCode::Char('y'))).unwrap();
        assert!(!quit);
        assert!(matches!(app.screen, Screen::Running));
    }

    #[test]
    fn keys_are_ignored_while_running() {
        let mut app = app_with(vec![row(Domain::Mcp, "a", Severity::Normal)]);
        app.screen = Screen::Running;
        app.handle_key(KeyEvent::from(KeyCode::Char('q'))).unwrap();
        assert!(
            matches!(app.screen, Screen::Running),
            "a stray keypress must not leave the running screen"
        );
    }

    #[test]
    fn ctrl_c_quits_from_the_run_screen() {
        let mut app = app_with(vec![row(Domain::Mcp, "a", Severity::Normal)]);
        app.screen = Screen::Run;
        let quit = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(quit);
    }
}
