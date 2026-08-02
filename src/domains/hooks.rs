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
use crate::core::plan::{FsOp, Plan, PlannedStep, Step};
use crate::domains::World;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShimSubstitution {
    pub target_host: String,
    pub plugin: String,
    pub marketplace: String,
    pub shim_plugin: String,
}

/// Installed shims that stand in for a desired original plugin.
///
/// The generated name is derived from the original hook identity. It is never
/// parsed back into its components: marketplace and plugin names may both
/// contain hyphens, so that reverse operation is ambiguous.
pub fn shim_substitutions(world: &World) -> Vec<ShimSubstitution> {
    let originals: std::collections::BTreeSet<(String, String)> = world
        .detected_snapshots()
        .flat_map(|snapshot| snapshot.hooks.values())
        .filter_map(|handler| split_source(&handler.id.source))
        .filter(|(plugin, marketplace)| {
            !plugin.starts_with("agentsync-shim-")
                && !crate::shim::generate::is_internal_marketplace(marketplace)
                && world.manifest.plugins.get(plugin).is_some_and(|entry| {
                    entry
                        .marketplace
                        .as_deref()
                        .is_none_or(|pin| pin == marketplace)
                })
        })
        .collect();

    let mut substitutions = std::collections::BTreeSet::new();
    for (target, snapshot) in world.detected() {
        for (plugin, marketplace) in &originals {
            let shim_plugin = crate::shim::generate::shim_plugin_name(marketplace, plugin);
            if snapshot.plugins.get(&shim_plugin).is_some_and(|installed| {
                crate::shim::generate::is_internal_marketplace(&installed.marketplace)
            }) {
                substitutions.insert(ShimSubstitution {
                    target_host: target.name().to_string(),
                    plugin: plugin.clone(),
                    marketplace: marketplace.clone(),
                    shim_plugin,
                });
            }
        }
    }
    substitutions.into_iter().collect()
}

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
pub const SHIMMABLE: &[HookCap] = &[HookCap::If, HookCap::RewakeMessage, HookCap::RewakeSummary];

