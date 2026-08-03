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

    let mut response = select_response(stdout)?.unwrap_or_default();
    response.remove("metrics");

    // Claude's rewake fields carry text for a person, but Codex does not
    // accept either key on stdout. Move their values into systemMessage before
    // validating the final object. A wrong-typed value is malformed output,
    // not an excuse to silently discard a finding.
    let mut folded = Vec::new();
    let mut fold_keys: std::collections::BTreeSet<&str> = spec
        .fold_into_system_message
        .iter()
        .map(String::as_str)
        .collect();
    fold_keys.insert("rewakeMessage");
    fold_keys.insert("rewakeSummary");
    for key in fold_keys {
        take_text(&mut response, key, &mut folded)?;
    }

    validate_response(&response, event)?;

    // A metrics-only record means the hook had nothing for Codex. Static
    // rewake text must not manufacture a finding in that case.
    if !response.is_empty() || !folded.is_empty() {
        let mut messages = Vec::new();
        push_unique(&mut messages, spec.rewake_message.as_deref());
        push_unique(&mut messages, spec.rewake_summary.as_deref());
        if let Some(existing) = response.get("systemMessage").and_then(Value::as_str) {
            push_unique(&mut messages, Some(existing));
        }
        for text in &folded {
            push_unique(&mut messages, Some(text));
        }
        if !messages.is_empty() {
            response.insert("systemMessage".into(), Value::String(messages.join("\n\n")));
        }
    }

    // This is already the final, event-validated Codex object. Passing it
    // through the legacy allow-list would discard valid common controls and
    // event-specific fields that the old, event-blind model does not know.
    Ok(serde_json::to_string(&Value::Object(response))?)
}

fn take_text(response: &mut Map<String, Value>, key: &str, out: &mut Vec<String>) -> Result<()> {
    let Some(value) = response.remove(key) else {
        return Ok(());
    };
    let Value::String(text) = value else {
        bail!("{key} must be a string");
    };
    out.push(text);
    Ok(())
}

fn push_unique(messages: &mut Vec<String>, text: Option<&str>) {
    if let Some(text) = text
        && !messages.iter().any(|existing| existing == text)
    {
        messages.push(text.to_string());
    }
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
            Err(error) if starts_like_json_object(line) => {
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

fn starts_like_json_object(line: &str) -> bool {
    line.trim_start()
        .strip_prefix('{')
        .and_then(|after_brace| after_brace.trim_start().chars().next())
        .is_some_and(|first| matches!(first, '}' | '"'))
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

fn validate_response(response: &Map<String, Value>, event: &str) -> Result<()> {
    let allowed = top_level_keys(event)?;
    for key in response.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        bail!("{event} hook output does not allow top-level {key}");
    }

    require_bool(response, "continue")?;
    require_string(response, "stopReason")?;
    require_bool(response, "suppressOutput")?;
    require_string(response, "systemMessage")?;
    require_string(response, "reason")?;
    if let Some(decision) = response.get("decision") {
        let decision = decision
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("decision must be a string"))?;
        let allowed = match event {
            "PreToolUse" => &["approve", "block"][..],
            "UserPromptSubmit" | "PostToolUse" | "Stop" => &["block"][..],
            _ => &[][..],
        };
        if !allowed.contains(&decision) {
            bail!("{event} decision does not allow {decision}");
        }
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
        require_string(nested, "additionalContext")?;
        require_string(nested, "permissionDecisionReason")?;
        if let Some(decision) = nested.get("permissionDecision") {
            let decision = decision
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("permissionDecision must be a string"))?;
            if !["allow", "deny", "ask"].contains(&decision) {
                bail!("PreToolUse permissionDecision does not allow {decision}");
            }
        }
    }
    Ok(())
}

fn require_bool(object: &Map<String, Value>, key: &str) -> Result<()> {
    if object.get(key).is_some_and(|value| !value.is_boolean()) {
        bail!("{key} must be a boolean");
    }
    Ok(())
}

fn require_string(object: &Map<String, Value>, key: &str) -> Result<()> {
    if object.get(key).is_some_and(|value| !value.is_string()) {
        bail!("{key} must be a string");
    }
    Ok(())
}

fn top_level_keys(event: &str) -> Result<&'static [&'static str]> {
    match event {
        "SessionStart" | "SessionEnd" => Ok(&[
            "continue",
            "stopReason",
            "suppressOutput",
            "systemMessage",
            "hookSpecificOutput",
        ]),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Ok(&[
            "continue",
            "stopReason",
            "suppressOutput",
            "systemMessage",
            "decision",
            "reason",
            "hookSpecificOutput",
        ]),
        "Stop" => Ok(&[
            "continue",
            "stopReason",
            "suppressOutput",
            "systemMessage",
            "decision",
            "reason",
        ]),
        _ => bail!("unsupported Codex hook event {event}"),
    }
}

