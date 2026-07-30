//! The MCP server domain.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::core::diff::{Action, ActionKind, Domain, Row, RowKey, Severity, join_hosts};
use crate::core::model::{Cap, McpServer, Scope, ScopeKind, Transport, short_repo};
use crate::core::plan::{ManifestOp, Plan, Step};
use crate::hosts::Host;
use crate::manifest::{McpEntry, secrets};

use super::World;

/// Why a host cannot hold this server. `None` means it can.
fn block_reason(host: &Host, server: &McpServer, scope: &Scope) -> Option<String> {
    let mcp = host.descriptor.mcp.as_ref()?;
    if !mcp.supports_scope(scope.kind()) {
        return Some(format!("no {} scope", scope.cli_name()));
    }
    let missing = mcp.missing_caps(&server.required_caps());
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|c| c.as_str()).collect();
        return Some(format!("unsupported: {}", names.join(", ")));
    }
    None
}

/// A literal credential sitting in a server definition, and the header holding it.
fn literal_secret(server: &McpServer) -> Option<(String, &'static str)> {
    match &server.transport {
        Transport::Http(h) => h
            .headers
            .iter()
            .find_map(|(k, v)| secrets::inspect(v).map(|why| (k.clone(), why))),
        Transport::Stdio(s) => s
            .env
            .iter()
            .find_map(|(k, v)| secrets::inspect(v).map(|why| (k.clone(), why))),
    }
}

/// `upskillai-knowledge` -> `UPSKILLAI_KNOWLEDGE_TOKEN`.
fn suggest_env_var(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{base}_TOKEN")
}

fn repo_note(scopes: &[Scope]) -> Option<String> {
    let repos: BTreeSet<&str> = scopes.iter().filter_map(Scope::repo).collect();
    match repos.len() {
        0 => None,
        1 => Some(format!(
            "1 repo ({})",
            short_repo(repos.iter().next().unwrap())
        )),
        n => Some(format!("{n} repos")),
    }
}

pub(super) fn rows(world: &World) -> Vec<Row> {
    let mut names: BTreeSet<String> = world.manifest.mcp.keys().cloned().collect();
    for snap in world.detected_snapshots() {
        names.extend(snap.mcp.keys().map(|(_, n)| n.clone()));
    }

    names
        .into_iter()
        .filter_map(|name| row_for(world, &name))
        .collect()
}

