//! The hooks domain: which of a host's hook handlers another host cannot run.
//!
//! A capability gap is only actionable when every missing capability has a named
//! shim strategy *and* the target can host a shim at all. Anything else is
//! blocked and names the capability, in the same spirit as `headers` for MCP.
//! A hook that silently does not run looks exactly like a hook that found
//! nothing, which is the worst outcome available for a security review.

use anyhow::Context;

use crate::core::diff::{Action, ActionKind, Domain, Row, Severity};
use crate::core::model::HookCap;
use crate::core::plan::{Plan, Step};
use crate::domains::World;

/// How the shim emulates a capability the target lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Evaluate the filter ourselves and exit without running the command when
    /// it does not match.
    Prefilter,
    /// Drop or fold the field on the way back out, driven by the target's
    /// declared `output` list.
    NormalizeOutput,
}

/// Capabilities a generated shim can emulate.
pub const SHIMMABLE: &[HookCap] = &[
    HookCap::If,
    HookCap::AsyncRewake,
    HookCap::RewakeMessage,
    HookCap::RewakeSummary,
];

pub fn strategy_for(cap: HookCap) -> Option<Strategy> {
    match cap {
        HookCap::If => Some(Strategy::Prefilter),
        HookCap::AsyncRewake | HookCap::RewakeMessage | HookCap::RewakeSummary => {
            Some(Strategy::NormalizeOutput)
        }
        // Without `matcher` the host never invokes the hook for the right tool
        // in the first place, so there is nothing for a shim to intercept.
        HookCap::Matcher => None,
        // A host that cannot express a timeout cannot be given one from outside.
        HookCap::Timeout => None,
    }
}

/// Severity for a gap, given whether the target can host a shim.
pub fn classify(missing: &[HookCap], target_can_shim: bool) -> Severity {
    if missing.is_empty() {
        return Severity::Synced;
    }
    if !target_can_shim {
        return Severity::Blocked;
    }
    if missing.iter().all(|c| strategy_for(*c).is_some()) {
        Severity::Normal
    } else {
        Severity::Blocked
    }
}

pub fn rows(world: &World) -> Vec<Row> {
    let mut out = Vec::new();
    for (source_host, source_snap) in world.detected() {
        for handler in source_snap.hooks.values() {
            for (target_host, _) in world.detected() {
                if target_host.name() == source_host.name() {
                    continue;
                }
                let Some(declared) = &target_host.descriptor.hooks else {
                    // A host that declares no `[hooks]` section at all can run
                    // no hooks whatsoever. That is not "nothing to report" —
                    // it is the most severe gap this domain can describe.
                    // Skipping it here would make a host that can run nothing
                    // look byte-identical to one that runs everything.
                    out.push(no_hook_engine_row(handler, target_host.name()));
                    continue;
                };
                let target = world.manifest.hooks_for(target_host.name(), declared);

                let mut row = if !target.supports_event(&handler.event) {
                    Some(blocked_event_row(handler, target_host.name()))
                } else {
                    let missing = target.missing_caps(&handler.required_caps());
                    match classify(&missing, target.can_shim()) {
                        Severity::Synced => None,
                        Severity::Blocked => {
                            Some(blocked_cap_row(handler, target_host.name(), &missing))
                        }
                        _ => Some(shim_row(
                            handler,
                            source_host.name(),
                            target_host.name(),
                            &missing,
                        )),
                    }
                };

                // A field this model does not know the meaning of can hide any
                // capability requirement. So it is folded into whatever row
                // already exists for this handler/target. When there is
                // otherwise nothing to report, it becomes its own blocked row.
                // Reporting it as portable would be inventing a verdict for a
                // field whose behaviour we cannot know.
                if !handler.unknown_fields.is_empty() {
                    let fields = unknown_fields_list(&handler.unknown_fields);
                    match &mut row {
                        Some(r) => {
                            // The unmodelled field carries strictly more unknown
                            // risk than whatever gap produced this row. So it
                            // cannot leave the row at a lighter severity, and a
                            // shim cannot be credited with emulating a field
                            // whose behaviour we do not know.
                            r.detail = format!(
                                "{}. Unmodelled fields: {fields} — portability cannot be verified",
                                r.detail
                            );
                            r.severity = Severity::Blocked;
                        }
                        None => {
                            row = Some(unknown_fields_row(handler, target_host.name(), &fields));
                        }
                    }
                }

                if let Some(r) = row {
                    out.push(r);
                }
            }
        }
    }
    out
}