pub fn strategy_for(cap: HookCap) -> Option<Strategy> {
    match cap {
        HookCap::If => Some(Strategy::Prefilter),
        HookCap::RewakeMessage | HookCap::RewakeSummary => Some(Strategy::NormalizeOutput),
        // Without `matcher` the host never invokes the hook for the right tool
        // in the first place, so there is nothing for a shim to intercept.
        HookCap::Matcher => None,
        // A host that cannot express a timeout cannot be given one from outside.
        HookCap::Timeout => None,
        // Asynchronous rewake is the host's own scheduling behaviour: it wakes
        // the agent up again after the hook returns. A shim runs inside a
        // single hook invocation and exits when the command does, so there is
        // no "later" for it to run in — nothing here can make a host wake up
        // on its own. Where a target DOES declare `async_rewake` (Codex does),
        // the generator re-emits the field directly (see
        // `src/shim/generate.rs`), so the cap never reaches `missing` and this
        // arm is not exercised. Where a target does not declare it, the honest
        // answer is a blocked row naming the cap, not a promised emulation.
        HookCap::AsyncRewake => None,
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
    let substitutions = shim_substitutions(world);
    for (source_host, source_snap) in world.detected() {
        for handler in source_snap.hooks.values() {
            for (target_host, _) in world.detected() {
                if target_host.name() == source_host.name() {
                    continue;
                }
                if split_source(&handler.id.source).is_some_and(|(plugin, marketplace)| {
                    let shim_plugin =
                        crate::shim::generate::shim_plugin_name(&marketplace, &plugin);
                    substitutions.iter().any(|substitution| {
                        substitution.target_host == target_host.name()
                            && substitution.plugin == plugin
                            && substitution.marketplace == marketplace
                            && substitution.shim_plugin == shim_plugin
                    })
                }) {
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
    // Only a plugin hook has a marketplace and plugin name a shim can be
    // installed as. A settings-file hook (or a path that merely contains an
    // `@`, such as a home directory) must not advertise a fix it cannot
    // deliver.
    let actions = if split_source(&handler.id.source).is_some() {
        vec![
            Action::new(
                format!("generate a shim for {target}"),
                ActionKind::Push {
                    hosts: vec![target.to_string()],
                },
            ),
            Action::new("nothing to do", ActionKind::Nothing),
        ]
    } else {
        vec![Action::new("nothing to do", ActionKind::Nothing)]
    };
    Row {
        domain: Domain::Hooks,
        name: handler.id.short(),
        headline: format!("{target} ignores {}", caps_list(missing)),
        detail: format!(
            "runs on {target} without honouring {} — a shim can emulate it",
            caps_list(missing)
        ),
        severity: Severity::Normal,
        actions,
        chosen: 0,
        accepted: false,
        key: crate::core::diff::RowKey {
            source_host: Some(source.to_string()),
            // `short()` drops the marketplace, so two same-named plugins from
            // different marketplaces can share a `name`. The full source
            // string is unambiguous and lets the planner find the exact
            // handler back, rather than the first one whose short name
            // matches.
            marketplace: Some(handler.id.source.clone()),
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

/// Reconcile already-installed substitutions even when they need no decision
/// row. Generated shims stay installed and their marketplace stays on disk;
/// only a duplicate original and stale durable manifest entries are removed.
pub(super) fn plan_substitution_cleanup(world: &World, plan: &mut Plan) {
    for substitution in shim_substitutions(world) {
        if world
            .snapshot(&substitution.target_host)
            .is_some_and(|snapshot| snapshot.plugins.contains_key(&substitution.plugin))
            && let Some(target) = world.host(&substitution.target_host)
        {
            match target.plugin_remove_argv(&substitution.plugin, Some(&substitution.marketplace)) {
                Ok(argv) => plan.push(
                    format!(
                        "remove the original {} from {}",
                        substitution.plugin, substitution.target_host
                    ),
                    Step::Host {
                        host: substitution.target_host.clone(),
                        argv,
                        cwd: None,
                    },
                ),
                Err(error) => plan.note(format!(
                    "{}: {} — {error:#}",
                    substitution.plugin, substitution.target_host
                )),
            }
        }
        plan_internal_manifest_cleanup(world, &substitution.shim_plugin, plan);
    }
}

fn plan_internal_manifest_cleanup(world: &World, shim_plugin: &str, plan: &mut Plan) {
    if world.manifest.plugins.contains_key(shim_plugin) {
        plan.remove_plugin_from_manifest(shim_plugin);
    }
    if world
        .manifest
        .marketplaces
        .contains_key(crate::shim::generate::MARKETPLACE_NAME)
    {
        plan.remove_marketplace_from_manifest(crate::shim::generate::MARKETPLACE_NAME);
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

    // The row records the handler's own full source (not its `short()` name),
    // because two same-named plugins from different marketplaces collapse to
    // the same short name. Matching on the full string finds the exact
    // handler this row was built from, never a same-named stand-in.
    let origin = row
        .key
        .marketplace
        .clone()
        .context("row does not record which plugin the hook came from")?;
    let handler = source
        .hooks
        .values()
        .find(|h| h.id.source == origin)
        .context("handler is no longer present")?;

    let (plugin, marketplace) =
        split_source(&origin).context("only plugin hooks can be shimmed today")?;
    let marketplace_dir = crate::paths::expand(&shim.marketplace);
    let shim_plugin = crate::shim::generate::shim_plugin_name(&marketplace, &plugin);

    // A plugin with two shimmable handlers (for example a PreToolUse and a
    // PostToolUse handler) produces two rows, since `rows()` emits one row per
    // handler. Both carry the same plugin, so the second row must not repeat
    // work the first already committed: the whole plugin is shimmed at once,
    // from every one of its handlers, the first time any of its rows is
    // planned.
    if shim_plugin_names(&marketplace_dir, &plan.steps).any(|p| p == shim_plugin) {
        return Ok(());
    }

    // Every handler from the same source plugin travels together: the shim
    // replaces the original wholesale, so a partial shim would drop the rest.
    let handlers: Vec<_> = source
        .hooks
        .values()
        .filter(|h| h.id.source == origin)
        .cloned()
        .collect();

    let input = crate::shim::generate::ShimInput {
        marketplace_dir: marketplace_dir.clone(),
        plugin: plugin.clone(),
        marketplace: marketplace.clone(),
        handlers,
        allowed_output: effective
            .output
            .iter()
            .map(|f| f.json_key().to_string())
            .collect(),
        fold_into_system_message: vec!["rewakeMessage".to_string()],
        output_strategy: shim.output_strategy,
        target_caps: effective.caps.clone(),
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

    // The guard that ties every step of this shim's install together. If the
    // sidecars or hooks.json fail to write, the install must not run against
    // a half-written plugin directory, and if the install itself fails, the
    // removal below must not run either. Computed once, up front, so every
    // step below shares the same key regardless of where it is pushed.
    let install_guard = format!("shim:{target_name}:{}", generated.shim_plugin);

    // Nothing is committed to `plan` until every fallible step below has
    // succeeded. An argv-building error partway through must not leave a plan
    // that writes shim content it never installs.
    let mut staged: Vec<PlannedStep> = Vec::new();
    for op in generated.ops {
        staged.push(PlannedStep {
            label: format!("write shim for {plugin}"),
            step: Step::Fs(op),
            order_hint: None,
            guard: Some(install_guard.clone()),
        });
    }
    staged.push(PlannedStep {
        label: format!("register the agentsync shim marketplace with {target_name}"),
        step: Step::Host {
            host: target_name.to_string(),
            argv: target.marketplace_add_argv(
                &generated.marketplace_name,
                &input.marketplace_dir.to_string_lossy(),
            )?,
            cwd: None,
        },
        order_hint: None,
        guard: Some(install_guard.clone()),
    });

    // The marketplace manifest lists every shim plugin at once, and applying
    // it is a whole-file write. Writing it per row would leave only the last
    // row's plugin registered, and the earlier ones would silently fail to
    // install. So rebuild it from every shim already committed to `plan`, plus
    // this one, which is why the manifest write is derived from `plan.steps`
    // and `staged` together rather than tracked separately.
    let mut plugins: Vec<String> = shim_plugin_names(&marketplace_dir, &plan.steps)
        .chain(shim_plugin_names(&marketplace_dir, &staged))
        .collect();
    plugins.sort();
    plugins.dedup();
    staged.push(PlannedStep {
        label: "list every generated shim in the agentsync marketplace".to_string(),
        step: Step::Fs(crate::shim::generate::marketplace_manifest_op(
            &marketplace_dir,
            &plugins,
        )?),
        order_hint: None,
        guard: None,
    });

    // Order 1 puts the install ahead of the removal below. A failed removal
    // leaves a duplicate hook, which is visible. A failed install after a
    // removal leaves no review at all, which reads as health.
    //
    // The guard key ties every step above and below together. If a write, the
    // marketplace registration, or the install itself fails, the removal is
    // skipped instead of run, so the ordering actually buys something: a
    // failed install never leaves the host with no hook at all.
    staged.push(PlannedStep {
        label: format!("install the {plugin} shim in {target_name}"),
        step: Step::Host {
            host: target_name.to_string(),
            argv: target
                .plugin_install_argv(&generated.shim_plugin, Some(&generated.marketplace_name))?,
            cwd: None,
        },
        order_hint: Some(1),
        guard: Some(install_guard.clone()),
    });
    if world
        .snapshot(target_name)
        .is_some_and(|s| s.plugins.contains_key(&plugin))
    {
        staged.push(PlannedStep {
            label: format!("remove the original {plugin} from {target_name}"),
            step: Step::Host {
                host: target_name.to_string(),
                argv: target.plugin_remove_argv(&plugin, Some(&marketplace))?,
                cwd: None,
            },
            order_hint: None,
            guard: Some(install_guard),
        });
    }

    // Every prior write of this manifest is superseded by the one just staged
    // above, which lists every shim `plan` and `staged` together carry.
    let manifest_path = marketplace_dir.join(".claude-plugin/marketplace.json");
    plan.steps.retain(
        |s| !matches!(&s.step, Step::Fs(FsOp::WriteFile { path, .. }) if path == &manifest_path),
    );
    plan.steps.extend(staged);
    plan_internal_manifest_cleanup(world, &generated.shim_plugin, plan);
    // `current_exe()` resolves symlinks, so a package manager that swaps the
    // binary on upgrade (for example Homebrew's Cellar path) leaves every
    // generated shim invoking a binary that no longer exists. Silently
    // shipping that is the kind of exposure this project never allows, so it
    // is said here instead.
    plan.note(format!(
        "the {plugin} shim for {target_name} invokes {}; regenerate it after upgrading agentsync",
        input.agentsync_bin.display()
    ));
    Ok(())
}

/// `<plugin>@<marketplace>:<file>` split into its two names.
///
/// Only a plugin source has this shape. A settings-file path can still
/// contain `@` (a home directory such as `/Users/logan@corp.com` is ordinary
/// on directory-joined machines), so the split alone is not proof of a plugin
/// hook. A plugin id has no path separator; a misparsed settings-file path
/// does, so that distinguishes the two.
fn split_source(source: &str) -> Option<(String, String)> {
    let head = source.split(':').next()?;
    let (plugin, marketplace) = head.split_once('@')?;
    let named = |s: &str| !s.is_empty() && !s.contains('/') && !s.contains('\\');
    (named(plugin) && named(marketplace)).then(|| (plugin.to_string(), marketplace.to_string()))
}

/// Names of every generated shim plugin already written under
/// `marketplace_dir`, found from the paths the plan actually writes to or
/// links into — the directory agentsync owns and nothing else writes under.
///
/// Scanning host command argv for `agentsync-shim-` was tried first and
/// rejected: a real user plugin coincidentally named that way would be
/// swept in from an unrelated removal step, and the prefix is one character
/// away from `MARKETPLACE_NAME` itself.
fn shim_plugin_names<'a>(
    marketplace_dir: &'a std::path::Path,
    steps: &'a [PlannedStep],
) -> impl Iterator<Item = String> + 'a {
    steps.iter().filter_map(move |s| match &s.step {
        Step::Fs(FsOp::WriteFile { path, .. }) | Step::Fs(FsOp::Link { link: path, .. }) => path
            .strip_prefix(marketplace_dir)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .filter(|c| c.starts_with("agentsync-shim-")),
        _ => None,
    })
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
    fn a_shim_cannot_make_a_host_wake_up_later_so_async_rewake_has_no_strategy() {
        // A shim runs inside one hook invocation and exits with the command.
        // There is no "later" for it to run in, so a target that genuinely
        // lacks `async_rewake` gets an honest blocked row, never a promised
        // emulation. (A target that DOES declare the cap, like Codex, gets it
        // re-emitted directly in `src/shim/generate.rs` and never reaches
        // this path at all.)
        assert!(strategy_for(HookCap::AsyncRewake).is_none());
        assert!(!SHIMMABLE.contains(&HookCap::AsyncRewake));
    }

    #[test]
    fn a_target_that_cannot_host_shims_blocks_even_a_shimmable_gap() {
        assert_eq!(classify(&[HookCap::If], false), Severity::Blocked);
    }
}
