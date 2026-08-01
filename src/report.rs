//! Structured reports, rendered by both the CLI and the TUI.
//!
//! These reports used to be `println!` calls inside `main.rs`. The TUI could
//! not show them at all. A structure instead lets one implementation serve
//! both surfaces. This also stops the two from drifting apart, the usual fate
//! of "the same" output written twice.

use crate::core::diff::{Domain, Row, Severity};
use crate::core::model::LinkState;
use crate::core::plan::{FsOp, ManifestOp, Step};
use crate::domains::World;
use crate::hosts::{Host, runner};
use crate::paths;
use crate::update;

/// How a line reads at a glance. The renderer maps these to glyphs and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Fine.
    Ok,
    /// Needs action. Counted in `problems`.
    Problem,
    /// Worth knowing, not blocking.
    Warn,
    /// Neutral information.
    Info,
    /// No glyph at all.
    Plain,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub mark: Mark,
    pub text: String,
    /// Extra indent levels, for continuation detail under a line.
    pub indent: u8,
}

impl Line {
    pub fn new(mark: Mark, text: impl Into<String>) -> Self {
        Line {
            mark,
            text: text.into(),
            indent: 0,
        }
    }
    pub fn detail(text: impl Into<String>) -> Self {
        Line {
            mark: Mark::Plain,
            text: text.into(),
            indent: 1,
        }
    }
    pub fn plain(text: impl Into<String>) -> Self {
        Line::new(Mark::Plain, text)
    }
}

#[derive(Clone, Debug)]
pub struct Section {
    pub title: String,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub sections: Vec<Section>,
    /// Lines marked [`Mark::Problem`]. The count the CLI exits on.
    pub problems: usize,
}

