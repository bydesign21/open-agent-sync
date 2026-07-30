//! The skills domain.
//!
//! Skills are the one domain with no CLI: canonical content lives in
//! `~/.config/agentsync/skills/<name>` and each host's skills directory gets a
//! symlink pointing in. Both Claude Code and Codex follow symlinked skill
//! directories, and both read the open Agent Skills layout (`SKILL.md` plus
//! optional `scripts/`, `references/`, `assets/`), so one directory genuinely
//! serves both.
//!
//! Three states must stay distinguishable, and conflating them is how a sync
//! tool destroys work:
//!
//! * **Linked** — a symlink into canonical. Synced.
//! * **RealDir** — the host owns the content. Adopting *moves* it into canonical
//!   after a backup; it is never silently overwritten.
//! * **Foreign** — a symlink somewhere else, e.g. another installer's tree.
//!   Reported. Only rewritten when the user explicitly picks that action, and
//!   the previous contents are backed up first.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::core::diff::{Action, ActionKind, Domain, Row, RowKey, Severity, join_hosts};
use crate::core::model::SkillState;
use crate::core::plan::{FsOp, ManifestOp, Plan, Step};
use crate::paths;

use super::World;

/// Where a skill's canonical content lives, honouring a manifest override.
fn canonical_path(world: &World, name: &str) -> PathBuf {
    match world.manifest.skills.get(name) {
        Some(entry) => entry.resolve(&world.manifest_dir()),
        None => paths::skills_dir().join(name),
    }
}

/// The `source` value written into the manifest for a newly adopted skill.
fn manifest_source(world: &World, name: &str) -> String {
    let canonical = paths::skills_dir().join(name);
    match canonical.strip_prefix(world.manifest_dir()) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => paths::contract(&canonical),
    }
}

pub(super) fn rows(world: &World) -> Vec<Row> {
    let mut names: BTreeSet<String> = world.manifest.skills.keys().cloned().collect();
    for snap in world.detected_snapshots() {
        // Plugin-provided skills belong to the plugin manager. Managing them
        // here would have the two fighting over the same directory.
        let owned_by_plugins: BTreeSet<&String> = snap.plugin_skills.iter().collect();
        names.extend(
            snap.skills
                .keys()
                .filter(|n| !owned_by_plugins.contains(n))
                .cloned(),
        );
    }

    names
        .into_iter()
        .filter_map(|name| row_for(world, &name))
        .collect()
}

