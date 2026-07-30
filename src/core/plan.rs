//! Plan types: the concrete, reviewable steps an accepted set of rows becomes.
//!
//! The plan is a first-class artifact, not an implementation detail. It is shown
//! before anything runs and can be exported as a shell script, which is what
//! makes the tool auditable: you can always see, and run yourself, exactly what
//! it intended to do.

use std::path::PathBuf;

use crate::manifest::{MarketplaceEntry, McpEntry};

#[derive(Clone, Debug)]
pub enum ManifestOp {
    UpsertMcp {
        name: String,
        entry: Box<McpEntry>,
    },
    RemoveMcp(String),
    SetMcpHosts {
        name: String,
        hosts: Option<Vec<String>>,
    },
    SetMcpBearerEnv {
        name: String,
        var: String,
    },
    UpsertSkill {
        name: String,
        source: String,
    },
    RemoveSkill(String),
    SetSkillHosts {
        name: String,
        hosts: Option<Vec<String>>,
    },
    UpsertInstruction {
        name: String,
        source: String,
        scope: crate::core::model::ScopeKind,
        repos: Vec<String>,
    },
    RemoveInstruction(String),
    SetInstructionHosts {
        name: String,
        hosts: Option<Vec<String>>,
    },
    UpsertPlugin {
        name: String,
        marketplace: Option<String>,
    },
    RemovePlugin(String),
    SetPluginHosts {
        name: String,
        hosts: Option<Vec<String>>,
    },
    SetMarketplaceHosts {
        name: String,
        hosts: Option<Vec<String>>,
    },
    UpsertMarketplace {
        name: String,
        entry: Box<MarketplaceEntry>,
    },
    RemoveMarketplace(String),
}

