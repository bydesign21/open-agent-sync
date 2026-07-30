//! The compiled parser registry — the escape hatch referenced by
//! `parser = "..."` in host descriptors.
//!
//! Config *formats* are irregular enough that describing them in data would be a
//! second programming language. Config *locations* and *CLI invocations* are
//! regular, so those stay in the descriptor. A new host normally reuses a parser
//! here; only a genuinely new file format needs code.

pub mod mcp;
pub mod plugins;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::core::model::{InstalledPlugin, MarketplaceSource, McpServer, Scope};

/// Context a parser needs beyond the file text.
#[derive(Clone, Debug, Default)]
pub struct ParseCtx {
    /// Repo this source was read for, when the descriptor path contained
    /// `{repo}`.
    pub repo: Option<String>,
    /// The file the text came from, for warning messages.
    pub origin: PathBuf,
}

/// What an MCP parser produces, plus anything questionable it noticed.
#[derive(Debug, Default)]
pub struct McpRead {
    pub servers: Vec<(Scope, McpServer)>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PluginRead {
    pub plugins: BTreeMap<String, InstalledPlugin>,
    pub marketplaces: BTreeMap<String, MarketplaceSource>,
    pub warnings: Vec<String>,
}

pub fn read_mcp(parser: &str, text: &str, ctx: &ParseCtx) -> Result<McpRead> {
    match parser {
        "claude_json_v1" => mcp::claude_json_v1(text, ctx),
        "mcp_json_v1" => mcp::mcp_json_v1(text, ctx),
        "codex_toml_v1" => mcp::codex_toml_v1(text, ctx),
        other => bail!("unknown mcp parser {other:?} (see `agentsync hosts --parsers`)"),
    }
}

pub fn read_plugins(parser: &str, text: &str, ctx: &ParseCtx) -> Result<PluginRead> {
    match parser {
        "claude_plugins_v1" => plugins::claude_plugins_v1(text, ctx),
        "claude_marketplaces_v1" => plugins::claude_marketplaces_v1(text, ctx),
        "codex_plugins_toml_v1" => plugins::codex_plugins_toml_v1(text, ctx),
        other => bail!("unknown plugin parser {other:?} (see `agentsync hosts --parsers`)"),
    }
}

/// Serialize a server into the JSON document a `style = "json"` host expects.
pub fn serialize_mcp(serializer: &str, server: &McpServer) -> Result<String> {
    match serializer {
        "claude_json_v1" => mcp::claude_json_v1_serialize(server),
        other => bail!("unknown mcp serializer {other:?}"),
    }
}

/// Names of everything registered, for `agentsync hosts --parsers`.
pub fn registry() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "claude_json_v1",
            "mcp: ~/.claude.json (user + per-repo local scopes)",
        ),
        (
            "mcp_json_v1",
            "mcp: <repo>/.mcp.json (committed project scope)",
        ),
        ("codex_toml_v1", "mcp: ~/.codex/config.toml [mcp_servers.*]"),
        (
            "claude_plugins_v1",
            "plugins: ~/.claude/plugins/installed_plugins.json",
        ),
        (
            "claude_marketplaces_v1",
            "plugins: ~/.claude/plugins/known_marketplaces.json",
        ),
        (
            "codex_plugins_toml_v1",
            "plugins: ~/.codex/config.toml [plugins.*]",
        ),
    ]
}
