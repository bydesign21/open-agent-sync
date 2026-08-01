//! Plan types: the concrete, reviewable steps an accepted set of rows becomes.
//!
//! The plan is a first-class artifact, not an implementation detail. The tool
//! shows the plan before it runs anything, and it can export the plan as a
//! shell script. This makes the tool auditable: you can see exactly what it
//! plans to do, then run the script yourself.

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
    /// Both the TUI and `agentsync plan` render this text. As a result, the plan
    /// you approve interactively and the plan you read in a terminal describe
    /// the edit the same way.
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
    /// Create (or replace) `link` so it points at `target`. When agentsync did
    /// not create the existing content at `link`, it backs that content up
    /// first.
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
    /// Delete canonical content. Only an explicit purge triggers this action.
    RemoveTree(PathBuf),
    /// Write a generated file, creating parent directories. Used for shim
    /// content, which agentsync owns outright.
    WriteFile {
        path: PathBuf,
        contents: String,
    },
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
    /// Something the user must do by hand, for example export an environment
    /// variable. The plan carries this step so the user cannot miss it, and the
    /// report marks it as skipped instead of silently succeeding.
    Manual(String),
}

impl Step {
    /// Ordering class. Marketplaces must exist before plugins install from
    /// them. Removals must precede adds, so a promote or demote never leaves
    /// the same name at two scopes at once.
    fn order(&self) -> u8 {
        match self {
            Step::Manifest(ManifestOp::UpsertMarketplace { .. }) => 0,
            Step::Host { argv, .. } if argv.iter().any(|a| a == "marketplace") => 1,
            Step::Host { argv, .. } if argv.iter().any(|a| a == "remove" || a == "uninstall") => 2,
            Step::Fs(FsOp::Unlink(_)) => 2,
            Step::Fs(FsOp::WriteFile { .. }) => 0,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlannedStep {
    /// Short label for a human reader, for example `add kicad to codex`.
    pub label: String,
    pub step: Step,
    /// Overrides the step's default ordering class. Needed where the general
    /// rule is wrong for one case: shim installs must precede the removal of
    /// the plugin they replace.
    pub order_hint: Option<u8>,
    /// Skip this step when an earlier step sharing this key failed.
    ///
    /// Ordering alone is not enough. A shim install that fails must not be
    /// followed by removing the plugin it was replacing, or the host is left
    /// with no hook at all — which looks exactly like a clean run.
    pub guard: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub steps: Vec<PlannedStep>,
    /// Information the user needs, that is not a step: capabilities that forced
    /// a host to be skipped, or coverage that was deliberately bounded. This
    /// information is never silent.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn push(&mut self, label: impl Into<String>, step: Step) {
        self.steps.push(PlannedStep {
            label: label.into(),
            step,
            order_hint: None,
            guard: None,
        });
    }

    /// Push a step with an explicit ordering class.
    pub fn push_ordered(&mut self, label: impl Into<String>, step: Step, order: u8) {
        self.steps.push(PlannedStep {
            label: label.into(),
            step,
            order_hint: Some(order),
            guard: None,
        });
    }

    /// Push a step with an explicit ordering class and a guard key.
    ///
    /// The guard key links this step to another step sharing the same key. If
    /// the earlier step with that key fails, this step is skipped instead of
    /// run. Use this for a removal that must not happen when the install it
    /// depends on failed.
    pub fn push_guarded(
        &mut self,
        label: impl Into<String>,
        step: Step,
        order: u8,
        guard: impl Into<String>,
    ) {
        self.steps.push(PlannedStep {
            label: label.into(),
            step,
            order_hint: Some(order),
            guard: Some(guard.into()),
        });
    }

    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Sort steps into dependency order. This sort is stable, so steps within
    /// one class keep the order in which the rows were listed. This keeps the
    /// plan readable.
    pub fn finalize(&mut self) {
        self.steps
            .sort_by_key(|s| s.order_hint.unwrap_or_else(|| s.step.order()));
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

    #[test]
    fn an_explicit_order_hint_overrides_the_default_class() {
        // Shims must be installed BEFORE the original is removed. If the removal
        // fails, a duplicate hook is noisy and visible. The other order fails into
        // silently no security review, which reads as health.
        let mut plan = Plan::default();
        plan.push(
            "remove original",
            host_step(&["plugin", "remove", "security-guidance"]),
        );
        plan.push_ordered(
            "install shim",
            host_step(&["plugin", "add", "agentsync-shim-security-guidance"]),
            1,
        );
        plan.finalize();
        let labels: Vec<&str> = plan.steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["install shim", "remove original"]);
    }

    #[test]
    fn writes_happen_before_any_host_command() {
        let mut plan = Plan::default();
        plan.push("install", host_step(&["plugin", "add", "x"]));
        plan.push(
            "write shim",
            Step::Fs(FsOp::WriteFile {
                path: "/tmp/x/hooks.json".into(),
                contents: "{}".into(),
            }),
        );
        plan.finalize();
        assert_eq!(plan.steps[0].label, "write shim");
    }
}
