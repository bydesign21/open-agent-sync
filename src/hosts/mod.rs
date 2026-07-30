//! The host layer: descriptor + detection + read path + argv construction.
//!
//! Writes always go through the host's own CLI. We parse host config files but
//! never rewrite them, because those files hold state that is none of our
//! business (Codex's `[projects.*]` trust levels, notice flags, model
//! preferences) and a generator that owns the whole file destroys it.

pub mod descriptor;
pub mod parsers;
pub mod runner;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::model::{
    HostSnapshot, MarketplaceSource, McpServer, Scope, SkillState, Transport,
};
use crate::paths;

use descriptor::{AddStyle, HostDescriptor, ReadSource};
use parsers::ParseCtx;

pub struct Host {
    pub descriptor: HostDescriptor,
    /// Resolved binary, or `None` when the host isn't installed.
    pub bin: Option<PathBuf>,
}

impl Host {
    pub fn new(descriptor: HostDescriptor) -> Self {
        let bin = which::which(&descriptor.detect.bin).ok();
        Host { descriptor, bin }
    }

    pub fn name(&self) -> &str {
        &self.descriptor.name
    }

    pub fn detected(&self) -> bool {
        self.bin.is_some()
    }

    /// Load built-in and user descriptors and probe for each binary.
    pub fn load_all() -> Result<Vec<Host>> {
        Ok(descriptor::load_all()?.into_iter().map(Host::new).collect())
    }

    // -----------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------

    /// Read everything this host currently has. `repos` supplies the paths for
    /// sources whose descriptor path contains `{repo}`.
    pub fn read(&self, repos: &[String]) -> Result<HostSnapshot> {
        let mut snap = HostSnapshot {
            host: self.descriptor.name.clone(),
            display: self.descriptor.display.clone(),
            detected: self.detected(),
            ..Default::default()
        };

        if let Some(mcp) = &self.descriptor.mcp {
            for source in &mcp.read {
                for (path, ctx) in self.expand_source(source, repos) {
                    let Some(text) = read_if_present(&path)? else {
                        continue;
                    };
                    let read = parsers::read_mcp(&source.parser, &text, &ctx)
                        .with_context(|| format!("reading {}", path.display()))?;
                    snap.warnings.extend(read.warnings);
                    for (scope, server) in read.servers {
                        if let Some(want) = source.scope
                            && scope.kind() != want
                        {
                            continue;
                        }
                        snap.mcp.insert((scope, server.name.clone()), server);
                    }
                }
            }
        }

        if let Some(plugins) = &self.descriptor.plugins {
            for source in &plugins.read {
                for (path, ctx) in self.expand_source(source, repos) {
                    let Some(text) = read_if_present(&path)? else {
                        continue;
                    };
                    let read = parsers::read_plugins(&source.parser, &text, &ctx)
                        .with_context(|| format!("reading {}", path.display()))?;
                    snap.warnings.extend(read.warnings);
                    snap.plugins.extend(read.plugins);
                    snap.marketplaces.extend(read.marketplaces);
                }
            }

            // What each marketplace offers. Read *after* the marketplaces
            // themselves, because a directory-source marketplace tells us where
            // its own manifest is and so needs no hardcoded cache location.
            for source in &plugins.catalog {
                for path in expand_glob(&source.glob) {
                    let Some(text) = read_if_present(&path)? else {
                        continue;
                    };
                    let ctx = ParseCtx {
                        repo: None,
                        origin: path.clone(),
                    };
                    match parsers::read_catalog(&source.parser, &text, &ctx) {
                        Ok(read) => {
                            snap.catalog
                                .entry(read.marketplace)
                                .or_default()
                                .extend(read.plugins);
                        }
                        Err(e) => snap
                            .warnings
                            .push(format!("catalog {}: {e:#}", path.display())),
                    }
                }
            }

            for (name, source) in snap.marketplaces.clone() {
                let MarketplaceSource::Directory(dir) = source else {
                    continue;
                };
                let path = paths::expand(&dir).join(".claude-plugin/marketplace.json");
                let Some(text) = read_if_present(&path)? else {
                    continue;
                };
                let ctx = ParseCtx {
                    repo: None,
                    origin: path.clone(),
                };
                match parsers::read_catalog("marketplace_manifest_v1", &text, &ctx) {
                    // Trust the configured name over the manifest's own, since
                    // that is the name the CLI will accept.
                    Ok(read) => snap.catalog.entry(name).or_default().extend(read.plugins),
                    Err(e) => snap
                        .warnings
                        .push(format!("catalog {}: {e:#}", path.display())),
                }
            }
        }

        if let Some(skills) = &self.descriptor.skills {
            let canonical = paths::skills_dir();
            let mut states: BTreeMap<String, SkillState> = BTreeMap::new();
            let mut plugin_dirs = Vec::new();

            for (index, dir) in skills.dirs.iter().enumerate() {
                let dir = paths::expand(dir);
                let scanned = scan_skills_dir(&dir, &canonical, &mut plugin_dirs)?;
                for (name, state) in scanned {
                    // dirs[0] is the write target and therefore authoritative.
                    // A skill seen only in a later, read-only directory is
                    // reported as foreign because we cannot manage it there.
                    if index == 0 {
                        states.insert(name, state);
                    } else {
                        states.entry(name).or_insert(match state {
                            SkillState::Absent => SkillState::Absent,
                            _ => SkillState::Foreign(dir.clone()),
                        });
                    }
                }
            }
            snap.skills = states;
            snap.plugin_skills = plugin_dirs;
        }

        Ok(snap)
    }

