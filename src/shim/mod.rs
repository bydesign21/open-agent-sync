//! The shim: what agentsync generates so a host can run a hook it cannot
//! natively express.
//!
//! A shim plugin carries no logic. It is a generated `hooks.json` whose every
//! command invokes `agentsync hook-shim --spec <sidecar>`, plus one sidecar per
//! handler. The sidecar is this type. All translation happens inside the
//! agentsync binary, so a fix to a strategy ships with the binary instead of
//! requiring every generated shim to be rewritten.

pub mod generate;
pub mod matcher;
pub mod output;
pub mod run;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What one generated shim handler needs to stand in for the original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimSpec {
    /// The original handler's full `HookId`, for warnings and for tracing a
    /// generated file back to what produced it.
    pub source_id: String,
    /// The original command, verbatim. Placeholders inside it are expanded by
    /// the shell that runs it, so `plugin_root` must be exported first.
    pub command: String,
    /// What `${CLAUDE_PLUGIN_ROOT}` in `command` refers to. The shim's own root
    /// would break every path in the command, so the original root is recorded
    /// here and re-exported at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_root: Option<PathBuf>,
    /// The filter the target host cannot honour. The runtime evaluates it and
    /// exits without running the command when it does not match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_pattern: Option<String>,
    /// Top-level stdout keys the target accepts. Anything else is dropped.
    pub allowed_output: Vec<String>,
    /// Keys whose value carries text meant for a person. When the target does
    /// not accept them, their content moves into `systemMessage` rather than
    /// being discarded.
    #[serde(default)]
    pub fold_into_system_message: Vec<String>,
    /// The handler's configured `rewakeMessage`, carried over only when the
    /// target cannot represent the field itself. `NormalizeOutput` folds this
    /// static text into `systemMessage` at run time, so the row's promise ("a
    /// shim can emulate it") is something the shim actually does rather than a
    /// mapping that goes nowhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewake_message: Option<String>,
    /// Same as `rewake_message`, for the handler's configured `rewakeSummary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewake_summary: Option<String>,
}

impl ShimSpec {
    pub fn load(path: &Path) -> Result<ShimSpec> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading shim spec {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing shim spec {}", path.display()))
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serialising shim spec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ShimSpec {
        ShimSpec {
            source_id:
                "security-guidance@claude-plugins-official:hooks/hooks.json:post_tool_use:1:0"
                    .into(),
            command: "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/review.sh\"".into(),
            plugin_root: Some("/cache/claude-plugins-official/security-guidance/2.0.6".into()),
            if_pattern: Some("Bash(git commit:*)".into()),
            allowed_output: vec!["systemMessage".into(), "additionalContext".into()],
            fold_into_system_message: vec!["rewakeMessage".into()],
            rewake_message: Some("findings follow".into()),
            rewake_summary: Some("Commit security review found issues".into()),
        }
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let json = spec().to_json().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.json");
        std::fs::write(&path, json).unwrap();
        assert_eq!(ShimSpec::load(&path).unwrap(), spec());
    }

    #[test]
    fn a_missing_spec_file_is_an_error_not_a_default() {
        // A spec we cannot read must never degrade into an empty spec that
        // runs nothing and reports success.
        let err = ShimSpec::load(std::path::Path::new("/nonexistent/spec.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("spec.json"), "error must name the file: {err}");
    }

    #[test]
    fn a_malformed_spec_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(ShimSpec::load(&path).is_err());
    }
}
