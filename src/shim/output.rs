//! Reshaping a hook's stdout into what the target host accepts.
//!
//! Dropping a key is never silent when its content was meant for a person.
//! Text moves into `systemMessage` where the target accepts one, and where it
//! does not, the drop is still named rather than performed quietly.

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::core::model::HookOutputStrategy;
use crate::shim::ShimSpec;

const SYSTEM_MESSAGE: &str = "systemMessage";

/// Filter `stdout` to the keys the target accepts.
///
/// Output that is not a JSON object passes through unchanged. A hook is free to
/// print plain text, and inventing a JSON envelope around it would be a claim
/// the hook never made.
pub fn normalize(stdout: &str, spec: &ShimSpec) -> Result<String> {
    match spec.output_strategy {
        HookOutputStrategy::Legacy => Ok(normalize_legacy(stdout, spec)),
        HookOutputStrategy::CodexV1 => {
            let event = spec
                .event
                .as_deref()
                .context("Codex shim sidecar is missing its event")?;
            crate::shim::codex_output::translate(stdout, event, spec)
        }
        HookOutputStrategy::OpenCodeV1 | HookOutputStrategy::KiloV1 => {
            let callback = spec
                .event
                .as_deref()
                .context("OpenCode-family shim sidecar is missing its callback")?;
            crate::shim::bridge_output::translate(stdout, callback, spec)
        }
    }
}

