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

use crate::core::model::{Cap, HookCap, HookOutputField, HookOutputStrategy, ScopeKind};

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
    #[serde(default)]
    pub hooks: Option<HooksSection>,
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
    /// One argv containing `{json}`. The whole definition goes in as a document.
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

    /// Optional. Omit it for a host with no machine-readable auth status.
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
/// to `CLAUDE.local.md`. The differ reports this as blocked, rather than
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
    /// directory does not get a duplicate symlink.
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
    /// Path with `*` wildcards, for example
    /// `~/.claude/plugins/marketplaces/*/.claude-plugin/marketplace.json`.
    pub glob: String,
    pub parser: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginsSection {
    pub read: Vec<ReadSource>,
    /// Marketplace manifests to read. In addition to these, agentsync reads the
    /// manifest of any directory-source marketplace this host has configured,
    /// from `<dir>/.claude-plugin/marketplace.json`. This is how local
    /// marketplaces are found without hardcoding where a host caches them.
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

/// Where a host reads hook definitions from. Exactly one of `file` or `glob`.
///
/// A separate type from [`ReadSource`] because plugin hooks live behind a
/// wildcard path and MCP sources never do.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookSource {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    pub parser: String,
}

/// Where generated shim plugins are written for this host.
///
/// Absent means the host can be a *source* of hooks but never a shim target.
/// Incompatibilities aimed at it are reported as blocked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HooksShim {
    /// Directory agentsync owns and registers as a local marketplace.
    pub marketplace: String,
    /// The stdout contract the generated shim must emit for this target.
    #[serde(default)]
    pub output_strategy: HookOutputStrategy,
}

/// What a host's hook engine can represent.
///
/// `caps` and `output` are two vocabularies on purpose: `caps` is what the host
/// understands in the manifest, `output` is what it accepts back on stdout.
/// These are *defaults*. The user manifest may override them per host, so a
/// user on a newer host release is not blocked waiting for a release here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HooksSection {
    pub events: Vec<String>,
    pub caps: Vec<HookCap>,
    pub output: Vec<HookOutputField>,
    #[serde(default)]
    pub read: Vec<HookSource>,
    #[serde(default)]
    pub shim: Option<HooksShim>,
}

impl HooksSection {
    pub fn supports(&self, cap: HookCap) -> bool {
        self.caps.contains(&cap)
    }
    pub fn supports_event(&self, event: &str) -> bool {
        self.events.iter().any(|e| e == event)
    }
    pub fn accepts_output(&self, field: HookOutputField) -> bool {
        self.output.contains(&field)
    }
    /// Capabilities `needed` that this host cannot represent.
    pub fn missing_caps(&self, needed: &[HookCap]) -> Vec<HookCap> {
        needed
            .iter()
            .copied()
            .filter(|c| !self.supports(*c))
            .collect()
    }
    pub fn can_shim(&self) -> bool {
        self.shim.is_some()
    }
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
    ("opencode", include_str!("builtin/opencode.toml")),
    ("kilo", include_str!("builtin/kilo.toml")),
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
                    anyhow::bail!(
                        "mcp.add.style = \"flags\" requires argv_stdio, argv_http, or both"
                    );
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
    if let Some(hooks) = &d.hooks {
        for (i, source) in hooks.read.iter().enumerate() {
            if source.file.is_some() == source.glob.is_some() {
                anyhow::bail!("hooks.read[{i}] must name exactly one of `file` or `glob`");
            }
        }
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

    fn builtin(name: &str) -> HostDescriptor {
        let (_, text) = BUILTIN
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("no builtin named {name}"));
        parse(text, name).unwrap()
    }

    #[test]
    fn opencode_and_kilo_are_detected_as_separate_hosts() {
        let opencode = builtin("opencode");
        let kilo = builtin("kilo");

        assert_eq!(opencode.detect.bin, "opencode");
        assert_eq!(kilo.detect.bin, "kilo");
        assert_ne!(opencode.name, kilo.name);
        assert_ne!(opencode.display, kilo.display);
    }

