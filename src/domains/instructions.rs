//! The instructions domain: `CLAUDE.md`, `AGENTS.md`, and their per-repo forms.
//!
//! Mechanically this is the skills domain with a file instead of a directory: one
//! canonical file in `~/.config/agentsync/prompts/`, symlinked into wherever each
//! host looks. Both read plain markdown, so one file genuinely serves both.
//!
//! Shared is the default because most of what goes in these files is about the
//! *repo* — package manager, deploy gate, conventions — not the tool. The parts
//! that do name one CLI are what `hosts = [...]` is for, and a row that differs
//! offers "keep them divergent" precisely so you can split those out once instead
//! of being nagged forever.
//!
//! A scope a host has no location for — Codex has no `CLAUDE.local.md` — is
//! blocked rather than given an invented path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::core::diff::{Action, ActionKind, Domain, Row, RowKey, Severity, join_hosts};
use crate::core::model::{LinkState, Scope, short_repo};
use crate::core::plan::{FsOp, ManifestOp, Plan, Step};
use crate::manifest::InstructionEntry;
use crate::paths;

use super::World;

/// The canonical file for a scope, when the manifest does not say otherwise.
///
/// Public because the host read path needs it to decide whether a symlink it
/// found points at us.
pub fn canonical_for(scope: &Scope) -> PathBuf {
    paths::prompts_dir().join(format!("{}.md", default_name(scope)))
}

/// A stable, filesystem-safe entry name for a scope.
pub fn default_name(scope: &Scope) -> String {
    match scope {
        Scope::User => "user".to_string(),
        Scope::Project(repo) => slug(repo),
        Scope::Local(repo) => format!("{}.local", slug(repo)),
    }
}

fn slug(repo: &str) -> String {
    short_repo(repo).replace(['/', ' '], "-")
}

/// How the row reads: the scope, in words.
fn label(scope: &Scope) -> String {
    match scope {
        Scope::User => "user instructions".to_string(),
        Scope::Project(repo) => format!("{} (project)", short_repo(repo)),
        Scope::Local(repo) => format!("{} (local)", short_repo(repo)),
    }
}

/// Which manifest entry, if any, governs this scope.
fn entry_for<'a>(world: &'a World, scope: &Scope) -> Option<(&'a String, &'a InstructionEntry)> {
    world
        .manifest
        .instructions
        .iter()
        .find(|(_, e)| e.scopes().contains(scope))
}

fn canonical_path(world: &World, scope: &Scope) -> PathBuf {
    match entry_for(world, scope) {
        Some((_, entry)) => entry.resolve(&world.manifest_dir()),
        None => canonical_for(scope),
    }
}

/// Removal labels that say what is actually destroyed, as for skills: unlinking
/// is reversible, deleting the only copy of a file you wrote is not.
fn removals(
    states: &BTreeMap<String, LinkState>,
    canonical_exists: bool,
    from_manifest: bool,
) -> Vec<Action> {
    let present: Vec<String> = states
        .iter()
        .filter(|(_, s)| s.present())
        .map(|(h, _)| h.clone())
        .collect();
    if present.is_empty() {
        return Vec::new();
    }
    let destroys =
        |host: &str| !canonical_exists && matches!(states.get(host), Some(LinkState::Owned));
    let any = present.iter().any(|h| destroys(h));

    let mut out = vec![Action::new(
        if any {
            format!(
                "delete from {} \u{2014} destroys the only copy (backed up)",
                join_hosts(&present)
            )
        } else {
            format!("unlink from {}", join_hosts(&present))
        },
        ActionKind::Delete {
            hosts: present.clone(),
            from_manifest,
            purge: false,
        },
    )];
    if present.len() > 1 {
        for host in &present {
            out.push(Action::new(
                if destroys(host) {
                    format!("delete the only copy on {host} (backed up)")
                } else {
                    format!("unlink from {host} only")
                },
                ActionKind::Delete {
                    hosts: vec![host.clone()],
                    from_manifest: false,
                    purge: false,
                },
            ));
        }
    }
    out
}

pub(super) fn rows(world: &World) -> Vec<Row> {
    // Every scope any host has a location for, plus anything the manifest names.
    let mut scopes: BTreeSet<Scope> = BTreeSet::new();
    for (_, snap) in world.detected() {
        scopes.extend(snap.instructions.keys().cloned());
    }
    for entry in world.manifest.instructions.values() {
        scopes.extend(entry.scopes());
    }

    scopes
        .into_iter()
        .filter_map(|scope| row_for(world, &scope))
        .collect()
}

