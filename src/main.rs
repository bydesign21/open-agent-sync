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
use agentsync::core::plan::{FsOp, Step};
use agentsync::domains::World;
use agentsync::hosts::{parsers, runner};
use agentsync::paths;

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

    /// Only this domain: mcp, skills, or plugins (repeatable).
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
            print_rows(&world, &rows);
            print_plan(&world, &rows);
            Ok(())
        }

        Some(Command::Apply { yes }) => {
            let world = World::load(&manifest_path, &cli.repos)?;
            let mut rows = filtered_rows(&world, &cli.only);
            print_rows(&world, &rows);
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
            "plugins" | "plugin" => Some(Domain::Plugins),
            other => {
                eprintln!("agentsync: unknown domain {other:?}; expected mcp, skills, or plugins");
                None
            }
        })
        .collect();
    rows.into_iter()
        .filter(|r| wanted.contains(&r.domain))
        .collect()
}

fn print_rows(world: &World, rows: &[Row]) {
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

    for domain in Domain::ALL {
        let group: Vec<&Row> = rows
            .iter()
            .filter(|r| r.domain == domain && r.severity != Severity::Synced)
            .collect();
        if group.is_empty() {
            continue;
        }
        println!("{}", domain.title());
        let width = group
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        let hwidth = group
            .iter()
            .map(|r| r.headline.chars().count())
            .max()
            .unwrap_or(0)
            .min(36);
        for row in group {
            println!(
                " {} {:<width$}  {:<hwidth$}  {}",
                row.severity.mark(false),
                truncate(&row.name, width),
                truncate(&row.headline, hwidth),
                row.action().label,
            );
            if !row.detail.is_empty() {
                println!("   {:<width$}  {}", "", row.detail);
            }
        }
        println!();
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Print the plan the default actions would produce.
fn print_plan(world: &World, rows: &[Row]) {
    let mut rows = rows.to_vec();
    accept_defaults(&mut rows);
    let plan = world.plan(&rows);

    if plan.is_empty() {
        println!("Nothing to do.");
    } else {
        println!("PLAN ({} steps)", plan.steps.len());
        for (i, step) in plan.steps.iter().enumerate() {
            println!("  {:>2}  {}", i + 1, step.label);
            if let Some(line) = describe_step(world, &step.step) {
                println!("      {line}");
            }
        }
        println!();
    }

    if !plan.notes.is_empty() {
        println!("NOTES");
        for note in &plan.notes {
            println!("  \u{2022} {note}");
        }
        println!();
    }
}

/// The concrete effect of a step, so the plan is auditable rather than a
/// description of intent.
fn describe_step(world: &World, step: &Step) -> Option<String> {
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

fn doctor(world: &World) -> Result<()> {
    let mut problems = 0usize;

    println!("HOSTS");
    for (host, snap) in world.detected() {
        println!(
            "  \u{2713} {:<10} {:<16} mcp:{} skills:{} plugins:{}",
            host.name(),
            host.descriptor.display,
            snap.mcp.len(),
            snap.skills.len(),
            snap.plugins.len()
        );
    }
    for host in world.missing_hosts() {
        println!(
            "  \u{2013} {:<10} not installed ({} not on PATH)",
            host.name(),
            host.descriptor.detect.bin
        );
    }
    println!();

    let secrets = world.manifest.audit_secrets();
    if !secrets.is_empty() {
        problems += secrets.len();
        println!("LITERAL CREDENTIALS IN THE MANIFEST");
        for f in &secrets {
            println!("  \u{2717} {} \u{2014} {}", f.location, f.reason);
        }
        println!();
    }

    let non_portable = world.manifest.non_portable();
    if !non_portable.is_empty() {
        println!("NON-PORTABLE COMMANDS (these will not resolve on another machine)");
        for (name, cmd) in &non_portable {
            println!("  ! mcp.{name}: {cmd}");
        }
        println!();
    }

    let missing_env = world.manifest.missing_env();
    if !missing_env.is_empty() {
        problems += missing_env.len();
        println!("UNSET ENVIRONMENT VARIABLES");
        for (name, var) in &missing_env {
            println!("  \u{2717} mcp.{name} needs ${var}");
        }
        println!();
    }

    // Auth status has to come from the CLI: a config file records how to
    // authenticate, never whether the credential is present. This is the gap that
    // let two OAuth servers be reported as pushed while being unable to connect.
    let mut auth_lines: Vec<String> = Vec::new();
    let mut unknown_hosts: Vec<String> = Vec::new();
    for (host, _) in world.detected() {
        match host.probe_auth() {
            Ok(None) => unknown_hosts.push(host.name().to_string()),
            Ok(Some(statuses)) => {
                for (name, status) in statuses {
                    if status.needs_login() {
                        let fix = host
                            .mcp_login_command(&name)
                            .unwrap_or_else(|| format!("log in to {name} on {}", host.name()));
                        auth_lines.push(format!("{}: {name} \u{2014} run `{fix}`", host.name()));
                    }
                }
            }
            Err(e) => world_warn(&mut auth_lines, host.name(), &e),
        }
    }
    if !auth_lines.is_empty() {
        problems += auth_lines.len();
        println!("MCP SERVERS THAT ARE CONFIGURED BUT NOT AUTHENTICATED");
        for line in &auth_lines {
            println!("  \u{2717} {line}");
        }
        println!();
    }
    if !unknown_hosts.is_empty() {
        println!(
            "NOTE: {} exposes no machine-readable auth status, so logged-out servers\n      \
             there cannot be detected \u{2014} only its own startup warnings will show them.\n",
            unknown_hosts.join(", ")
        );
    }

    if !world.warnings.is_empty() {
        println!("READ WARNINGS");
        for w in &world.warnings {
            println!("  ! {w}");
        }
        println!();
    }

    let foreign: Vec<String> = world
        .detected()
        .flat_map(|(h, s)| {
            s.skills
                .iter()
                .filter_map(move |(name, state)| match state {
                    agentsync::core::model::SkillState::Foreign(target) => Some(format!(
                        "{}: {name} -> {}",
                        h.name(),
                        paths::contract(target)
                    )),
                    _ => None,
                })
        })
        .collect();
    if !foreign.is_empty() {
        println!("SKILLS LINKED OUTSIDE agentsync (left alone)");
        for f in &foreign {
            println!("  \u{2013} {f}");
        }
        println!();
    }

    if problems == 0 {
        println!("No blocking problems found.");
    } else {
        println!("{problems} problem(s) need attention.");
    }
    Ok(())
}

/// A probe that failed is itself worth reporting: silence would read as "fine".
fn world_warn(out: &mut Vec<String>, host: &str, e: &anyhow::Error) {
    out.push(format!("{host}: could not read auth status \u{2014} {e:#}"));
}

fn hosts_command(want_parsers: bool) -> Result<()> {
    use agentsync::core::model::ScopeKind;

    if want_parsers {
        println!("COMPILED PARSERS (reference these as `parser = \"...\"` in a descriptor)");
        for (name, what) in parsers::registry() {
            println!("  {name:<24} {what}");
        }
        return Ok(());
    }

    let hosts = agentsync::hosts::Host::load_all().context("loading host descriptors")?;
    println!(
        "Descriptors are built in, and overridden by files in {}\n",
        paths::contract(&paths::hosts_dir())
    );
    for host in hosts {
        let status = if host.detected() {
            "installed"
        } else {
            "not installed"
        };
        println!(
            "{} ({}) \u{2014} {status}",
            host.name(),
            host.descriptor.display
        );
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
            println!("  mcp scopes: {}", scopes.join(", "));
            println!("  mcp caps:   {}", caps.join(", "));
        }
        if let Some(skills) = &host.descriptor.skills {
            println!(
                "  skills:     {} (first is the link target)",
                skills.dirs.join(", ")
            );
        }
        if host.descriptor.plugins.is_some() {
            println!("  plugins:    yes");
        }
        println!();
    }
    Ok(())
}
