//! agentsync — reconcile MCP servers, skills, and plugins across agentic coding CLIs.
//!
//! Layering (see `docs/architecture.md`):
//!
//! ```text
//! tui/       ratatui views. Renders rows, stages actions.
//! core/      Model, differ, planner, applier. Pure — no host knowledge.
//! domains/   Per-domain read + diff + plan glue (mcp, skills, plugins).
//! hosts/     Descriptor loader, parser registry, CLI runner.
//! manifest/  Canonical file load/save + secret gate.
//! ```
//!
//! The read path parses host config files directly. The write path always goes
//! through the host's own CLI, so agentsync never takes ownership of files it
//! does not own.

pub mod core;
pub mod domains;
pub mod hosts;
pub mod manifest;
pub mod paths;
pub mod platform;
pub mod report;
#[cfg(test)]
pub(crate) mod testutil;
pub mod tui;
pub mod update;