fn row_for(world: &World, scope: &Scope) -> Option<Row> {
    // What each host has for this scope, and which hosts have nowhere to put it.
    let mut states: BTreeMap<String, LinkState> = BTreeMap::new();
    let mut no_location: Vec<String> = Vec::new();
    for (host, snap) in world.detected() {
        if host.descriptor.instructions.is_none() {
            continue;
        }
        match snap.instructions.get(scope) {
            Some(file) => {
                states.insert(host.name().to_string(), file.state.clone());
            }
            None => no_location.push(host.name().to_string()),
        }
    }
    if states.is_empty() && no_location.is_empty() {
        return None;
    }

    let managed = entry_for(world, scope);
    let canonical = canonical_path(world, scope);
    let name = managed
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| default_name(scope));

    let owned: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, LinkState::Owned))
        .map(|(h, _)| h.clone())
        .collect();
    let linked: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, LinkState::Linked))
        .map(|(h, _)| h.clone())
        .collect();
    let absent: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, LinkState::Absent))
        .map(|(h, _)| h.clone())
        .collect();
    let foreign: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, LinkState::Foreign(_)))
        .map(|(h, _)| h.clone())
        .collect();

    let mut detail = format!(
        "{}   \u{b7}   {}",
        label(scope),
        paths::contract(&canonical)
    );
    if !no_location.is_empty() {
        detail = format!(
            "{detail}   \u{b7}   {} has no location for {} scope",
            join_hosts(&no_location),
            scope.cli_name()
        );
    }
    for (host, state) in &states {
        if let LinkState::Owned = state
            && let Some(file) = world.snapshot(host).and_then(|s| s.instructions.get(scope))
        {
            detail = format!(
                "{detail}   \u{b7}   {host}: {}",
                paths::contract(&file.path)
            );
        }
    }

    let key = RowKey {
        host_scopes: vec![scope.clone()],
        source_host: owned.first().cloned().or_else(|| linked.first().cloned()),
        source_path: owned
            .first()
            .and_then(|h| world.host(h))
            .and_then(|h| h.instruction_path(scope)),
        ..Default::default()
    };

    // Unmanaged: at least one host has a real file we could adopt.
    if managed.is_none() {
        if owned.is_empty() {
            // Nothing but foreign links, or nothing at all: informational.
            if foreign.is_empty() {
                return None;
            }
            return Some(Row {
                domain: Domain::Instructions,
                name,
                headline: format!("managed outside agentsync ({})", join_hosts(&foreign)),
                detail,
                severity: Severity::Blocked,
                actions: vec![Action::new("leave it", ActionKind::Nothing)],
                chosen: 0,
                accepted: false,
                key,
            });
        }

        let others: Vec<String> = states
            .keys()
            .filter(|h| !owned.contains(h))
            .cloned()
            .collect();
        // When two hosts each wrote their own file, there is no defensible
        // default: picking one silently discards the other's wording. So the
        // only offers are explicit, and you choose whose becomes canonical.
        let mut actions = Vec::new();
        if owned.len() > 1 {
            for host in &owned {
                actions.push(Action::new(
                    format!("adopt {host}'s version as canonical, link the rest"),
                    ActionKind::AdoptFrom { host: host.clone() },
                ));
            }
        } else {
            actions.push(Action::new(
                if others.is_empty() {
                    "adopt into the manifest".to_string()
                } else {
                    format!("adopt + link into {}", join_hosts(&others))
                },
                ActionKind::Adopt {
                    push: !others.is_empty(),
                    promote: false,
                },
            ));
            actions.push(Action::new(
                "adopt only, don't link",
                ActionKind::Adopt {
                    push: false,
                    promote: false,
                },
            ));
        }
        actions.push(Action::new(
            format!("keep {}-only", join_hosts(&owned)),
            ActionKind::KeepDivergent {
                hosts: owned.clone(),
            },
        ));
        actions.extend(removals(&states, canonical.exists(), false));

        return Some(Row {
            domain: Domain::Instructions,
            name,
            headline: if owned.len() > 1 {
                format!("{} each have their own", join_hosts(&owned))
            } else {
                format!("only in {}", join_hosts(&owned))
            },
            detail,
            severity: if owned.len() > 1 || !foreign.is_empty() {
                Severity::Warn
            } else {
                Severity::Normal
            },
            actions,
            chosen: 0,
            accepted: false,
            key,
        });
    }

    let (_, entry) = managed?;

    if !canonical.exists() {
        return Some(Row {
            domain: Domain::Instructions,
            name,
            headline: "canonical file is missing".into(),
            detail,
            severity: Severity::Warn,
            actions: vec![
                Action::new(
                    "drop it from the manifest",
                    ActionKind::Delete {
                        hosts: states.keys().cloned().collect(),
                        from_manifest: true,
                        purge: false,
                    },
                ),
                Action::new("leave it", ActionKind::Nothing),
            ],
            chosen: 0,
            accepted: false,
            key,
        });
    }

    let targets: Vec<String> = states
        .keys()
        .filter(|h| entry.targets_host(h))
        .cloned()
        .collect();
    let missing: Vec<String> = absent
        .iter()
        .filter(|h| targets.contains(h))
        .cloned()
        .collect();
    let clobber: Vec<String> = owned
        .iter()
        .chain(foreign.iter())
        .filter(|h| targets.contains(h))
        .cloned()
        .collect();

    if !clobber.is_empty() {
        let host = clobber[0].clone();
        let kind = if owned.contains(&host) {
            "its own file"
        } else {
            "a link elsewhere"
        };
        let mut actions = vec![
            Action::new(
                format!("replace with a link on {}", join_hosts(&clobber)),
                ActionKind::Push {
                    hosts: clobber.clone(),
                },
            ),
            Action::new(
                format!("adopt {host}'s version as canonical"),
                ActionKind::AdoptFrom { host: host.clone() },
            ),
        ];
        if !linked.is_empty() {
            actions.push(Action::new(
                format!("keep {}-only", join_hosts(&linked)),
                ActionKind::KeepDivergent {
                    hosts: linked.clone(),
                },
            ));
        }
        return Some(Row {
            domain: Domain::Instructions,
            name,
            headline: format!("{host} has {kind}"),
            detail: format!("{detail}   \u{b7}   replacing it backs up the current contents"),
            severity: Severity::Warn,
            actions,
            chosen: 0,
            accepted: false,
            key,
        });
    }

    if !missing.is_empty() {
        let mut actions = vec![Action::new(
            format!("link into {}", join_hosts(&missing)),
            ActionKind::Push {
                hosts: missing.clone(),
            },
        )];
        if !linked.is_empty() {
            actions.push(Action::new(
                format!("keep {}-only", join_hosts(&linked)),
                ActionKind::KeepDivergent {
                    hosts: linked.clone(),
                },
            ));
        }
        actions.extend(removals(&states, true, true));
        return Some(Row {
            domain: Domain::Instructions,
            name,
            headline: format!("missing from {}", join_hosts(&missing)),
            detail,
            severity: Severity::Normal,
            actions,
            chosen: 0,
            accepted: false,
            key,
        });
    }

    let mut row = Row::synced_removable(
        Domain::Instructions,
        name,
        detail,
        removals(&states, true, true),
    );
    row.key = key;
    Some(row)
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub(super) fn plan_row(world: &World, row: &Row, plan: &mut Plan) {
    let Some(scope) = row.key.host_scopes.first().cloned() else {
        return;
    };
    let name = row.name.clone();
    let canonical = canonical_path(world, &scope);

    match &row.action().kind {
        ActionKind::Nothing => {}

        ActionKind::Adopt { push, .. } => {
            adopt(
                world,
                &name,
                &scope,
                &row.key.source_path,
                &canonical,
                plan,
                None,
            );
            if *push {
                link_into(
                    world,
                    &scope,
                    &canonical,
                    &instruction_hosts(world, &scope),
                    plan,
                );
            }
        }

        ActionKind::AdoptFrom { host } => {
            let source = world.host(host).and_then(|h| h.instruction_path(&scope));
            adopt(world, &name, &scope, &source, &canonical, plan, None);
            link_into(
                world,
                &scope,
                &canonical,
                &instruction_hosts(world, &scope),
                plan,
            );
        }

        ActionKind::Push { hosts } => link_into(world, &scope, &canonical, hosts, plan),

        ActionKind::Delete {
            hosts,
            from_manifest,
            purge,
        } => {
            for (host, snap) in world.detected() {
                let hname = host.name().to_string();
                if !hosts.contains(&hname) {
                    continue;
                }
                let Some(file) = snap.instructions.get(&scope) else {
                    continue;
                };
                match file.state {
                    LinkState::Linked | LinkState::Foreign(_) => plan.push(
                        format!("unlink {name} from {hname}"),
                        Step::Fs(FsOp::Unlink(file.path.clone())),
                    ),
                    LinkState::Owned => plan.push(
                        format!("remove {name} from {hname} (backed up)"),
                        Step::Fs(FsOp::RemoveTree(file.path.clone())),
                    ),
                    LinkState::Absent => {}
                }
            }
            narrow_or_drop(world, &name, &scope, hosts, *from_manifest, plan);
            if *purge {
                plan.push(
                    format!("delete the canonical file for {name} (backed up)"),
                    Step::Fs(FsOp::RemoveTree(canonical)),
                );
            }
        }

        ActionKind::KeepDivergent { hosts } => {
            if world.manifest.instructions.contains_key(&name) {
                plan.push(
                    format!("record {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::SetInstructionHosts {
                        name: name.clone(),
                        hosts: Some(hosts.clone()),
                    }),
                );
            } else {
                adopt(
                    world,
                    &name,
                    &scope,
                    &row.key.source_path,
                    &canonical,
                    plan,
                    Some(hosts.clone()),
                );
                link_into(world, &scope, &canonical, hosts, plan);
            }
        }

        _ => plan.note(format!(
            "{name}: that action does not apply to instruction files"
        )),
    }
}

