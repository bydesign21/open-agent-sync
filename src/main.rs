//! agentsync — keep MCP servers, skills, and plugins in sync across agentic
//! coding CLIs.
//!
//! With no subcommand, this opens the review TUI. The subcommands exist so
//! the same core stays scriptable, and so a plan can be inspected without a
//! terminal.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agentsync::core::apply::{self, Outcome, Progress};
use agentsync::core::diff::{Domain, Row, Severity};
use agentsync::core::model::HookCap;
use agentsync::domains::World;
use agentsync::paths;
use agentsync::report::{self, Mark};

#[derive(Parser)]
#[command(
    name = "agentsync",
    version,
    about = "Reconcile MCP servers, skills, and plugins across agentic coding CLIs"
)]
struct Cli {
    /// Manifest to use. Defaults to ~/.config/agentsync/manifest.toml.
    #[arg(long, global = true)]
    manifest: Option<PathBuf>,

    /// Additional repo to consider for per-repo configuration (repeatable).
    #[arg(long = "repo", global = true)]
    repos: Vec<String>,

    /// Only this domain: mcp, skills, instructions, plugins, or hooks (repeatable).
    #[arg(long = "only", global = true)]
    only: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the differences and the plan the default actions would produce.
    Plan,
    /// Accept the default action on every difference and run it.
    Apply {
        /// Required. Without it nothing runs — this is the confirmation gate the
        /// TUI provides interactively.
        #[arg(long)]
        yes: bool,
    },
    /// Report problems that are not differences: non-portable paths, literal
    /// credentials, foreign links, unset environment variables.
    Doctor,
    /// List known hosts and the compiled parsers descriptors can reference.
    Hosts {
        /// List the parser registry instead of the hosts.
        #[arg(long)]
        parsers: bool,
    },
    /// Run one generated hook shim. Invoked by a host, not by a person.
    #[command(hide = true)]
    HookShim {
        /// Path to the generated sidecar describing the original handler.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Compare each host's declared hook capabilities against its installed
    /// binary, and report where they disagree.
    Hooks {
        /// Probe the installed host binaries.
        #[arg(long)]
        probe: bool,
    },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("agentsync: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let manifest_path = cli.manifest.clone().unwrap_or_else(paths::manifest_path);

    match &cli.command {
        Some(Command::Hosts { parsers: want }) => hosts_command(*want),

        Some(Command::Plan) => {
            let world = World::load(&manifest_path, &cli.repos)?;
            let rows = filtered_rows(&world, &cli.only);
            print_host_line(&world, &rows);
            print_plan(&world, &rows);
            Ok(())
        }

        Some(Command::Apply { yes }) => {
            let world = World::load(&manifest_path, &cli.repos)?;
            let mut rows = filtered_rows(&world, &cli.only);
            print_host_line(&world, &rows);
            print_plan(&world, &rows);
            if !yes {
                println!(
                    "Nothing changed. Re-run with --yes to accept every default action. \
                     Or run `agentsync` with no arguments to choose per row."
                );
                return Ok(());
            }
            accept_defaults(&mut rows);
            run_plan(world, &rows)
        }

        Some(Command::Doctor) => {
            let world = World::load(&manifest_path, &cli.repos)?;
            doctor(&world)
        }

        Some(Command::HookShim { spec }) => {
            let code = agentsync::shim::run::main(spec)?;
            std::process::exit(code);
        }

        Some(Command::Hooks { probe }) => {
            if !probe {
                println!("Nothing to do. Use `agentsync hooks --probe`.");
                return Ok(());
            }
            let world = World::load(&manifest_path, &cli.repos)?;
            hooks_probe(&world)
        }

        None => {
            let world = World::load(&manifest_path, &cli.repos)?;
            let rows = filtered_rows(&world, &cli.only);
            agentsync::tui::run(world, rows)
        }
    }
}

fn accept_defaults(rows: &mut [Row]) {
    for row in rows.iter_mut() {
        if row.actionable() {
            row.accepted = true;
        }
    }
}

fn filtered_rows(world: &World, only: &[String]) -> Vec<Row> {
    let rows = world.rows();
    if only.is_empty() {
        return rows;
    }
    let wanted: Vec<Domain> = only
        .iter()
        .filter_map(|o| match o.to_ascii_lowercase().as_str() {
            "mcp" => Some(Domain::Mcp),
            "skills" | "skill" => Some(Domain::Skills),
            "instructions" | "instruction" | "prompts" | "prompt" => Some(Domain::Instructions),
            "plugins" | "plugin" => Some(Domain::Plugins),
            "hooks" | "hook" => Some(Domain::Hooks),
            other => {
                eprintln!(
                    "agentsync: unknown domain {other:?}. Expected mcp, skills, \
                     instructions, plugins, or hooks"
                );
                None
            }
        })
        .collect();
    rows.into_iter()
        .filter(|r| wanted.contains(&r.domain))
        .collect()
}

/// The one thing the report does not carry: which hosts were found.
fn print_host_line(world: &World, rows: &[Row]) {
    let detected: Vec<String> = world
        .detected()
        .map(|(h, _)| h.name().to_string())
        .collect();
    let missing: Vec<String> = world
        .missing_hosts()
        .iter()
        .map(|h| h.name().to_string())
        .collect();

    print!("hosts: {}", detected.join(", "));
    if !missing.is_empty() {
        print!("   (not installed: {})", missing.join(", "));
    }
    println!();

    let todo = rows
        .iter()
        .filter(|r| r.severity != Severity::Synced)
        .count();
    println!(
        "{todo} to review, {} in sync\n",
        rows.len().saturating_sub(todo)
    );
}

/// Print the rows and the plan the default actions would produce.
fn print_plan(world: &World, rows: &[Row]) {
    let mut rows = rows.to_vec();
    accept_defaults(&mut rows);
    print_report(&report::plan_report(world, &rows));
}

fn run_plan(world: World, rows: &[Row]) -> Result<()> {
    let plan = world.plan(rows);
    if plan.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }

    println!("Running {} steps.\n", plan.steps.len());
    let mut manifest = world.manifest.clone();
    let report = apply::run(
        &plan,
        &mut manifest,
        &world.manifest_path,
        &world.hosts,
        |progress| match progress {
            // Print the label before the step runs, and flush, so a slow step
            // shows what is in flight instead of looking stalled.
            Progress::Started { index, label } => {
                print!(" \u{2026} [{}/{}] {label}", index + 1, plan.steps.len());
                let _ = io::stdout().flush();
            }
            Progress::Finished(result) => {
                let mark = match result.outcome {
                    Outcome::Done => "\u{2713}",
                    Outcome::Failed => "\u{2717}",
                    Outcome::Skipped => "\u{2013}",
                };
                // Overwrite the in-flight line.
                println!("\r\u{1b}[2K {mark} {}", result.label);
                if result.outcome != Outcome::Done && !result.message.is_empty() {
                    println!("     {}", result.message);
                }
            }
        },
    );

    println!("\n{}", report.summary());
    if report.manifest_written {
        println!(
            "manifest written to {}",
            paths::contract(&world.manifest_path)
        );
    }
    if let Some(e) = &report.manifest_error {
        eprintln!("manifest NOT written: {e}");
    }
    for note in &plan.notes {
        println!("note: {note}");
    }

    if report.any_failed() {
        std::process::exit(1);
    }
    Ok(())
}

/// Print a report the same way the TUI shows it.
fn print_report(report: &report::Report) {
    for section in &report.sections {
        println!("{}", section.title);
        for line in &section.lines {
            let glyph = match line.mark {
                Mark::Ok => "\u{2713}",
                Mark::Problem => "\u{2717}",
                Mark::Warn => "!",
                Mark::Info => "\u{2013}",
                Mark::Plain => " ",
            };
            let indent = "  ".repeat(line.indent as usize + 1);
            println!("{indent}{glyph} {}", line.text);
        }
        println!();
    }
}

fn doctor(world: &World) -> Result<()> {
    let report = report::doctor(world, true);
    print_report(&report);
    if report.problems == 0 {
        println!("No blocking problems found.");
    } else {
        println!("{} problem(s) need attention.", report.problems);
    }
    Ok(())
}

/// Compare each detected host's binary against its declared hook capabilities.
///
/// This is a human-facing report only. A name found in a binary is evidence
/// that a feature might exist, never proof that the host honours it — this
/// project has already seen `codex` mention `asyncRewake` in its string table
/// while still failing to honour the surrounding hook config. Nothing here
/// feeds `plan` or `apply`.
fn hooks_probe(world: &agentsync::domains::World) -> Result<()> {
    println!("Probing installed host binaries for hook field names.");
    println!("A name in a binary is evidence, not proof. Nothing here changes behaviour.\n");
    for (host, _) in world.detected() {
        let Some(declared) = &host.descriptor.hooks else {
            continue;
        };
        let Ok(bin) = which::which(&host.descriptor.detect.bin) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&bin) else {
            continue;
        };
        let lines = probe_lines(&bytes, declared);
        if lines.is_empty() {
            continue;
        }
        println!("{} ({})", host.descriptor.display, bin.display());
        for line in lines {
            println!("  {line}");
        }
    }
    Ok(())
}

