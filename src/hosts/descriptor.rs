//! Host descriptors: the declarative half of the extension mechanism.
//!
//! A descriptor says where a host keeps its configuration (read path), how to
//! invoke its CLI (write path), and what it is capable of representing. Adding a
//! host is normally a new TOML file in `~/.config/agentsync/hosts/`.
//!
//! Two things are deliberately *not* declarative:
//!
//! * **Config parsing** — `parser = "..."` names a compiled parser. Config
//!   formats are too irregular to describe in data, but there are only a handful
//!   of shapes, and a new host usually reuses an existing one.
//! * **JSON serialization** for hosts whose `add` takes a whole JSON document.
//!
//! Everything else — argv, flag spellings, scopes, capabilities, directories —
//! lives in the file.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::model::{Cap, ScopeKind};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostDescriptor {
    pub name: String,
    pub display: String,
    pub detect: Detect,

    #[serde(default)]
    pub mcp: Option<McpSection>,
    #[serde(default)]
    pub skills: Option<SkillsSection>,
    #[serde(default)]
    pub instructions: Option<InstructionsSection>,
    #[serde(default)]
    pub plugins: Option<PluginsSection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Detect {
    /// Binary looked up on `PATH`. Absent binary means the host renders dimmed
    /// and stages nothing — absent is not divergent.
    pub bin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadSource {
    /// `~`-expandable path. `{repo}` is substituted for per-repo sources.
    pub file: String,
    /// Name of a compiled parser in [`crate::hosts::parsers`].
    pub parser: String,
    /// When set, this source is only consulted for this scope.
    #[serde(default)]
    pub scope: Option<ScopeKind>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invocation {
    pub argv: Vec<String>,
}

/// How a host's `mcp add` accepts a server definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddStyle {
    /// One argv containing `{json}`; the whole definition goes in as a document.
    Json,
    /// Separate argv per transport, with repeated flags for env and headers.
    Flags,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpAdd {
    pub style: AddStyle,

    /// `style = "json"`: the single argv template. Must contain `{json}`.
    #[serde(default)]
    pub argv: Vec<String>,
    /// Name of the compiled serializer producing `{json}`.
    #[serde(default)]
    pub json_serializer: Option<String>,

    /// `style = "flags"`: one argv per transport.
    #[serde(default)]
    pub argv_stdio: Vec<String>,
    #[serde(default)]
    pub argv_http: Vec<String>,

    /// Flag spellings for the repeated groups referenced as `{env_flags...}`,
    /// `{header_flags...}` and `{bearer_flags...}`.
    #[serde(default)]
    pub env_flag: Option<String>,
    #[serde(default)]
    pub env_format: Option<String>,
    #[serde(default)]
    pub header_flag: Option<String>,
    #[serde(default)]
    pub header_format: Option<String>,
    #[serde(default)]
    pub bearer_env_flag: Option<String>,
}

/// How to ask a host which of its servers actually have credentials.
///
/// A separate probe rather than part of the read path: it costs a subprocess, and
/// the answer is only interesting to `doctor`. It has to come from the CLI because
/// a config file records *how* to authenticate, never whether the credential is
/// present.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthProbe {
    pub argv: Vec<String>,
    pub parser: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpSection {
    /// Scopes this host can represent. A scope the host lacks renders blocked.
    pub scopes: Vec<ScopeKind>,
    /// What the host's `add` can express. Anything a server needs and this list
    /// lacks makes the row blocked rather than silently lossy.
    pub caps: Vec<Cap>,
    pub read: Vec<ReadSource>,
    pub add: McpAdd,
    pub remove: Invocation,

    /// How to authenticate a server interactively. Declared so that pushing an
    /// OAuth-backed server can tell the user the exact command, instead of
    /// reporting a config entry that cannot connect as done.
    #[serde(default)]
    pub login: Option<Invocation>,

    /// Optional; omit for a host with no machine-readable auth status.
    #[serde(default)]
    pub auth_status: Option<AuthProbe>,
}

impl McpSection {
    pub fn supports(&self, cap: Cap) -> bool {
        self.caps.contains(&cap)
    }
    pub fn supports_scope(&self, scope: ScopeKind) -> bool {
        self.scopes.contains(&scope)
    }
    /// Capabilities `needed` that this host cannot represent.
    pub fn missing_caps(&self, needed: &[Cap]) -> Vec<Cap> {
        needed
            .iter()
            .copied()
            .filter(|c| !self.supports(*c))
            .collect()
    }
}

/// Where a host reads its instructions ("system prompt") from, per scope.
///
/// Omitting a scope means the host has no equivalent — Codex has no counterpart
/// to `CLAUDE.local.md` — which the differ reports as blocked rather than
/// inventing a location.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstructionsSection {
    #[serde(default)]
    pub user: Option<String>,
    /// `{repo}` is substituted per repo.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub local: Option<String>,
}

impl InstructionsSection {
    pub fn path_for(&self, scope: &ScopeKind) -> Option<&String> {
        match scope {
            ScopeKind::User => self.user.as_ref(),
            ScopeKind::Project => self.project.as_ref(),
            ScopeKind::Local => self.local.as_ref(),
        }
    }
    pub fn scopes(&self) -> Vec<ScopeKind> {
        [ScopeKind::User, ScopeKind::Project, ScopeKind::Local]
            .into_iter()
            .filter(|s| self.path_for(s).is_some())
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillsSection {
    /// Directories this host reads skills from. **`dirs[0]` is the write
    /// target** — the rest are read-only, so a host that also reads a shared
    /// directory doesn't get a duplicate symlink.
    pub dirs: Vec<String>,
}

impl SkillsSection {
    pub fn link_dir(&self) -> Option<&String> {
        self.dirs.first()
    }
}

/// Where to find what a marketplace offers.
///
/// Necessary because neither CLI resolves a bare plugin name — an install must be
/// `<plugin>@<marketplace>` — so the marketplace has to be looked up rather than
/// guessed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogSource {
    /// Path with `*` wildcards, e.g.
    /// `~/.claude/plugins/marketplaces/*/.claude-plugin/marketplace.json`.
    pub glob: String,
    pub parser: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginsSection {
    pub read: Vec<ReadSource>,
    /// Marketplace manifests to read. In addition to these, the manifest of any
    /// directory-source marketplace this host has configured is read from
    /// `<dir>/.claude-plugin/marketplace.json`, which is how local marketplaces
    /// are found without hardcoding where a host caches them.
    #[serde(default)]
    pub catalog: Vec<CatalogSource>,
    pub install: Invocation,
    pub remove: Invocation,
    pub marketplace_add: Invocation,
    #[serde(default)]
    pub marketplace_remove: Option<Invocation>,
    /// Marketplaces the host always has without declaring them. These are never
    /// reported as missing and never removed.
    #[serde(default)]
    pub implicit_marketplaces: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Descriptors compiled into the binary. User files with the same `name`
/// replace these wholesale, which is the escape hatch for a host whose CLI
/// changed before we shipped an update.
pub const BUILTIN: &[(&str, &str)] = &[
    ("claude", include_str!("builtin/claude.toml")),
    ("codex", include_str!("builtin/codex.toml")),
];

pub fn parse(text: &str, origin: &str) -> Result<HostDescriptor> {
    let d: HostDescriptor =
        toml::from_str(text).with_context(|| format!("parsing host descriptor {origin}"))?;
    validate(&d).with_context(|| format!("validating host descriptor {origin}"))?;
    Ok(d)
}

fn validate(d: &HostDescriptor) -> Result<()> {
    if d.name.is_empty() {
        anyhow::bail!("`name` must not be empty");
    }
    if let Some(mcp) = &d.mcp {
        match mcp.add.style {
            AddStyle::Json => {
                if !mcp.add.argv.iter().any(|a| a.contains("{json}")) {
                    anyhow::bail!("mcp.add.style = \"json\" requires `{{json}}` in argv");
                }
                if mcp.add.json_serializer.is_none() {
                    anyhow::bail!("mcp.add.style = \"json\" requires `json_serializer`");
                }
            }
            AddStyle::Flags => {
                if mcp.add.argv_stdio.is_empty() && mcp.add.argv_http.is_empty() {
                    anyhow::bail!("mcp.add.style = \"flags\" requires argv_stdio and/or argv_http");
                }
                if mcp.supports(Cap::Env)
                    && (mcp.add.env_flag.is_none() || mcp.add.env_format.is_none())
                {
                    anyhow::bail!("declares the `env` capability but no env_flag/env_format");
                }
                if mcp.supports(Cap::Headers)
                    && (mcp.add.header_flag.is_none() || mcp.add.header_format.is_none())
                {
                    anyhow::bail!(
                        "declares the `headers` capability but no header_flag/header_format"
                    );
                }
                if mcp.supports(Cap::BearerEnv) && mcp.add.bearer_env_flag.is_none() {
                    anyhow::bail!("declares the `bearer_env` capability but no bearer_env_flag");
                }
            }
        }
        if mcp.scopes.is_empty() {
            anyhow::bail!("mcp.scopes must not be empty");
        }
    }
    if let Some(skills) = &d.skills
        && skills.dirs.is_empty()
    {
        anyhow::bail!("skills.dirs must not be empty");
    }
    Ok(())
}

/// Built-ins, then user files from `~/.config/agentsync/hosts/*.toml`
/// overriding by `name`.
pub fn load_all() -> Result<Vec<HostDescriptor>> {
    let mut by_name: BTreeMap<String, HostDescriptor> = BTreeMap::new();
    for (name, text) in BUILTIN {
        let d = parse(text, &format!("builtin:{name}"))?;
        by_name.insert(d.name.clone(), d);
    }

    let dir = crate::paths::hosts_dir();
    if dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let d = parse(&text, &path.display().to_string())?;
            by_name.insert(d.name.clone(), d);
        }
    }

    Ok(by_name.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse_and_validate() {
        for (name, text) in BUILTIN {
            parse(text, name).unwrap_or_else(|e| panic!("builtin {name} invalid: {e:#}"));
        }
    }

    #[test]
    fn claude_declares_all_three_scopes() {
        let d = parse(BUILTIN[0].1, "claude").unwrap();
        let mcp = d.mcp.unwrap();
        assert!(mcp.supports_scope(ScopeKind::User));
        assert!(mcp.supports_scope(ScopeKind::Local));
        assert!(mcp.supports_scope(ScopeKind::Project));
    }

    #[test]
    fn codex_cannot_represent_arbitrary_headers() {
        let d = parse(BUILTIN[1].1, "codex").unwrap();
        let mcp = d.mcp.unwrap();
        // `codex mcp add` has no --header flag. Encoding that here is what makes
        // the tool refuse to push such a server rather than drop the headers.
        assert!(!mcp.supports(Cap::Headers));
        assert_eq!(
            mcp.missing_caps(&[Cap::Http, Cap::Headers]),
            vec![Cap::Headers]
        );
    }

    #[test]
    fn a_flags_host_claiming_headers_without_a_flag_is_rejected() {
        let bad = r#"
name = "bogus"
display = "Bogus"
detect = { bin = "bogus" }
[mcp]
scopes = ["user"]
caps = ["stdio", "http", "headers"]
read = []
remove = { argv = ["mcp", "remove", "{name}"] }
[mcp.add]
style = "flags"
argv_http = ["mcp", "add", "{name}", "--url", "{url}"]
"#;
        let err = parse(bad, "bogus").unwrap_err();
        assert!(format!("{err:#}").contains("header_flag"), "{err:#}");
    }
}
