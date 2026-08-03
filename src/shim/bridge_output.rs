//! Translating hook stdout into the one typed bridge action object the
//! OpenCode-family runtimes (OpenCode, Kilo) receive.
//!
//! Both hosts were measured to expose the identical callback surface (see
//! `docs/open-work.md`, "Verified runtime contracts"), so `OpenCodeV1` and
//! `KiloV1` share this single translator rather than each inventing its own
//! shape. Every call produces exactly one [`BridgeAction`] object — never a
//! raw passthrough of whatever the hook happened to print — so the generated
//! bridge (OW-008/OW-009) has one typed thing to interpret, not a per-target
//! grab bag of possible JSON keys.
//!
//! A callback with no measured output channel (`config`, `auth`, `event`)
//! must never be handed a `BridgeAction` that looks like delivery happened.
//! Refusing outright, loudly, is the honest answer; anything else would be a
//! plausible value manufactured in place of missing data.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::model::{self, HookFidelity};
use crate::shim::ShimSpec;

/// The one typed action a bridged callback hands back to its host runtime.
///
/// There is deliberately no field here that could claim an asynchronous
/// rewake happened: the shape itself makes that unrepresentable, rather than
/// relying on every caller to remember not to set one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeAction {
    /// The measured callback this action targets, e.g. `tool.execute.before`.
    pub callback: String,
    pub fidelity: HookFidelity,
    /// Text surfaced to the user or a log. `None` when the hook produced no
    /// output at all — never fabricated to fill the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether the intercepted operation should be blocked/denied. Only ever
    /// `true` for a callback that runs *before* its operation and can
    /// actually stop it; a callback that only observes after the fact must
    /// never claim this, no matter what the hook's stdout asked for.
    pub block: bool,
}

/// Callbacks that run before their operation and can therefore actually deny
/// it. Every other callback structurally cannot, so `block` stays `false`
/// there regardless of what the hook printed.
const CAN_BLOCK: &[&str] = &["tool.execute.before"];

/// Translate one hook run's stdout into the single bridge action object for
/// `callback`.
///
/// Errors rather than returning a degraded action when:
/// - the callback has no measured output channel (never claim delivery);
/// - the handler asked for `asyncRewake` (a bridge action is one round trip
///   inside a single callback invocation; there is no "later" for an async
///   wake to happen in, so it stays blocked here exactly as it does for every
///   other target).
pub fn translate(stdout: &str, callback: &str, spec: &ShimSpec) -> Result<String> {
    if spec.rewake_message.is_some() || spec.rewake_summary.is_some() {
        // These are carried into the sidecar only when the target cannot
        // represent the field itself (see `src/shim/generate.rs`), meaning
        // an OpenCode-family target here. There is no rewake channel to fold
        // them into, so — unlike the legacy path, which can fold this text
        // into `systemMessage` — a bridge action has nowhere honest to put
        // it and must say so rather than silently drop or silently deliver
        // it as if it were the hook's own output.
        bail!(
            "asyncRewake cannot be bridged to the OpenCode family for {}; it stays blocked",
            spec.source_id
        );
    }

    let fidelity = model::opencode_family_hook_fidelity(callback).with_context(|| {
        format!(
            "{callback} has no measured output channel a bridge action could travel through, \
             so {} must stay blocked rather than claim delivery",
            spec.source_id
        )
    })?;

    let (message, wants_block) = extract(stdout)?;
    let block = wants_block && CAN_BLOCK.contains(&callback);

    let action = BridgeAction {
        callback: callback.to_string(),
        fidelity,
        message,
        block,
    };
    serde_json::to_string(&action).context("serialising bridge action")
}