impl ManifestOp {
    /// The manifest change in the manifest's own syntax.
    ///
    /// Both the TUI and `agentsync plan` render this, so the plan you approve
    /// interactively and the plan you read in a terminal describe the edit the
    /// same way.
    pub fn describe(&self) -> String {
        fn hosts(h: &Option<Vec<String>>) -> String {
            h.clone().unwrap_or_default().join(", ")
        }
        match self {
            ManifestOp::UpsertMcp { name, entry } => match entry.hosts {
                Some(_) => format!("set mcp.{name} (hosts = [{}])", hosts(&entry.hosts)),
                None => format!("set mcp.{name}"),
            },
            ManifestOp::RemoveMcp(name) => format!("remove mcp.{name}"),
            ManifestOp::SetMcpHosts { name, hosts: h } => {
                format!("set mcp.{name}.hosts = [{}]", hosts(h))
            }
            ManifestOp::SetMcpBearerEnv { name, var } => {
                format!("set mcp.{name}.bearer_token_env = \"{var}\"")
            }
            ManifestOp::UpsertSkill { name, source } => {
                format!("set skills.{name}.source = \"{source}\"")
            }
            ManifestOp::RemoveSkill(name) => format!("remove skills.{name}"),
            ManifestOp::SetSkillHosts { name, hosts: h } => {
                format!("set skills.{name}.hosts = [{}]", hosts(h))
            }
            ManifestOp::UpsertInstruction { name, source, .. } => {
                format!("set instructions.{name}.source = \"{source}\"")
            }
            ManifestOp::RemoveInstruction(name) => format!("remove instructions.{name}"),
            ManifestOp::SetInstructionHosts { name, hosts: h } => {
                format!("set instructions.{name}.hosts = [{}]", hosts(h))
            }
            ManifestOp::UpsertPlugin { name, marketplace } => match marketplace {
                Some(m) => format!("set plugins.{name}.marketplace = \"{m}\""),
                None => format!("set plugins.{name}"),
            },
            ManifestOp::RemovePlugin(name) => format!("remove plugins.{name}"),
            ManifestOp::SetPluginHosts { name, hosts: h } => {
                format!("set plugins.{name}.hosts = [{}]", hosts(h))
            }
            ManifestOp::UpsertMarketplace { name, entry } => match entry.source() {
                Some(source) => format!("set marketplaces.{name} = {source}"),
                None => format!("set marketplaces.{name}"),
            },
            ManifestOp::RemoveMarketplace(name) => format!("remove marketplaces.{name}"),
            ManifestOp::SetMarketplaceHosts { name, hosts: h } => {
                format!("set marketplaces.{name}.hosts = [{}]", hosts(h))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum FsOp {
    /// Create (or replace) `link` pointing at `target`. Replacing anything we
    /// did not create is preceded by a backup.
    Link {
        target: PathBuf,
        link: PathBuf,
    },
    Unlink(PathBuf),
    /// Move a host-owned skill directory into canonical storage.
    MoveIntoCanonical {
        from: PathBuf,
        to: PathBuf,
    },
    /// Delete canonical content. Only reachable via an explicit purge.
    RemoveTree(PathBuf),
}

#[derive(Clone, Debug)]
pub enum Step {
    Manifest(ManifestOp),
    /// Invoke a host CLI. `cwd` matters for repo-scoped operations, since both
    /// CLIs infer the project from the working directory.
    Host {
        host: String,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
    },
    Fs(FsOp),
    /// Something the user must do themselves, e.g. export an env var. Carried in
    /// the plan so it is impossible to miss, and reported as skipped rather than
    /// silently succeeding.
    Manual(String),
}

impl Step {
    /// Ordering class. Marketplaces must exist before plugins are installed
    /// from them, and removals must precede adds so a promote/demote never has
    /// the same name at two scopes at once.
    fn order(&self) -> u8 {
        match self {
            Step::Manifest(ManifestOp::UpsertMarketplace { .. }) => 0,
            Step::Host { argv, .. } if argv.iter().any(|a| a == "marketplace") => 1,
            Step::Host { argv, .. } if argv.iter().any(|a| a == "remove" || a == "uninstall") => 2,
            Step::Fs(FsOp::Unlink(_)) => 2,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlannedStep {
    /// Short human label, e.g. `add kicad to codex`.
    pub label: String,
    pub step: Step,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub steps: Vec<PlannedStep>,
    /// Things the user should know that are not steps: capabilities that forced
    /// a host to be skipped, coverage that was deliberately bounded. Never
    /// silent.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn push(&mut self, label: impl Into<String>, step: Step) {
        self.steps.push(PlannedStep {
            label: label.into(),
            step,
        });
    }

    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Stable-sort into dependency order. Stable so that steps within a class
    /// keep the order the rows were listed in, which makes the plan readable.
    pub fn finalize(&mut self) {
        self.steps.sort_by_key(|s| s.step.order());
    }

    pub fn touches_manifest(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.step, Step::Manifest(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_step(argv: &[&str]) -> Step {
        Step::Host {
            host: "codex".into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
        }
    }

    #[test]
    fn marketplaces_are_added_before_plugins_are_installed_from_them() {
        let mut plan = Plan::default();
        plan.push(
            "install superpowers",
            host_step(&["plugin", "add", "superpowers"]),
        );
        plan.push(
            "add marketplace",
            host_step(&["plugin", "marketplace", "add", "owner/repo"]),
        );
        plan.finalize();
        let labels: Vec<&str> = plan.steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["add marketplace", "install superpowers"]);
    }

    #[test]
    fn removals_precede_adds_so_promote_never_double_scopes() {
        let mut plan = Plan::default();
        plan.push("add at user", host_step(&["mcp", "add", "pulumi"]));
        plan.push("remove at local", host_step(&["mcp", "remove", "pulumi"]));
        plan.finalize();
        assert_eq!(plan.steps[0].label, "remove at local");
    }

    #[test]
    fn finalize_is_stable_within_a_class() {
        let mut plan = Plan::default();
        plan.push("a", host_step(&["mcp", "add", "a"]));
        plan.push("b", host_step(&["mcp", "add", "b"]));
        plan.push("c", host_step(&["mcp", "add", "c"]));
        plan.finalize();
        let labels: Vec<&str> = plan.steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b", "c"]);
    }
}
