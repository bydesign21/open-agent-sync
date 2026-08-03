//! The plugins domain: marketplaces first, then the plugins installed from them.
//!
//! The curated registries are genuinely different between hosts. `superpowers`
//! exists in both `claude-plugins-official` and `openai-api-curated` under
//! different marketplace names, and plenty of plugins exist in only one. So:
//!
//! * The manifest stores a **plugin name**, not a `name@marketplace` id. The
//!   id is resolved per host from that host's marketplace manifests. Neither CLI
//!   resolves a bare name, so the marketplace must be looked up, never assumed.
//! * `marketplace = "..."` is an optional pin, for when several of one host's
//!   marketplaces offer the same name.
//! * A name no configured marketplace offers is **not available**, not drift.
//!   Treating it as drift would nag forever about something that cannot be fixed.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::diff::{
    Action, ActionKind, Domain, Row, RowKey, Severity, join_hosts, removal_actions,
};
use crate::core::model::{HostSnapshot, MarketplaceSource};
use crate::core::plan::{ManifestOp, Plan, Step};
use crate::hosts::opencode_family::{layers::Family, plugins as ocp};
use crate::manifest::{MarketplaceEntry, PluginEntry, PluginIdentity, PluginTarget};
use crate::transaction::{
    ConfigEditOperation, ConfigTransaction, FilePrecondition, FileTransaction, GuardedSource,
    SourceEdit, compute_sha256,
};

use super::World;

pub(super) fn rows(world: &World) -> Vec<Row> {
    let mut out = marketplace_rows(world);
    out.extend(plugin_rows(world));
    out.extend(target_rows(world));
    out
}

// ---------------------------------------------------------------------------
// Marketplaces
// ---------------------------------------------------------------------------

fn marketplace_rows(world: &World) -> Vec<Row> {
    let mut names: BTreeSet<String> = world.manifest.marketplaces.keys().cloned().collect();
    for (host, snap) in world.detected() {
        let implicit: BTreeSet<&str> = host
            .implicit_marketplaces()
            .iter()
            .map(String::as_str)
            .collect();
        names.extend(
            snap.marketplaces
                .keys()
                .filter(|n| !implicit.contains(n.as_str()))
                .cloned(),
        );
    }

    names
        .into_iter()
        .filter(|name| !crate::shim::generate::is_internal_marketplace(name))
        .filter_map(|name| marketplace_row(world, &name))
        .collect()
}

fn marketplace_row(world: &World, name: &str) -> Option<Row> {
    let mut present: BTreeMap<String, MarketplaceSource> = BTreeMap::new();
    for (host, snap) in world.detected() {
        if host.descriptor.plugins.is_none() {
            continue;
        }
        if host.implicit_marketplaces().iter().any(|m| m == name) {
            continue;
        }
        if let Some(source) = snap.marketplaces.get(name) {
            present.insert(host.name().to_string(), source.clone());
        }
    }

    let plugin_hosts: Vec<String> = world
        .detected()
        .filter(|(h, _)| h.descriptor.plugins.is_some())
        .map(|(h, _)| h.name().to_string())
        .collect();

    let key = RowKey {
        is_marketplace: true,
        source_host: present.keys().next().cloned(),
        ..Default::default()
    };

    match world.manifest.marketplaces.get(name) {
        None => {
            let hosts: Vec<String> = present.keys().cloned().collect();
            if hosts.is_empty() {
                return None;
            }
            let source = present.values().next().cloned()?;
            Some(Row {
                domain: Domain::Plugins,
                name: format!("marketplace {name}"),
                headline: format!("only in {}", join_hosts(&hosts)),
                detail: source.to_string(),
                severity: Severity::Normal,
                actions: vec![
                    Action::new(
                        "adopt + add to the others",
                        ActionKind::Adopt {
                            push: true,
                            promote: false,
                        },
                    ),
                    Action::new(
                        "adopt only",
                        ActionKind::Adopt {
                            push: false,
                            promote: false,
                        },
                    ),
                    Action::new(
                        format!("keep {}-only", join_hosts(&hosts)),
                        ActionKind::KeepDivergent { hosts },
                    ),
                ],
                chosen: 0,
                accepted: false,
                key,
            })
        }
        Some(entry) => {
            let Some(source) = entry.source() else {
                return Some(Row {
                    domain: Domain::Plugins,
                    name: format!("marketplace {name}"),
                    headline: "no source declared".into(),
                    detail: "set one of github, directory, or url".into(),
                    severity: Severity::Warn,
                    actions: vec![Action::new("leave it", ActionKind::Nothing)],
                    chosen: 0,
                    accepted: false,
                    key,
                });
            };
            let missing: Vec<String> = plugin_hosts
                .iter()
                .filter(|h| entry.targets_host(h) && !present.contains_key(*h))
                .cloned()
                .collect();
            if missing.is_empty() {
                return Some(Row::synced_removable(
                    Domain::Plugins,
                    format!("marketplace {name}"),
                    source.to_string(),
                    removal_actions(&present.keys().cloned().collect::<Vec<_>>(), "remove", true),
                ));
            }
            let kept: Vec<String> = present.keys().cloned().collect();
            Some(Row {
                domain: Domain::Plugins,
                name: format!("marketplace {name}"),
                headline: format!("missing from {}", join_hosts(&missing)),
                detail: source.to_string(),
                severity: Severity::Normal,
                actions: vec![
                    Action::new(
                        format!("add to {}", join_hosts(&missing)),
                        ActionKind::Push { hosts: missing },
                    ),
                    Action::new(
                        format!("keep {}-only", join_hosts(&kept)),
                        ActionKind::KeepDivergent { hosts: kept },
                    ),
                ]
                .into_iter()
                .chain(removal_actions(
                    &present.keys().cloned().collect::<Vec<_>>(),
                    "remove",
                    true,
                ))
                .collect(),
                chosen: 0,
                accepted: false,
                key,
            })
        }
    }
}

