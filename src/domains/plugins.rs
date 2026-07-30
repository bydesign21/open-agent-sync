//! The plugins domain: marketplaces first, then the plugins installed from them.
//!
//! The subtlety here is that the curated registries are genuinely different
//! between hosts — `superpowers` exists in both `claude-plugins-official` and
//! `openai-api-curated` under different marketplace names, and plenty of plugins
//! exist in only one. So:
//!
//! * The manifest stores a **plugin name**, not a `name@marketplace` id, and the
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
use crate::core::model::MarketplaceSource;
use crate::core::plan::{ManifestOp, Plan, Step};
use crate::manifest::MarketplaceEntry;

use super::World;

pub(super) fn rows(world: &World) -> Vec<Row> {
    let mut out = marketplace_rows(world);
    out.extend(plugin_rows(world));
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
    let mut names: BTreeSet<String> = world.manifest.plugins.keys().cloned().collect();
    for (host, snap) in world.detected() {
        if host.descriptor.plugins.is_some() {
            names.extend(snap.plugins.keys().cloned());
        }
    }
    names
        .into_iter()
        .filter_map(|name| plugin_row(world, &name))
        .collect()
}

fn plugin_row(world: &World, name: &str) -> Option<Row> {
    let mut installed: BTreeMap<String, String> = BTreeMap::new();
    for (host, snap) in world.detected() {
        if host.descriptor.plugins.is_none() {
            continue;
        }
        if let Some(p) = snap.plugins.get(name) {
            installed.insert(host.name().to_string(), p.marketplace.clone());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_marketplace_display_prefix() {
        assert_eq!(marketplace_name("marketplace i-have-adhd"), "i-have-adhd");
        assert_eq!(marketplace_name("superpowers"), "superpowers");
    }
}
