//! The neutral data model. Nothing here knows that Claude Code or Codex exist —
//! host parsers translate into these types, and the planner translates back out.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which of a host's configuration layers an entry lives in.
///
/// Scope is part of an entry's *identity*, not a display attribute: the same
/// name at two scopes is a shadowing bug, not a synced pair, and the differ can
/// only see that if it keys on scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Global to the machine.
    User,
    /// This machine, one repo. Not shared. (`claude mcp add` defaults here.)
    Local(String),
    /// Committed in the repo and shared with collaborators.
    Project(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    User,
    Local,
    Project,
}

impl Scope {
    pub fn kind(&self) -> ScopeKind {
        match self {
            Scope::User => ScopeKind::User,
            Scope::Local(_) => ScopeKind::Local,
            Scope::Project(_) => ScopeKind::Project,
        }
    }

    pub fn repo(&self) -> Option<&str> {
        match self {
            Scope::User => None,
            Scope::Local(p) | Scope::Project(p) => Some(p),
        }
    }

    /// The literal value each host CLI expects for `--scope`.
    pub fn cli_name(&self) -> &'static str {
        match self.kind() {
            ScopeKind::User => "user",
            ScopeKind::Local => "local",
            ScopeKind::Project => "project",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::User => write!(f, "user"),
            Scope::Local(p) => write!(f, "local \u{25b8} {}", short_repo(p)),
            Scope::Project(p) => write!(f, "project \u{25b8} {}", short_repo(p)),
        }
    }
}

/// Trailing two path components, which is enough to identify a repo on screen
/// without wrapping the line.
pub fn short_repo(path: &str) -> String {
    // Empty components must be dropped, or a leading `/` counts as a component
    // and `/tmp` renders as `/tmp` instead of `tmp`.
    let parts: Vec<&str> = path
        .trim_end_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    match parts.len() {
        0 => path.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

// ---------------------------------------------------------------------------
// MCP servers
// ---------------------------------------------------------------------------

/// A feature an MCP server definition requires from a host.
///
/// This is the mechanism that prevents silent data loss: `codex mcp add` has no
/// `--header`, so a server carrying [`Cap::Headers`] is *blocked* for Codex and
/// reported, never pushed with the headers quietly dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    Stdio,
    Http,
    /// Literal environment variables passed to a stdio server.
    Env,
    /// Environment variables forwarded from the ambient shell by name.
    EnvFrom,
    /// Arbitrary HTTP headers.
    Headers,
    /// Bearer token read from a named environment variable.
    BearerEnv,
}

impl Cap {
    pub fn as_str(self) -> &'static str {
        match self {
            Cap::Stdio => "stdio",
            Cap::Http => "http",
            Cap::Env => "env",
            Cap::EnvFrom => "env_from",
            Cap::Headers => "headers",
            Cap::BearerEnv => "bearer_env",
        }
    }
}

impl fmt::Display for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct StdioServer {
    pub command: String,
    pub args: Vec<String>,
    /// Literal values. Never secrets — the manifest gate rejects those.
    pub env: BTreeMap<String, String>,
    /// Names only; values are read from the ambient environment at launch.
    pub env_from: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HttpServer {
    pub url: String,
    /// Values may contain `${VAR}` references, which are resolved by the host.
    pub headers: BTreeMap<String, String>,
    pub bearer_token_env: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Stdio(StdioServer),
    Http(HttpServer),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub transport: Transport,
}

impl McpServer {
    pub fn required_caps(&self) -> Vec<Cap> {
        let mut caps = Vec::new();
        match &self.transport {
            Transport::Stdio(s) => {
                caps.push(Cap::Stdio);
                if !s.env.is_empty() {
                    caps.push(Cap::Env);
                }
                if !s.env_from.is_empty() {
                    caps.push(Cap::EnvFrom);
                }
            }
            Transport::Http(h) => {
                caps.push(Cap::Http);
                if !h.headers.is_empty() {
                    caps.push(Cap::Headers);
                }
                if h.bearer_token_env.is_some() {
                    caps.push(Cap::BearerEnv);
                }
            }
        }
        caps
    }

    /// A one-line summary for the detail pane.
    pub fn summary(&self) -> String {
        match &self.transport {
            Transport::Stdio(s) => {
                let mut out = format!("stdio \u{b7} {}", s.command);
                if !s.args.is_empty() {
                    out.push(' ');
                    out.push_str(&s.args.join(" "));
                }
                out
            }
            Transport::Http(h) => {
                let mut out = format!("http \u{b7} {}", h.url);
                if h.bearer_token_env.is_some() {
                    out.push_str(" \u{b7} bearer from env");
                }
                if !h.headers.is_empty() {
                    out.push_str(&format!(" \u{b7} {} header(s)", h.headers.len()));
                }
                out
            }
        }
    }