/// Strip the display prefix back off a marketplace row name.
fn marketplace_name(row_name: &str) -> &str {
    row_name.strip_prefix("marketplace ").unwrap_or(row_name)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// The outcome of resolving `name` to an installable id on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    /// Exactly one marketplace offers it. This is the id to install.
    One(String),
    /// Several offer it. Installing would be a coin flip, so ask for a pin.
    Ambiguous(Vec<String>),
    /// Nothing this host has configured offers it.
    None,
}

/// Resolve `name` against what `host` actually has.
///
/// This must be a lookup, not a guess. Neither CLI resolves a bare plugin id:
/// `codex plugin add superpowers` exits 1 with "requires --marketplace unless
/// passed as <plugin>@<marketplace>", and `claude plugin install` exits 1 with
/// "not found in any configured marketplace".
fn resolve(world: &World, host_name: &str, name: &str, pin: Option<&str>) -> Resolution {
    let Some((host, snap)) = world.detected().find(|(h, _)| h.name() == host_name) else {
        return Resolution::None;
    };
    if host.descriptor.plugins.is_none() {
        return Resolution::None;
    }

    // Already installed: whatever it came from is the right answer.
    if let Some(installed) = snap.plugins.get(name)
        && !installed.marketplace.is_empty()
    {
        return Resolution::One(installed.marketplace.clone());
    }

    let offering: Vec<String> = snap
        .catalog
        .iter()
        .filter(|(_, plugins)| plugins.contains(name))
        .map(|(market, _)| market.clone())
        .collect();

    if let Some(pin) = pin {
        return if offering.iter().any(|m| m == pin) {
            Resolution::One(pin.to_string())
        } else {
            Resolution::None
        };
    }

    match offering.len() {
        0 => Resolution::None,
        1 => Resolution::One(offering[0].clone()),
        _ => Resolution::Ambiguous(offering),
    }
}

fn plugin_rows(world: &World) -> Vec<Row> {
    let substitutions = super::hooks::shim_substitutions(world);
    let mut names: BTreeSet<String> = world.manifest.plugins.keys().cloned().collect();
    for (host, snap) in world.detected() {
        if host.descriptor.plugins.is_some() {
            names.extend(snap.plugins.keys().cloned());
        }
    }
    names
        .into_iter()
        .filter(|name| !name.starts_with("agentsync-shim-"))
        // A manifest entry with explicit npm/local targets is handled
        // entirely by `target_rows`. It is a different mechanism (no
        // marketplace to resolve a bare name against), and letting both paths
        // emit a row for the same name would violate one row per name per
        // domain.
        .filter(|name| {
            world
                .manifest
                .plugins
                .get(name)
                .is_none_or(|e| e.targets.is_empty())
        })
        .filter_map(|name| plugin_row(world, &name, &substitutions))
        .collect()
}

