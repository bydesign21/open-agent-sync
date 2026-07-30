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
//! The read path parses host config files directly; the write path always goes
//! through the host's own CLI so we never take ownership of files we don't own.

pub mod core;
pub mod domains;
pub mod hosts;
pub mod manifest;
pub mod paths;
pub mod platform;
pub mod tui;