/// Hosts that have somewhere to put this scope.
fn instruction_hosts(world: &World, scope: &Scope) -> Vec<String> {
    world
        .detected()
        .filter(|(h, _)| h.instruction_path(scope).is_some())
        .map(|(h, _)| h.name().to_string())
        .collect()
}

fn adopt(
    world: &World,
    name: &str,
    scope: &Scope,
    source: &Option<PathBuf>,
    canonical: &Path,
    plan: &mut Plan,
    hosts: Option<Vec<String>>,
) {
    match source {
        Some(from) if from.exists() && !canonical.exists() => plan.push(
            format!("move {name} into canonical storage"),
            Step::Fs(FsOp::MoveIntoCanonical {
                from: from.clone(),
                to: canonical.to_path_buf(),
            }),
        ),
        Some(from) if !from.exists() => {
            plan.note(format!(
                "{name}: {} is gone, so there is nothing to adopt",
                paths::contract(from)
            ));
            return;
        }
        _ => {}
    }

    plan.push(
        format!("register {name} in the manifest"),
        Step::Manifest(ManifestOp::UpsertInstruction {
            name: name.to_string(),
            source: manifest_source(world, scope),
            scope: scope.kind(),
            repos: scope
                .repo()
                .map(|r| vec![r.to_string()])
                .unwrap_or_default(),
        }),
    );
    if let Some(hosts) = hosts {
        plan.push(
            format!("record {name} as {}-only", join_hosts(&hosts)),
            Step::Manifest(ManifestOp::SetInstructionHosts {
                name: name.to_string(),
                hosts: Some(hosts),
            }),
        );
    }
}