    #[test]
    fn opencode_and_kilo_never_read_each_others_paths() {
        let opencode = builtin("opencode");
        let kilo = builtin("kilo");

        let opencode_paths: Vec<String> = opencode
            .skills
            .as_ref()
            .unwrap()
            .dirs
            .iter()
            .cloned()
            .chain(opencode.instructions.as_ref().unwrap().user.clone())
            .collect();
        let kilo_paths: Vec<String> = kilo
            .skills
            .as_ref()
            .unwrap()
            .dirs
            .iter()
            .cloned()
            .chain(kilo.instructions.as_ref().unwrap().user.clone())
            .collect();

        for path in &opencode_paths {
            assert!(
                !path.contains("kilo"),
                "an OpenCode path must never reference Kilo: {path}"
            );
        }
        for path in &kilo_paths {
            assert!(
                !path.contains("opencode"),
                "a Kilo path must never reference OpenCode: {path}"
            );
        }
    }

    #[test]
    fn the_opencode_family_shares_one_skill_write_target_with_codex() {
        let shared = "~/.agents/skills";
        for name in ["codex", "opencode", "kilo"] {
            let host = builtin(name);
            assert_eq!(
                host.skills.as_ref().unwrap().link_dir().unwrap(),
                shared,
                "{name} must write skills to the shared directory so a synced \
                 skill produces one filesystem operation, not three"
            );
        }
    }

    #[test]
    fn the_opencode_family_is_xdg_rooted_not_home_rooted() {
        for name in ["opencode", "kilo"] {
            let host = builtin(name);
            let native: Vec<&String> = host
                .skills
                .as_ref()
                .unwrap()
                .dirs
                .iter()
                .filter(|dir| dir.contains(name))
                .collect();
            assert!(
                !native.is_empty(),
                "{name} must read its own native skill directory"
            );
            for dir in native {
                assert!(
                    dir.starts_with("{xdg_config}/"),
                    "{name} native path {dir} must resolve through XDG, or the \
                     live gate silently reads the caller's real config"
                );
            }
            let user = host.instructions.as_ref().unwrap().user.clone().unwrap();
            assert!(user.starts_with("{xdg_config}/"), "{name}: {user}");
        }
    }

    #[test]
    fn the_opencode_family_blocks_the_local_instruction_scope() {
        for name in ["opencode", "kilo"] {
            let host = builtin(name);
            let instructions = host.instructions.as_ref().unwrap();
            assert!(
                instructions.local.is_none(),
                "{name} has no CLAUDE.local.md counterpart, so the local scope \
                 must be blocked rather than given an invented location"
            );
            assert_eq!(
                instructions.scopes(),
                vec![ScopeKind::User, ScopeKind::Project]
            );
            assert_eq!(instructions.project.as_deref(), Some("{repo}/AGENTS.md"));
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

    #[test]
    fn codex_declares_hook_caps_that_exclude_if() {
        let d = parse(include_str!("builtin/codex.toml"), "builtin/codex.toml").unwrap();
        let hooks = d.hooks.expect("codex declares a hooks section");
        assert!(hooks.supports(HookCap::Matcher));
        assert!(hooks.supports(HookCap::AsyncRewake));
        assert!(!hooks.supports(HookCap::If));
        assert!(!hooks.accepts_output(HookOutputField::RewakeSummary));
        assert!(hooks.can_shim());
    }

    #[test]
    fn claude_supports_events_codex_cannot_express() {
        let c = parse(include_str!("builtin/claude.toml"), "claude").unwrap();
        let x = parse(include_str!("builtin/codex.toml"), "codex").unwrap();
        let claude = c.hooks.unwrap();
        let codex = x.hooks.unwrap();
        assert!(claude.supports_event("PreCompact"));
        assert!(!codex.supports_event("PreCompact"));
    }

    #[test]
    fn missing_caps_names_exactly_what_the_target_lacks() {
        let d = parse(include_str!("builtin/codex.toml"), "codex").unwrap();
        let hooks = d.hooks.unwrap();
        assert_eq!(
            hooks.missing_caps(&[HookCap::Matcher, HookCap::If, HookCap::RewakeSummary]),
            vec![HookCap::If, HookCap::RewakeSummary]
        );
    }

    #[test]
    fn a_hook_source_must_name_exactly_one_of_file_or_glob() {
        let text = r#"
name = "x"
display = "X"
detect = { bin = "x" }

[hooks]
events = ["Stop"]
caps = []
output = []

[[hooks.read]]
parser = "claude_hooks_json_v1"
"#;
        let err = format!("{:#}", parse(text, "x").unwrap_err());
        assert!(err.contains("hooks.read"), "unexpected error: {err}");
    }
}
