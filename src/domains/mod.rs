//! Domain glue: reads the world, produces rows, turns accepted rows into a plan.
//!
//! Each domain implements the same three-stage shape — read, diff into rows,
//! plan from a chosen action — so the TUI treats them identically and a fourth
//! domain is a new module rather than a UI change.

pub mod instructions;
pub mod mcp;
pub mod plugins;
pub mod skills;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::diff::{Domain, Row};
use crate::core::model::{HostSnapshot, Scope};
use crate::core::plan::Plan;
use crate::hosts::Host;
use crate::manifest::Manifest;
use crate::paths;

pub struct World {
    pub manifest: Manifest,
    pub manifest_path: PathBuf,
    pub hosts: Vec<Host>,
    /// Parallel to `hosts`.
    pub snapshots: Vec<HostSnapshot>,
    /// Repos whose per-repo configuration was consulted.
    pub repos: Vec<String>,
    /// Non-fatal problems from loading, surfaced by `doctor`.
    pub warnings: Vec<String>,
}

impl World {
    /// Read the manifest and every detected host.
    ///
    /// Repo discovery is two-pass because per-repo config paths are themselves
    /// derived from a repo list: pass one uses what the manifest and the current
    /// directory imply, pass two adds any repo a host turned out to mention.
    pub fn load(manifest_path: &Path, extra_repos: &[String]) -> Result<World> {
        let mut manifest = Manifest::load(manifest_path)?;
        let mut warnings = Vec::new();
        let hosts = Host::load_all()?;

        let mut repos: BTreeSet<String> = manifest
            .mcp
            .values()
            .flat_map(|e| e.repos.iter().cloned())
            .collect();
        repos.extend(extra_repos.iter().cloned());
        if let Some(cwd) = current_repo() {
            repos.insert(cwd);
        }

        // Per-repo manifests are additive and only ever describe their own repo.
        for repo in repos.clone() {
            let path = paths::project_manifest_path(Path::new(&repo));
            if !path.is_file() {
                continue;
            }
            let project = Manifest::load(&path)
                .with_context(|| format!("reading project manifest {}", path.display()))?;
            warnings.extend(merge_project(&mut manifest, project, &repo));
        }

        let mut ordered: Vec<String> = repos.iter().cloned().collect();
        let mut snapshots = read_all(&hosts, &ordered)?;

        let discovered: BTreeSet<String> = snapshots
            .iter()
            .flat_map(|s| s.mcp.keys())
            .filter_map(|(scope, _)| scope.repo().map(str::to_string))
            .collect();
        if !discovered.is_subset(&repos) {
            repos.extend(discovered);
            ordered = repos.iter().cloned().collect();
            snapshots = read_all(&hosts, &ordered)?;
        }

        for snap in &snapshots {
            warnings.extend(snap.warnings.iter().cloned());
        }

        Ok(World {
            manifest,
            manifest_path: manifest_path.to_path_buf(),
            hosts,
            snapshots,
            repos: ordered,
            warnings,
        })
    }

    pub fn manifest_dir(&self) -> PathBuf {
        self.manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Installed hosts, paired with what they contain. A host that is not
    /// installed is skipped everywhere: absent is not divergent.
    pub fn detected(&self) -> impl Iterator<Item = (&Host, &HostSnapshot)> {
        self.hosts
            .iter()
            .zip(self.snapshots.iter())
            .filter(|(h, _)| h.detected())
    }

    pub fn detected_snapshots(&self) -> impl Iterator<Item = &HostSnapshot> {
        self.detected().map(|(_, s)| s)
    }

    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name() == name)
    }

    pub fn snapshot(&self, name: &str) -> Option<&HostSnapshot> {
        self.detected()
            .find(|(h, _)| h.name() == name)
            .map(|(_, s)| s)
    }

    pub fn missing_hosts(&self) -> Vec<&Host> {
        self.hosts.iter().filter(|h| !h.detected()).collect()
    }

    /// Every row across every domain, in display order.
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for domain in Domain::ALL {
            out.extend(match domain {
                Domain::Mcp => mcp::rows(self),
                Domain::Skills => skills::rows(self),
                Domain::Instructions => instructions::rows(self),
                Domain::Plugins => plugins::rows(self),
            });
        }
        out
    }

    /// Turn accepted rows into an ordered plan.
    pub fn plan(&self, rows: &[Row]) -> Plan {
        let mut plan = Plan::default();
        for row in rows.iter().filter(|r| r.accepted && r.actionable()) {
            match row.domain {
                Domain::Mcp => mcp::plan_row(self, row, &mut plan),
                Domain::Skills => skills::plan_row(self, row, &mut plan),
                Domain::Instructions => instructions::plan_row(self, row, &mut plan),
                Domain::Plugins => plugins::plan_row(self, row, &mut plan),
            }
        }
        plan.finalize();
        plan
    }
}