fn row_for(world: &World, name: &str) -> Option<Row> {
    let mut states: BTreeMap<String, SkillState> = BTreeMap::new();
    for (host, snap) in world.detected() {
        if host.descriptor.skills.is_none() {
            continue;
        }
        if snap.plugin_skills.iter().any(|p| p == name) {
            continue;
        }
        states.insert(
            host.name().to_string(),
            snap.skills.get(name).cloned().unwrap_or(SkillState::Absent),
        );
    }

    let canonical = canonical_path(world, name);
    let managed = world.manifest.skills.get(name);

    let real_dirs: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, SkillState::RealDir))
        .map(|(h, _)| h.clone())
        .collect();
    let foreign: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, SkillState::Foreign(_)))
        .map(|(h, _)| h.clone())
        .collect();
    let linked: Vec<String> = states
        .iter()
        .filter(|(_, s)| matches!(s, SkillState::Linked))
        .map(|(h, _)| h.clone())
        .collect();

    match managed {
        None => {
            if real_dirs.is_empty() && foreign.is_empty() {
                return None;
            }
            if real_dirs.is_empty() {
                // Only foreign links: we do not know who owns the content, so
                // this is information, not a decision.
                let targets: Vec<String> = states
                    .values()
                    .filter_map(|s| match s {
                        SkillState::Foreign(p) => Some(paths::contract(p)),
                        _ => None,
                    })
                    .collect();
                return Some(Row {
                    domain: Domain::Skills,
                    name: name.to_string(),
                    headline: format!("managed outside agentsync ({})", join_hosts(&foreign)),
                    detail: format!("links to {}", targets.join(", ")),
                    severity: Severity::Blocked,
                    actions: vec![Action::new("leave it", ActionKind::Nothing)],
                    chosen: 0,
                    accepted: false,
                    key: RowKey::default(),
                });
            }

            let source_host = real_dirs[0].clone();
            let source_path = world
                .host(&source_host)
                .and_then(|h| h.skills_link_dir())
                .map(|d| d.join(name));

            let mut detail = source_path
                .as_ref()
                .map(|p| format!("real directory at {}", paths::contract(p)))
                .unwrap_or_default();
            if !foreign.is_empty() {
                detail = format!(
                    "{detail}   \u{b7}   {} link elsewhere and would be repointed",
                    join_hosts(&foreign)
                );
            }

            let severity = if foreign.is_empty() {
                Severity::Normal
            } else {
                Severity::Warn
            };

            Some(Row {
                domain: Domain::Skills,
                name: name.to_string(),
                headline: format!("only in {}", join_hosts(&real_dirs)),
                detail,
                severity,
                actions: vec![
                    Action::new(
                        "adopt + link into the others",
                        ActionKind::Adopt {
                            push: true,
                            promote: false,
                        },
                    ),
                    Action::new(
                        "adopt only, don't link",
                        ActionKind::Adopt {
                            push: false,
                            promote: false,
                        },
                    ),
                    Action::new(
                        format!("keep {}-only", join_hosts(&real_dirs)),
                        ActionKind::KeepDivergent {
                            hosts: real_dirs.clone(),
                        },
                    ),
                    Action::new(
                        "delete everywhere",
                        ActionKind::Delete {
                            hosts: states.keys().cloned().collect(),
                            from_manifest: false,
                            purge: false,
                        },
                    ),
                ],
                chosen: 0,
                accepted: false,
                key: RowKey {
                    source_host: Some(source_host),
                    source_path,
                    ..Default::default()
                },
            })
        }

        Some(entry) => {
            if !canonical.exists() {
                return Some(Row {
                    domain: Domain::Skills,
                    name: name.to_string(),
                    headline: "canonical content is missing".into(),
                    detail: format!("{} does not exist", paths::contract(&canonical)),
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
                    key: RowKey::default(),
                });
            }

            let targets: Vec<String> = states
                .keys()
                .filter(|h| entry.targets_host(h))
                .cloned()
                .collect();
            let missing: Vec<String> = targets
                .iter()
                .filter(|h| matches!(states.get(*h), Some(SkillState::Absent)))
                .cloned()
                .collect();
            let clobber: Vec<String> = targets
                .iter()
                .filter(|h| {
                    matches!(
                        states.get(*h),
                        Some(SkillState::RealDir) | Some(SkillState::Foreign(_))
                    )
                })
                .cloned()
                .collect();

            let detail = format!("canonical: {}", paths::contract(&canonical));

            if !clobber.is_empty() {
                let host = clobber[0].clone();
                let kind = match states.get(&host) {
                    Some(SkillState::RealDir) => "an unlinked copy",
                    _ => "a link elsewhere",
                };
                return Some(Row {
                    domain: Domain::Skills,
                    name: name.to_string(),
                    headline: format!("{host} has {kind}"),
                    detail: format!(
                        "{detail}   \u{b7}   replacing it backs up the current contents"
                    ),
                    severity: Severity::Warn,
                    actions: vec![
                        Action::new(
                            format!("replace with a link on {}", join_hosts(&clobber)),
                            ActionKind::Push {
                                hosts: clobber.clone(),
                            },
                        ),
                        Action::new(
                            format!("adopt {host}'s copy as canonical"),
                            ActionKind::AdoptFrom { host: host.clone() },
                        ),
                        Action::new(
                            "keep them divergent",
                            ActionKind::KeepDivergent {
                                hosts: linked.clone(),
                            },
                        ),
                    ],
                    chosen: 0,
                    accepted: false,
                    key: RowKey {
                        source_host: Some(host.clone()),
                        source_path: world
                            .host(&host)
                            .and_then(|h| h.skills_link_dir())
                            .map(|d| d.join(name)),
                        ..Default::default()
                    },
                });
            }

            if !missing.is_empty() {
                return Some(Row {
                    domain: Domain::Skills,
                    name: name.to_string(),
                    headline: format!("missing from {}", join_hosts(&missing)),
                    detail,
                    severity: Severity::Normal,
                    actions: vec![
                        Action::new(
                            format!("link into {}", join_hosts(&missing)),
                            ActionKind::Push {
                                hosts: missing.clone(),
                            },
                        ),
                        Action::new(
                            format!("keep {}-only", join_hosts(&linked)),
                            ActionKind::KeepDivergent {
                                hosts: linked.clone(),
                            },
                        ),
                        Action::new(
                            "delete everywhere",
                            ActionKind::Delete {
                                hosts: states.keys().cloned().collect(),
                                from_manifest: true,
                                purge: false,
                            },
                        ),
                    ],
                    chosen: 0,
                    accepted: false,
                    key: RowKey::default(),
                });
            }

            Some(Row::synced(Domain::Skills, name, detail))
        }
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub(super) fn plan_row(world: &World, row: &Row, plan: &mut Plan) {
    let name = row.name.clone();
    let canonical = paths::skills_dir().join(&name);

    match &row.action().kind {
        ActionKind::Nothing => {}

        ActionKind::Adopt { push, .. } => {
            adopt(world, &name, &row.key.source_path, &canonical, plan, None);
            if *push {
                link_into(world, &name, &canonical, &all_skill_hosts(world), plan);
            }
        }

        ActionKind::AdoptFrom { host } => {
            let source = world
                .host(host)
                .and_then(|h| h.skills_link_dir())
                .map(|d| d.join(&name));
            adopt(world, &name, &source, &canonical, plan, None);
            link_into(world, &name, &canonical, &all_skill_hosts(world), plan);
        }

        ActionKind::Push { hosts } => {
            link_into(world, &name, &canonical_path(world, &name), hosts, plan);
        }

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
                let Some(dir) = host.skills_link_dir() else {
                    continue;
                };
                let path = dir.join(&name);
                match snap.skills.get(&name) {
                    Some(SkillState::Linked) | Some(SkillState::Foreign(_)) => plan.push(
                        format!("unlink {name} from {hname}"),
                        Step::Fs(FsOp::Unlink(path)),
                    ),
                    Some(SkillState::RealDir) => plan.push(
                        format!("remove {name} from {hname} (backed up)"),
                        Step::Fs(FsOp::RemoveTree(path)),
                    ),
                    _ => {}
                }
            }
            if *from_manifest {
                plan.push(
                    format!("drop {name} from the manifest"),
                    Step::Manifest(ManifestOp::RemoveSkill(name.clone())),
                );
            }
            if *purge {
                plan.push(
                    format!("delete canonical content for {name} (backed up)"),
                    Step::Fs(FsOp::RemoveTree(canonical_path(world, &name))),
                );
            } else if world.manifest.skills.contains_key(&name) || canonical.exists() {
                plan.note(format!(
                    "{name}: canonical content kept at {}; re-run with a purge action to delete it",
                    paths::contract(&canonical_path(world, &name))
                ));
            }
        }

        ActionKind::KeepDivergent { hosts } => {
            if world.manifest.skills.contains_key(&name) {
                plan.push(
                    format!("record {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::SetSkillHosts {
                        name: name.clone(),
                        hosts: Some(hosts.clone()),
                    }),
                );
            } else {
                adopt(
                    world,
                    &name,
                    &row.key.source_path,
                    &canonical,
                    plan,
                    Some(hosts.clone()),
                );
                link_into(world, &name, &canonical, hosts, plan);
            }
        }

        // Not meaningful for skills.
        ActionKind::CollapseScope { .. }
        | ActionKind::SecretToEnv { .. }
        | ActionKind::PinMarketplace { .. } => {
            plan.note(format!("{name}: that action does not apply to skills"));
        }
    }
}

