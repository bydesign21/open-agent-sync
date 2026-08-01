//! Hook manifest parsers.
//!
//! Identity is positional — `<source>:<event>:<group>:<index>` — because a hook
//! manifest legitimately contains several handlers with byte-identical commands
//! that differ only in their filter. Keying on content would collapse them,
//! which is precisely the Codex bug this domain reports.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::core::model::{HookHandler, HookId};
use crate::hosts::parsers::{HookRead, ParseCtx};

/// Fields this model understands. Anything else lands in `unknown_fields`.
const KNOWN: &[&str] = &[
    "type",
    "command",
    "timeout",
    "if",
    "asyncRewake",
    "rewakeMessage",
    "rewakeSummary",
];

/// `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/hooks/hooks.json`
pub fn claude_hooks_json_v1(text: &str, ctx: &ParseCtx) -> Result<HookRead> {
    let doc: Value =
        serde_json::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let (source, root) = plugin_identity(&ctx.origin);
    let mut out = HookRead::default();
    collect(&doc, &source, root.as_deref(), &mut out);
    Ok(out)
}

/// Derive `<plugin>@<marketplace>:<relative file>` and the plugin root from a
/// cache path. Returns a path-based fallback when the layout is unfamiliar,
/// rather than guessing a plugin name.
fn plugin_identity(origin: &Path) -> (String, Option<PathBuf>) {
    // .../<marketplace>/<plugin>/<version>/hooks/hooks.json
    let parts: Vec<String> = origin
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() >= 5 {
        let n = parts.len();
        let rel = format!("{}/{}", parts[n - 2], parts[n - 1]);
        let source = format!("{}@{}:{}", parts[n - 4], parts[n - 5], rel);
        let root = origin.ancestors().nth(2).map(Path::to_path_buf);
        return (source, root);
    }
    (origin.to_string_lossy().into_owned(), None)
}

fn collect(doc: &Value, source: &str, root: Option<&Path>, out: &mut HookRead) {
    let Some(events) = doc.get("hooks").and_then(Value::as_object) else {
        out.warnings
            .push(format!("{source}: no `hooks` object; nothing read"));
        return;
    };
    for (event, groups) in events {
        let Some(groups) = groups.as_array() else {
            out.warnings
                .push(format!("{source}: hooks.{event} is not an array"));
            continue;
        };
        for (gi, group) in groups.iter().enumerate() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                out.warnings.push(format!(
                    "{source}: hooks.{event}[{gi}] has no `hooks` array"
                ));
                continue;
            };
            for (hi, h) in handlers.iter().enumerate() {
                let Some(command) = h.get("command").and_then(Value::as_str) else {
                    out.warnings.push(format!(
                        "{source}: hooks.{event}[{gi}].hooks[{hi}] has no command"
                    ));
                    continue;
                };
                let id = HookId {
                    source: source.to_string(),
                    event: event.clone(),
                    group: gi,
                    index: hi,
                };
                let mut handler = HookHandler::new(id, event, command);
                handler.matcher = matcher.clone();
                handler.if_pattern = h.get("if").and_then(Value::as_str).map(str::to_string);
                handler.timeout = h.get("timeout").and_then(Value::as_u64);
                handler.async_rewake = h
                    .get("asyncRewake")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                handler.rewake_message = h
                    .get("rewakeMessage")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                handler.rewake_summary = h
                    .get("rewakeSummary")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                handler.plugin_root = root.map(Path::to_path_buf);
                handler.unknown_fields = h
                    .as_object()
                    .map(|o| {
                        o.keys()
                            .filter(|k| !KNOWN.contains(&k.as_str()))
                            .cloned()
                            .collect::<BTreeSet<String>>()
                    })
                    .unwrap_or_default();
                out.handlers.push(handler);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::parsers::ParseCtx;

    fn ctx() -> ParseCtx {
        ParseCtx {
            repo: None,
            origin: "/cache/claude-plugins-official/security-guidance/2.0.6/hooks/hooks.json"
                .into(),
        }
    }

    #[test]
    fn the_five_bash_handlers_are_distinct_and_each_carries_its_if() {
        let text = include_str!("../../../tests/fixtures/security-guidance-hooks.json");
        let read = claude_hooks_json_v1(text, &ctx()).unwrap();

        let bash: Vec<_> = read
            .handlers
            .iter()
            .filter(|h| h.event == "PostToolUse" && h.matcher.as_deref() == Some("Bash"))
            .collect();
        assert_eq!(bash.len(), 5, "the manifest declares five Bash handlers");

        let ifs: Vec<&str> = bash
            .iter()
            .filter_map(|h| h.if_pattern.as_deref())
            .collect();
        assert_eq!(
            ifs,
            vec![
                "Bash(git commit:*)",
                "Bash(git push:*)",
                "Bash(gt create:*)",
                "Bash(gt modify:*)",
                "Bash(gt submit:*)",
            ]
        );

        // The bug this domain exists to catch: Codex collapses these to one
        // hash because it reads only `command`. Every command here is byte
        // identical, so identity must come from position, not content.
        let commands: std::collections::BTreeSet<&str> =
            bash.iter().map(|h| h.command.as_str()).collect();
        assert_eq!(commands.len(), 1);
        let ids: std::collections::BTreeSet<String> =
            bash.iter().map(|h| h.id.to_string()).collect();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn handler_ids_use_the_plugin_and_marketplace_from_the_cache_path() {
        let text = include_str!("../../../tests/fixtures/security-guidance-hooks.json");
        let read = claude_hooks_json_v1(text, &ctx()).unwrap();
        let stop = read.handlers.iter().find(|h| h.event == "Stop").unwrap();
        assert_eq!(
            stop.id.to_string(),
            "security-guidance@claude-plugins-official:hooks/hooks.json:stop:0:0"
        );
        assert_eq!(
            stop.plugin_root.as_deref(),
            Some(std::path::Path::new(
                "/cache/claude-plugins-official/security-guidance/2.0.6"
            ))
        );
    }

    #[test]
    fn rewake_fields_are_captured_so_their_caps_can_be_derived() {
        let text = include_str!("../../../tests/fixtures/security-guidance-hooks.json");
        let read = claude_hooks_json_v1(text, &ctx()).unwrap();
        let stop = read.handlers.iter().find(|h| h.event == "Stop").unwrap();
        assert!(stop.async_rewake);
        assert_eq!(
            stop.rewake_summary.as_deref(),
            Some("Background security review found issues")
        );
        assert!(
            stop.required_caps()
                .contains(&crate::core::model::HookCap::AsyncRewake)
        );
    }

    #[test]
    fn an_unrecognised_field_is_recorded_rather_than_dropped() {
        let text = r#"{"hooks":{"Stop":[{"hooks":[
            {"type":"command","command":"x","futureThing":true}]}]}}"#;
        let read = claude_hooks_json_v1(text, &ctx()).unwrap();
        assert!(read.handlers[0].unknown_fields.contains("futureThing"));
    }

    #[test]
    fn a_matcher_group_with_no_hooks_array_warns_rather_than_vanishing() {
        let text = r#"{"hooks":{"PostToolUse":[{"matcher":"Bash"}]}}"#;
        let read = claude_hooks_json_v1(text, &ctx()).unwrap();
        assert!(read.handlers.is_empty());
        assert_eq!(read.warnings.len(), 1, "a skipped group must be reported");
        assert!(read.warnings[0].contains("has no `hooks` array"));
    }
}