fn row_for(world: &World, name: &str) -> Option<Row> {
    // What each detected host currently holds, per scope.
    let mut host_entries: BTreeMap<String, Vec<(Scope, McpServer)>> = BTreeMap::new();
    for (host, snap) in world.detected() {
        let mut found: Vec<(Scope, McpServer)> = snap
            .mcp
            .iter()
            .filter(|((_, n), _)| n == name)
            .map(|((s, _), srv)| (s.clone(), srv.clone()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        if !found.is_empty() {
            host_entries.insert(host.name().to_string(), found);
        }
    }

    match world.manifest.mcp.get(name) {
        None => unmanaged_row(world, name, &host_entries),
        Some(entry) => managed_row(world, name, entry, &host_entries),
    }
}

/// A server that exists on at least one host but is not in the manifest.
fn unmanaged_row(
    world: &World,
    name: &str,
    host_entries: &BTreeMap<String, Vec<(Scope, McpServer)>>,
) -> Option<Row> {
    if host_entries.is_empty() {
        return None;
    }
    let present: Vec<String> = host_entries.keys().cloned().collect();
    let (source_host, source) = host_entries
        .iter()
        .next()
        .map(|(h, v)| (h.clone(), v[0].clone()))?;
    let all_scopes: Vec<Scope> = host_entries
        .values()
        .flat_map(|v| v.iter().map(|(s, _)| s.clone()))
        .collect();
    let needs_promote = all_scopes.iter().any(|s| s.kind() != ScopeKind::User);

    let mut detail = source.1.summary();
    let mut extra = Vec::new();

    // Which hosts could not take it even if we adopted it.
    let blocked: Vec<String> = world
        .detected()
        .filter(|(h, _)| !present.contains(&h.name().to_string()))
        .filter_map(|(h, _)| {
            block_reason(h, &source.1, &source.0).map(|why| format!("{}: {why}", h.name()))
        })
        .collect();
    if !blocked.is_empty() {
        extra.push(format!("blocked \u{2014} {}", blocked.join("; ")));
    }

    // Shadowing: the same name at two scopes on one host. One silently wins.
    let shadowing: Vec<(String, Vec<Scope>)> = host_entries
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(h, v)| (h.clone(), v.iter().map(|(s, _)| s.clone()).collect()))
        .collect();

    let secret = literal_secret(&source.1);

    let mut actions = Vec::new();
    let severity;
    let headline;

    if let Some((header, why)) = &secret {
        severity = Severity::Warn;
        headline = format!("credential in the clear on {source_host}");
        extra.push(format!("{header}: {why}"));
        let var = suggest_env_var(name);
        actions.push(Action::new(
            format!("adopt with the token moved to ${var}"),
            ActionKind::SecretToEnv { var },
        ));
        actions.push(Action::new(
            "delete everywhere",
            ActionKind::Delete {
                hosts: present.clone(),
                from_manifest: false,
                purge: false,
            },
        ));
    } else if let Some((host, scopes)) = shadowing.first() {
        severity = Severity::Warn;
        headline = format!("defined at {} scopes on {host}", scopes.len());
        extra.push(format!(
            "one silently wins; scopes: {}",
            scopes
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // Default to one global definition. Collapsing to *a* scope is also
        // offered, but which per-repo copy survives is arbitrary, so it must not
        // be what happens when you hold down the accept key.
        actions.push(Action::new(
            "adopt + make global (drops the duplicates)",
            ActionKind::Adopt {
                push: true,
                promote: true,
            },
        ));
        for scope in scopes {
            actions.push(Action::new(
                format!("collapse to {scope}"),
                ActionKind::CollapseScope {
                    keep: scope.clone(),
                },
            ));
        }
    } else {
        severity = Severity::Normal;
        // "only in X" is a lie when every host has it — what is missing then is
        // the manifest entry, not a host.
        let everywhere = world
            .detected()
            .filter(|(h, _)| h.descriptor.mcp.is_some())
            .all(|(h, _)| present.contains(&h.name().to_string()));
        let where_ = if everywhere {
            "not in the manifest yet".to_string()
        } else {
            format!("only in {}", join_hosts(&present))
        };
        headline = match repo_note(&all_scopes) {
            Some(note) => format!("{where_}, {note}"),
            None => where_,
        };
        actions.push(Action::new(
            match (needs_promote, everywhere) {
                (true, _) => "adopt + make global",
                (false, true) => "adopt into the manifest",
                (false, false) => "adopt + add to the others",
            },
            ActionKind::Adopt {
                push: true,
                promote: needs_promote,
            },
        ));
        actions.push(Action::new(
            if needs_promote {
                "adopt, keep it per-repo"
            } else {
                "adopt only, don't push"
            },
            ActionKind::Adopt {
                push: false,
                promote: false,
            },
        ));
        if !everywhere {
            actions.push(Action::new(
                format!("keep {}-only", join_hosts(&present)),
                ActionKind::KeepDivergent {
                    hosts: present.clone(),
                },
            ));
        }
        actions.push(Action::new(
            "delete everywhere",
            ActionKind::Delete {
                hosts: present.clone(),
                from_manifest: false,
                purge: false,
            },
        ));
    }

    if !extra.is_empty() {
        detail = format!("{detail}   \u{b7}   {}", extra.join("   \u{b7}   "));
    }

    Some(Row {
        domain: Domain::Mcp,
        name: name.to_string(),
        headline,
        detail,
        severity,
        actions,
        chosen: 0,
        accepted: false,
        key: RowKey {
            host_scopes: all_scopes,
            source_host: Some(source_host),
            ..Default::default()
        },
    })
}

/// A server the manifest knows about.
fn managed_row(
    world: &World,
    name: &str,
    entry: &McpEntry,
    host_entries: &BTreeMap<String, Vec<(Scope, McpServer)>>,
) -> Option<Row> {
    let want = entry.to_server(name).ok()?;
    let want_scopes = entry.scopes();
    let mut detail = want.summary();
    let mut extra = Vec::new();

    let mut missing: Vec<String> = Vec::new();
    let mut differing: Vec<String> = Vec::new();
    let mut misscoped: Vec<String> = Vec::new();
    let mut blocked: Vec<(String, String)> = Vec::new();
    let mut capable: Vec<String> = Vec::new();
    let mut all_host_scopes: Vec<Scope> = Vec::new();

    for (host, _) in world.detected() {
        let hname = host.name().to_string();
        if !entry.targets_host(&hname) {
            continue;
        }

        // A host that cannot represent this server is blocked, never pushed to
        // with the unrepresentable part quietly dropped.
        let representative_scope = want_scopes.first().cloned().unwrap_or(Scope::User);
        if let Some(why) = block_reason(host, &want, &representative_scope) {
            blocked.push((hname, why));
            continue;
        }
        capable.push(hname.clone());

        let have = host_entries.get(&hname).cloned().unwrap_or_default();
        all_host_scopes.extend(have.iter().map(|(s, _)| s.clone()));

        if have.is_empty() {
            missing.push(hname);
            continue;
        }
        if have.len() > 1 {
            extra.push(format!(
                "{hname}: defined at {} scopes, one silently wins",
                have.len()
            ));
        }
        let have_scopes: Vec<Scope> = have.iter().map(|(s, _)| s.clone()).collect();
        if have_scopes != want_scopes {
            misscoped.push(hname.clone());
        }
        if have.iter().any(|(_, srv)| *srv != want) {
            differing.push(hname);
        }
    }

    if !blocked.is_empty() {
        extra.push(format!(
            "blocked \u{2014} {}",
            blocked
                .iter()
                .map(|(h, w)| format!("{h}: {w}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    // Priority: a credential in the clear beats a value difference, which beats
    // a scope difference, which beats absence. One row, most severe headline.
    let manifest_secret = literal_secret(&want);
    let host_secret = host_entries.iter().find_map(|(h, v)| {
        v.iter()
            .find_map(|(_, s)| literal_secret(s).map(|x| (h.clone(), x)))
    });

    let key = RowKey {
        host_scopes: all_host_scopes,
        source_host: differing
            .first()
            .cloned()
            .or_else(|| host_entries.keys().next().cloned()),
        ..Default::default()
    };

    let (severity, headline, actions) = if let Some((header, why)) = manifest_secret {
        extra.push(format!("manifest {header}: {why}"));
        let var = suggest_env_var(name);
        (
            Severity::Warn,
            "credential in the clear in the manifest".to_string(),
            vec![
                Action::new(
                    format!("move the token to ${var} and re-push"),
                    ActionKind::SecretToEnv { var },
                ),
                Action::new("leave it", ActionKind::Nothing),
            ],
        )
    } else if let Some((host, (header, why))) = host_secret {
        extra.push(format!("{host} {header}: {why}"));
        let var = suggest_env_var(name);
        (
            Severity::Warn,
            format!("credential in the clear on {host}"),
            vec![
                Action::new(
                    format!("move the token to ${var} and re-push"),
                    ActionKind::SecretToEnv { var },
                ),
                Action::new("leave it", ActionKind::Nothing),
            ],
        )
    } else if !differing.is_empty() {
        let host = differing[0].clone();
        if let Some(have) = host_entries.get(&host).and_then(|v| v.first()) {
            let fields = want.diff(&have.1);
            if !fields.is_empty() {
                extra.push(
                    fields
                        .iter()
                        .map(|f| {
                            format!(
                                "{}: manifest {:?} vs {host} {:?}",
                                f.field, f.manifest, f.host
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("   \u{b7}   "),
                );
            }
        }
        (
            Severity::Normal,
            format!("differs on {}", join_hosts(&differing)),
            vec![
                Action::new(
                    format!("push the manifest value to {}", join_hosts(&differing)),
                    ActionKind::Push {
                        hosts: differing.clone(),
                    },
                ),
                Action::new(
                    format!("adopt {host}'s value instead"),
                    ActionKind::AdoptFrom { host },
                ),
                Action::new(
                    "keep them divergent",
                    ActionKind::KeepDivergent {
                        hosts: capable.clone(),
                    },
                ),
            ],
        )
    } else if !misscoped.is_empty() {
        (
            Severity::Normal,
            format!("wrong scope on {}", join_hosts(&misscoped)),
            vec![
                Action::new(
                    format!(
                        "move to {} on {}",
                        scope_label(&want_scopes),
                        join_hosts(&misscoped)
                    ),
                    ActionKind::Push {
                        hosts: misscoped.clone(),
                    },
                ),
                Action::new(
                    format!("adopt {}'s scope", misscoped[0]),
                    ActionKind::AdoptFrom {
                        host: misscoped[0].clone(),
                    },
                ),
            ],
        )
    } else if !missing.is_empty() {
        (
            Severity::Normal,
            format!("missing from {}", join_hosts(&missing)),
            vec![
                Action::new(
                    format!("add to {}", join_hosts(&missing)),
                    ActionKind::Push {
                        hosts: missing.clone(),
                    },
                ),
                Action::new(
                    format!("keep {}-only", join_hosts(&present_of(&capable, &missing))),
                    ActionKind::KeepDivergent {
                        hosts: present_of(&capable, &missing),
                    },
                ),
                Action::new(
                    "delete everywhere",
                    ActionKind::Delete {
                        hosts: host_entries.keys().cloned().collect(),
                        from_manifest: true,
                        purge: false,
                    },
                ),
            ],
        )
    } else if capable.is_empty() && !blocked.is_empty() {
        // Nothing can hold it. Recording the divergence is the only honest fix.
        (
            Severity::Blocked,
            format!(
                "no host can represent this ({})",
                blocked
                    .iter()
                    .map(|(_, w)| w.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            vec![Action::new("leave it", ActionKind::Nothing)],
        )
    } else if !blocked.is_empty() {
        (
            Severity::Blocked,
            format!(
                "in sync on {}; {} cannot hold it",
                join_hosts(&capable),
                join_hosts(&blocked.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>())
            ),
            vec![
                Action::new(
                    format!("record it as {}-only", join_hosts(&capable)),
                    ActionKind::KeepDivergent {
                        hosts: capable.clone(),
                    },
                ),
                Action::new("leave it", ActionKind::Nothing),
            ],
        )
    } else {
        if !extra.is_empty() {
            detail = format!("{detail}   \u{b7}   {}", extra.join("   \u{b7}   "));
        }
        let mut row = Row::synced(Domain::Mcp, name, detail);
        row.key = key;
        return Some(row);
    };

    if !extra.is_empty() {
        detail = format!("{detail}   \u{b7}   {}", extra.join("   \u{b7}   "));
    }

    Some(Row {
        domain: Domain::Mcp,
        name: name.to_string(),
        headline,
        detail,
        severity,
        actions,
        chosen: 0,
        accepted: false,
        key,
    })
}

fn scope_label(scopes: &[Scope]) -> String {
    match scopes.first() {
        Some(s) => s.to_string(),
        None => "user".to_string(),
    }
}

fn present_of(capable: &[String], missing: &[String]) -> Vec<String> {
    capable
        .iter()
        .filter(|h| !missing.contains(h))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub(super) fn plan_row(world: &World, row: &Row, plan: &mut Plan) {
    let name = row.name.clone();
    match &row.action().kind {
        ActionKind::Nothing => {}

        ActionKind::Adopt { push, promote } => {
            let Some((source_host, scope, server)) = source_of(world, &name, &row.key) else {
                plan.note(format!("{name}: no host value to adopt"));
                return;
            };
            let (kind, repos) = if *promote {
                (ScopeKind::User, Vec::new())
            } else {
                (scope.kind(), repos_of(&row.key.host_scopes))
            };
            let entry = McpEntry::from_server(&server, kind, repos);
            plan.push(
                format!("adopt {name} from {source_host}"),
                Step::Manifest(ManifestOp::UpsertMcp {
                    name: name.clone(),
                    entry: Box::new(entry.clone()),
                }),
            );

            if *promote {
                // Remove the per-repo definitions we are replacing, before the
                // global one lands. A host that already holds it at user scope is
                // left alone — removing and re-adding it would be pure churn, and
                // would briefly delete a correct entry.
                remove_from_hosts(world, &name, &row.key.host_scopes, plan, |scope| {
                    scope.kind() != ScopeKind::User
                });
            }
            if *push {
                push_entry(world, &name, &entry, plan, *promote);
            }
        }

        ActionKind::AdoptFrom { host } => {
            let Some((_, scope, server)) = source_of(
                world,
                &name,
                &RowKey {
                    source_host: Some(host.clone()),
                    ..row.key.clone()
                },
            ) else {
                plan.note(format!("{name}: {host} has no value to adopt"));
                return;
            };
            let entry = McpEntry::from_server(
                &server,
                scope.kind(),
                repos_of(std::slice::from_ref(&scope)),
            );
            plan.push(
                format!("adopt {name} from {host}"),
                Step::Manifest(ManifestOp::UpsertMcp {
                    name: name.clone(),
                    entry: Box::new(entry.clone()),
                }),
            );
            push_entry(world, &name, &entry, plan, true);
        }

        ActionKind::Push { hosts } => {
            let Some(entry) = world.manifest.mcp.get(&name) else {
                plan.note(format!("{name}: not in the manifest, nothing to push"));
                return;
            };
            // Drop definitions at scopes the manifest does not want, first.
            let want = entry.scopes();
            remove_from_hosts(world, &name, &row.key.host_scopes, plan, |scope| {
                !want.contains(scope)
            });
            push_entry_to(world, &name, entry, hosts, plan, true);
        }

        ActionKind::Delete {
            hosts,
            from_manifest,
            ..
        } => {
            let scopes = if row.key.host_scopes.is_empty() {
                world
                    .manifest
                    .mcp
                    .get(&name)
                    .map(|e| e.scopes())
                    .unwrap_or_default()
            } else {
                row.key.host_scopes.clone()
            };
            for (host, _) in world.detected() {
                if !hosts.contains(&host.name().to_string()) {
                    continue;
                }
                for scope in dedup(&scopes) {
                    if let Ok(argv) = host.mcp_remove_argv(&name, &scope) {
                        plan.push(
                            format!("remove {name} from {} ({scope})", host.name()),
                            Step::Host {
                                host: host.name().to_string(),
                                argv,
                                cwd: cwd_for(&scope),
                            },
                        );
                    }
                }
            }
            if *from_manifest {
                plan.push(
                    format!("drop {name} from the manifest"),
                    Step::Manifest(ManifestOp::RemoveMcp(name.clone())),
                );
            }
        }

        ActionKind::KeepDivergent { hosts } => {
            if world.manifest.mcp.contains_key(&name) {
                plan.push(
                    format!("record {name} as {}-only", join_hosts(hosts)),
                    Step::Manifest(ManifestOp::SetMcpHosts {
                        name: name.clone(),
                        hosts: Some(hosts.clone()),
                    }),
                );
            } else if let Some((source_host, scope, server)) = source_of(world, &name, &row.key) {
                let mut entry =
                    McpEntry::from_server(&server, scope.kind(), repos_of(&row.key.host_scopes));
                entry.hosts = Some(hosts.clone());
                plan.push(
                    format!(
                        "adopt {name} from {source_host} as {}-only",
                        join_hosts(hosts)
                    ),
                    Step::Manifest(ManifestOp::UpsertMcp {
                        name: name.clone(),
                        entry: Box::new(entry),
                    }),
                );
            }
        }

        ActionKind::CollapseScope { keep } => {
            remove_from_hosts(world, &name, &row.key.host_scopes, plan, |scope| {
                scope != keep
            });
        }

        ActionKind::SecretToEnv { var } => {
            // Land a corrected definition in the manifest, then push it so the
            // host's own config stops holding the literal too.
            let entry = match world.manifest.mcp.get(&name) {
                Some(existing) => {
                    plan.push(
                        format!("point {name} at ${var}"),
                        Step::Manifest(ManifestOp::SetMcpBearerEnv {
                            name: name.clone(),
                            var: var.clone(),
                        }),
                    );
                    let mut e = existing.clone();
                    e.headers
                        .retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
                    e.bearer_token_env = Some(var.clone());
                    e
                }
                None => {
                    let Some((source_host, scope, server)) = source_of(world, &name, &row.key)
                    else {
                        plan.note(format!("{name}: no host value to adopt"));
                        return;
                    };
                    let mut e = McpEntry::from_server(
                        &server,
                        scope.kind(),
                        repos_of(&row.key.host_scopes),
                    );
                    e.headers
                        .retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
                    e.env.retain(|_, v| secrets::inspect(v).is_none());
                    e.bearer_token_env = Some(var.clone());
                    plan.push(
                        format!("adopt {name} from {source_host} with the token in ${var}"),
                        Step::Manifest(ManifestOp::UpsertMcp {
                            name: name.clone(),
                            entry: Box::new(e.clone()),
                        }),
                    );
                    e
                }
            };

            push_entry(world, &name, &entry, plan, true);
            plan.push(
                format!("set ${var} in your shell profile"),
                Step::Manual(format!(
                    "export {var}=<the token that was in the config>  \
                     \u{2014} the old literal is in the backup under {}",
                    crate::paths::contract(&crate::paths::backups_dir())
                )),
            );
        }
    }
}

/// The host value a row would adopt.
fn source_of(world: &World, name: &str, key: &RowKey) -> Option<(String, Scope, McpServer)> {
    let want_host = key.source_host.clone();
    for (host, snap) in world.detected() {
        if let Some(w) = &want_host
            && host.name() != w
        {
            continue;
        }
        let mut found: Vec<(Scope, McpServer)> = snap
            .mcp
            .iter()
            .filter(|((_, n), _)| n == name)
            .map(|((s, _), srv)| (s.clone(), srv.clone()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((scope, server)) = found.into_iter().next() {
            return Some((host.name().to_string(), scope, server));
        }
    }
    None
}

fn repos_of(scopes: &[Scope]) -> Vec<String> {
    let set: BTreeSet<String> = scopes
        .iter()
        .filter_map(|s| s.repo().map(str::to_string))
        .collect();
    set.into_iter().collect()
}

fn dedup(scopes: &[Scope]) -> Vec<Scope> {
    let set: BTreeSet<Scope> = scopes.iter().cloned().collect();
    set.into_iter().collect()
}

fn cwd_for(scope: &Scope) -> Option<PathBuf> {
    scope.repo().map(PathBuf::from)
}

fn push_entry(world: &World, name: &str, entry: &McpEntry, plan: &mut Plan, replace: bool) {
    let hosts: Vec<String> = world
        .detected()
        .map(|(h, _)| h.name().to_string())
        .filter(|h| entry.targets_host(h))
        .collect();
    push_entry_to(world, name, entry, &hosts, plan, replace);
}

/// Emit `add` steps for each target host at each manifest scope.
///
/// `replace` decides whether a host that already holds a *different* definition
/// gets overwritten. Overwriting is done as remove-then-add, because `add` is not
/// an upsert: `claude mcp add-json` exits 1 with "already exists in user config".
/// The removal lands first via [`Plan::finalize`], which orders removals ahead of
/// adds.
fn push_entry_to(
    world: &World,
    name: &str,
    entry: &McpEntry,
    hosts: &[String],
    plan: &mut Plan,
    replace: bool,
) {
    let Ok(server) = entry.to_server(name) else {
        plan.note(format!("{name}: manifest entry is not a valid server"));
        return;
    };
    let scopes = entry.scopes();
    if scopes.is_empty() {
        plan.note(format!(
            "{name}: scope is {:?} but no repos are listed, so there is nowhere to put it",
            entry.scope
        ));
        return;
    }

    for (host, snap) in world.detected() {
        let hname = host.name().to_string();
        if !hosts.contains(&hname) {
            continue;
        }
        for scope in &scopes {
            if let Some(why) = block_reason(host, &server, scope) {
                plan.note(format!("{name}: skipped {hname} \u{2014} {why}"));
                continue;
            }
            let existing = snap.mcp_at(scope, name);
            if existing.is_some_and(|s| *s == server) {
                continue;
            }
            if existing.is_some() {
                if !replace {
                    continue;
                }
                // `add` refuses to overwrite, so clear the old definition first.
                match host.mcp_remove_argv(name, scope) {
                    Ok(argv) => plan.push(
                        format!("replace {name} on {hname} ({scope}): remove the old definition"),
                        Step::Host {
                            host: hname.clone(),
                            argv,
                            cwd: cwd_for(scope),
                        },
                    ),
                    Err(e) => plan.note(format!(
                        "{name}: cannot build {hname} removal \u{2014} {e:#}"
                    )),
                }
            }
            match host.mcp_add_argv(&server, scope) {
                Ok(argv) => plan.push(
                    format!("add {name} to {hname} ({scope})"),
                    Step::Host {
                        host: hname.clone(),
                        argv,
                        cwd: cwd_for(scope),
                    },
                ),
                Err(e) => plan.note(format!(
                    "{name}: cannot build {hname} command \u{2014} {e:#}"
                )),
            }
        }
    }
}

fn remove_from_hosts(
    world: &World,
    name: &str,
    scopes: &[Scope],
    plan: &mut Plan,
    keep: impl Fn(&Scope) -> bool,
) {
    for (host, snap) in world.detected() {
        for scope in dedup(scopes) {
            if !keep(&scope) || snap.mcp_at(&scope, name).is_none() {
                continue;
            }
            if let Ok(argv) = host.mcp_remove_argv(name, &scope) {
                plan.push(
                    format!("remove {name} from {} ({scope})", host.name()),
                    Step::Host {
                        host: host.name().to_string(),
                        argv,
                        cwd: cwd_for(&scope),
                    },
                );
            }
        }
    }
}

/// Capabilities the manifest entry needs, for `doctor`.
pub fn needed_caps(entry: &McpEntry, name: &str) -> Vec<Cap> {
    entry
        .to_server(name)
        .map(|s| s.required_caps())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_a_screaming_snake_env_var() {
        assert_eq!(
            suggest_env_var("upskillai-knowledge"),
            "UPSKILLAI_KNOWLEDGE_TOKEN"
        );
        assert_eq!(suggest_env_var("kicad"), "KICAD_TOKEN");
    }

    #[test]
    fn repo_note_names_a_single_repo_and_counts_many() {
        assert_eq!(repo_note(&[Scope::User]), None);
        assert_eq!(
            repo_note(&[Scope::Local("/a/b/core/infra".into())]),
            Some("1 repo (core/infra)".to_string())
        );
        assert_eq!(
            repo_note(&[
                Scope::Local("/one".into()),
                Scope::Local("/two".into()),
                Scope::Local("/three".into()),
            ]),
            Some("3 repos".to_string())
        );
    }
}