/// The capabilities a hook engine can declare or mention, checked in this
/// fixed order so output is stable across runs.
const HOOK_CAPS: [HookCap; 6] = [
    HookCap::Matcher,
    HookCap::If,
    HookCap::Timeout,
    HookCap::AsyncRewake,
    HookCap::RewakeMessage,
    HookCap::RewakeSummary,
];

/// Compare one host binary's bytes against its declared capabilities.
///
/// A file that mentions none of the wire names at all is far more likely
/// something this probe cannot read for this purpose — a launcher script, a
/// wrapper, a stripped binary — than a host with zero hook support. Reporting
/// every declared capability as absent in that case would manufacture a
/// conclusion from input the probe never actually saw, so it says plainly
/// that it could not read the file instead of enumerating false absences.
fn probe_lines(bytes: &[u8], declared: &agentsync::hosts::descriptor::HooksSection) -> Vec<String> {
    let found_any = HOOK_CAPS
        .iter()
        .any(|c| find_bytes(bytes, wire_name(*c).as_bytes()));
    if !found_any {
        return vec![
            "could not read hook field names from this file. \
             It may be a launcher script rather than the host binary."
                .to_string(),
        ];
    }
    let mut out = Vec::new();
    for cap in HOOK_CAPS {
        let needle = wire_name(cap);
        let present = find_bytes(bytes, needle.as_bytes());
        let declared_here = declared.supports(cap);
        let mark = match (present, declared_here) {
            (true, true) | (false, false) => continue,
            (true, false) => "binary mentions it, descriptor does not declare it",
            (false, true) => "descriptor declares it, binary never mentions it",
        };
        out.push(format!("{:<16} {mark}", cap.as_str()));
    }
    out
}