/// Preserve the original output-filtering semantics for old sidecars and for
/// the already schema-validated object emitted by the Codex translator.
pub(super) fn normalize_legacy(stdout: &str, spec: &ShimSpec) -> String {
    let Ok(Value::Object(original)) = serde_json::from_str::<Value>(stdout) else {
        return stdout.to_string();
    };

    // Whether the hook produced any output at all. The configured
    // `rewakeMessage`/`rewakeSummary` text is static — carried on the
    // handler's own config, not on this run's output — so folding it in when
    // the hook printed nothing would manufacture a message for a hook that
    // never ran, or ran and had nothing to say. Folding it in only when there
    // is already something to attach it to keeps the emulation honest.
    let produced_output = !original.is_empty();

    let mut kept = Map::new();
    let mut folded: Vec<String> = Vec::new();
    let mut suppressed: Vec<String> = Vec::new();

    for (key, value) in original {
        if spec.allowed_output.contains(&key) {
            kept.insert(key, value);
        } else if spec.fold_into_system_message.contains(&key) {
            if let Some(text) = value.as_str() {
                folded.push(text.to_string());
            } else {
                suppressed.push(key);
            }
        } else {
            suppressed.push(key);
        }
    }

    if spec.allowed_output.iter().any(|k| k == SYSTEM_MESSAGE) {
        let mut parts: Vec<String> = Vec::new();
        // The handler's configured rewake text is emulation, not the hook's
        // own output, so it leads the message rather than getting buried
        // after what the hook actually printed.
        if produced_output {
            if let Some(message) = &spec.rewake_message {
                parts.push(message.clone());
            }
            if let Some(summary) = &spec.rewake_summary {
                parts.push(summary.clone());
            }
        }
        if let Some(Value::String(existing)) = kept.get(SYSTEM_MESSAGE) {
            parts.push(existing.clone());
        }
        parts.extend(folded);
        if !suppressed.is_empty() {
            parts.push(format!(
                "agentsync dropped fields this host cannot accept: {}",
                suppressed.join(", ")
            ));
        }
        if !parts.is_empty() {
            kept.insert(
                SYSTEM_MESSAGE.to_string(),
                Value::String(parts.join("\n\n")),
            );
        }
    }

    serde_json::to_string(&Value::Object(kept)).unwrap_or_else(|_| stdout.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::ShimSpec;

    fn spec(allowed: &[&str], fold: &[&str]) -> ShimSpec {
        ShimSpec {
            source_id: "x".into(),
            command: "true".into(),
            plugin_root: None,
            if_pattern: None,
            event: None,
            output_strategy: HookOutputStrategy::Legacy,
            allowed_output: allowed.iter().map(|s| s.to_string()).collect(),
            fold_into_system_message: fold.iter().map(|s| s.to_string()).collect(),
            rewake_message: None,
            rewake_summary: None,
            timeout_seconds: None,
        }
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("normalised output must be valid JSON")
    }

    #[test]
    fn a_key_the_target_rejects_is_dropped_and_named() {
        let out = normalize(
            r#"{"systemMessage":"keep me","rewakeSummary":"drop me"}"#,
            &spec(&["systemMessage"], &[]),
        )
        .unwrap();
        let v = parse(&out);
        assert!(v.get("rewakeSummary").is_none(), "the key must not survive");
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.contains("keep me"), "existing text lost: {msg}");
        assert!(
            msg.contains("rewakeSummary"),
            "a dropped field must be named, never deleted quietly: {msg}"
        );
    }

    #[test]
    fn a_dropped_key_carrying_human_text_is_folded_into_system_message() {
        let out = normalize(
            r#"{"rewakeMessage":"security findings follow"}"#,
            &spec(&["systemMessage"], &["rewakeMessage"]),
        )
        .unwrap();
        let v = parse(&out);
        assert!(
            v["systemMessage"]
                .as_str()
                .unwrap()
                .contains("security findings follow"),
            "user-visible text must survive, got {v}"
        );
    }

    #[test]
    fn folding_appends_rather_than_overwriting_an_existing_system_message() {
        let out = normalize(
            r#"{"systemMessage":"first","rewakeMessage":"second"}"#,
            &spec(&["systemMessage"], &["rewakeMessage"]),
        )
        .unwrap();
        let msg = parse(&out)["systemMessage"].as_str().unwrap().to_string();
        assert!(msg.contains("first"), "existing text lost: {msg}");
        assert!(msg.contains("second"), "folded text lost: {msg}");
    }

    #[test]
    fn dropping_a_key_that_cannot_be_folded_leaves_a_note_rather_than_deleting_quietly() {
        let out = normalize(
            r#"{"systemMessage":"hi","metrics":{"n":1}}"#,
            &spec(&["systemMessage"], &[]),
        )
        .unwrap();
        let msg = parse(&out)["systemMessage"].as_str().unwrap().to_string();
        assert!(msg.contains("metrics"), "suppression must be named: {msg}");
    }

    #[test]
    fn a_target_that_rejects_system_message_cannot_be_given_a_note() {
        // Nothing to fold into, so the note is impossible. Dropping is all that
        // is left, and the output must still be valid for the target.
        let out = normalize(
            r#"{"systemMessage":"hi","metrics":1}"#,
            &spec(&["additionalContext"], &[]),
        )
        .unwrap();
        let v = parse(&out);
        assert!(v.get("systemMessage").is_none());
        assert!(v.get("metrics").is_none());
    }

    #[test]
    fn a_configured_rewake_message_is_folded_into_system_message_when_the_hook_has_output() {
        let mut s = spec(&["systemMessage"], &[]);
        s.rewake_message = Some("a rewake would have followed up here".into());
        let out = normalize(r#"{"systemMessage":"ran clean"}"#, &s).unwrap();
        let v = parse(&out);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(
            msg.contains("a rewake would have followed up here"),
            "the configured rewake text must survive, got {msg}"
        );
        assert!(
            msg.contains("ran clean"),
            "the hook's own text must survive too: {msg}"
        );
    }

    #[test]
    fn a_configured_rewake_message_is_not_manufactured_for_a_silent_hook() {
        // The hook printed nothing, so there is nothing to attach the static
        // rewake text to. Fabricating a systemMessage here would invent output
        // for a hook that had none.
        let mut s = spec(&["systemMessage"], &[]);
        s.rewake_message = Some("should not appear".into());
        let out = normalize("{}", &s).unwrap();
        let v = parse(&out);
        assert!(
            v.get("systemMessage").is_none(),
            "must not manufacture a message for silent output: {v}"
        );
    }

    #[test]
    fn a_configured_rewake_summary_is_folded_in_alongside_the_message() {
        let mut s = spec(&["systemMessage"], &[]);
        s.rewake_message = Some("message text".into());
        s.rewake_summary = Some("summary text".into());
        let out = normalize(r#"{"systemMessage":"ran"}"#, &s).unwrap();
        let msg = parse(&out)["systemMessage"].as_str().unwrap().to_string();
        assert!(msg.contains("message text"), "got {msg}");
        assert!(msg.contains("summary text"), "got {msg}");
    }

    #[test]
    fn non_json_output_passes_through_untouched() {
        // Never wrap plain text in a fabricated JSON envelope.
        let text = "plain text warning\n";
        assert_eq!(
            normalize(text, &spec(&["systemMessage"], &[])).unwrap(),
            text
        );
    }

    #[test]
    fn empty_output_stays_empty() {
        assert_eq!(normalize("", &spec(&["systemMessage"], &[])).unwrap(), "");
    }

    #[test]
    fn json_that_is_not_an_object_passes_through_untouched() {
        assert_eq!(
            normalize("[1,2]", &spec(&["systemMessage"], &[])).unwrap(),
            "[1,2]"
        );
    }

    #[test]
    fn codex_v1_uses_the_event_aware_translator() {
        // A regression to the legacy path would leave this as bare text,
        // which Codex cannot unambiguously treat as hook output.
        let mut s = spec(&["systemMessage"], &[]);
        s.event = Some("SessionStart".into());
        s.output_strategy = crate::core::model::HookOutputStrategy::CodexV1;
        assert_eq!(
            normalize("security guidance", &s).unwrap(),
            r#"{"systemMessage":"security guidance"}"#
        );
    }
}