fn manifest_source(world: &World, scope: &Scope) -> String {
    let canonical = canonical_for(scope);
    match canonical.strip_prefix(world.manifest_dir()) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => paths::contract(&canonical),
    }
}

fn link_into(world: &World, scope: &Scope, canonical: &Path, hosts: &[String], plan: &mut Plan) {
    for (host, snap) in world.detected() {
        let hname = host.name().to_string();
        if !hosts.contains(&hname) {
            continue;
        }
        let Some(path) = host.instruction_path(scope) else {
            plan.note(format!(
                "{hname}: no location for {} instructions, so it was skipped",
                scope.cli_name()
            ));
            continue;
        };
        if matches!(
            snap.instructions.get(scope).map(|f| &f.state),
            Some(LinkState::Linked)
        ) {
            continue;
        }
        plan.push(
            format!("link {} into {hname}", label(scope)),
            Step::Fs(FsOp::Link {
                target: canonical.to_path_buf(),
                link: path,
            }),
        );
    }
}

/// Removing from some hosts narrows the manifest; removing from all drops it.
fn narrow_or_drop(
    world: &World,
    name: &str,
    scope: &Scope,
    removed: &[String],
    from_manifest: bool,
    plan: &mut Plan,
) {
    let Some(entry) = world.manifest.instructions.get(name) else {
        return;
    };
    let targeted: Vec<String> = instruction_hosts(world, scope)
        .into_iter()
        .filter(|h| entry.targets_host(h))
        .collect();
    let remaining: Vec<String> = targeted
        .iter()
        .filter(|h| !removed.contains(h))
        .cloned()
        .collect();

    if from_manifest || remaining.is_empty() {
        plan.push(
            format!("drop {name} from the manifest"),
            Step::Manifest(ManifestOp::RemoveInstruction(name.to_string())),
        );
    } else if remaining.len() < targeted.len() {
        plan.push(
            format!("narrow {name} to {}", join_hosts(&remaining)),
            Step::Manifest(ManifestOp::SetInstructionHosts {
                name: name.to_string(),
                hosts: Some(remaining),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names_are_stable_and_filesystem_safe() {
        assert_eq!(default_name(&Scope::User), "user");
        assert_eq!(
            default_name(&Scope::Project("/Users/me/repos/core/infra".into())),
            "core-infra"
        );
        assert_eq!(
            default_name(&Scope::Local("/Users/me/repos/core/infra".into())),
            "core-infra.local"
        );
    }

    #[test]
    fn canonical_paths_land_under_prompts() {
        let path = canonical_for(&Scope::User);
        assert!(path.ends_with("prompts/user.md"), "{}", path.display());
    }

    #[test]
    fn labels_say_which_scope() {
        assert_eq!(label(&Scope::User), "user instructions");
        assert_eq!(label(&Scope::Project("/a/b/web".into())), "b/web (project)");
    }
}
