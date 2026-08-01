//! The neutral data model. Nothing here knows that Claude Code or Codex exist —
//! host parsers translate into these types, and the planner translates back out.

use std::collections::{BTreeMap, BTreeSet};
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

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// A feature a hook handler requires from a host's hook engine.
///
/// Same role as [`Cap`] for MCP: a capability the target lacks makes the row
/// blocked or shimmable, never silently dropped. Codex ignores `if` entirely —
/// it hashes only the command, so five handlers distinguished only by `if`
/// collapse to five identical entries in its `[hooks.state]` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookCap {
    /// Coarse per-event tool filter.
    Matcher,
    /// Fine-grained permission-style filter, e.g. `Bash(git commit:*)`.
    If,
    Timeout,
    AsyncRewake,
    RewakeMessage,
    RewakeSummary,
}

impl HookCap {
    pub fn as_str(self) -> &'static str {
        match self {
            HookCap::Matcher => "matcher",
            HookCap::If => "if",
            HookCap::Timeout => "timeout",
            HookCap::AsyncRewake => "async_rewake",
            HookCap::RewakeMessage => "rewake_message",
            HookCap::RewakeSummary => "rewake_summary",
        }
    }
}

impl fmt::Display for HookCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A top-level key a host accepts on a hook's stdout.
///
/// Separate vocabulary from [`HookCap`] on purpose: `caps` is what the host
/// understands in the *manifest*, `output` is what it accepts back on *stdout*.
/// The observed Codex failure is one of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutputField {
    HookSpecificOutput,
    SystemMessage,
    AdditionalContext,
    SuppressOutput,
    RewakeMessage,
    RewakeSummary,
}

impl HookOutputField {
    /// The literal JSON key, which is camelCase on the wire.
    pub fn json_key(self) -> &'static str {
        match self {
            HookOutputField::HookSpecificOutput => "hookSpecificOutput",
            HookOutputField::SystemMessage => "systemMessage",
            HookOutputField::AdditionalContext => "additionalContext",
            HookOutputField::SuppressOutput => "suppressOutput",
            HookOutputField::RewakeMessage => "rewakeMessage",
            HookOutputField::RewakeSummary => "rewakeSummary",
        }
    }
}

/// Stable identity for one hook handler.
///
/// Deliberately the scheme Codex records in `[hooks.state]`, so a row can be
/// traced to a host's own state without a translation step.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HookId {
    /// `<plugin>@<marketplace>:<relative file>` for plugin hooks, or the
    /// settings file path for user-level hooks.
    pub source: String,
    pub event: String,
    /// Index of the matcher group within the event.
    pub group: usize,
    /// Index of the handler within its group.
    pub index: usize,
}

/// `PostToolUse` -> `post_tool_use`.
pub fn event_key(event: &str) -> String {
    let mut out = String::with_capacity(event.len() + 4);
    for (i, c) in event.char_indices() {
        if c.is_uppercase() && i != 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

impl fmt::Display for HookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.source,
            event_key(&self.event),
            self.group,
            self.index
        )
    }
}

/// One hook handler, normalised away from any host's spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookHandler {
    pub id: HookId,
    pub event: String,
    pub command: String,
    pub matcher: Option<String>,
    pub if_pattern: Option<String>,
    pub timeout: Option<u64>,
    pub async_rewake: bool,
    pub rewake_message: Option<String>,
    pub rewake_summary: Option<String>,
    /// Absolute root the command's `${CLAUDE_PLUGIN_ROOT}` refers to. `None`
    /// for handlers that came from a settings file rather than a plugin.
    pub plugin_root: Option<PathBuf>,
    /// Keys present in the source that this model does not know. Reported
    /// rather than dropped, so an unrecognised field can never look handled.
    pub unknown_fields: BTreeSet<String>,
}

impl HookHandler {
    pub fn new(id: HookId, event: impl Into<String>, command: impl Into<String>) -> Self {
        HookHandler {
            id,
            event: event.into(),
            command: command.into(),
            matcher: None,
            if_pattern: None,
            timeout: None,
            async_rewake: false,
            rewake_message: None,
            rewake_summary: None,
            plugin_root: None,
            unknown_fields: BTreeSet::new(),
        }
    }