    /// Field-level differences against another definition of the same server.
    pub fn diff(&self, other: &McpServer) -> Vec<FieldDiff> {
        let mut out = Vec::new();
        match (&self.transport, &other.transport) {
            (Transport::Stdio(a), Transport::Stdio(b)) => {
                push_diff(&mut out, "command", &a.command, &b.command);
                push_diff(&mut out, "args", &a.args.join(" "), &b.args.join(" "));
                push_diff(&mut out, "env", &render_map(&a.env), &render_map(&b.env));
                push_diff(
                    &mut out,
                    "env_from",
                    &a.env_from.join(", "),
                    &b.env_from.join(", "),
                );
            }
            (Transport::Http(a), Transport::Http(b)) => {
                push_diff(&mut out, "url", &a.url, &b.url);
                push_diff(
                    &mut out,
                    "headers",
                    &render_map(&a.headers),
                    &render_map(&b.headers),
                );
                push_diff(
                    &mut out,
                    "bearer_token_env",
                    a.bearer_token_env.as_deref().unwrap_or(""),
                    b.bearer_token_env.as_deref().unwrap_or(""),
                );
            }
            _ => out.push(FieldDiff {
                field: "transport".into(),
                manifest: transport_name(&self.transport).into(),
                host: transport_name(&other.transport).into(),
            }),
        }
        out
    }
}

fn transport_name(t: &Transport) -> &'static str {
    match t {
        Transport::Stdio(_) => "stdio",
        Transport::Http(_) => "http",
    }
}

fn push_diff(out: &mut Vec<FieldDiff>, field: &str, manifest: &str, host: &str) {
    if manifest != host {
        out.push(FieldDiff {
            field: field.to_string(),
            manifest: manifest.to_string(),
            host: host.to_string(),
        });
    }
}

fn render_map(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDiff {
    pub field: String,
    pub manifest: String,
    pub host: String,
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// How a skill currently exists inside one host's skills directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillState {
    /// Symlink pointing at our canonical directory. Synced.
    Linked,
    /// Symlink pointing somewhere else — typically another tool's install.
    /// Reported, never rewritten.
    Foreign(PathBuf),
    /// A real directory the host owns. Adopting it moves it into canonical.
    RealDir,
    /// Not present.
    Absent,
}

impl SkillState {
    pub fn present(&self) -> bool {
        !matches!(self, SkillState::Absent)
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub name: String,
    pub marketplace: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketplaceSource {
    GitHub(String),
    Directory(String),
    Url(String),
}

impl MarketplaceSource {
    /// The argument form each host's `marketplace add` accepts.
    pub fn as_arg(&self) -> &str {
        match self {
            MarketplaceSource::GitHub(v)
            | MarketplaceSource::Directory(v)
            | MarketplaceSource::Url(v) => v,
        }
    }
}

impl fmt::Display for MarketplaceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketplaceSource::GitHub(v) => write!(f, "github:{v}"),
            MarketplaceSource::Directory(v) => write!(f, "dir:{v}"),
            MarketplaceSource::Url(v) => write!(f, "{v}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Everything we read from one host in one pass.
#[derive(Clone, Debug, Default)]
pub struct HostSnapshot {
    pub host: String,
    pub display: String,
    /// False when the host's binary isn't on PATH. Such a host renders dimmed
    /// and stages nothing: absent is not the same as divergent.
    pub detected: bool,
    pub mcp: BTreeMap<(Scope, String), McpServer>,
    /// Skill name -> state, per skills directory this host reads.
    pub skills: BTreeMap<String, SkillState>,
    /// Skill names provided by installed plugins. These belong to the plugin
    /// manager, so the skills domain must ignore them entirely.
    pub plugin_skills: Vec<String>,
    pub plugins: BTreeMap<String, InstalledPlugin>,
    pub marketplaces: BTreeMap<String, MarketplaceSource>,
    /// Non-fatal problems hit while reading, surfaced by `doctor`.
    pub warnings: Vec<String>,
}

impl HostSnapshot {
    pub fn mcp_at(&self, scope: &Scope, name: &str) -> Option<&McpServer> {
        self.mcp.get(&(scope.clone(), name.to_string()))
    }

    /// Every scope at which `name` appears. More than one means shadowing.
    pub fn mcp_scopes(&self, name: &str) -> Vec<Scope> {
        self.mcp
            .keys()
            .filter(|(_, n)| n == name)
            .map(|(s, _)| s.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(command: &str) -> McpServer {
        McpServer {
            name: "x".into(),
            transport: Transport::Stdio(StdioServer {
                command: command.into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn header_servers_require_the_headers_cap() {
        let s = McpServer {
            name: "corridor".into(),
            transport: Transport::Http(HttpServer {
                url: "https://example.test/mcp".into(),
                headers: BTreeMap::from([("X-Key".to_string(), "${K}".to_string())]),
                bearer_token_env: None,
            }),
        };
        assert!(s.required_caps().contains(&Cap::Headers));
    }

    #[test]
    fn bare_and_absolute_commands_are_a_real_difference() {
        let a = stdio("node");
        let b = stdio("/Users/x/.nix-profile/bin/node");
        let diffs = a.diff(&b);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "command");
    }

    #[test]
    fn transport_mismatch_reports_one_diff_not_a_field_storm() {
        let a = stdio("node");
        let b = McpServer {
            name: "x".into(),
            transport: Transport::Http(HttpServer::default()),
        };
        let diffs = a.diff(&b);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "transport");
    }

    #[test]
    fn short_repo_keeps_two_components() {
        assert_eq!(
            short_repo("/Users/x/Documents/Repos/core/infra"),
            "core/infra"
        );
        assert_eq!(short_repo("/tmp"), "tmp");
        assert_eq!(short_repo("/a/b/"), "a/b");
        assert_eq!(short_repo(""), "");
    }
}