fn caps_list(missing: &[HookCap]) -> String {
    missing
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn unknown_fields_list(fields: &std::collections::BTreeSet<String>) -> String {
    fields.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn no_hook_engine_row(handler: &crate::core::model::HookHandler, target: &str) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("{target} has no hook engine"),
        detail: format!("{target} declares no [hooks] section, so no hook can run there"),
        severity: Severity::Blocked,
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

fn unknown_fields_row(
    handler: &crate::core::model::HookHandler,
    target: &str,
    fields: &str,
) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("uses fields agentsync does not model ({fields})"),
        detail: format!("unmodelled fields: {fields} — portability to {target} cannot be verified"),
        severity: Severity::Blocked,
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

fn blocked_event_row(handler: &crate::core::model::HookHandler, target: &str) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("{target} has no {} event", handler.event),
        detail: "the event does not exist on the target; nothing can emulate it".into(),
        severity: Severity::Blocked,
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

fn blocked_cap_row(
    handler: &crate::core::model::HookHandler,
    target: &str,
    missing: &[HookCap],
) -> Row {
    let unshimmable: Vec<HookCap> = missing
        .iter()
        .copied()
        .filter(|c| strategy_for(*c).is_none())
        .collect();
    let reason = if unshimmable.is_empty() {
        format!("{target} cannot host a generated shim")
    } else {
        format!("no shim strategy for {}", caps_list(&unshimmable))
    };
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("{target} cannot run this hook ({})", caps_list(missing)),
        detail: reason,
        severity: Severity::Blocked,
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

fn shim_row(
    handler: &crate::core::model::HookHandler,
    source: &str,
    target: &str,
    missing: &[HookCap],
) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("{target} ignores {}", caps_list(missing)),
        detail: format!(
            "runs on {target} without honouring {} — a shim can emulate it",
            caps_list(missing)
        ),
        severity: Severity::Normal,
        actions: vec![
            Action::new(
                format!("generate a shim for {target}"),
                ActionKind::Push {
                    hosts: vec![target.to_string()],
                },
            ),
            Action::new("nothing to do", ActionKind::Nothing),
        ],
        chosen: 0,
        accepted: false,
        key: crate::core::diff::RowKey {
            source_host: Some(source.to_string()),
            ..Default::default()
        },
    }
}

pub fn plan_row(world: &World, row: &Row, plan: &mut Plan) {
    let ActionKind::Push { hosts } = &row.action().kind else {
        return;
    };
    for target_name in hosts {
        if let Err(e) = plan_one(world, row, target_name, plan) {
            // A shim we cannot build must be said out loud. Skipping quietly
            // would leave the row looking handled.
            plan.note(format!("{}: {e:#}", row.name));
        }
    }
}

fn plan_one(world: &World, row: &Row, target_name: &str, plan: &mut Plan) -> anyhow::Result<()> {
    let source_name = row
        .key
        .source_host
        .as_deref()
        .context("row does not record which host the hook came from")?;
    let source = world
        .snapshot(source_name)
        .context("source host is not detected")?;
    let target = world.host(target_name).context("target host is unknown")?;
    let declared = target
        .descriptor
        .hooks
        .as_ref()
        .context("target declares no [hooks] section")?;
    let effective = world.manifest.hooks_for(target_name, declared);
    let shim = effective
        .shim
        .as_ref()
        .context("target declares no [hooks.shim] marketplace")?;

    // Every handler from the same source plugin travels together: the shim
    // replaces the original wholesale, so a partial shim would drop the rest.
    let handler = source
        .hooks
        .values()
        .find(|h| h.id.short() == row.name)
        .context("handler is no longer present")?;
    let origin = handler.id.source.clone();
    let handlers: Vec<_> = source
        .hooks
        .values()
        .filter(|h| h.id.source == origin)
        .cloned()
        .collect();

    let (plugin, marketplace) =
        split_source(&origin).context("only plugin hooks can be shimmed today")?;

    let input = crate::shim::generate::ShimInput {
        marketplace_dir: crate::paths::expand(&shim.marketplace),
        plugin: plugin.clone(),
        marketplace: marketplace.clone(),
        handlers,
        allowed_output: effective
            .output
            .iter()
            .map(|f| f.json_key().to_string())
            .collect(),
        fold_into_system_message: vec!["rewakeMessage".to_string()],
        agentsync_bin: std::env::current_exe()
            .context("cannot find the agentsync binary to invoke from the shim")?,
        // The shim supersedes the original, so its other content has to travel
        // with it. Only directories that actually exist are linked, which is a
        // filesystem question and therefore answered here, not in the pure
        // generator.
        vendor: handler
            .plugin_root
            .as_ref()
            .map(|root| {
                ["skills", "commands", "agents"]
                    .iter()
                    .map(|d| root.join(d))
                    .filter(|d| d.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let generated = crate::shim::generate::plan_shim(&input)?;

    for op in generated.ops {
        plan.push(format!("write shim for {plugin}"), Step::Fs(op));
    }
    plan.push(
        format!("register the agentsync shim marketplace with {target_name}"),
        Step::Host {
            host: target_name.to_string(),
            argv: target.marketplace_add_argv(
                &generated.marketplace_name,
                &input.marketplace_dir.to_string_lossy(),
            )?,
            cwd: None,
        },
    );

    // The marketplace manifest lists every shim plugin at once, and applying it
    // is a whole-file write. Writing it per row would leave only the last row's
    // plugin registered, and the earlier ones would silently fail to install.
    // So rebuild it from every shim this plan already carries, plus this one.
    rewrite_marketplace_manifest(plan, &input.marketplace_dir, &generated.shim_plugin)?;
    // Order 1 puts the install ahead of the removal below. A failed removal
    // leaves a duplicate hook, which is visible. A failed install after a
    // removal leaves no review at all, which reads as health.
    plan.push_ordered(
        format!("install the {plugin} shim in {target_name}"),
        Step::Host {
            host: target_name.to_string(),
            argv: target
                .plugin_install_argv(&generated.shim_plugin, Some(&generated.marketplace_name))?,
            cwd: None,
        },
        1,
    );
    if world
        .snapshot(target_name)
        .is_some_and(|s| s.plugins.contains_key(&plugin))
    {
        plan.push(
            format!("remove the original {plugin} from {target_name}"),
            Step::Host {
                host: target_name.to_string(),
                argv: target.plugin_remove_argv(&plugin, Some(&marketplace))?,
                cwd: None,
            },
        );
    }
    Ok(())
}

/// `<plugin>@<marketplace>:<file>` split into its two names.
fn split_source(source: &str) -> Option<(String, String)> {
    let (plugin, rest) = source.split_once('@')?;
    let marketplace = rest.split(':').next()?;
    Some((plugin.to_string(), marketplace.to_string()))
}

/// Replace the marketplace manifest op with one naming every shim in the plan.
///
/// Rows are planned one at a time, but the manifest is a single file listing
/// them all. Reading the set back out of the plan keeps this correct however
/// many rows the user accepts.
fn rewrite_marketplace_manifest(
    plan: &mut Plan,
    marketplace_dir: &std::path::Path,
    new_plugin: &str,
) -> anyhow::Result<()> {
    let manifest_path = marketplace_dir.join(".claude-plugin/marketplace.json");

    let mut plugins: Vec<String> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.step {
            // An install step names the shim as `<plugin>@<marketplace>`.
            Step::Host { argv, .. } => argv
                .iter()
                .find(|a| a.starts_with("agentsync-shim-"))
                .map(|a| a.split('@').next().unwrap_or(a).to_string()),
            _ => None,
        })
        .collect();
    if !plugins.iter().any(|p| p == new_plugin) {
        plugins.push(new_plugin.to_string());
    }
    plugins.sort();
    plugins.dedup();

    plan.steps.retain(|s| {
        !matches!(&s.step, Step::Fs(crate::core::plan::FsOp::WriteFile { path, .. })
            if path == &manifest_path)
    });
    plan.push(
        "list every generated shim in the agentsync marketplace",
        Step::Fs(crate::shim::generate::marketplace_manifest_op(
            marketplace_dir,
            &plugins,
        )?),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::diff::Severity;
    use crate::core::model::HookCap;

    #[test]
    fn every_shimmable_cap_has_a_strategy_and_vice_versa() {
        for cap in SHIMMABLE {
            assert!(
                strategy_for(*cap).is_some(),
                "{cap} is listed shimmable but has no strategy"
            );
        }
        for cap in [
            HookCap::Matcher,
            HookCap::If,
            HookCap::Timeout,
            HookCap::AsyncRewake,
            HookCap::RewakeMessage,
            HookCap::RewakeSummary,
        ] {
            if strategy_for(cap).is_some() {
                assert!(
                    SHIMMABLE.contains(&cap),
                    "{cap} has a strategy but is not listed shimmable"
                );
            }
        }
    }

    #[test]
    fn a_gap_of_only_shimmable_caps_is_actionable() {
        // `if` is shimmable by prefiltering before the command runs.
        assert_eq!(classify(&[HookCap::If], true), Severity::Normal);
    }

    #[test]
    fn a_gap_with_an_unshimmable_cap_is_blocked_and_names_it() {
        // `matcher` cannot be emulated: without it the host never invokes us.
        assert_eq!(classify(&[HookCap::Matcher], true), Severity::Blocked);
    }

    #[test]
    fn a_target_that_cannot_host_shims_blocks_even_a_shimmable_gap() {
        assert_eq!(classify(&[HookCap::If], false), Severity::Blocked);
    }
}