    /// Capabilities this handler needs from whatever host runs it.
    ///
    /// Derived from which fields are present. What the hook *prints* is not
    /// knowable statically, so output compatibility is handled separately.
    pub fn required_caps(&self) -> Vec<HookCap> {
        let mut caps = Vec::new();
        if self.matcher.is_some() {
            caps.push(HookCap::Matcher);
        }
        if self.if_pattern.is_some() {
            caps.push(HookCap::If);
        }
        if self.timeout.is_some() {
            caps.push(HookCap::Timeout);
        }
        if self.async_rewake {
            caps.push(HookCap::AsyncRewake);
        }
        if self.rewake_message.is_some() {
            caps.push(HookCap::RewakeMessage);
        }
        if self.rewake_summary.is_some() {
            caps.push(HookCap::RewakeSummary);
        }
        caps
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

    /// True when the definition carries no credential of its own, so the host
    /// has to acquire one interactively.
    ///
    /// Pushing such a server writes a perfectly valid config entry that cannot
    /// connect until someone runs the host's login command. OAuth credentials are
    /// per-host and do not travel with the definition, so reporting the add as
    /// done without saying this is reporting success for something non-functional.
    pub fn needs_interactive_login(&self) -> bool {
        match &self.transport {
            Transport::Http(h) => h.bearer_token_env.is_none() && h.headers.is_empty(),
            // A stdio server authenticates however its command does; nothing for
            // us to say.
            Transport::Stdio(_) => false,
        }
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

/// Whether a host currently holds working credentials for a server.
///
/// Read from the host's CLI, not its config file: the file records *how* to
/// authenticate, never whether the credential is present and valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStatus {
    /// Credentials present, or none needed.
    Ok(String),
    /// Configured but unauthenticated — the server will fail to start.
    NotLoggedIn,
    /// The server does not authenticate.
    NotApplicable,
}

impl AuthStatus {
    /// Parse a host's own vocabulary. Unknown values are treated as fine rather
    /// than as failures, so a new status string does not manufacture alarms.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "not_logged_in" => AuthStatus::NotLoggedIn,
            "unsupported" => AuthStatus::NotApplicable,
            other => AuthStatus::Ok(other.to_string()),
        }
    }

    pub fn needs_login(&self) -> bool {
        matches!(self, AuthStatus::NotLoggedIn)
    }
}

impl fmt::Display for AuthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthStatus::Ok(v) => f.write_str(v),
            AuthStatus::NotLoggedIn => f.write_str("not logged in"),
            AuthStatus::NotApplicable => f.write_str("no auth"),
        }
    }
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// How a linked thing currently exists inside one host's directory.
///
/// Shared by skills and instruction files, because the states and their
/// consequences are identical: a symlink into canonical storage is synced, a real
/// file or directory is content the host owns and must not be silently
/// overwritten, and a symlink elsewhere belongs to something else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Symlink pointing at our canonical path. Synced.
    Linked,
    /// Symlink pointing somewhere else — typically another tool's install.
    /// Reported, never rewritten.
    Foreign(PathBuf),
    /// Real content the host owns. Adopting it moves it into canonical.
    Owned,
    /// Not present.
    Absent,
}

impl LinkState {
    pub fn present(&self) -> bool {
        !matches!(self, LinkState::Absent)
    }
}

// ---------------------------------------------------------------------------
// Instruction files
// ---------------------------------------------------------------------------