    /// Expand one read source into concrete paths. A `{repo}` path yields one
    /// entry per repo; everything else yields exactly one.
    fn expand_source(&self, source: &ReadSource, repos: &[String]) -> Vec<(PathBuf, ParseCtx)> {
        if source.file.contains("{repo}") {
            repos
                .iter()
                .map(|repo| {
                    let path = paths::expand(&source.file.replace("{repo}", repo));
                    let ctx = ParseCtx {
                        repo: Some(repo.clone()),
                        origin: path.clone(),
                    };
                    (path, ctx)
                })
                .collect()
        } else {
            let path = paths::expand(&source.file);
            let ctx = ParseCtx {
                repo: None,
                origin: path.clone(),
            };
            vec![(path, ctx)]
        }
    }

    /// Directory this host's skill symlinks are written into.
    pub fn skills_link_dir(&self) -> Option<PathBuf> {
        self.descriptor
            .skills
            .as_ref()
            .and_then(|s| s.link_dir())
            .map(|d| paths::expand(d))
    }

    // -----------------------------------------------------------------
    // Write path: argv construction
    // -----------------------------------------------------------------

    /// Build the argv that adds `server` at `scope`.
    ///
    /// Capability gating happens in the differ, not here — by the time we build
    /// argv the row has already been judged pushable.
    pub fn mcp_add_argv(&self, server: &McpServer, scope: &Scope) -> Result<Vec<String>> {
        let mcp = self
            .descriptor
            .mcp
            .as_ref()
            .with_context(|| format!("host {} declares no mcp section", self.name()))?;
        let add = &mcp.add;

        let mut scalars = BTreeMap::from([
            ("name".to_string(), server.name.clone()),
            ("scope".to_string(), scope.cli_name().to_string()),
        ]);
        let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();

        match add.style {
            AddStyle::Json => {
                let serializer = add
                    .json_serializer
                    .as_deref()
                    .context("json add style without json_serializer")?;
                scalars.insert(
                    "json".to_string(),
                    parsers::serialize_mcp(serializer, server)?,
                );
                runner::render(&add.argv, &scalars, &lists)
            }
            AddStyle::Flags => match &server.transport {
                Transport::Stdio(s) => {
                    scalars.insert("command".to_string(), s.command.clone());
                    lists.insert("args".to_string(), s.args.clone());

                    let mut env_flags = Vec::new();
                    if let (Some(flag), Some(format)) = (&add.env_flag, &add.env_format) {
                        for (k, v) in &s.env {
                            env_flags.push(flag.clone());
                            env_flags.push(format.replace("{key}", k).replace("{value}", v));
                        }
                    }
                    lists.insert("env_flags".to_string(), env_flags);
                    runner::render(&add.argv_stdio, &scalars, &lists)
                }
                Transport::Http(h) => {
                    scalars.insert("url".to_string(), h.url.clone());

                    let mut bearer_flags = Vec::new();
                    if let (Some(flag), Some(var)) = (&add.bearer_env_flag, &h.bearer_token_env) {
                        bearer_flags.push(flag.clone());
                        bearer_flags.push(var.clone());
                    }
                    lists.insert("bearer_flags".to_string(), bearer_flags);

                    let mut header_flags = Vec::new();
                    if let (Some(flag), Some(format)) = (&add.header_flag, &add.header_format) {
                        for (k, v) in &h.headers {
                            header_flags.push(flag.clone());
                            header_flags.push(format.replace("{key}", k).replace("{value}", v));
                        }
                    }
                    lists.insert("header_flags".to_string(), header_flags);
                    runner::render(&add.argv_http, &scalars, &lists)
                }
            },
        }
    }