fn read_all(hosts: &[Host], repos: &[String]) -> Result<Vec<HostSnapshot>> {
    hosts
        .iter()
        .map(|h| {
            if h.detected() {
                h.read(repos)
                    .with_context(|| format!("reading host {}", h.name()))
            } else {
                Ok(HostSnapshot {
                    host: h.name().to_string(),
                    display: h.descriptor.display.clone(),
                    detected: false,
                    ..Default::default()
                })
            }
        })
        .collect()
}

/// Fold a per-repo manifest into the user manifest, forcing its entries to
/// project scope for that repo. A name already claimed by the user manifest is
/// left alone and reported, rather than being silently shadowed.
fn merge_project(manifest: &mut Manifest, project: Manifest, repo: &str) -> Vec<String> {
    let mut warnings = Vec::new();

    for (name, mut entry) in project.mcp {
        if manifest.mcp.contains_key(&name) {
            warnings.push(format!(
                "{repo}/.agentsync.toml: mcp.{name} is already in the user manifest; \
                 the project entry was ignored"
            ));
            continue;
        }
        entry.scope = crate::core::model::ScopeKind::Project;
        entry.repos = vec![repo.to_string()];
        manifest.mcp.insert(name, entry);
    }
    for (name, entry) in project.instructions {
        manifest.instructions.entry(name).or_insert(entry);
    }
    for (name, entry) in project.skills {
        manifest.skills.entry(name).or_insert(entry);
    }
    for (name, entry) in project.plugins {
        manifest.plugins.entry(name).or_insert(entry);
    }
    for (name, entry) in project.marketplaces {
        manifest.marketplaces.entry(name).or_insert(entry);
    }
    if !project.hosts.is_empty() {
        warnings.push(format!(
            "{repo}/.agentsync.toml: [hosts.*] overrides are user-scope only and were ignored"
        ));
    }
    warnings
}

/// The current directory, if it looks like a project root.
fn current_repo() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let looks_like_a_repo = cwd.join(".git").exists()
        || cwd.join(".agentsync.toml").is_file()
        || cwd.join(".mcp.json").is_file();
    looks_like_a_repo.then(|| cwd.to_string_lossy().to_string())
}

/// Repos referenced anywhere, for display.
pub fn repos_in_use(world: &World) -> Vec<String> {
    let mut set: BTreeSet<String> = world
        .manifest
        .mcp
        .values()
        .flat_map(|e| e.repos.iter().cloned())
        .collect();
    for snap in world.detected_snapshots() {
        for (scope, _) in snap.mcp.keys() {
            if let Scope::Local(p) | Scope::Project(p) = scope {
                set.insert(p.clone());
            }
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::McpEntry;
    use std::collections::BTreeMap;

    fn entry(command: &str) -> McpEntry {
        McpEntry {
            transport: "stdio".into(),
            command: Some(command.into()),
            args: vec![],
            env: BTreeMap::new(),
            env_from: vec![],
            url: None,
            headers: BTreeMap::new(),
            bearer_token_env: None,
            scope: crate::core::model::ScopeKind::User,
            repos: vec![],
            hosts: None,
        }
    }

    #[test]
    fn project_entries_are_forced_to_project_scope_for_their_repo() {
        let mut user = Manifest::default();
        let mut project = Manifest::default();
        project.mcp.insert("local-thing".into(), entry("thing"));

        let warnings = merge_project(&mut user, project, "/repos/x");
        assert!(warnings.is_empty());
        let merged = &user.mcp["local-thing"];
        assert_eq!(merged.scope, crate::core::model::ScopeKind::Project);
        assert_eq!(merged.repos, vec!["/repos/x"]);
    }

    #[test]
    fn a_project_entry_never_shadows_the_user_manifest_silently() {
        let mut user = Manifest::default();
        user.mcp.insert("shared".into(), entry("user-version"));
        let mut project = Manifest::default();
        project
            .mcp
            .insert("shared".into(), entry("project-version"));

        let warnings = merge_project(&mut user, project, "/repos/x");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("already in the user manifest"));
        assert_eq!(user.mcp["shared"].command.as_deref(), Some("user-version"));
    }
}