/// One host's instruction file for one scope — `CLAUDE.md`, `AGENTS.md`, and so on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    /// Where this host expects the file.
    pub path: PathBuf,
    pub state: LinkState,
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
    pub skills: BTreeMap<String, LinkState>,
    /// Skill names provided by installed plugins. These belong to the plugin
    /// manager, so the skills domain must ignore them entirely.
    pub plugin_skills: Vec<String>,
    /// Instruction files, keyed by the scope they apply to.
    pub instructions: BTreeMap<Scope, InstructionFile>,
    pub plugins: BTreeMap<String, InstalledPlugin>,
    pub marketplaces: BTreeMap<String, MarketplaceSource>,
    /// What each of this host's marketplaces actually offers: marketplace name ->
    /// plugin names.
    ///
    /// This has to be read rather than assumed. Neither CLI resolves a bare
    /// plugin id: `codex plugin add superpowers` fails with "requires
    /// --marketplace unless passed as <plugin>@<marketplace>", and
    /// `claude plugin install` fails outright when no configured marketplace
    /// carries the name.
    pub catalog: BTreeMap<String, BTreeSet<String>>,
    pub hooks: BTreeMap<HookId, HookHandler>,
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
    fn only_a_credential_less_http_server_needs_an_interactive_login() {
        let oauth = McpServer {
            name: "sentry".into(),
            transport: Transport::Http(HttpServer {
                url: "https://mcp.sentry.dev/mcp".into(),
                ..Default::default()
            }),
        };
        assert!(oauth.needs_interactive_login());

        let with_env = McpServer {
            name: "k".into(),
            transport: Transport::Http(HttpServer {
                url: "https://a.test/mcp".into(),
                bearer_token_env: Some("TOK".into()),
                ..Default::default()
            }),
        };
        assert!(!with_env.needs_interactive_login());

        let with_header = McpServer {
            name: "k".into(),
            transport: Transport::Http(HttpServer {
                url: "https://a.test/mcp".into(),
                headers: BTreeMap::from([("X-Key".to_string(), "${K}".to_string())]),
                bearer_token_env: None,
            }),
        };
        assert!(!with_header.needs_interactive_login());

        // A stdio server authenticates however its command does.
        assert!(!stdio("node").needs_interactive_login());
    }

    #[test]
    fn an_unknown_auth_status_is_not_treated_as_a_failure() {
        assert!(AuthStatus::parse("not_logged_in").needs_login());
        assert!(!AuthStatus::parse("o_auth").needs_login());
        assert!(!AuthStatus::parse("bearer_token").needs_login());
        assert!(!AuthStatus::parse("unsupported").needs_login());
        // A status string we have never seen must not manufacture an alarm.
        assert!(!AuthStatus::parse("something_new").needs_login());
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

    #[test]
    fn required_caps_come_from_the_fields_that_are_present() {
        let h = HookHandler {
            id: HookId {
                source: "security-guidance@claude-plugins-official:hooks/hooks.json".into(),
                event: "PostToolUse".into(),
                group: 1,
                index: 0,
            },
            event: "PostToolUse".into(),
            command: "bash review.sh".into(),
            matcher: Some("Bash".into()),
            if_pattern: Some("Bash(git commit:*)".into()),
            timeout: None,
            async_rewake: true,
            rewake_message: Some("findings:".into()),
            rewake_summary: Some("Commit security review found issues".into()),
            plugin_root: None,
            unknown_fields: BTreeSet::new(),
        };
        assert_eq!(
            h.required_caps(),
            vec![
                HookCap::Matcher,
                HookCap::If,
                HookCap::AsyncRewake,
                HookCap::RewakeMessage,
                HookCap::RewakeSummary,
            ]
        );
    }

    #[test]
    fn a_bare_handler_requires_nothing() {
        let h = HookHandler::new(
            HookId { source: "s".into(), event: "Stop".into(), group: 0, index: 0 },
            "Stop",
            "echo hi",
        );
        assert!(h.required_caps().is_empty());
    }

    #[test]
    fn hook_id_renders_the_scheme_codex_uses_in_its_own_state_table() {
        let id = HookId {
            source: "security-guidance@claude-plugins-official:hooks/hooks.json".into(),
            event: "PostToolUse".into(),
            group: 1,
            index: 4,
        };
        assert_eq!(
            id.to_string(),
            "security-guidance@claude-plugins-official:hooks/hooks.json:post_tool_use:1:4"
        );
    }
}
