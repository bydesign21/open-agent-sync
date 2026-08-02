//! Codex's event-aware hook stdout contract.
//!
//! Claude hooks can emit transport records and event-specific response
//! objects. Codex accepts one response object, and a key that is harmless for
//! one event can be invalid for another, so this conversion validates the
//! event before anything reaches the target runtime.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::shim::ShimSpec;

/// Translate one hook command's stdout to Codex's single-object contract.
pub fn translate(stdout: &str, event: &str, spec: &ShimSpec) -> Result<String> {
    if stdout.trim().is_empty() {
        return Ok(String::new());
    }

    let response = select_response(stdout)?;
    let mut response = response.unwrap_or_default();
    response.remove("metrics");
    validate_response(&response, event, spec)?;

    let json = serde_json::to_string(&Value::Object(response))?;
    Ok(crate::shim::output::normalize_legacy(&json, spec))
}

fn select_response(stdout: &str) -> Result<Option<Map<String, Value>>> {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        return select_records(vec![as_object(value)?]);
    }

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut records = Vec::new();
    let mut plain = 0usize;
    for line in &lines {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => records.push(as_object(value)?),
            Err(error) if line.trim_start().starts_with('{') => {
                bail!("malformed JSON hook output: {error}")
            }
            Err(_) => plain += 1,
        }
    }

    if !records.is_empty() && plain != 0 {
        bail!("mixed JSON and plain-text hook output");
    }
    if records.is_empty() {
        let mut wrapped = Map::new();
        wrapped.insert("systemMessage".into(), Value::String(stdout.to_string()));
        return Ok(Some(wrapped));
    }

    select_records(records)
}

fn select_records(records: Vec<Map<String, Value>>) -> Result<Option<Map<String, Value>>> {
    let mut response = None;
    for record in records {
        if is_transport_record(&record) {
            continue;
        }
        if response.replace(record).is_some() {
            bail!("hook emitted more than one response record");
        }
    }
    Ok(response)
}

fn as_object(value: Value) -> Result<Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("hook output must be a JSON object"))
}

fn is_transport_record(record: &Map<String, Value>) -> bool {
    !record.is_empty()
        && record
            .keys()
            .all(|key| matches!(key.as_str(), "async" | "asyncTimeout"))
}

fn validate_response(response: &Map<String, Value>, event: &str, spec: &ShimSpec) -> Result<()> {
    let allowed = top_level_keys(event)?;
    for key in response.keys() {
        if key == "metrics"
            || allowed.contains(&key.as_str())
            || spec
                .fold_into_system_message
                .iter()
                .any(|field| field == key)
        {
            continue;
        }
        bail!("{event} hook output does not allow top-level {key}");
    }

    if let Some(output) = response.get("hookSpecificOutput") {
        let nested = output
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("{event} hookSpecificOutput must be an object"))?;
        let actual = nested
            .get("hookEventName")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("{event} hookSpecificOutput is missing hookEventName")
            })?;
        if actual != event {
            bail!("hookSpecificOutput hookEventName {actual} does not match {event}");
        }
        let nested_allowed = nested_keys(event)?;
        for key in nested.keys() {
            if key != "hookEventName" && !nested_allowed.contains(&key.as_str()) {
                bail!("{event} hookSpecificOutput does not allow {key}");
            }
        }
    }
    Ok(())
}

fn top_level_keys(event: &str) -> Result<&'static [&'static str]> {
    match event {
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SessionEnd" => Ok(&[
            "continue",
            "suppressOutput",
            "systemMessage",
            "hookSpecificOutput",
        ]),
        "Stop" => Ok(&[
            "continue",
            "stopReason",
            "suppressOutput",
            "systemMessage",
            "hookSpecificOutput",
        ]),
        _ => bail!("unsupported Codex hook event {event}"),
    }
}

