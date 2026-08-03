//! The canonical manifest: what you have decided to keep, as opposed to
//! what any given host happens to contain right now.

pub mod secrets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::core::model::{
    Cap, HttpServer, MarketplaceSource, McpServer, Scope, ScopeKind, StdioServer, Transport,
};
use crate::paths;

use secrets::SecretFinding;

const HEADER: &str = "\
# agentsync manifest
#
# This file is the source of truth for MCP servers, skills, and plugins across
# your agentic coding CLIs. It is safe to commit: values that look like live
# credentials are rejected on save. Reference secrets by environment variable
# name instead (bearer_token_env, env_from, or ${VAR} inside headers).
";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, SkillEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instructions: BTreeMap<String, InstructionEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub marketplaces: BTreeMap<String, MarketplaceEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, PluginEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, HostOverride>,
}

/// Per-host overrides of what a descriptor declares.
///
/// Descriptor capabilities are defaults compiled into the binary. A host CLI can
/// gain a capability before agentsync ships a release that knows about it.
/// Waiting for a release is not an acceptable answer, so the manifest wins.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HookOverride>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HookOverride {
    /// Replaces the declared list wholesale when present. Not merged: a user
    /// removing a capability the descriptor wrongly claims must be able to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<Vec<crate::core::model::HookCap>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<crate::core::model::HookOutputField>>,
}