/// How a capability is spelled in a host's own config, which is what a binary
/// containing a parser for it would carry.
fn wire_name(cap: HookCap) -> String {
    match cap {
        HookCap::Matcher => "matcher".into(),
        HookCap::If => "\"if\"".into(),
        HookCap::Timeout => "timeout".into(),
        HookCap::AsyncRewake => "asyncRewake".into(),
        HookCap::RewakeMessage => "rewakeMessage".into(),
        HookCap::RewakeSummary => "rewakeSummary".into(),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn hosts_command(want_parsers: bool) -> Result<()> {
    let hosts = agentsync::hosts::Host::load_all().context("loading host descriptors")?;
    let report = report::hosts_report(&hosts);
    if want_parsers {
        // The parser registry is the last section. Show only that.
        if let Some(section) = report.sections.last() {
            println!("{}", section.title);
            for line in &section.lines {
                println!("  {}", line.text);
            }
        }
        return Ok(());
    }
    print_report(&report);
    Ok(())
}

#[cfg(test)]
mod hooks_probe_tests {
    use super::*;
    use agentsync::hosts::descriptor::HooksSection;

    fn declared(caps: Vec<HookCap>) -> HooksSection {
        HooksSection {
            events: Vec::new(),
            caps,
            output: Vec::new(),
            read: Vec::new(),
            shim: None,
        }
    }

    #[test]
    fn a_file_mentioning_none_of_the_wire_names_reports_unreadable_not_absent() {
        let declared = declared(vec![HookCap::Matcher, HookCap::Timeout]);
        let lines = probe_lines(b"this is a node launcher script, not a binary", &declared);
        assert_eq!(
            lines.len(),
            1,
            "must not enumerate per-capability absences: {lines:?}"
        );
        assert!(
            lines[0].contains("could not read"),
            "must say it could not read the file: {lines:?}"
        );
    }

    #[test]
    fn a_file_mentioning_at_least_one_name_gets_per_capability_lines() {
        let declared = declared(vec![HookCap::Timeout]);
        // Mentions "matcher" but the descriptor does not declare it, and
        // never mentions "timeout" though the descriptor declares it.
        let lines = probe_lines(b"...matcher...", &declared);
        assert!(
            lines.iter().any(|l| l.contains("matcher")
                && l.contains("binary mentions it, descriptor does not declare it")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("timeout") && l.contains("binary never mentions it")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("could not read")),
            "a file with at least one hit must not claim it is unreadable: {lines:?}"
        );
    }
}