    pub fn mcp_remove_argv(&self, name: &str, scope: &Scope) -> Result<Vec<String>> {
        let mcp = self
            .descriptor
            .mcp
            .as_ref()
            .with_context(|| format!("host {} declares no mcp section", self.name()))?;
        let scalars = BTreeMap::from([
            ("name".to_string(), name.to_string()),
            ("scope".to_string(), scope.cli_name().to_string()),
        ]);
        runner::render(&mcp.remove.argv, &scalars, &BTreeMap::new())
    }

    pub fn plugin_install_argv(
        &self,
        name: &str,
        marketplace: Option<&str>,
    ) -> Result<Vec<String>> {
        let plugins = self
            .descriptor
            .plugins
            .as_ref()
            .with_context(|| format!("host {} declares no plugins section", self.name()))?;
        let id = match marketplace {
            Some(m) if !m.is_empty() => format!("{name}@{m}"),
            _ => name.to_string(),
        };
        let scalars = BTreeMap::from([
            ("id".to_string(), id),
            ("name".to_string(), name.to_string()),
        ]);
        runner::render(&plugins.install.argv, &scalars, &BTreeMap::new())
    }

    pub fn plugin_remove_argv(&self, name: &str, marketplace: Option<&str>) -> Result<Vec<String>> {
        let plugins = self
            .descriptor
            .plugins
            .as_ref()
            .with_context(|| format!("host {} declares no plugins section", self.name()))?;
        let id = match marketplace {
            Some(m) if !m.is_empty() => format!("{name}@{m}"),
            _ => name.to_string(),
        };
        let scalars = BTreeMap::from([
            ("id".to_string(), id),
            ("name".to_string(), name.to_string()),
        ]);
        runner::render(&plugins.remove.argv, &scalars, &BTreeMap::new())
    }

    pub fn marketplace_add_argv(&self, name: &str, source: &str) -> Result<Vec<String>> {
        let plugins = self
            .descriptor
            .plugins
            .as_ref()
            .with_context(|| format!("host {} declares no plugins section", self.name()))?;
        let scalars = BTreeMap::from([
            ("name".to_string(), name.to_string()),
            ("source".to_string(), source.to_string()),
        ]);
        runner::render(&plugins.marketplace_add.argv, &scalars, &BTreeMap::new())
    }

    pub fn marketplace_remove_argv(&self, name: &str) -> Result<Option<Vec<String>>> {
        let Some(plugins) = &self.descriptor.plugins else {
            return Ok(None);
        };
        let Some(inv) = &plugins.marketplace_remove else {
            return Ok(None);
        };
        let scalars = BTreeMap::from([("name".to_string(), name.to_string())]);
        Ok(Some(runner::render(&inv.argv, &scalars, &BTreeMap::new())?))
    }