fn nested_keys(event: &str) -> Result<&'static [&'static str]> {
    match event {
        "SessionStart" | "UserPromptSubmit" => Ok(&["additionalContext"]),
        "PreToolUse" => Ok(&[
            "permissionDecision",
            "permissionDecisionReason",
            "updatedInput",
        ]),
        "PostToolUse" => Ok(&["additionalContext", "updatedMCPToolOutput"]),
        "Stop" => Ok(&["decision", "reason"]),
        "SessionEnd" => Ok(&[]),
        _ => bail!("unsupported Codex hook event {event}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::ShimSpec;
    use crate::core::model::HookOutputStrategy;

    fn spec() -> ShimSpec {
        ShimSpec {
            source_id:
                "security-guidance@claude-plugins-official:hooks/hooks.json:session_start:0:0"
                    .into(),
            command: "true".into(),
            plugin_root: None,
            if_pattern: None,
            event: Some("SessionStart".into()),
            output_strategy: HookOutputStrategy::CodexV1,
            allowed_output: vec!["systemMessage".into()],
            fold_into_system_message: vec!["rewakeMessage".into()],
            rewake_message: None,
            rewake_summary: None,
        }
    }

    #[test]
    fn security_guidance_session_start_becomes_one_codex_object() {
        // A change that passed `metrics` through, emitted two records, or made a
        // default user-visible message from diagnostics must fail this test.
        let stdout = r#"{"async": true, "asyncTimeout": 180000}
{"metrics": {"sdk_bootstrap": 8, "sdk_bootstrap_ms": 338, "sdk_hook_py": 311, "pv": 20006}}"#;

        let output = super::translate(stdout, "SessionStart", &spec()).unwrap();
        let parsed: Value = serde_json::from_str(&output).expect("one JSON object");
        let object = parsed.as_object().expect("a JSON object, not a stream");
        assert!(
            object.is_empty(),
            "metrics without a user message must not survive: {parsed}"
        );
    }

    #[test]
    fn plain_text_becomes_a_system_message() {
        // Returning bare text would make the hook protocol ambiguous to Codex.
        let output = super::translate(
            "security guidance: update dependencies",
            "SessionStart",
            &spec(),
        )
        .unwrap();
        assert_eq!(
            output,
            r#"{"systemMessage":"security guidance: update dependencies"}"#
        );
    }

    #[test]
    fn top_level_additional_context_is_rejected() {
        let error = super::translate(
            r#"{"additionalContext":"wrong level"}"#,
            "SessionStart",
            &spec(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("additionalContext"), "got {error}");
    }

    #[test]
    fn mismatched_hook_event_name_is_rejected() {
        let error = super::translate(
            r#"{"hookSpecificOutput":{"hookEventName":"Stop","decision":"block"}}"#,
            "SessionStart",
            &spec(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Stop"), "got {error}");
        assert!(error.contains("SessionStart"), "got {error}");
    }

    #[test]
    fn two_response_records_are_rejected() {
        let error = super::translate(
            "{\"systemMessage\":\"first\"}\n{\"systemMessage\":\"second\"}",
            "SessionStart",
            &spec(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("more than one"), "got {error}");
    }

    #[test]
    fn mixed_json_and_plain_text_records_are_rejected() {
        let error = super::translate(
            "{\"systemMessage\":\"structured\"}\nplain text",
            "SessionStart",
            &spec(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("mixed"), "got {error}");
    }

    #[test]
    fn malformed_json_is_rejected_instead_of_becoming_a_system_message() {
        // A missing closing brace is a broken response record, not human text.
        let error = super::translate(r#"{"systemMessage":"unterminated"#, "SessionStart", &spec())
            .unwrap_err()
            .to_string();
        assert!(error.contains("JSON"), "got {error}");
    }

    #[test]
    fn nested_fields_are_rejected_when_the_discriminator_matches_the_wrong_event() {
        // Keep `hookEventName` correct so this reaches the event-specific
        // nested field validation instead of failing only at the discriminator.
        let error = super::translate(
            r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","permissionDecision":"deny"}}"#,
            "SessionStart",
            &spec(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("permissionDecision"), "got {error}");
    }

    #[test]
    fn event_specific_fields_are_only_accepted_for_their_event() {
        // Each fixture is a legal field for its named event. Changing the event
        // must fail rather than passing a plausible but invalid payload through.
        let fixtures = [
            (
                "SessionStart",
                r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"start"}}"#,
            ),
            (
                "UserPromptSubmit",
                r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"prompt"}}"#,
            ),
            (
                "PreToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"unsafe"}}"#,
            ),
            (
                "PostToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"result"}}"#,
            ),
            (
                "Stop",
                r#"{"hookSpecificOutput":{"hookEventName":"Stop","decision":"block","reason":"findings"}}"#,
            ),
            (
                "SessionEnd",
                r#"{"hookSpecificOutput":{"hookEventName":"SessionEnd"}}"#,
            ),
        ];

        for (event, stdout) in fixtures {
            let output = super::translate(stdout, event, &spec())
                .unwrap_or_else(|error| panic!("{event} fixture must be accepted: {error}"));
            let parsed: Value = serde_json::from_str(&output).expect("one translated object");
            assert!(
                parsed.is_object(),
                "{event} output must be an object: {parsed}"
            );

            let wrong_event = if event == "Stop" {
                "SessionStart"
            } else {
                "Stop"
            };
            assert!(
                super::translate(stdout, wrong_event, &spec()).is_err(),
                "{event} fields must not be accepted as {wrong_event}"
            );
        }
    }
}