fn nested_keys(event: &str) -> Result<&'static [&'static str]> {
    match event {
        "SessionStart" | "UserPromptSubmit" => Ok(&["additionalContext"]),
        "PreToolUse" => Ok(&[
            "additionalContext",
            "permissionDecision",
            "permissionDecisionReason",
            "updatedInput",
        ]),
        "PostToolUse" => Ok(&["additionalContext", "updatedMCPToolOutput"]),
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
            timeout_seconds: None,
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
    fn security_guidance_post_tool_use_findings_become_one_codex_object() {
        // Verbatim shape emitted by security-guidance 2.0.6 when a pattern
        // finding survives its baseline and de-duplication filters.
        let stdout = r#"{"metrics":{"pattern_hits":1,"rule_id":7,"rule_mask":128,"pv":20006},"rewakeSummary":"Commit security review found issues","hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"[from security-guidance@claude-code-plugins plugin]\n\nDo not deserialize untrusted data."}}"#;

        let output = super::translate(stdout, "PostToolUse", &spec()).unwrap();
        let parsed: Value = serde_json::from_str(&output).expect("one JSON object");
        assert_eq!(
            parsed["hookSpecificOutput"],
            serde_json::json!({
                "hookEventName": "PostToolUse",
                "additionalContext": "[from security-guidance@claude-code-plugins plugin]\n\nDo not deserialize untrusted data."
            })
        );
        assert_eq!(
            parsed["systemMessage"], "Commit security review found issues",
            "the human-facing finding summary must survive in a Codex field"
        );
        assert!(
            parsed.get("metrics").is_none(),
            "telemetry must not survive"
        );
        assert!(
            parsed.get("rewakeSummary").is_none(),
            "Claude-only fields must not survive"
        );
    }

    #[test]
    fn security_guidance_stop_findings_keep_the_top_level_block() {
        // Verbatim shape built by security-guidance 2.0.6 emit_metrics() for
        // Stop findings. Stop deliberately does not use hookSpecificOutput.
        let stdout = r#"{"metrics":{"vulns_found":1,"pv":20006},"rewakeSummary":"Background security review found issues","decision":"block","reason":"[from security-guidance@claude-code-plugins plugin]\n\nPotential command injection."}"#;

        let output = super::translate(stdout, "Stop", &spec()).unwrap();
        let parsed: Value = serde_json::from_str(&output).expect("one JSON object");
        assert_eq!(parsed["decision"], "block");
        assert_eq!(
            parsed["reason"],
            "[from security-guidance@claude-code-plugins plugin]\n\nPotential command injection."
        );
        assert_eq!(
            parsed["systemMessage"],
            "Background security review found issues"
        );
        assert!(
            parsed.get("metrics").is_none(),
            "telemetry must not survive"
        );
        assert!(parsed.get("rewakeSummary").is_none());
        assert!(parsed.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn valid_common_control_fields_are_preserved() {
        let output = super::translate(
            r#"{"continue":false,"stopReason":"operator requested stop","suppressOutput":true,"systemMessage":"stopping"}"#,
            "SessionStart",
            &spec(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&output).expect("one JSON object");
        assert_eq!(
            parsed,
            serde_json::json!({
                "continue": false,
                "stopReason": "operator requested stop",
                "suppressOutput": true,
                "systemMessage": "stopping"
            })
        );
    }

    #[test]
    fn configured_human_text_is_folded_before_final_codex_validation() {
        let mut configured = spec();
        configured.fold_into_system_message = vec!["customSummary".into()];

        let output = super::translate(
            r#"{"customSummary":"review completed with findings"}"#,
            "SessionStart",
            &configured,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&output).expect("one JSON object");
        assert_eq!(
            parsed,
            serde_json::json!({"systemMessage": "review completed with findings"}),
            "a configured fold key must not leak into the final Codex object"
        );
    }

    #[test]
    fn wrong_typed_codex_fields_are_rejected_by_name() {
        let fixtures = [
            ("SessionStart", r#"{"continue":"false"}"#, "continue"),
            ("SessionStart", r#"{"stopReason":false}"#, "stopReason"),
            (
                "SessionStart",
                r#"{"suppressOutput":"yes"}"#,
                "suppressOutput",
            ),
            ("SessionStart", r#"{"systemMessage":5}"#, "systemMessage"),
            ("PostToolUse", r#"{"decision":5}"#, "decision"),
            ("PostToolUse", r#"{"reason":false}"#, "reason"),
            (
                "PostToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":7}}"#,
                "additionalContext",
            ),
            (
                "PreToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":true}}"#,
                "permissionDecision",
            ),
            (
                "PreToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecisionReason":[]}}"#,
                "permissionDecisionReason",
            ),
            (
                "PostToolUse",
                r#"{"rewakeSummary":9,"hookSpecificOutput":{"hookEventName":"PostToolUse"}}"#,
                "rewakeSummary",
            ),
        ];

        for (event, stdout, field) in fixtures {
            let error = super::translate(stdout, event, &spec())
                .expect_err("wrong-typed structured output must fail")
                .to_string();
            assert!(
                error.contains(field),
                "{event} error must name {field}, got {error}"
            );
        }
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
    fn brace_prefixed_plain_text_becomes_a_system_message() {
        // A human note may begin with a brace without being a JSON object.
        let output = super::translate("{draft reminder", "SessionStart", &spec()).unwrap();
        assert_eq!(output, r#"{"systemMessage":"{draft reminder"}"#);
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
            ("Stop", r#"{"decision":"block","reason":"findings"}"#),
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