/// Move host-owned content into canonical storage and register it.
fn adopt(
    world: &World,
    name: &str,
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
        _ => {
            // Canonical already holds the content; registering it is enough.
        }
    }

    plan.push(
        format!("register {name} in the manifest"),
        Step::Manifest(ManifestOp::UpsertSkill {
            name: name.to_string(),
            source: manifest_source(world, name),
        }),
    );
    if let Some(hosts) = hosts {
        plan.push(
            format!("record {name} as {}-only", join_hosts(&hosts)),
            Step::Manifest(ManifestOp::SetSkillHosts {
                name: name.to_string(),
                hosts: Some(hosts),
            }),
        );
    }
}

fn link_into(world: &World, name: &str, canonical: &Path, hosts: &[String], plan: &mut Plan) {
    for (host, snap) in world.detected() {
        let hname = host.name().to_string();
        if !hosts.contains(&hname) {
            continue;
        }
        let Some(dir) = host.skills_link_dir() else {
            continue;
        };
        if matches!(snap.skills.get(name), Some(SkillState::Linked)) {
            continue;
        }
        plan.push(
            format!("link {name} into {hname}"),
            Step::Fs(FsOp::Link {
                target: canonical.to_path_buf(),
                link: dir.join(name),
            }),
        );
    }
}

fn all_skill_hosts(world: &World) -> Vec<String> {
    world
        .detected()
        .filter(|(h, _)| h.descriptor.skills.is_some())
        .map(|(h, _)| h.name().to_string())
        .collect()
}
