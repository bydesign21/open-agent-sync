//! The hooks domain: which of a host's hook handlers another host cannot run.
//!
//! A capability gap is only actionable when every missing capability has a named
//! shim strategy *and* the target can host a shim at all. Anything else is
//! blocked and names the capability, in the same spirit as `headers` for MCP —
//! a hook that silently does not run looks exactly like a hook that found
//! nothing, which is the worst available outcome for a security review.

use crate::core::diff::{Action, ActionKind, Domain, Row, Severity};
use crate::core::model::HookCap;
use crate::core::plan::Plan;
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
                    continue;
                };
                let target = world.manifest.hooks_for(target_host.name(), declared);

                if !target.supports_event(&handler.event) {
                    out.push(blocked_event_row(handler, target_host.name()));
                    continue;
                }
                let missing = target.missing_caps(&handler.required_caps());
                match classify(&missing, target.can_shim()) {
                    Severity::Synced => {}
                    Severity::Blocked => {
                        out.push(blocked_cap_row(handler, target_host.name(), &missing))
                    }
                    _ => out.push(shim_row(handler, target_host.name(), &missing)),
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

fn blocked_event_row(handler: &crate::core::model::HookHandler, target: &str) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.to_string(),
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
        name: handler.id.to_string(),
        headline: format!("{target} cannot run this hook ({})", caps_list(missing)),
        detail: reason,
        severity: Severity::Blocked,
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

fn shim_row(handler: &crate::core::model::HookHandler, target: &str, missing: &[HookCap]) -> Row {
    Row {
        domain: Domain::Hooks,
        name: handler.id.to_string(),
        headline: format!("{target} ignores {}", caps_list(missing)),
        detail: format!(
            "runs on {target} without honouring {}; a shim can emulate it",
            caps_list(missing)
        ),
        severity: Severity::Normal,
        // Generating the shim lands in the next plan. Until then the only
        // honest action is none: claiming a fix that does not run is worse
        // than reporting the gap.
        actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
        chosen: 0,
        accepted: false,
        key: Default::default(),
    }
}

/// No actionable row is produced yet, so nothing reaches the planner. The next
/// plan replaces this with shim generation.
pub fn plan_row(_world: &World, _row: &Row, _plan: &mut Plan) {}

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