    /// Marketplaces this host always has, which must never be reported missing.
    pub fn implicit_marketplaces(&self) -> &[String] {
        self.descriptor
            .plugins
            .as_ref()
            .map(|p| p.implicit_marketplaces.as_slice())
            .unwrap_or(&[])
    }
}

/// Expand a `*`-containing path. A pattern that matches nothing yields nothing,
/// which is normal — a host may simply have no marketplaces cached yet.
fn expand_glob(pattern: &str) -> Vec<PathBuf> {
    let expanded = paths::expand(pattern);
    let text = expanded.to_string_lossy();
    if !text.contains('*') {
        return vec![expanded];
    }
    match glob::glob(&text) {
        Ok(paths) => paths.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn read_if_present(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    ))
}

/// Classify every entry in one skills directory.
///
/// Skipped: dotfiles (`.DS_Store`, `.skill-lock.json`), plain files, and
/// directories with no `SKILL.md`. A directory carrying a plugin manifest is
/// recorded in `plugin_dirs` and excluded — `claude plugin init` scaffolds
/// plugins into the skills directory, and those belong to the plugin manager.
fn scan_skills_dir(
    dir: &Path,
    canonical: &Path,
    plugin_dirs: &mut Vec<String>,
) -> Result<BTreeMap<String, SkillState>> {
    let mut out = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }

    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let meta = entry.metadata()?;

        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            let resolved = if target.is_absolute() {
                target.clone()
            } else {
                dir.join(&target)
            };
            let expected = canonical.join(&name);
            let same = std::fs::canonicalize(&resolved)
                .ok()
                .zip(std::fs::canonicalize(&expected).ok())
                .map(|(a, b)| a == b)
                .unwrap_or(false);
            out.insert(
                name,
                if same {
                    SkillState::Linked
                } else {
                    SkillState::Foreign(resolved)
                },
            );
            continue;
        }

        if !meta.is_dir() {
            continue;
        }
        if path.join(".claude-plugin/plugin.json").exists()
            || path.join(".codex-plugin/plugin.json").exists()
        {
            plugin_dirs.push(name);
            continue;
        }
        if !path.join("SKILL.md").exists() {
            continue;
        }
        out.insert(name, SkillState::RealDir);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{HttpServer, StdioServer};
    use pretty_assertions::assert_eq;

    fn host(name: &str) -> Host {
        let text = descriptor::BUILTIN
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap()
            .1;
        Host {
            descriptor: descriptor::parse(text, name).unwrap(),
            bin: Some(PathBuf::from(name)),
        }
    }

    #[test]
    fn claude_add_argv_carries_a_json_document_and_scope() {
        let h = host("claude");
        let server = McpServer {
            name: "kicad".into(),
            transport: Transport::Stdio(StdioServer {
                command: "node".into(),
                args: vec!["/x/index.js".into()],
                ..Default::default()
            }),
        };
        let argv = h.mcp_add_argv(&server, &Scope::User).unwrap();
        assert_eq!(argv[0], "mcp");
        assert_eq!(argv[1], "add-json");
        assert_eq!(argv[2], "kicad");
        assert!(argv[3].contains("\"command\":\"node\""));
        assert_eq!(argv[argv.len() - 2], "--scope");
        assert_eq!(argv[argv.len() - 1], "user");
    }

    #[test]
    fn codex_stdio_add_splices_env_flags_and_args_around_the_separator() {
        let h = host("codex");
        let server = McpServer {
            name: "kicad".into(),
            transport: Transport::Stdio(StdioServer {
                command: "node".into(),
                args: vec!["/x/index.js".into(), "--flag".into()],
                env: BTreeMap::from([("LOG_LEVEL".to_string(), "info".to_string())]),
                env_from: vec![],
            }),
        };
        let argv = h.mcp_add_argv(&server, &Scope::User).unwrap();
        assert_eq!(
            argv,
            vec![
                "mcp",
                "add",
                "kicad",
                "--env",
                "LOG_LEVEL=info",
                "--",
                "node",
                "/x/index.js",
                "--flag",
            ]
        );
    }

    #[test]
    fn codex_http_add_omits_the_bearer_flag_when_unused() {
        let h = host("codex");
        let server = McpServer {
            name: "rovo".into(),
            transport: Transport::Http(HttpServer {
                url: "https://mcp.atlassian.com/v1/mcp".into(),
                headers: BTreeMap::new(),
                bearer_token_env: None,
            }),
        };
        let argv = h.mcp_add_argv(&server, &Scope::User).unwrap();
        assert_eq!(
            argv,
            vec![
                "mcp",
                "add",
                "rovo",
                "--url",
                "https://mcp.atlassian.com/v1/mcp"
            ]
        );
    }

    #[test]
    fn codex_http_add_includes_the_bearer_flag_when_present() {
        let h = host("codex");
        let server = McpServer {
            name: "k".into(),
            transport: Transport::Http(HttpServer {
                url: "https://a.test/mcp".into(),
                headers: BTreeMap::new(),
                bearer_token_env: Some("TOK".into()),
            }),
        };
        let argv = h.mcp_add_argv(&server, &Scope::User).unwrap();
        assert_eq!(argv[argv.len() - 2], "--bearer-token-env-var");
        assert_eq!(argv[argv.len() - 1], "TOK");
    }

    #[test]
    fn plugin_ids_are_reassembled_with_the_marketplace() {
        let h = host("codex");
        let argv = h
            .plugin_install_argv("superpowers", Some("openai-api-curated"))
            .unwrap();
        assert_eq!(
            argv,
            vec!["plugin", "add", "superpowers@openai-api-curated"]
        );

        let bare = h.plugin_install_argv("local-thing", None).unwrap();
        assert_eq!(bare, vec!["plugin", "add", "local-thing"]);
    }

    #[test]
    fn scan_distinguishes_linked_real_and_foreign_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = tmp.path().join("canonical");
        let hostdir = tmp.path().join("hostdir");
        std::fs::create_dir_all(canonical.join("mine")).unwrap();
        std::fs::write(canonical.join("mine/SKILL.md"), "---\nname: mine\n---\n").unwrap();
        std::fs::create_dir_all(&hostdir).unwrap();

        // linked
        crate::platform::symlink(&canonical.join("mine"), &hostdir.join("mine")).unwrap();
        // foreign symlink
        let elsewhere = tmp.path().join("elsewhere/theirs");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("SKILL.md"), "x").unwrap();
        crate::platform::symlink(&elsewhere, &hostdir.join("theirs")).unwrap();
        // real directory
        std::fs::create_dir_all(hostdir.join("local")).unwrap();
        std::fs::write(hostdir.join("local/SKILL.md"), "x").unwrap();
        // noise that must be ignored
        std::fs::write(hostdir.join(".DS_Store"), "x").unwrap();
        std::fs::write(hostdir.join(".skill-lock.json"), "{}").unwrap();
        std::fs::create_dir_all(hostdir.join("not-a-skill")).unwrap();
        // a plugin scaffolded into the skills dir
        std::fs::create_dir_all(hostdir.join("aplugin/.claude-plugin")).unwrap();
        std::fs::write(hostdir.join("aplugin/.claude-plugin/plugin.json"), "{}").unwrap();

        let mut plugin_dirs = Vec::new();
        let states = scan_skills_dir(&hostdir, &canonical, &mut plugin_dirs).unwrap();

        assert_eq!(states.get("mine"), Some(&SkillState::Linked));
        assert!(matches!(states.get("theirs"), Some(SkillState::Foreign(_))));
        assert_eq!(states.get("local"), Some(&SkillState::RealDir));
        assert!(!states.contains_key("not-a-skill"));
        assert!(!states.contains_key(".DS_Store"));
        assert_eq!(plugin_dirs, vec!["aplugin"]);
        assert!(!states.contains_key("aplugin"));
    }
}