fn plugin_row(
    world: &World,
    name: &str,
    substitutions: &[super::hooks::ShimSubstitution],
) -> Option<Row> {
    let mut installed: BTreeMap<String, String> = BTreeMap::new();
    for (host, snap) in world.detected() {
        if host.descriptor.plugins.is_none() {
            continue;
        }
        if let Some(p) = snap.plugins.get(name) {
            installed.insert(host.name().to_string(), p.marketplace.clone());
        }
    }
    for substitution in substitutions
        .iter()
        .filter(|substitution| substitution.plugin == name)
    {
        installed
            .entry(substitution.target_host.clone())
            .or_insert_with(|| substitution.marketplace.clone());
    }

    let plugin_hosts: Vec<String> = world
        .detected()
        .filter(|(h, _)| h.descriptor.plugins.is_some())
        .map(|(h, _)| h.name().to_string())
        .collect();

    let entry = world.manifest.plugins.get(name);
    let pin = entry.and_then(|e| e.marketplace.as_deref());

    let key = RowKey {
        source_host: installed.keys().next().cloned(),
        marketplace: installed.values().next().cloned(),
        ..Default::default()
    };

    let detail = if installed.is_empty() {
        String::new()
    } else {
        installed
            .iter()
            .map(|(h, m)| {
                if m.is_empty() {
                    h.clone()
                } else {
                    format!("{h}: {m}")
                }
            })
            .collect::<Vec<_>>()
            .join("   \u{b7}   ")
    };

    match entry {
        None => {
            let hosts: Vec<String> = installed.keys().cloned().collect();
            if hosts.is_empty() {
                return None;
            }

            // Which other hosts could actually install it. Offering "install in
            // the others" without checking is what made 17 installs fail: the
            // curated registries differ, so plenty of plugins exist on only one
            // side and no id will ever resolve on the other.
            let installable: Vec<String> = plugin_hosts
                .iter()
                .filter(|h| !installed.contains_key(*h))
                .filter(|h| matches!(resolve(world, h, name, None), Resolution::One(_)))
                .cloned()
                .collect();

            if installable.is_empty() {
                return Some(Row {
                    domain: Domain::Plugins,
                    name: name.to_string(),
                    headline: format!("{}-only; no other host offers it", join_hosts(&hosts)),
                    detail,
                    severity: Severity::Blocked,
                    actions: vec![
                        Action::new(
                            format!("adopt as {}-only", join_hosts(&hosts)),
                            ActionKind::KeepDivergent {
                                hosts: hosts.clone(),
                            },
                        ),
                        Action::new("leave it", ActionKind::Nothing),
                    ]
                    .into_iter()
                    .chain(removal_actions(&hosts, "uninstall", false))
                    .collect(),
                    chosen: 0,
                    accepted: false,
                    key,
                });
            }

            Some(Row {
                domain: Domain::Plugins,
                name: name.to_string(),
                headline: format!("only in {}", join_hosts(&hosts)),
                detail,
                severity: Severity::Normal,
                actions: vec![
                    Action::new(
                        format!("adopt + install in {}", join_hosts(&installable)),
                        ActionKind::Adopt {
                            push: true,
                            promote: false,
                        },
                    ),
                    Action::new(
                        "adopt only",
                        ActionKind::Adopt {
                            push: false,
                            promote: false,
                        },
                    ),
                    Action::new(
                        format!("keep {}-only", join_hosts(&hosts)),
                        ActionKind::KeepDivergent {
                            hosts: hosts.clone(),
                        },
                    ),
                ]
                .into_iter()
                .chain(removal_actions(&hosts, "uninstall", false))
                .collect(),
                chosen: 0,
                accepted: false,
                key,
            })
        }
        Some(entry) => {
            let targets: Vec<String> = plugin_hosts
                .iter()
                .filter(|h| entry.targets_host(h))
                .cloned()
                .collect();

            let mut missing = Vec::new();
            let mut unavailable = Vec::new();
            let mut ambiguous: Vec<(String, Vec<String>)> = Vec::new();
            for host in &targets {
                if installed.contains_key(host) {
                    continue;
                }
                match resolve(world, host, name, pin) {
                    Resolution::One(_) => missing.push(host.clone()),
                    Resolution::Ambiguous(markets) => ambiguous.push((host.clone(), markets)),
                    Resolution::None => unavailable.push(host.clone()),
                }
            }

            let mut detail = detail;
            if !unavailable.is_empty() {
                let pin_note = pin.map(|p| format!(" (pinned to {p})")).unwrap_or_default();
                detail = format!(
                    "{detail}   \u{b7}   no marketplace on {} offers it{pin_note}",
                    join_hosts(&unavailable)
                );
            }

            // Ambiguity is its own decision, and pinning is the only fix.
            if let Some((host, markets)) = ambiguous.first() {
                return Some(Row {
                    domain: Domain::Plugins,
                    name: name.to_string(),
                    headline: format!("{} marketplaces on {host} offer it", markets.len()),
                    detail: format!("{detail}   \u{b7}   {}", markets.join(", ")),
                    severity: Severity::Warn,
                    actions: markets
                        .iter()
                        .map(|m| {
                            Action::new(
                                format!("pin to {m}"),
                                ActionKind::PinMarketplace {
                                    marketplace: m.clone(),
                                },
                            )
                        })
                        .collect(),
                    chosen: 0,
                    accepted: false,
                    key,
                });
            }

            if missing.is_empty() {
                if unavailable.is_empty() {
                    return Some(Row::synced_removable(
                        Domain::Plugins,
                        name,
                        detail,
                        removal_actions(
                            &installed.keys().cloned().collect::<Vec<_>>(),
                            "uninstall",
                            true,
                        ),
                    ));
                }
                let kept: Vec<String> = installed.keys().cloned().collect();
                return Some(Row {
                    domain: Domain::Plugins,
                    name: name.to_string(),
                    headline: format!("not available on {}", join_hosts(&unavailable)),
                    detail,
                    severity: Severity::Blocked,
                    actions: vec![
                        Action::new(
                            format!("record it as {}-only", join_hosts(&kept)),
                            ActionKind::KeepDivergent { hosts: kept },
                        ),
                        Action::new("leave it", ActionKind::Nothing),
                    ],
                    chosen: 0,
                    accepted: false,
                    key,
                });
            }

            let kept: Vec<String> = installed.keys().cloned().collect();
            Some(Row {
                domain: Domain::Plugins,
                name: name.to_string(),
                headline: format!("missing from {}", join_hosts(&missing)),
                detail,
                severity: Severity::Normal,
                actions: vec![
                    Action::new(
                        format!("install in {}", join_hosts(&missing)),
                        ActionKind::Push { hosts: missing },
                    ),
                    Action::new(
                        format!("keep {}-only", join_hosts(&kept)),
                        ActionKind::KeepDivergent {
                            hosts: kept.clone(),
                        },
                    ),
                ]
                .into_iter()
                .chain(removal_actions(&kept, "uninstall", true))
                .collect(),
                chosen: 0,
                accepted: false,
                key,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub(super) fn plan_row(world: &World, row: &Row, plan: &mut Plan) {
    if row.key.is_marketplace {
        plan_marketplace_row(world, row, plan);
    } else if row.key.is_target {
        plan_target_row(world, row, plan);
    } else {
        plan_plugin_row(world, row, plan);
    }
}

fn plan_marketplace_row(world: &World, row: &Row, plan: &mut Plan) {
    let name = marketplace_name(&row.name).to_string();

    let observed = row
        .key
        .source_host
        .as_ref()
        .and_then(|h| world.snapshot(h))
        .and_then(|s| s.marketplaces.get(&name).cloned());

    match &row.action().kind {
        ActionKind::Nothing => {}

        ActionKind::Adopt { push, .. } => {
            let Some(source) = adopt_marketplace(&name, observed.clone(), plan) else {
                return;
            };
            if *push {
                let hosts: Vec<String> = world
                    .detected()
                    .filter(|(h, s)| {
                        h.descriptor.plugins.is_some() && !s.marketplaces.contains_key(&name)
                    })
                    .map(|(h, _)| h.name().to_string())
                    .collect();
                add_marketplace(world, &name, source.as_arg(), &hosts, plan);
            }
        }

        ActionKind::KeepDivergent { hosts } => {
            if !world.manifest.marketplaces.contains_key(&name)
                && adopt_marketplace(&name, observed.clone(), plan).is_none()
            {
                return;
            }
            plan.push(
                format!("record marketplace {name} as {}-only", join_hosts(hosts)),
                Step::Manifest(ManifestOp::SetMarketplaceHosts {
                    name: name.clone(),
                    hosts: Some(hosts.clone()),
                }),
            );
        }

        ActionKind::Push { hosts } => {
            let source = world
                .manifest
                .marketplaces
                .get(&name)
                .and_then(MarketplaceEntry::source)
                .or(observed);
            match source {
                Some(source) => add_marketplace(world, &name, source.as_arg(), hosts, plan),
                None => plan.note(format!("marketplace {name}: no source to add from")),
            }
        }

        ActionKind::Delete {
            hosts,
            from_manifest,
            ..
        } => {
            for (host, _) in world.detected() {
                let hname = host.name().to_string();
                if !hosts.contains(&hname) {
                    continue;
                }
                match host.marketplace_remove_argv(&name) {
                    Ok(Some(argv)) => plan.push(
                        format!("remove marketplace {name} from {hname}"),
                        Step::Host {
                            host: hname,
                            argv,
                            cwd: None,
                        },
                    ),
                    _ => plan.note(format!(
                        "marketplace {name}: {hname} declares no marketplace removal command"
                    )),
                }
            }
            if *from_manifest {
                plan.push(
                    format!("drop marketplace {name} from the manifest"),
                    Step::Manifest(ManifestOp::RemoveMarketplace(name.clone())),
                );
            }
        }

        _ => plan.note(format!(
            "marketplace {name}: that action does not apply to marketplaces"
        )),
    }
}

/// Emit the manifest upsert for a marketplace we observed on a host.
fn adopt_marketplace(
    name: &str,
    observed: Option<MarketplaceSource>,
    plan: &mut Plan,
) -> Option<MarketplaceSource> {
    let Some(source) = observed else {
        plan.note(format!("marketplace {name}: nothing to adopt"));
        return None;
    };
    plan.push(
        format!("adopt marketplace {name}"),
        Step::Manifest(ManifestOp::UpsertMarketplace {
            name: name.to_string(),
            entry: Box::new(entry_for(&source)),
        }),
    );
    Some(source)
}

fn entry_for(source: &MarketplaceSource) -> MarketplaceEntry {
    let mut entry = MarketplaceEntry {
        github: None,
        directory: None,
        url: None,
        hosts: None,
    };
    match source {
        MarketplaceSource::GitHub(v) => entry.github = Some(v.clone()),
        MarketplaceSource::Directory(v) => entry.directory = Some(v.clone()),
        MarketplaceSource::Url(v) => entry.url = Some(v.clone()),
    }
    entry
}

fn add_marketplace(world: &World, name: &str, source: &str, hosts: &[String], plan: &mut Plan) {
    for (host, _) in world.detected() {
        let hname = host.name().to_string();
        if !hosts.contains(&hname) || host.descriptor.plugins.is_none() {
            continue;
        }
        match host.marketplace_add_argv(name, source) {
            Ok(argv) => plan.push(
                format!("add marketplace {name} to {hname}"),
                Step::Host {
                    host: hname,
                    argv,
                    cwd: None,
                },
            ),
            Err(e) => plan.note(format!("marketplace {name}: {hname} \u{2014} {e:#}")),
        }
    }
}

fn plan_plugin_row(world: &World, row: &Row, plan: &mut Plan) {
    let name = row.name.clone();
    let pin = world
        .manifest
        .plugins
        .get(&name)
        .and_then(|e| e.marketplace.clone());

    match &row.action().kind {
        ActionKind::Nothing => {}

        ActionKind::Adopt { push, .. } => {
            plan.push(
                format!("adopt {name}"),
                Step::Manifest(ManifestOp::UpsertPlugin {
                    name: name.clone(),
                    // Deliberately unpinned: the id is resolved per host, since
                    // the curated registries differ between them.
                    marketplace: None,
                }),
            );
            if *push {
                let hosts: Vec<String> = world
                    .detected()
                    .filter(|(h, s)| {
                        h.descriptor.plugins.is_some() && !s.plugins.contains_key(&name)
                    })
                    .map(|(h, _)| h.name().to_string())
                    .collect();
                install(world, &name, None, &hosts, plan);
            }
        }

        ActionKind::AdoptFrom { host } => {
            let marketplace = world
                .snapshot(host)
                .and_then(|s| s.plugins.get(&name).map(|p| p.marketplace.clone()))
                .filter(|m| !m.is_empty());
            plan.push(
                format!(
                    "adopt {name} pinned to {}",
                    marketplace.clone().unwrap_or_default()
                ),
                Step::Manifest(ManifestOp::UpsertPlugin {
                    name: name.clone(),
                    marketplace,
                }),
            );
        }

        ActionKind::Push { hosts } => {
            install(world, &name, pin.as_deref(), hosts, plan);
        }

        ActionKind::PinMarketplace { marketplace } => {
            plan.push(
                format!("pin {name} to {marketplace}"),
                Step::Manifest(ManifestOp::UpsertPlugin {
                    name: name.clone(),
                    marketplace: Some(marketplace.clone()),
                }),
            );
            // Install where it is still absent, now that the id is unambiguous.
            let hosts: Vec<String> = world
                .detected()
                .filter(|(h, s)| h.descriptor.plugins.is_some() && !s.plugins.contains_key(&name))
                .map(|(h, _)| h.name().to_string())
                .collect();
            install(world, &name, Some(marketplace), &hosts, plan);
        }

        ActionKind::Delete {
            hosts,
            from_manifest,
            ..
        } => {
            for (host, snap) in world.detected() {
                let hname = host.name().to_string();
                if !hosts.contains(&hname) {
                    continue;
                }
                let marketplace = snap.plugins.get(&name).map(|p| p.marketplace.clone());
                match host.plugin_remove_argv(&name, marketplace.as_deref()) {
                    Ok(argv) => plan.push(
                        format!("uninstall {name} from {hname}"),
                        Step::Host {
                            host: hname,
                            argv,
                            cwd: None,
                        },
                    ),
                    Err(e) => plan.note(format!("{name}: {hname} \u{2014} {e:#}")),
                }
            }
            if let Some(entry) = world.manifest.plugins.get(&name) {
                let targeted: Vec<String> = world
                    .detected()
                    .filter(|(h, _)| h.descriptor.plugins.is_some() && entry.targets_host(h.name()))
                    .map(|(h, _)| h.name().to_string())
                    .collect();
                let remaining: Vec<String> = targeted
                    .iter()
                    .filter(|h| !hosts.contains(h))
                    .cloned()
                    .collect();
                if *from_manifest || remaining.is_empty() {
                    plan.push(
                        format!("drop {name} from the manifest"),
                        Step::Manifest(ManifestOp::RemovePlugin(name.clone())),
                    );
                } else if remaining.len() < targeted.len() {
                    plan.push(
                        format!("narrow {name} to {}", join_hosts(&remaining)),
                        Step::Manifest(ManifestOp::SetPluginHosts {
                            name: name.clone(),
                            hosts: Some(remaining),
                        }),
                    );
                }
            }
        }

        ActionKind::KeepDivergent { hosts } => {
            if world.manifest.plugins.contains_key(&name) {
                plan.push(
                    format!("record {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::SetPluginHosts {
                        name: name.clone(),
                        hosts: Some(hosts.clone()),
                    }),
                );
            } else {
                plan.push(
                    format!("adopt {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::UpsertPlugin {
                        name: name.clone(),
                        marketplace: None,
                    }),
                );
                plan.push(
                    format!("record {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::SetPluginHosts {
                        name: name.clone(),
                        hosts: Some(hosts.clone()),
                    }),
                );
            }
        }

        _ => plan.note(format!("{name}: that action does not apply to plugins")),
    }
}

fn install(world: &World, name: &str, pin: Option<&str>, hosts: &[String], plan: &mut Plan) {
    for (host, _) in world.detected() {
        let hname = host.name().to_string();
        if !hosts.contains(&hname) || host.descriptor.plugins.is_none() {
            continue;
        }
        // Always resolve to `name@marketplace`. Passing a bare name is what made
        // 17 installs fail: neither CLI resolves one.
        let marketplace = match resolve(world, &hname, name, pin) {
            Resolution::One(m) => m,
            Resolution::Ambiguous(markets) => {
                plan.note(format!(
                    "{name}: skipped {hname} \u{2014} offered by {}; pin one with \
                     marketplace = \"...\"",
                    markets.join(", ")
                ));
                continue;
            }
            Resolution::None => {
                plan.note(format!(
                    "{name}: skipped {hname} \u{2014} no configured marketplace offers it{}",
                    pin.map(|p| format!(" (pinned to {p})")).unwrap_or_default()
                ));
                continue;
            }
        };
        let marketplace = (!marketplace.is_empty()).then_some(marketplace);
        match host.plugin_install_argv(name, marketplace.as_deref()) {
            Ok(argv) => plan.push(
                format!("install {name} in {hname}"),
                Step::Host {
                    host: hname,
                    argv,
                    cwd: None,
                },
            ),
            Err(e) => plan.note(format!("{name}: {hname} \u{2014} {e:#}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Npm and local plugin targets (OpenCode family)
// ---------------------------------------------------------------------------
//
// A separate mechanism from the marketplace-resolved installs above: OpenCode
// and Kilo have no marketplace to resolve a bare plugin name against, so a
// mapping from a manifest plugin to one of these hosts must be named
// explicitly as a `targets.<host>` table. This code never invents one.

/// The key an occurrence is looked up under: an npm spec is looked up by
/// itself; a local target is looked up by the host-owned destination name
/// agentsync would copy it to, since that is what actually appears on disk.
fn occurrence_key(name: &str, identity: &PluginIdentity) -> String {
    match identity {
        PluginIdentity::Npm(spec) => spec.clone(),
        PluginIdentity::Local(_) => format!("agentsync-{name}"),
    }
}

fn target_rows(world: &World) -> Vec<Row> {
    world
        .manifest
        .plugins
        .iter()
        .filter(|(_, entry)| !entry.targets.is_empty())
        .filter_map(|(name, entry)| target_row(world, name, entry))
        .collect()
}

fn target_row(world: &World, name: &str, entry: &PluginEntry) -> Option<Row> {
    let mut missing = Vec::new();
    let mut present = Vec::new();
    let mut duplicate: Vec<(String, usize)> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    // A local target whose destination directory already exists and carries
    // no agentsync ownership marker. Never auto-claimed: reported and left
    // for explicit user action instead.
    let mut blocked: Vec<String> = Vec::new();

    for (host, target) in &entry.targets {
        let Some(snap) = world.snapshot(host) else {
            // Not installed: absent is not divergent.
            continue;
        };
        let Some(identity) = target.identity() else {
            ambiguous.push(host.clone());
            continue;
        };
        let key = occurrence_key(name, &identity);
        let count = snap.plugin_targets.occurrences.get(&key).map(Vec::len);
        if matches!(identity, PluginIdentity::Local(_))
            && matches!(count, None | Some(0))
            && snap
                .plugin_targets
                .local_dir_claimable
                .get(&target.scope)
                .is_some_and(|claimable| !claimable)
        {
            blocked.push(host.clone());
            continue;
        }
        match count {
            None | Some(0) => missing.push(host.clone()),
            Some(1) => present.push(host.clone()),
            Some(n) => duplicate.push((host.clone(), n)),
        }
    }

    if missing.is_empty() && duplicate.is_empty() && ambiguous.is_empty() && blocked.is_empty() {
        if present.is_empty() {
            return None;
        }
        let mut row = Row::synced_removable(
            Domain::Plugins,
            name,
            "npm/local target(s) in sync".to_string(),
            removal_actions(&present, "remove", false),
        );
        row.key = RowKey {
            is_target: true,
            ..Default::default()
        };
        return Some(row);
    }

    let (severity, headline) = if !blocked.is_empty() {
        (
            Severity::Blocked,
            format!(
                "an existing, unowned directory blocks copying to {}",
                join_hosts(&blocked)
            ),
        )
    } else if !ambiguous.is_empty() {
        (
            Severity::Warn,
            format!(
                "target on {} names neither or both of npm/local",
                join_hosts(&ambiguous)
            ),
        )
    } else if !duplicate.is_empty() {
        (
            Severity::Warn,
            format!(
                "duplicate occurrences on {}",
                join_hosts(&duplicate.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>())
            ),
        )
    } else {
        (
            Severity::Normal,
            format!("missing from {}", join_hosts(&missing)),
        )
    };

    let mut actions = Vec::new();
    if !missing.is_empty() {
        actions.push(Action::new(
            format!("install target(s) on {}", join_hosts(&missing)),
            ActionKind::Push {
                hosts: missing.clone(),
            },
        ));
    }
    if ambiguous.is_empty() && duplicate.is_empty() && blocked.is_empty() && !present.is_empty() {
        actions.extend(removal_actions(&present, "remove", false));
    }
    if actions.is_empty() {
        actions.push(Action::new("leave it", ActionKind::Nothing));
    }

    Some(Row {
        domain: Domain::Plugins,
        name: name.to_string(),
        headline,
        detail: String::new(),
        severity,
        actions,
        chosen: 0,
        accepted: false,
        key: RowKey {
            is_target: true,
            ..Default::default()
        },
    })
}

fn plan_target_row(world: &World, row: &Row, plan: &mut Plan) {
    let name = row.name.clone();
    let Some(entry) = world.manifest.plugins.get(&name).cloned() else {
        return;
    };

    match &row.action().kind {
        ActionKind::Nothing => {}
        ActionKind::Push { hosts } => {
            for host in hosts {
                push_one_target(world, &name, host, &entry, plan);
            }
        }
        ActionKind::Delete { hosts, .. } => {
            for host in hosts {
                remove_one_target(world, &name, host, &entry, plan);
            }
        }
        _ => plan.note(format!(
            "{name}: that action does not apply to an npm/local plugin target"
        )),
    }
}

fn push_one_target(world: &World, name: &str, host: &str, entry: &PluginEntry, plan: &mut Plan) {
    let Some(target) = entry.targets.get(host) else {
        return;
    };
    let Some(identity) = target.identity() else {
        plan.note(format!(
            "{name}: the {host} target names neither or both of npm/local; skipped"
        ));
        return;
    };
    let Some(snap) = world.snapshot(host) else {
        return;
    };
    match identity {
        PluginIdentity::Npm(spec) => push_npm_target(world, name, host, target, &spec, snap, plan),
        PluginIdentity::Local(source) => {
            push_local_target(world, name, host, target, &source, plan)
        }
    }
}

fn remove_one_target(world: &World, name: &str, host: &str, entry: &PluginEntry, plan: &mut Plan) {
    let Some(target) = entry.targets.get(host) else {
        return;
    };
    let Some(identity) = target.identity() else {
        return;
    };
    let Some(snap) = world.snapshot(host) else {
        return;
    };
    match identity {
        PluginIdentity::Npm(spec) => remove_npm_target(name, host, target, &spec, snap, plan),
        PluginIdentity::Local(_) => remove_local_target(world, name, host, target, plan),
    }
}

fn npm_transaction(
    name: &str,
    host: &str,
    target: &PluginTarget,
    spec: &str,
    snap: &HostSnapshot,
    remove: bool,
    plan: &mut Plan,
) {
    let Some(source) = snap.plugin_targets.config.get(&target.scope) else {
        plan.note(format!(
            "{name}: {host} has no writable plugin config source for {:?} scope",
            target.scope
        ));
        return;
    };
    let Some(path) = source.origin.path.clone() else {
        plan.note(format!("{name}: {host}'s plugin config source has no path"));
        return;
    };

    let entries: Vec<ocp::PluginArrayEntry> = source
        .entries
        .iter()
        .map(|(identity, raw)| ocp::PluginArrayEntry {
            identity: identity.clone(),
            raw: raw.clone(),
        })
        .collect();
    let desired_text = if remove {
        ocp::remove_from_plugin_array_json(&entries, spec)
    } else {
        ocp::upsert_plugin_array_json(&entries, spec, serde_json::Value::String(spec.to_string()))
    };
    let Ok(desired_value) = serde_json::from_str::<serde_json::Value>(&desired_text) else {
        plan.note(format!("{name}: could not render the {host} plugin array"));
        return;
    };

    let exists = ocp::config_source_exists(&path);
    let mut document = if exists {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| crate::jsonc::parse(&text).map_err(|e| e.to_string()))
        {
            Ok(doc) => doc.value,
            Err(e) => {
                plan.note(format!(
                    "{name}: reading {host}'s plugin config \u{2014} {e}"
                ));
                return;
            }
        }
    } else {
        serde_json::json!({})
    };
    document["plugin"] = desired_value.clone();

    let precondition = if exists {
        FilePrecondition::Sha256(source.origin.hash.clone())
    } else {
        FilePrecondition::Absent
    };

    let transaction = ConfigTransaction::new(document)
        .with_source(GuardedSource::new(path, precondition))
        .with_edit(SourceEdit {
            origin: source.origin.clone(),
            config_path: vec!["plugin".into()],
            operation: ConfigEditOperation::Set {
                value: desired_value,
                raw_json: Some(desired_text),
            },
        });

    let verb = if remove { "remove" } else { "add" };
    plan.push(
        format!(
            "{verb} npm target {spec} {} {host}'s plugin config",
            if remove { "from" } else { "in" }
        ),
        Step::ConfigTransaction(transaction),
    );
}

fn push_npm_target(
    _world: &World,
    name: &str,
    host: &str,
    target: &PluginTarget,
    spec: &str,
    snap: &HostSnapshot,
    plan: &mut Plan,
) {
    npm_transaction(name, host, target, spec, snap, false, plan);
}

fn remove_npm_target(
    name: &str,
    host: &str,
    target: &PluginTarget,
    spec: &str,
    snap: &HostSnapshot,
    plan: &mut Plan,
) {
    npm_transaction(name, host, target, spec, snap, true, plan);
}

fn push_local_target(
    world: &World,
    name: &str,
    host: &str,
    target: &PluginTarget,
    source_rel: &str,
    plan: &mut Plan,
) {
    let _ = source_rel;
    let Some(family) = Family::from_host_name(host) else {
        plan.note(format!("{name}: {host} is not an OpenCode-family host"));
        return;
    };
    let Some(snap) = world.snapshot(host) else {
        return;
    };
    let Some(profile_dir) = snap.plugin_targets.profile_dir.get(&target.scope) else {
        plan.note(format!(
            "{name}: {host} has no profile directory for {:?} scope",
            target.scope
        ));
        return;
    };
    let Some(source_path) = target.resolve_local(&world.manifest_dir()) else {
        plan.note(format!("{name}: target for {host} has no local source"));
        return;
    };
    let content = match std::fs::read(&source_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            plan.note(format!(
                "{name}: reading local source {} \u{2014} {e}",
                source_path.display()
            ));
            return;
        }
    };
    let destination = ocp::host_owned_local_path(family, profile_dir, name, &source_path);
    let Some(plugin_dir) = destination.parent() else {
        plan.note(format!(
            "{name}: {host}'s destination has no parent directory"
        ));
        return;
    };

    // A destination under a host's own config tree is not automatically
    // agentsync-owned. `claim_fresh_directory` only ever succeeds when the
    // directory does not exist yet — that is the one legitimate way to
    // establish ownership, because there is no pre-existing content to
    // endanger. A directory that already exists must already carry the
    // marker (from an earlier, legitimate claim); otherwise this is a
    // pre-existing, unowned directory that must never be silently claimed or
    // written into. The row-level check should already have kept this
    // function from being called in that case, but the guard here is not
    // optional: it is what actually stops the write, in depth.
    if !ocp::local_dir_claimable(plugin_dir) {
        plan.note(format!(
            "{name}: {} already exists and is not agentsync-owned; refusing to claim it",
            plugin_dir.display()
        ));
        return;
    }

    let mut transaction = FileTransaction::new();
    if !plugin_dir.is_dir() {
        transaction = transaction.claim_fresh_directory(plugin_dir);
    }
    let precondition = match std::fs::read(&destination) {
        Ok(existing) => FilePrecondition::Sha256(compute_sha256(&existing)),
        Err(_) => FilePrecondition::Absent,
    };
    transaction = transaction.write(&destination, content, precondition);
    plan.push(
        format!(
            "copy local target {name} to {host} ({})",
            destination.display()
        ),
        Step::FileTransaction(transaction),
    );
}

fn remove_local_target(
    world: &World,
    name: &str,
    host: &str,
    target: &PluginTarget,
    plan: &mut Plan,
) {
    let Some(family) = Family::from_host_name(host) else {
        return;
    };
    let Some(snap) = world.snapshot(host) else {
        return;
    };
    let Some(profile_dir) = snap.plugin_targets.profile_dir.get(&target.scope) else {
        return;
    };
    let Some(source_path) = target.resolve_local(&world.manifest_dir()) else {
        return;
    };
    let destination = ocp::host_owned_local_path(family, profile_dir, name, &source_path);
    let Ok(existing) = std::fs::read(&destination) else {
        plan.note(format!(
            "{name}: nothing to remove at {} on {host}",
            destination.display()
        ));
        return;
    };
    let transaction = FileTransaction::new().remove(
        &destination,
        FilePrecondition::Sha256(compute_sha256(&existing)),
    );
    plan.push(
        format!(
            "remove local target {name} from {host} ({})",
            destination.display()
        ),
        Step::FileTransaction(transaction),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_marketplace_display_prefix() {
        assert_eq!(marketplace_name("marketplace i-have-adhd"), "i-have-adhd");
        assert_eq!(marketplace_name("superpowers"), "superpowers");
    }
}