fn default_scope() -> ScopeKind {
    ScopeKind::User
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpEntry {
    /// `"stdio"` or `"http"`.
    pub transport: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env: Option<String>,

    #[serde(default = "default_scope")]
    pub scope: ScopeKind,
    /// Repos this entry applies to. Only meaningful for local/project scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,

    /// When present, restrict this entry to these hosts. This is how
    /// intentional divergence is *recorded*, so it stops being reported as
    /// drift on every run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

impl McpEntry {
    pub fn from_server(server: &McpServer, scope: ScopeKind, repos: Vec<String>) -> Self {
        let mut entry = McpEntry {
            transport: String::new(),
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            env_from: Vec::new(),
            url: None,
            headers: BTreeMap::new(),
            bearer_token_env: None,
            scope,
            repos,
            hosts: None,
        };
        match &server.transport {
            Transport::Stdio(s) => {
                entry.transport = "stdio".into();
                entry.command = Some(s.command.clone());
                entry.args = s.args.clone();
                entry.env = s.env.clone();
                entry.env_from = s.env_from.clone();
            }
            Transport::Http(h) => {
                entry.transport = "http".into();
                entry.url = Some(h.url.clone());
                entry.headers = h.headers.clone();
                entry.bearer_token_env = h.bearer_token_env.clone();
            }
        }
        entry
    }

    pub fn to_server(&self, name: &str) -> Result<McpServer> {
        let transport = match self.transport.as_str() {
            "stdio" => {
                let command = self
                    .command
                    .clone()
                    .with_context(|| format!("mcp.{name}: stdio transport requires `command`"))?;
                Transport::Stdio(StdioServer {
                    command,
                    args: self.args.clone(),
                    env: self.env.clone(),
                    env_from: self.env_from.clone(),
                })
            }
            "http" | "sse" => {
                let url = self
                    .url
                    .clone()
                    .with_context(|| format!("mcp.{name}: http transport requires `url`"))?;
                Transport::Http(HttpServer {
                    url,
                    headers: self.headers.clone(),
                    bearer_token_env: self.bearer_token_env.clone(),
                })
            }
            other => bail!("mcp.{name}: unknown transport {other:?} (expected stdio or http)"),
        };
        Ok(McpServer {
            name: name.to_string(),
            transport,
            ..Default::default()
        })
    }

    /// Every scope this entry occupies. A local/project entry with three repos
    /// occupies three scopes.
    pub fn scopes(&self) -> Vec<Scope> {
        match self.scope {
            ScopeKind::User => vec![Scope::User],
            ScopeKind::Local => self.repos.iter().cloned().map(Scope::Local).collect(),
            ScopeKind::Project => self.repos.iter().cloned().map(Scope::Project).collect(),
        }
    }

    pub fn targets_host(&self, host: &str) -> bool {
        match &self.hosts {
            None => true,
            Some(list) => list.iter().any(|h| h == host),
        }
    }

    /// `true` when this entry pins an absolute interpreter path, which will not
    /// survive being carried to another machine.
    pub fn non_portable_command(&self) -> Option<&str> {
        let cmd = self.command.as_deref()?;
        if cmd.starts_with('/') {
            Some(cmd)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Path to the canonical skill directory, relative to the manifest's
    /// directory (or absolute / `~`-prefixed).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

impl SkillEntry {
    pub fn resolve(&self, manifest_dir: &Path) -> PathBuf {
        let expanded = paths::expand(&self.source);
        if expanded.is_absolute() {
            expanded
        } else {
            manifest_dir.join(expanded)
        }
    }

    pub fn targets_host(&self, host: &str) -> bool {
        match &self.hosts {
            None => true,
            Some(list) => list.iter().any(|h| h == host),
        }
    }
}

/// A shared instruction file: one canonical markdown file linked into each host.
///
/// Shared by default because most of what goes in these files is about the repo,
/// not the tool — package manager, deploy gate, conventions. `hosts = [...]` is
/// the opt-out for the parts that genuinely name one CLI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstructionEntry {
    /// Path to the canonical file, relative to the manifest's directory (or
    /// absolute / `~`-prefixed).
    pub source: String,
    #[serde(default = "default_scope")]
    pub scope: ScopeKind,
    /// Repos this applies to. Only meaningful for project/local scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

impl InstructionEntry {
    pub fn resolve(&self, manifest_dir: &Path) -> PathBuf {
        let expanded = paths::expand(&self.source);
        if expanded.is_absolute() {
            expanded
        } else {
            manifest_dir.join(expanded)
        }
    }
    pub fn scopes(&self) -> Vec<Scope> {
        match self.scope {
            ScopeKind::User => vec![Scope::User],
            ScopeKind::Local => self.repos.iter().cloned().map(Scope::Local).collect(),
            ScopeKind::Project => self.repos.iter().cloned().map(Scope::Project).collect(),
        }
    }
    pub fn targets_host(&self, host: &str) -> bool {
        match &self.hosts {
            None => true,
            Some(list) => list.iter().any(|h| h == host),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
}

impl MarketplaceEntry {
    pub fn source(&self) -> Option<MarketplaceSource> {
        if let Some(v) = &self.github {
            return Some(MarketplaceSource::GitHub(v.clone()));
        }
        if let Some(v) = &self.directory {
            return Some(MarketplaceSource::Directory(v.clone()));
        }
        self.url.clone().map(MarketplaceSource::Url)
    }

    pub fn targets_host(&self, host: &str) -> bool {
        match &self.hosts {
            None => true,
            Some(list) => list.iter().any(|h| h == host),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Pin the marketplace. Omit it and the marketplace is *derived* per host.
    /// The curated registries genuinely differ between hosts, and one hardcoded
    /// id would produce phantom drift on the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    /// Explicit per-host npm/local mappings for hosts with no marketplace to
    /// resolve a bare name against (OpenCode, Kilo). Additive: an existing
    /// manifest with no `targets` table keeps parsing unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, PluginTarget>,
}

impl PluginEntry {
    pub fn targets_host(&self, host: &str) -> bool {
        match &self.hosts {
            None => true,
            Some(list) => list.iter().any(|h| h == host),
        }
    }
}

/// An explicit npm or local mapping from a marketplace plugin to one
/// OpenCode-family host. Neither CLI resolves a bare plugin id for these
/// hosts and neither has a marketplace to look one up in, so a target here
/// must be named explicitly — never guessed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: ScopeKind,
}

/// The one distinct identity a [`PluginTarget`] names. npm and local
/// identities are never conflated: an npm spec and a local file path live in
/// different namespaces even if their text happened to collide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginIdentity {
    Npm(String),
    Local(String),
}

impl PluginTarget {
    /// The identity this target names, or `None` when it names zero or both
    /// of `npm`/`local`. Ambiguity is a condition to report, never a guess.
    pub fn identity(&self) -> Option<PluginIdentity> {
        match (&self.npm, &self.local) {
            (Some(npm), None) => Some(PluginIdentity::Npm(npm.clone())),
            (None, Some(local)) => Some(PluginIdentity::Local(local.clone())),
            _ => None,
        }
    }

    /// Resolve a `local` source relative to the manifest's own directory,
    /// same convention as [`SkillEntry::resolve`]. `None` for an npm target.
    pub fn resolve_local(&self, manifest_dir: &Path) -> Option<PathBuf> {
        let source = self.local.as_ref()?;
        let expanded = paths::expand(source);
        Some(if expanded.is_absolute() {
            expanded
        } else {
            manifest_dir.join(expanded)
        })
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Manifest::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(manifest)
    }

    /// Validate, back up the previous file, then write.
    ///
    /// Validation is a gate: a manifest containing a literal credential is not
    /// written at all.
    pub fn save(&self, path: &Path) -> Result<()> {
        let findings = self.audit_secrets();
        if !findings.is_empty() {
            let detail = findings
                .iter()
                .map(|f| format!("  {} — {}", f.location, f.reason))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "refusing to write a manifest containing literal credentials:\n{detail}\n\n\
                 Replace the value with an environment variable reference \
                 (bearer_token_env = \"NAME\", env_from = [\"NAME\"], or ${{NAME}} in headers)."
            );
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        paths::backup(path)?;

        let body = toml::to_string_pretty(self).context("serializing manifest")?;
        std::fs::write(path, format!("{HEADER}\n{body}"))
            .with_context(|| format!("writing manifest {}", path.display()))?;
        Ok(())
    }

    /// Every value in the manifest that looks like a live credential.
    pub fn audit_secrets(&self) -> Vec<SecretFinding> {
        let mut out = Vec::new();
        for (name, entry) in &self.mcp {
            for (k, v) in &entry.env {
                secrets::check(&format!("mcp.{name}.env.{k}"), v, &mut out);
            }
            for (k, v) in &entry.headers {
                secrets::check(&format!("mcp.{name}.headers.{k}"), v, &mut out);
            }
            if let Some(url) = &entry.url {
                secrets::check(&format!("mcp.{name}.url"), url, &mut out);
            }
            for (i, arg) in entry.args.iter().enumerate() {
                secrets::check(&format!("mcp.{name}.args[{i}]"), arg, &mut out);
            }
        }
        out
    }

    /// Entries whose `command` is an absolute path. Not an error, but a thing
    /// that breaks on a different machine, so `doctor` reports it.
    pub fn non_portable(&self) -> Vec<(String, String)> {
        self.mcp
            .iter()
            .filter_map(|(name, e)| {
                e.non_portable_command()
                    .map(|c| (name.clone(), c.to_string()))
            })
            .collect()
    }

    /// Environment variables the manifest depends on but which are not set.
    pub fn missing_env(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, entry) in &self.mcp {
            let mut needed: Vec<String> = entry.env_from.clone();
            if let Some(var) = &entry.bearer_token_env {
                needed.push(var.clone());
            }
            for value in entry.headers.values() {
                needed.extend(referenced_vars(value));
            }
            for var in needed {
                if std::env::var_os(&var).is_none() {
                    out.push((name.clone(), var));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The hook capabilities actually in force for a host: what its descriptor
    /// declares, with any manifest override substituted.
    pub fn hooks_for(
        &self,
        host: &str,
        declared: &crate::hosts::descriptor::HooksSection,
    ) -> crate::hosts::descriptor::HooksSection {
        let mut effective = declared.clone();
        if let Some(over) = self.hosts.get(host).and_then(|h| h.hooks.as_ref()) {
            if let Some(caps) = &over.caps {
                effective.caps = caps.clone();
            }
            if let Some(output) = &over.output {
                effective.output = output.clone();
            }
        }
        effective
    }

    /// Which capabilities `name` needs, for gating against a host's `caps`.
    pub fn required_caps(&self, name: &str) -> Vec<Cap> {
        match self.mcp.get(name) {
            Some(entry) => entry
                .to_server(name)
                .map(|s| s.required_caps())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

/// Extract `VAR` from every `${VAR}` in a string.
pub fn referenced_vars(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_trips_a_stdio_entry() {
        let text = r#"
[mcp.kicad]
transport = "stdio"
command = "node"
args = ["~/repos/kicad/dist/index.js"]
env_from = ["KICAD_PYTHON", "PYTHONPATH"]
env = { LOG_LEVEL = "info" }
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        let entry = &m.mcp["kicad"];
        assert_eq!(entry.scope, ScopeKind::User);
        let server = entry.to_server("kicad").unwrap();
        assert!(server.required_caps().contains(&Cap::EnvFrom));
        assert!(server.required_caps().contains(&Cap::Env));
    }

    #[test]
    fn save_refuses_a_literal_credential() {
        let mut m = Manifest::default();
        m.mcp.insert(
            "knowledge".into(),
            McpEntry {
                transport: "http".into(),
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                env_from: vec![],
                url: Some("https://api.example.test/mcp".into()),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae"
                        .to_string(),
                )]),
                bearer_token_env: None,
                scope: ScopeKind::User,
                repos: vec![],
                hosts: None,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let err = m.save(&dir.path().join("manifest.toml")).unwrap_err();
        assert!(err.to_string().contains("literal credentials"), "{err}");
        assert!(!dir.path().join("manifest.toml").exists());
    }

    #[test]
    fn save_accepts_an_env_reference() {
        let mut m = Manifest::default();
        m.mcp.insert(
            "knowledge".into(),
            McpEntry {
                transport: "http".into(),
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                env_from: vec![],
                url: Some("https://api.example.test/mcp".into()),
                headers: BTreeMap::new(),
                bearer_token_env: Some("KNOWLEDGE_TOKEN".into()),
                scope: ScopeKind::User,
                repos: vec![],
                hosts: None,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.toml");
        m.save(&path).unwrap();
        let reloaded = Manifest::load(&path).unwrap();
        assert_eq!(
            reloaded.mcp["knowledge"].bearer_token_env.as_deref(),
            Some("KNOWLEDGE_TOKEN")
        );
    }

    #[test]
    fn local_scope_expands_to_one_scope_per_repo() {
        let text = r#"
[mcp.vanta]
transport = "stdio"
command = "vanta-mcp"
scope = "local"
repos = ["/a/one", "/b/two", "/c/three"]
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        assert_eq!(m.mcp["vanta"].scopes().len(), 3);
    }

    #[test]
    fn extracts_referenced_vars() {
        assert_eq!(referenced_vars("Bearer ${A} and ${B}"), vec!["A", "B"]);
        assert!(referenced_vars("nothing here").is_empty());
    }

    #[test]
    fn hosts_list_records_intentional_divergence() {
        let text = r#"
[mcp.unityMCP]
transport = "http"
url = "http://127.0.0.1:8080/mcp"
hosts = ["codex"]
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        assert!(m.mcp["unityMCP"].targets_host("codex"));
        assert!(!m.mcp["unityMCP"].targets_host("claude"));
    }

    #[test]
    fn a_host_override_replaces_declared_hook_caps() {
        let text = r#"
[hosts.codex.hooks]
caps = ["matcher", "timeout", "async_rewake", "if"]
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        let declared =
            crate::hosts::descriptor::parse(include_str!("../hosts/builtin/codex.toml"), "codex")
                .unwrap()
                .hooks
                .unwrap();
        assert!(!declared.supports(crate::core::model::HookCap::If));

        let effective = m.hooks_for("codex", &declared);
        assert!(
            effective.supports(crate::core::model::HookCap::If),
            "the override must win, so a user on a newer Codex is not blocked"
        );
        // Untouched lists survive.
        assert!(!effective.accepts_output(crate::core::model::HookOutputField::RewakeSummary));
        assert!(effective.can_shim());
    }

    #[test]
    fn plugin_target_backward_compatible_manifest_without_targets_still_parses() {
        let text = r#"
[plugins.superpowers]
marketplace = "claude-plugins-official"
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        assert!(m.plugins["superpowers"].targets.is_empty());
        // Round-trips without inventing a `[plugins.superpowers.targets]` table.
        let rendered = toml::to_string_pretty(&m).unwrap();
        assert!(!rendered.contains("targets"));
    }

    #[test]
    fn plugin_target_parses_the_documented_npm_and_local_examples() {
        let text = r#"
[plugins.security-guidance.targets.opencode]
npm = "@company/opencode-security@1.4.2"
scope = "user"

[plugins.local-policy.targets.kilo]
local = "plugins/local-policy.ts"
scope = "project"
"#;
        let m: Manifest = toml::from_str(text).unwrap();
        let npm_target = &m.plugins["security-guidance"].targets["opencode"];
        assert_eq!(
            npm_target.identity(),
            Some(PluginIdentity::Npm(
                "@company/opencode-security@1.4.2".into()
            ))
        );
        assert_eq!(npm_target.scope, ScopeKind::User);

        let local_target = &m.plugins["local-policy"].targets["kilo"];
        assert_eq!(
            local_target.identity(),
            Some(PluginIdentity::Local("plugins/local-policy.ts".into()))
        );
        assert_eq!(local_target.scope, ScopeKind::Project);
    }

    #[test]
    fn plugin_target_npm_and_local_are_distinct_identities_even_with_the_same_text() {
        let npm = PluginTarget {
            npm: Some("same-text".into()),
            local: None,
            scope: ScopeKind::User,
        };
        let local = PluginTarget {
            npm: None,
            local: Some("same-text".into()),
            scope: ScopeKind::User,
        };
        assert_ne!(npm.identity(), local.identity());
    }

    #[test]
    fn plugin_target_with_neither_npm_nor_local_has_no_identity() {
        let target = PluginTarget {
            npm: None,
            local: None,
            scope: ScopeKind::User,
        };
        assert_eq!(target.identity(), None);
    }

    #[test]
    fn plugin_target_with_both_npm_and_local_has_no_identity_rather_than_a_guess() {
        let target = PluginTarget {
            npm: Some("pkg".into()),
            local: Some("plugins/pkg.ts".into()),
            scope: ScopeKind::User,
        };
        assert_eq!(
            target.identity(),
            None,
            "an ambiguous target must never silently pick one of the two"
        );
    }

    #[test]
    fn plugin_target_round_trips_through_save_and_load() {
        let mut m = Manifest::default();
        m.plugins.insert(
            "security-guidance".into(),
            PluginEntry {
                marketplace: None,
                hosts: None,
                targets: BTreeMap::from([(
                    "opencode".to_string(),
                    PluginTarget {
                        npm: Some("@company/opencode-security@1.4.2".into()),
                        local: None,
                        scope: ScopeKind::User,
                    },
                )]),
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.toml");
        m.save(&path).unwrap();
        let reloaded = Manifest::load(&path).unwrap();
        assert_eq!(
            reloaded.plugins["security-guidance"].targets["opencode"]
                .npm
                .as_deref(),
            Some("@company/opencode-security@1.4.2")
        );
    }

    #[test]
    fn no_override_leaves_the_declared_section_untouched() {
        let m = Manifest::default();
        let declared =
            crate::hosts::descriptor::parse(include_str!("../hosts/builtin/codex.toml"), "codex")
                .unwrap()
                .hooks
                .unwrap();
        let effective = m.hooks_for("codex", &declared);
        assert_eq!(effective.caps, declared.caps);
        assert_eq!(effective.output, declared.output);
    }
}