/// Pull a user-facing message and a block request out of the hook's stdout.
///
/// Empty output produces no message: fabricating text for a hook that said
/// nothing would be inventing output it never produced. Plain, non-JSON text
/// becomes the message verbatim — it is still folded into the one typed
/// object rather than passed through raw, because a bridge action is always
/// exactly one object.
fn extract(stdout: &str) -> Result<(Option<String>, bool)> {
    if stdout.is_empty() {
        return Ok((None, false));
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(map)) => {
            let mut parts: Vec<String> = Vec::new();
            for key in ["systemMessage", "message", "additionalContext"] {
                if let Some(Value::String(text)) = map.get(key) {
                    parts.push(text.clone());
                }
            }
            let known: std::collections::BTreeSet<&str> = [
                "systemMessage",
                "message",
                "additionalContext",
                "block",
                "decision",
            ]
            .into_iter()
            .collect();
            let unknown: Vec<&String> =
                map.keys().filter(|k| !known.contains(k.as_str())).collect();
            if !unknown.is_empty() {
                parts.push(format!(
                    "agentsync dropped fields the bridge cannot accept: {}",
                    unknown
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let wants_block = matches!(map.get("block"), Some(Value::Bool(true)))
                || matches!(map.get("decision"), Some(Value::String(d)) if d == "block");
            let message = (!parts.is_empty()).then(|| parts.join("\n\n"));
            Ok((message, wants_block))
        }
        // Not a JSON object — plain text, a JSON array, a bare number, or
        // malformed JSON. All of it is still the hook's own output and must
        // still reach the user, just with nothing structured to interpret.
        _ => Ok((Some(stdout.to_string()), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::HookOutputStrategy;

    fn spec() -> ShimSpec {
        ShimSpec {
            source_id: "demo@mkt:hooks/hooks.json:pre_tool_use:0:0".into(),
            command: "true".into(),
            plugin_root: None,
            if_pattern: None,
            event: Some("PreToolUse".into()),
            output_strategy: HookOutputStrategy::OpenCodeV1,
            allowed_output: vec![],
            fold_into_system_message: vec![],
            rewake_message: None,
            rewake_summary: None,
            timeout_seconds: None,
        }
    }

    fn action(json: &str) -> BridgeAction {
        serde_json::from_str(json).expect("translate must produce a valid BridgeAction")
    }

    #[test]
    fn tool_execute_before_is_exact_and_can_block() {
        let out = translate(
            r#"{"block":true,"systemMessage":"denied"}"#,
            "tool.execute.before",
            &spec(),
        )
        .unwrap();
        let a = action(&out);
        assert_eq!(a.callback, "tool.execute.before");
        assert_eq!(a.fidelity, HookFidelity::Exact);
        assert!(a.block, "an exact before-hook must be able to block");
        assert_eq!(a.message.as_deref(), Some("denied"));
    }

    #[test]
    fn tool_execute_after_never_claims_a_block_even_if_asked() {
        // An after-the-fact callback structurally cannot stop what already
        // ran. Never letting `block` become true there is the difference
        // between an honest bridge and one that lies about what it did.
        let out = translate(r#"{"block":true}"#, "tool.execute.after", &spec()).unwrap();
        let a = action(&out);
        assert_eq!(a.fidelity, HookFidelity::Exact);
        assert!(
            !a.block,
            "tool.execute.after must never claim it blocked anything"
        );
    }

    #[test]
    fn session_idle_is_side_effect_only() {
        let out = translate(
            r#"{"systemMessage":"idle logged"}"#,
            "session.idle",
            &spec(),
        )
        .unwrap();
        let a = action(&out);
        assert_eq!(a.fidelity, HookFidelity::SideEffectOnly);
        assert!(!a.block);
    }

    #[test]
    fn chat_message_is_best_effort() {
        let out = translate(r#"{"message":"seen"}"#, "chat.message", &spec()).unwrap();
        assert_eq!(action(&out).fidelity, HookFidelity::BestEffort);
    }

    #[test]
    fn config_auth_and_event_have_no_output_channel_and_are_refused() {
        for callback in ["config", "auth", "event"] {
            let err = translate("{}", callback, &spec()).unwrap_err().to_string();
            assert!(
                err.contains(callback),
                "error must name the callback it refused: {err}"
            );
        }
    }

    #[test]
    fn async_rewake_stays_blocked_rather_than_silently_dropped_or_delivered() {
        let mut s = spec();
        s.rewake_message = Some("would rewake here".into());
        let err = translate("{}", "tool.execute.before", &s)
            .unwrap_err()
            .to_string();
        assert!(err.contains("asyncRewake"), "got {err}");
    }

    #[test]
    fn empty_output_produces_no_manufactured_message() {
        let out = translate("", "session.idle", &spec()).unwrap();
        assert_eq!(action(&out).message, None);
    }

    #[test]
    fn a_dropped_unknown_field_is_named_not_silently_discarded() {
        let out = translate(r#"{"metrics":{"n":1}}"#, "session.idle", &spec()).unwrap();
        let msg = action(&out).message.expect("must name the drop");
        assert!(msg.contains("metrics"), "got {msg}");
    }

    #[test]
    fn plain_text_output_still_becomes_one_typed_action() {
        let out = translate("plain text warning", "session.error", &spec()).unwrap();
        let a = action(&out);
        assert_eq!(a.message.as_deref(), Some("plain text warning"));
        assert_eq!(a.fidelity, HookFidelity::SideEffectOnly);
    }

    #[test]
    fn an_unmeasured_callback_name_is_refused_not_guessed_at() {
        assert!(translate("{}", "tool.execute.retry", &spec()).is_err());
    }
}
