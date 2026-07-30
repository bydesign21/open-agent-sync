//! Row and action types.
//!
//! The UI shows a to-do list, not a matrix, so the differ's output is already
//! shaped like a decision: a headline sentence, a default action, and the legal
//! alternatives. Keeping that shaping here rather than in the TUI is what lets
//! `agentsync plan` print the identical set of decisions with no terminal.
//!
//! There is exactly **one row per name per domain**. Several problems can apply
//! to the same name; the most severe wins the headline and the rest go in the
//! detail line. Emitting three rows for one server is how a matrix design
//! becomes unreadable.

use std::fmt;

use crate::core::model::Scope;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Domain {
    Mcp,
    Skills,
    Plugins,
}

impl Domain {
    pub fn title(self) -> &'static str {
        match self {
            Domain::Mcp => "MCP SERVERS",
            Domain::Skills => "SKILLS",
            Domain::Plugins => "PLUGINS",
        }
    }
    pub const ALL: [Domain; 3] = [Domain::Mcp, Domain::Skills, Domain::Plugins];
}

/// How a row is marked. Three visible marks, not six glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Nothing to do. Hidden unless the user asks to see synced rows.
    Synced,
    /// An ordinary difference with a safe default.
    Normal,
    /// Needs care — a credential in the clear, or a mutation that overwrites
    /// something we did not create.
    Warn,
    /// Cannot be resolved by pushing: the host is incapable of representing it.
    /// Offers only "record this divergence" so it stops nagging.
    Blocked,
}

impl Severity {
    /// The single-character mark shown before the name.
    pub fn mark(self, accepted: bool) -> &'static str {
        match (self, accepted) {
            (_, true) => "\u{2713}",
            (Severity::Warn, false) => "!",
            (Severity::Blocked, false) => "\u{2014}",
            _ => " ",
        }
    }
}

/// What to do about a row. Eight variants cover all three domains; the planner
/// dispatches on `(domain, kind)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// Informational row. Deliberately available so a blocked row can be left
    /// alone without pretending it was handled.
    Nothing,

    /// Pull the host's value into the manifest.
    /// `push` also sends it to the hosts that lack it.
    /// `promote` rewrites a per-repo entry as user-global.
    Adopt { push: bool, promote: bool },

    /// Hosts disagree: take this host's value as the manifest's.
    AdoptFrom { host: String },

    /// Send the manifest's value to these hosts.
    Push { hosts: Vec<String> },

    /// Remove it. `from_manifest` also drops the manifest entry; `purge` also
    /// deletes canonical skill content (never the default).
    Delete {
        hosts: Vec<String>,
        from_manifest: bool,
        purge: bool,
    },

    /// Record that this entry belongs only to these hosts, so the difference is
    /// no longer reported. This is the resolution for a genuinely one-sided
    /// entry, and the reason the list converges to empty instead of nagging.
    KeepDivergent { hosts: Vec<String> },

    /// Same name at two scopes on one host — one silently wins. Collapse to one.
    CollapseScope { keep: Scope },

    /// Replace a literal credential with an environment-variable reference.
    SecretToEnv { var: String },

    /// Plugins only: record which marketplace to install from, when more than
    /// one of a host's marketplaces offers the same name and installing would
    /// otherwise be a coin flip.
    PinMarketplace { marketplace: String },
}

#[derive(Clone, Debug)]
pub struct Action {
    pub label: String,
    pub kind: ActionKind,
}

impl Action {
    pub fn new(label: impl Into<String>, kind: ActionKind) -> Self {
        Action {
            label: label.into(),
            kind,
        }
    }
}

/// Everything the planner needs to turn a chosen action into steps.
#[derive(Clone, Debug, Default)]
pub struct RowKey {
    /// Scopes the entry currently occupies on hosts (MCP only).
    pub host_scopes: Vec<Scope>,
    /// Host whose value would be adopted.
    pub source_host: Option<String>,
    /// For skills: absolute path of the real directory to move into canonical.
    pub source_path: Option<std::path::PathBuf>,
    /// For plugins: marketplace observed on the source host.
    pub marketplace: Option<String>,
    /// True when this row is a marketplace rather than a plugin.
    pub is_marketplace: bool,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub domain: Domain,
    pub name: String,
    /// The sentence shown in the list, e.g. `only in claude, 3 repos`.
    pub headline: String,
    /// Extra context for the detail line, including any lower-priority problems
    /// folded into this row.
    pub detail: String,
    pub severity: Severity,
    pub actions: Vec<Action>,
    pub chosen: usize,
    pub accepted: bool,
    pub key: RowKey,
}

impl Row {
    pub fn synced(domain: Domain, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Row {
            domain,
            name: name.into(),
            headline: "in sync".into(),
            detail: detail.into(),
            severity: Severity::Synced,
            actions: vec![Action::new("nothing to do", ActionKind::Nothing)],
            chosen: 0,
            accepted: false,
            key: RowKey::default(),
        }
    }

    pub fn action(&self) -> &Action {
        &self.actions[self.chosen.min(self.actions.len() - 1)]
    }

    /// Cycle to the next legal action. Changing the action clears acceptance so
    /// you cannot accept one thing and run another.
    pub fn cycle(&mut self) {
        if self.actions.len() > 1 {
            self.chosen = (self.chosen + 1) % self.actions.len();
            self.accepted = false;
        }
    }

    /// A blocked or informational row has nothing to accept.
    pub fn actionable(&self) -> bool {
        !matches!(self.action().kind, ActionKind::Nothing)
    }

    pub fn toggle(&mut self) {
        if self.actionable() {
            self.accepted = !self.accepted;
        }
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} \u{2014} {}", self.name, self.headline)
    }
}

/// Join host names for a headline: `claude`, `claude and codex`,
/// `claude, codex and opencode`.
pub fn join_hosts(hosts: &[String]) -> String {
    match hosts {
        [] => "nothing".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_host_lists_readably() {
        assert_eq!(join_hosts(&[]), "nothing");
        assert_eq!(join_hosts(&["claude".into()]), "claude");
        assert_eq!(
            join_hosts(&["claude".into(), "codex".into()]),
            "claude and codex"
        );
        assert_eq!(
            join_hosts(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
    }

    #[test]
    fn changing_the_action_clears_acceptance() {
        let mut row = Row {
            domain: Domain::Mcp,
            name: "x".into(),
            headline: "only in codex".into(),
            detail: String::new(),
            severity: Severity::Normal,
            actions: vec![
                Action::new(
                    "adopt + push",
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
            ],
            chosen: 0,
            accepted: false,
            key: RowKey::default(),
        };
        row.toggle();
        assert!(row.accepted);
        row.cycle();
        assert!(!row.accepted, "cycling must not leave a stale acceptance");
        assert_eq!(row.chosen, 1);
    }

    #[test]
    fn informational_rows_cannot_be_accepted() {
        let mut row = Row::synced(Domain::Skills, "x", "");
        row.toggle();
        assert!(!row.accepted);
    }

    #[test]
    fn accepted_mark_wins_over_severity() {
        assert_eq!(Severity::Warn.mark(true), "\u{2713}");
        assert_eq!(Severity::Warn.mark(false), "!");
        assert_eq!(Severity::Blocked.mark(false), "\u{2014}");
    }
}
