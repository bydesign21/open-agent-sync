//! Reshaping a hook's stdout into what the target host accepts.
//!
//! Dropping a key is never silent when its content was meant for a person.
//! Text moves into `systemMessage` where the target accepts one, and where it
//! does not, the drop is still named rather than performed quietly.

use serde_json::{Map, Value};

use crate::shim::ShimSpec;

const SYSTEM_MESSAGE: &str = "systemMessage";

/// Filter `stdout` to the keys the target accepts.
///
/// Output that is not a JSON object passes through unchanged. A hook is free to
/// print plain text, and inventing a JSON envelope around it would be a claim
/// the hook never made.
pub fn normalize(stdout: &str, spec: &ShimSpec) -> String {
    let Ok(Value::Object(original)) = serde_json::from_str::<Value>(stdout) else {
        return stdout.to_string();
    };

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
        } else if value.is_string() {
            // A plain string key that nobody declared as carrying human text.
            // Drop it without a note. A note on every unlisted string field
            // would bury the notes that matter under routine noise.
            continue;
        } else {
            suppressed.push(key);
        }
    }

    if spec.allowed_output.iter().any(|k| k == SYSTEM_MESSAGE) {
        let mut parts: Vec<String> = Vec::new();
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
            allowed_output: allowed.iter().map(|s| s.to_string()).collect(),
            fold_into_system_message: fold.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("normalised output must be valid JSON")
    }

    #[test]
    fn a_key_the_target_rejects_is_dropped() {
        let out = normalize(
            r#"{"systemMessage":"keep me","rewakeSummary":"drop me"}"#,
            &spec(&["systemMessage"], &[]),
        );
        let v = parse(&out);
        assert_eq!(v["systemMessage"], "keep me");
        assert!(v.get("rewakeSummary").is_none());
    }

    #[test]
    fn a_dropped_key_carrying_human_text_is_folded_into_system_message() {
        let out = normalize(
            r#"{"rewakeMessage":"security findings follow"}"#,
            &spec(&["systemMessage"], &["rewakeMessage"]),
        );
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
        );
        let msg = parse(&out)["systemMessage"].as_str().unwrap().to_string();
        assert!(msg.contains("first"), "existing text lost: {msg}");
        assert!(msg.contains("second"), "folded text lost: {msg}");
    }

    #[test]
    fn dropping_a_key_that_cannot_be_folded_leaves_a_note_rather_than_deleting_quietly() {
        let out = normalize(
            r#"{"systemMessage":"hi","metrics":{"n":1}}"#,
            &spec(&["systemMessage"], &[]),
        );
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
        );
        let v = parse(&out);
        assert!(v.get("systemMessage").is_none());
        assert!(v.get("metrics").is_none());
    }

    #[test]
    fn non_json_output_passes_through_untouched() {
        // Never wrap plain text in a fabricated JSON envelope.
        let text = "plain text warning\n";
        assert_eq!(normalize(text, &spec(&["systemMessage"], &[])), text);
    }

    #[test]
    fn empty_output_stays_empty() {
        assert_eq!(normalize("", &spec(&["systemMessage"], &[])), "");
    }

    #[test]
    fn json_that_is_not_an_object_passes_through_untouched() {
        assert_eq!(normalize("[1,2]", &spec(&["systemMessage"], &[])), "[1,2]");
    }
}
