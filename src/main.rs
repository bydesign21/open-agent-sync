//! agentsync — keep MCP servers, skills, and plugins in sync across agentic
//! coding CLIs.
//!
//! With no subcommand this opens the review TUI. The subcommands exist so the
//! same core is scriptable and so the plan can be inspected without a terminal.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agentsync::core::apply::{self, Outcome, Progress};
use agentsync::core::diff::{Domain, Row, Severity};
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

    /// Only this domain: mcp, skills, instructions, or plugins (repeatable).
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
                    "Nothing was changed. Re-run with --yes to accept every default action, \
                     or run `agentsync` with no arguments to choose per row."
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
            other => {
                eprintln!(
                    "agentsync: unknown domain {other:?}; expected mcp, skills, \
                     instructions, or plugins"
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

fn hosts_command(want_parsers: bool) -> Result<()> {
    let hosts = agentsync::hosts::Host::load_all().context("loading host descriptors")?;
    let report = report::hosts_report(&hosts);
    if want_parsers {
        // The parser registry is the last section; show only that.
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