impl Report {
    fn push(&mut self, title: impl Into<String>, lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        self.problems += lines.iter().filter(|l| l.mark == Mark::Problem).count();
        self.sections.push(Section {
            title: title.into(),
            lines,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Problems that are not differences.
///
/// `probe_network` gates the two things that cost a subprocess: each host's auth
/// status and the update check. The caller decides, because the TUI has to run
/// this off the main thread and wants to say so while it happens.
pub fn doctor(world: &World, probe_network: bool) -> Report {
    let mut report = Report::default();

    let mut hosts = Vec::new();
    for (host, snap) in world.detected() {
        hosts.push(Line::new(
            Mark::Ok,
            format!(
                "{:<10} {:<16} mcp:{} skills:{} plugins:{}",
                host.name(),
                host.descriptor.display,
                snap.mcp.len(),
                snap.skills.len(),
                snap.plugins.len()
            ),
        ));
    }
    for host in world.missing_hosts() {
        hosts.push(Line::new(
            Mark::Info,
            format!(
                "{:<10} not installed ({} not on PATH)",
                host.name(),
                host.descriptor.detect.bin
            ),
        ));
    }
    report.push("HOSTS", hosts);

    report.push(
        "LITERAL CREDENTIALS IN THE MANIFEST",
        world
            .manifest
            .audit_secrets()
            .iter()
            .map(|f| Line::new(Mark::Problem, format!("{} — {}", f.location, f.reason)))
            .collect(),
    );

    report.push(
        "NON-PORTABLE COMMANDS (these will not resolve on another machine)",
        world
            .manifest
            .non_portable()
            .iter()
            .map(|(name, cmd)| Line::new(Mark::Warn, format!("mcp.{name}: {cmd}")))
            .collect(),
    );

    report.push(
        "UNSET ENVIRONMENT VARIABLES",
        world
            .manifest
            .missing_env()
            .iter()
            .map(|(name, var)| Line::new(Mark::Problem, format!("mcp.{name} needs ${var}")))
            .collect(),
    );

    if probe_network {
        let mut auth = Vec::new();
        let mut unknown_hosts = Vec::new();
        for (host, _) in world.detected() {
            match host.probe_auth() {
                Ok(None) => unknown_hosts.push(host.name().to_string()),
                Ok(Some(statuses)) => {
                    for (name, status) in statuses {
                        if status.needs_login() {
                            let fix = host
                                .mcp_login_command(&name)
                                .unwrap_or_else(|| format!("log in to {name} on {}", host.name()));
                            auth.push(Line::new(
                                Mark::Problem,
                                format!("{}: {name} — run `{fix}`", host.name()),
                            ));
                        }
                    }
                }
                // A failed probe is itself worth reporting. Silence would
                // read as "fine".
                Err(e) => auth.push(Line::new(
                    Mark::Warn,
                    format!("{}: could not read auth status — {e:#}", host.name()),
                )),
            }
        }
        report.push(
            "MCP SERVERS THAT ARE CONFIGURED BUT NOT AUTHENTICATED",
            auth,
        );

        if !unknown_hosts.is_empty() {
            report.push(
                "AUTH STATUS NOT READABLE",
                vec![Line::new(
                    Mark::Info,
                    format!(
                        "{} has no machine-readable auth status. A logged-out server \
                         there shows up only in its own startup warnings",
                        unknown_hosts.join(", ")
                    ),
                )],
            );
        }
    }

    report.push(
        "READ WARNINGS",
        world
            .warnings
            .iter()
            .map(|w| Line::new(Mark::Warn, w.clone()))
            .collect(),
    );

    let shim_dirs: Vec<std::path::PathBuf> = world
        .detected()
        .filter_map(|(h, _)| h.descriptor.hooks.as_ref())
        .filter_map(|h| h.shim.as_ref())
        .map(|s| paths::expand(&s.marketplace))
        .filter(|d| d.is_dir())
        .collect();
    if let Ok(agentsync_bin) = std::env::current_exe() {
        report.push(
            "SHIM HEALTH",
            shim_health(&agentsync_bin, &shim_dirs)
                .into_iter()
                .map(|text| Line::new(Mark::Problem, text))
                .collect(),
        );
    }

    let foreign: Vec<Line> = world
        .detected()
        .flat_map(|(h, s)| {
            s.skills
                .iter()
                .filter_map(move |(name, state)| match state {
                    LinkState::Foreign(target) => Some(Line::new(
                        Mark::Info,
                        format!("{}: {name} -> {}", h.name(), paths::contract(target)),
                    )),
                    _ => None,
                })
        })
        .collect();
    report.push("SKILLS LINKED OUTSIDE agentsync (left alone)", foreign);

    report.push("MEMORIES (reported, never synced)", memories());

    if probe_network {
        let line = match update::check() {
            update::Status::Current => Line::new(
                Mark::Ok,
                format!(
                    "agentsync {} is the newest release",
                    update::current_version()
                ),
            ),
            update::Status::Newer { latest } => Line::new(
                Mark::Warn,
                format!(
                    "{} → {latest}   {}",
                    update::current_version(),
                    update::upgrade_hint(&latest)
                ),
            ),
            update::Status::Ahead { latest } => Line::new(
                Mark::Info,
                format!(
                    "agentsync {} is newer than the latest release ({latest}) — a local build",
                    update::current_version()
                ),
            ),
            // This is not a problem to fix, but not silence either. A failed
            // check must not read as "you are up to date".
            update::Status::Unknown { reason } => {
                Line::new(Mark::Warn, format!("could not check for updates: {reason}"))
            }
        };
        report.push("VERSION", vec![line]);
    }

    report
}

/// What each host stores as "memory", and why none of it is synced.
///
/// Claude Code keeps per-project markdown under a directory keyed by an
/// encoded project path. Codex keeps its own memory in SQLite. There is no
/// file-level match between them, so agentsync reports what exists and stops
/// there. Claiming to sync these would invent a mapping that does not exist.
fn memories() -> Vec<Line> {
    let mut out = Vec::new();

    let claude_root = paths::expand("~/.claude/projects");
    if claude_root.is_dir() {
        let mut projects = 0usize;
        let mut files = 0usize;
        if let Ok(entries) = std::fs::read_dir(&claude_root) {
            for entry in entries.filter_map(Result::ok) {
                let dir = entry.path().join("memory");
                if !dir.is_dir() {
                    continue;
                }
                projects += 1;
                files += std::fs::read_dir(&dir)
                    .map(|d| {
                        d.filter_map(Result::ok)
                            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                            .count()
                    })
                    .unwrap_or(0);
            }
        }
        if projects > 0 {
            out.push(Line::new(
                Mark::Info,
                format!(
                    "claude: {files} note(s) across {projects} project(s) under {} \u{2014} \
                     keyed by project path, so they do not transfer",
                    paths::contract(&claude_root)
                ),
            ));
        }
    }

    let codex_db = paths::expand("~/.codex/memories_1.sqlite");
    if codex_db.is_file() {
        let size = std::fs::metadata(&codex_db).map(|m| m.len()).unwrap_or(0);
        out.push(Line::new(
            Mark::Info,
            format!(
                "codex: {} ({} KB) \u{2014} SQLite, with no file-level counterpart on the \
                 other side",
                paths::contract(&codex_db),
                size / 1024
            ),
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// hosts
// ---------------------------------------------------------------------------

pub fn hosts_report(hosts: &[Host]) -> Report {
    use crate::core::model::ScopeKind;
    let mut report = Report::default();

    report.push(
        "WHERE DESCRIPTORS COME FROM",
        vec![Line::new(
            Mark::Info,
            format!(
                "built in, overridden by files in {}",
                paths::contract(&paths::hosts_dir())
            ),
        )],
    );

    for host in hosts {
        let mut lines = vec![Line::new(
            if host.detected() {
                Mark::Ok
            } else {
                Mark::Info
            },
            format!(
                "{} ({}) — {}",
                host.name(),
                host.descriptor.display,
                if host.detected() {
                    "installed"
                } else {
                    "not installed"
                }
            ),
        )];

        if let Some(mcp) = &host.descriptor.mcp {
            let scopes: Vec<&str> = mcp
                .scopes
                .iter()
                .map(|s| match s {
                    ScopeKind::User => "user",
                    ScopeKind::Local => "local",
                    ScopeKind::Project => "project",
                })
                .collect();
            let caps: Vec<&str> = mcp.caps.iter().map(|c| c.as_str()).collect();
            lines.push(Line::detail(format!("mcp scopes: {}", scopes.join(", "))));
            lines.push(Line::detail(format!("mcp caps:   {}", caps.join(", "))));
            lines.push(Line::detail(format!(
                "auth status readable: {}",
                if mcp.auth_status.is_some() {
                    "yes"
                } else {
                    "no"
                }
            )));
        }
        if let Some(skills) = &host.descriptor.skills {
            lines.push(Line::detail(format!(
                "skills:     {} (first is the link target)",
                skills.dirs.join(", ")
            )));
        }
        if host.descriptor.plugins.is_some() {
            lines.push(Line::detail("plugins:    yes"));
        }
        report.push(format!("HOST: {}", host.name()), lines);
    }

    report.push(
        "COMPILED PARSERS (reference these as `parser = \"...\"` in a descriptor)",
        crate::hosts::parsers::registry()
            .iter()
            .map(|(name, what)| Line::new(Mark::Plain, format!("{name:<24} {what}")))
            .collect(),
    );

    report
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// The rows, and the plan the given selection produces.
pub fn plan_report(world: &World, rows: &[Row]) -> Report {
    let mut report = Report::default();

    for domain in Domain::ALL {
        let group: Vec<&Row> = rows
            .iter()
            .filter(|r| r.domain == domain && r.severity != Severity::Synced)
            .collect();
        if group.is_empty() {
            continue;
        }
        let width = group
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(30);
        let mut lines = Vec::new();
        for row in group {
            lines.push(Line::new(
                match row.severity {
                    Severity::Warn => Mark::Warn,
                    Severity::Blocked => Mark::Info,
                    _ => Mark::Plain,
                },
                format!(
                    "{:<width$}  {}  →  {}",
                    row.name,
                    row.headline,
                    row.action().label
                ),
            ));
            if !row.detail.is_empty() {
                lines.push(Line::detail(row.detail.clone()));
            }
        }
        report.push(domain.title(), lines);
    }

    let plan = world.plan(rows);
    let mut steps = Vec::new();
    for (i, step) in plan.steps.iter().enumerate() {
        steps.push(Line::new(
            Mark::Plain,
            format!("{:>2}  {}", i + 1, step.label),
        ));
        if let Some(effect) = describe_step(world, &step.step) {
            steps.push(Line::detail(effect));
        }
    }
    if steps.is_empty() {
        steps.push(Line::new(Mark::Ok, "nothing to do"));
    }
    report.push(format!("PLAN ({} steps)", plan.steps.len()), steps);

    report.push(
        "NOTES",
        plan.notes
            .iter()
            .map(|n| Line::new(Mark::Warn, n.clone()))
            .collect(),
    );

    report
}

/// The concrete effect of a step. This makes a plan auditable, not just a
/// description of intent.
pub fn describe_step(world: &World, step: &Step) -> Option<String> {
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
        Step::Fs(FsOp::WriteFile { path, .. }) => Some(format!("write {}", paths::contract(path))),
        Step::Manual(text) => Some(format!("you must: {text}")),
        Step::Manifest(op) => Some(format!("manifest: {}", ManifestOp::describe(op))),
    }
}

/// Problems with generated shims, for `doctor`.
///
/// A shim whose binary has moved cannot run. It must be reported, because the
/// host will keep invoking it and every invocation will fail.
pub fn shim_health(bin: &std::path::Path, shim_dirs: &[std::path::PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    if shim_dirs.is_empty() {
        return out;
    }
    if !bin.exists() {
        out.push(format!(
            "generated shims invoke {}, which no longer exists. \
             Re-run agentsync to regenerate them.",
            bin.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_a_shim_whose_binary_no_longer_exists() {
        let lines = shim_health(
            std::path::Path::new("/nonexistent/agentsync"),
            &[std::path::PathBuf::from("/nonexistent/shims/demo")],
        );
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("/nonexistent/agentsync"),
            "must name the missing binary: {lines:?}"
        );
    }

    #[test]
    fn doctor_is_quiet_when_there_are_no_shims() {
        let lines = shim_health(std::path::Path::new("/bin/sh"), &[]);
        assert!(lines.is_empty(), "no shims means nothing to say: {lines:?}");
    }

    #[test]
    fn empty_sections_are_dropped_rather_than_shown_as_headings() {
        let mut report = Report::default();
        report.push("NOTHING", vec![]);
        assert!(report.is_empty());
        assert_eq!(report.problems, 0);
    }

    #[test]
    fn only_problem_lines_are_counted() {
        let mut report = Report::default();
        report.push(
            "MIXED",
            vec![
                Line::new(Mark::Problem, "a"),
                Line::new(Mark::Warn, "b"),
                Line::new(Mark::Ok, "c"),
                Line::new(Mark::Info, "d"),
            ],
        );
        assert_eq!(report.problems, 1, "warnings and info are not problems");
    }
}
