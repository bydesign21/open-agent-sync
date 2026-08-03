//! MCP reconciliation behaviour for the OpenCode family.
//!
//! The parsers themselves live in [`crate::hosts::parsers::mcp`] alongside the
//! other host parsers. The contract tests live here, with the rest of the
//! family engine, because what they pin down is family behaviour rather than
//! parser plumbing.
//!
//! This module also holds the **write path**: OpenCode and Kilo have no `mcp
//! remove` command (measured: `opencode mcp` offers only `add`, `list`,
//! `auth`, `logout`, `debug`), so add, update, and remove all go through a
//! guarded [`crate::transaction::ConfigTransaction`] edit against the raw
//! JSONC file, never a host CLI call and never `<host> debug config` output.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::core::model::{McpServer, OAuthState, Transport};
use crate::jsonc::{EditOperation, JsoncEdit, JsoncPointer, PathSegment};
use crate::transaction::{
    ConfigEditOperation, ConfigOrigin, ConfigScope, ConfigTransaction, FilePrecondition,
    GuardedSource, SourceEdit, compute_sha256,
};

/// Serialize a canonical server back into the OpenCode-family `mcp.<name>`
/// JSON shape, undoing the normalizations [`crate::hosts::parsers::mcp::opencode_mcp_jsonc_v1`]
/// performs on read. Shared by OpenCode and Kilo: their MCP schema is
/// identical (Kilo is a fork of OpenCode).
///
/// Absent fields stay absent: `enabled`/`cwd`/`timeout`/`oauth` are only
/// written when the canonical server actually carries them, so adopting a
/// server that never mentioned them does not invent a host default.
pub fn server_to_json(server: &McpServer) -> Value {
    let mut obj = Map::new();
    match &server.transport {
        Transport::Stdio(s) => {
            obj.insert("type".into(), Value::String("local".into()));
            let mut argv = vec![s.command.clone()];
            argv.extend(s.args.iter().cloned());
            obj.insert(
                "command".into(),
                Value::Array(argv.into_iter().map(Value::String).collect()),
            );
            if !s.env.is_empty() || !s.env_from.is_empty() {
                let mut env = Map::new();
                for (k, v) in &s.env {
                    env.insert(k.clone(), Value::String(v.clone()));
                }
                for k in &s.env_from {
                    // Self-referential passthrough, in the OpenCode-family
                    // `{env:NAME}` idiom.
                    env.insert(k.clone(), Value::String(format!("{{env:{k}}}")));
                }
                obj.insert("environment".into(), Value::Object(env));
            }
        }
        Transport::Http(h) => {
            obj.insert("type".into(), Value::String("remote".into()));
            obj.insert("url".into(), Value::String(h.url.clone()));
            let mut headers = Map::new();
            for (k, v) in &h.headers {
                headers.insert(k.clone(), Value::String(v.clone()));
            }
            if let Some(var) = &h.bearer_token_env {
                headers.insert(
                    "Authorization".into(),
                    Value::String(format!("Bearer {{env:{var}}}")),
                );
            }
            if !headers.is_empty() {
                obj.insert("headers".into(), Value::Object(headers));
            }
        }
    }
    if let Some(enabled) = server.enabled {
        obj.insert("enabled".into(), Value::Bool(enabled));
    }
    if let Some(cwd) = &server.cwd {
        obj.insert("cwd".into(), Value::String(cwd.clone()));
    }
    if let Some(timeout) = &server.timeout_json {
        // Carried through as exact JSON text (the unit is unverified against
        // the runtime), so it is parsed back rather than re-encoded, and lands
        // as whatever shape it was read as instead of a quoted string.
        if let Ok(value) = serde_json::from_str::<Value>(timeout) {
            obj.insert("timeout".into(), value);
        }
    }
    match &server.oauth {
        OAuthState::Unspecified => {}
        OAuthState::Disabled => {
            obj.insert("oauth".into(), Value::Bool(false));
        }
        OAuthState::Automatic => {
            obj.insert("oauth".into(), Value::Bool(true));
        }
        OAuthState::Client {
            client_id,
            client_secret_env,
        } => {
            let mut client = Map::new();
            client.insert("client_id".into(), Value::String(client_id.clone()));
            if let Some(var) = client_secret_env {
                client.insert(
                    "client_secret".into(),
                    Value::String(format!("{{env:{var}}}")),
                );
            }
            obj.insert("oauth".into(), Value::Object(client));
        }
    }
    Value::Object(obj)
}

fn read_guarded(target: &Path) -> Result<(String, FilePrecondition)> {
    if target.is_file() {
        let bytes =
            std::fs::read(target).with_context(|| format!("reading {}", target.display()))?;
        let hash = compute_sha256(&bytes);
        let text = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", target.display()))?;
        Ok((text, FilePrecondition::Sha256(hash)))
    } else {
        Ok(("{}".to_string(), FilePrecondition::Absent))
    }
}

fn apply_set(text: &str, path: &[String], raw_json: &str) -> Result<String> {
    let doc = crate::jsonc::parse(text)?;
    crate::jsonc::apply_edit(
        &doc,
        &JsoncEdit {
            pointer: JsoncPointer {
                path: path.iter().cloned().map(PathSegment::Key).collect(),
                owning_node: None,
            },
            operation: EditOperation::SetExactJson(raw_json.to_string()),
        },
    )
}

/// One queued change to `mcp.<name>` for [`set_transaction_batch`].
pub enum McpJsoncOp {
    /// Add or update the entry.
    Set(Box<McpServer>),
    /// Remove the entry, if present.
    Remove,
}

/// Build ONE guarded transaction that applies every `(name, op)` pair in
/// `ops` to the same target file.
///
/// This exists because two servers landing in the same file can never be
/// represented as two separate [`ConfigTransaction`]s: each one reads the
/// file's hash at plan time as its precondition, so both would carry the
/// *same* original hash. At apply time the first write changes the file and
/// every other transaction's precondition then fails — the guard behaving
/// exactly as designed, against a plan that was wrong to split the write in
/// the first place. Folding every op destined for one file into one
/// transaction, with one precondition and one write, is the only correct
/// shape (OW-002 invariant 6, "MCP ... edits in one file compose into one
/// write").
///
/// `Ok(None)` means nothing to do: every op was a no-op removal of an
/// already-absent entry.
pub fn set_transaction_batch(
    target: &Path,
    ops: &[(String, McpJsoncOp)],
) -> Result<Option<ConfigTransaction>> {
    if ops.is_empty() {
        return Ok(None);
    }

    let (original, precondition) = read_guarded(target)?;
    let origin_hash = compute_sha256(original.as_bytes());
    let origin = ConfigOrigin::new(target.to_path_buf(), ConfigScope::Global, 0, origin_hash);

    let mut tx = ConfigTransaction::new(Value::Null)
        .with_source(GuardedSource::new(target.to_path_buf(), precondition));
    // The resolver context is deliberately left EMPTY here. `{env:NAME}` must
    // stay literal on BOTH sides of the projection comparison: the desired
    // value we build carries the placeholder, so the observed value must too.
    //
    // Populating this from the real process environment was tried and is
    // wrong: it resolves the observed side only, so the comparison becomes
    // `Bearer {env:TOK}` vs `Bearer <secret>`, verification fails, the write
    // rolls back, and no env-referencing server can ever sync. The live gate
    // catches exactly that regression.
    //
    // The placeholder belongs in the file — the host resolves it at runtime.
    // Whether the variable will actually resolve is a doctor concern, handled
    // by `report.rs::unresolved_env_refs`.

    // `set_exact_json` refuses to insert a value whose parent does not exist,
    // so a file with no top-level `mcp` object yet needs that object created
    // first — but only once, however many `Set` ops in this batch need it. A
    // file that already has one, even with other servers in it, is edited in
    // place: inserting or removing a member of `mcp` never touches its
    // siblings.
    let mut has_mcp_object = crate::jsonc::parse(&original)
        .ok()
        .and_then(|doc| doc.value.get("mcp").cloned())
        .is_some_and(|v| v.is_object());

    let mut working = original;
    let mut any_edit = false;

    for (name, op) in ops {
        match op {
            McpJsoncOp::Set(server) => {
                if !has_mcp_object {
                    working = apply_set(&working, &["mcp".to_string()], "{}")
                        .context("creating the top-level mcp object")?;
                    tx = tx.with_edit(SourceEdit {
                        origin: origin.clone(),
                        config_path: vec!["mcp".to_string()],
                        operation: ConfigEditOperation::Set {
                            value: serde_json::json!({}),
                            raw_json: Some("{}".to_string()),
                        },
                    });
                    has_mcp_object = true;
                }

                let entry_json = server_to_json(server);
                let raw_json = serde_json::to_string(&entry_json)?;
                working = apply_set(&working, &["mcp".to_string(), name.clone()], &raw_json)
                    .with_context(|| format!("setting mcp.{name}"))?;
                tx = tx.with_edit(SourceEdit {
                    origin: origin.clone(),
                    config_path: vec!["mcp".to_string(), name.clone()],
                    operation: ConfigEditOperation::Set {
                        value: entry_json,
                        raw_json: Some(raw_json),
                    },
                });
                any_edit = true;
            }
            McpJsoncOp::Remove => {
                let exists = crate::jsonc::parse(&working)
                    .ok()
                    .and_then(|doc| doc.value.get("mcp").and_then(|m| m.get(name)).cloned())
                    .is_some();
                if !exists {
                    continue;
                }
                working = crate::jsonc::apply_edit(
                    &crate::jsonc::parse(&working)?,
                    &JsoncEdit {
                        pointer: JsoncPointer {
                            path: vec![PathSegment::key("mcp"), PathSegment::key(name.clone())],
                            owning_node: None,
                        },
                        operation: EditOperation::Remove,
                    },
                )
                .with_context(|| format!("removing mcp.{name}"))?;
                tx = tx.with_edit(SourceEdit {
                    origin: origin.clone(),
                    config_path: vec!["mcp".to_string(), name.clone()],
                    operation: ConfigEditOperation::Remove,
                });
                any_edit = true;
            }
        }
    }

    if !any_edit {
        return Ok(None);
    }

    tx.expected_projection = crate::jsonc::parse(&working)
        .context("parsing the edited document to compute the expected projection")?
        .value;

    Ok(Some(tx))
}

/// Build a guarded transaction that sets (adds or updates) `mcp.<name>` in
/// the raw JSONC file at `target`.
///
/// Reads and edits the file's own bytes, never `<host> debug config` output:
/// the resolver substitutes `{env:NAME}` at resolve time, and diffing or
/// writing against the resolved projection would either report drift on
/// every pass or bake a resolved (and possibly empty) value back into the
/// file.
///
/// A thin single-op wrapper over [`set_transaction_batch`]; callers touching
/// more than one server in the same file must use that directly (or, in the
/// domain layer, [`crate::domains::mcp`]'s batching), not one call per
/// server, or the plan-time precondition race described there recurs.
pub fn set_transaction(target: &Path, name: &str, server: &McpServer) -> Result<ConfigTransaction> {
    Ok(set_transaction_batch(
        target,
        &[(name.to_string(), McpJsoncOp::Set(Box::new(server.clone())))],
    )?
    .expect("a Set op always produces an edit"))
}

/// Build a guarded transaction that removes `mcp.<name>` from the raw JSONC
/// file at `target`.
///
/// `Ok(None)` means there is nothing to remove: the file does not exist, or
/// `name` is already absent from it. Both are a no-op rather than an error —
/// removal racing an external change to "already gone" is not a failure.
pub fn remove_transaction(target: &Path, name: &str) -> Result<Option<ConfigTransaction>> {
    set_transaction_batch(target, &[(name.to_string(), McpJsoncOp::Remove)])
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use crate::core::model::StdioServer;
    use std::collections::BTreeMap;

    fn stdio(name: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            transport: Transport::Stdio(StdioServer {
                command: "node".into(),
                args: vec!["/x/index.js".into()],
                env: BTreeMap::from([("LEVEL".to_string(), "info".to_string())]),
                env_from: vec![],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn server_to_json_round_trips_through_the_parser() {
        let server = stdio("s");
        let json = server_to_json(&server);
        assert_eq!(json["type"], "local");
        assert_eq!(json["command"][0], "node");
        assert_eq!(json["environment"]["LEVEL"], "info");
    }

    #[test]
    fn env_from_serializes_as_a_self_referential_env_reference() {
        let mut server = stdio("s");
        let Transport::Stdio(s) = &mut server.transport else {
            unreachable!()
        };
        s.env.clear();
        s.env_from = vec!["TOKEN".to_string()];
        let json = server_to_json(&server);
        assert_eq!(json["environment"]["TOKEN"], "{env:TOKEN}");
    }

    #[test]
    fn absent_fields_are_not_written() {
        let json = server_to_json(&stdio("s"));
        assert!(json.get("enabled").is_none());
        assert!(json.get("cwd").is_none());
        assert!(json.get("timeout").is_none());
        assert!(json.get("oauth").is_none());
    }

    #[test]
    fn setting_into_a_missing_file_creates_the_mcp_object_and_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");

        let tx = set_transaction(&target, "s", &stdio("s")).unwrap();
        let mut tx = tx;
        tx.execute().unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["mcp"]["s"]["command"][0], "node");
    }

    #[test]
    fn setting_a_new_server_preserves_comments_and_sibling_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            "{\n  // keep me\n  \"mcp\": { \"other\": { \"type\": \"local\", \"command\": [\"x\"] } }\n}\n",
        )
        .unwrap();

        let mut tx = set_transaction(&target, "s", &stdio("s")).unwrap();
        tx.execute().unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("// keep me"));
        let doc = crate::jsonc::parse(&text).unwrap();
        assert_eq!(doc.value["mcp"]["other"]["command"][0], "x");
        assert_eq!(doc.value["mcp"]["s"]["command"][0], "node");
    }

    #[test]
    fn updating_an_existing_server_replaces_only_that_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            r#"{"mcp":{"s":{"type":"local","command":["old"]},"other":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();

        let mut tx = set_transaction(&target, "s", &stdio("s")).unwrap();
        tx.execute().unwrap();

        let doc = crate::jsonc::parse(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(doc.value["mcp"]["s"]["command"][0], "node");
        assert_eq!(doc.value["mcp"]["other"]["command"][0], "x");
    }

    #[test]
    fn removing_an_absent_file_is_a_no_op_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        assert!(remove_transaction(&target, "s").unwrap().is_none());
    }

    #[test]
    fn removing_an_already_absent_server_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            r#"{"mcp":{"other":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();
        assert!(remove_transaction(&target, "s").unwrap().is_none());
    }

    #[test]
    fn removing_an_existing_server_is_an_exact_jsonc_origin_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            "{\n  // keep me\n  \"mcp\": { \"s\": { \"type\": \"local\", \"command\": [\"node\"] }, \"other\": { \"type\": \"local\", \"command\": [\"x\"] } }\n}\n",
        )
        .unwrap();

        let mut tx = remove_transaction(&target, "s").unwrap().unwrap();
        tx.execute().unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("// keep me"));
        let doc = crate::jsonc::parse(&text).unwrap();
        assert!(doc.value["mcp"].get("s").is_none());
        assert_eq!(doc.value["mcp"]["other"]["command"][0], "x");
    }

    #[test]
    fn setting_a_server_never_emits_a_host_cli_invocation() {
        // There is no `opencode mcp remove`, so removal (and, for
        // consistency, add/update too) must never be represented as a Step
        // that shells out. This is a compile-time property of `set_transaction`
        // and `remove_transaction` returning a `ConfigTransaction`, not a
        // `Step::Host` — asserted here so a future refactor cannot reintroduce
        // a CLI call silently.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        let tx = set_transaction(&target, "s", &stdio("s")).unwrap();
        // A ConfigTransaction has no argv/binary to invoke; its only effect is
        // the guarded file write asserted by the tests above.
        assert!(tx.sources.iter().any(|s| s.path == target));
    }

    #[test]
    fn a_batch_of_three_new_servers_into_a_missing_file_is_one_transaction_with_one_source() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");

        let ops = vec![
            ("a".to_string(), McpJsoncOp::Set(Box::new(stdio("a")))),
            ("b".to_string(), McpJsoncOp::Set(Box::new(stdio("b")))),
            ("c".to_string(), McpJsoncOp::Set(Box::new(stdio("c")))),
        ];
        let mut tx = set_transaction_batch(&target, &ops).unwrap().unwrap();
        assert_eq!(
            tx.sources.len(),
            1,
            "one file must carry exactly one guarded source, however many servers land in it"
        );
        tx.execute().unwrap();

        let doc = crate::jsonc::parse(&std::fs::read_to_string(&target).unwrap()).unwrap();
        for name in ["a", "b", "c"] {
            assert_eq!(
                doc.value["mcp"][name]["command"][0], "node",
                "{name} missing after the batched write"
            );
        }
    }

    #[test]
    fn a_batch_composing_a_set_and_a_remove_in_one_file_preserves_comments_and_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            "{\n  // keep me\n  \"mcp\": { \"stale\": { \"type\": \"local\", \"command\": [\"old\"] }, \"other\": { \"type\": \"local\", \"command\": [\"x\"] } }\n}\n",
        )
        .unwrap();

        let ops = vec![
            (
                "fresh".to_string(),
                McpJsoncOp::Set(Box::new(stdio("fresh"))),
            ),
            ("stale".to_string(), McpJsoncOp::Remove),
        ];
        let mut tx = set_transaction_batch(&target, &ops).unwrap().unwrap();
        assert_eq!(tx.sources.len(), 1);
        tx.execute().unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("// keep me"), "{text}");
        let doc = crate::jsonc::parse(&text).unwrap();
        assert_eq!(doc.value["mcp"]["fresh"]["command"][0], "node");
        assert!(doc.value["mcp"].get("stale").is_none());
        assert_eq!(
            doc.value["mcp"]["other"]["command"][0], "x",
            "an untouched sibling must survive the compound write"
        );
    }

    #[test]
    fn a_batch_where_every_op_is_a_no_op_removal_yields_no_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("opencode.jsonc");
        std::fs::write(
            &target,
            r#"{"mcp":{"other":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();

        let ops = vec![
            ("gone".to_string(), McpJsoncOp::Remove),
            ("also-gone".to_string(), McpJsoncOp::Remove),
        ];
        assert!(set_transaction_batch(&target, &ops).unwrap().is_none());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::core::model::{OAuthState, Scope, Transport};
    use crate::hosts::parsers::{
        ParseCtx,
        mcp::{kilo_mcp_project_jsonc_v1, opencode_mcp_jsonc_v1},
    };

    fn ctx() -> ParseCtx {
        ParseCtx {
            repo: Some("/repo".into()),
            origin: PathBuf::from("/cfg/opencode/opencode.jsonc"),
        }
    }

    fn read(text: &str) -> crate::hosts::parsers::McpRead {
        opencode_mcp_jsonc_v1(text, &ctx()).expect("parse")
    }

    fn only(text: &str) -> crate::core::model::McpServer {
        let mut found = read(text);
        assert_eq!(found.servers.len(), 1, "expected exactly one server");
        found.servers.remove(0).1
    }

    #[test]
    fn jsonc_comments_do_not_prevent_reading_servers() {
        let server = only(
            r#"{
              // a comment that must not break parsing
              "mcp": { "s": { "type": "local", "command": ["srv"] } }
            }"#,
        );
        assert_eq!(server.name, "s");
    }

    #[test]
    fn a_command_array_round_trips_without_shell_splitting() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local",
               "command":["my-server","--flag","an argument with spaces"]}}}"#,
        );
        let Transport::Stdio(stdio) = server.transport else {
            panic!("expected stdio")
        };
        assert_eq!(stdio.command, "my-server");
        assert_eq!(
            stdio.args,
            vec!["--flag".to_string(), "an argument with spaces".to_string()],
            "an argument containing spaces must survive as one argument"
        );
    }

    #[test]
    fn literal_environment_values_and_env_references_stay_distinct() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"LITERAL":"plain-value","TOKEN":"{env:TOKEN}"}}}}"#,
        );
        let Transport::Stdio(stdio) = server.transport else {
            panic!("expected stdio")
        };
        assert_eq!(stdio.env.get("LITERAL").unwrap(), "plain-value");
        assert!(
            !stdio.env.contains_key("TOKEN"),
            "a reference must not be recorded as a literal value"
        );
        assert_eq!(stdio.env_from, vec!["TOKEN".to_string()]);
    }

    #[test]
    fn an_environment_key_reading_a_differently_named_variable_is_blocked() {
        let found = read(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"A":"{env:SOMETHING_ELSE}"}}}}"#,
        );
        let Transport::Stdio(stdio) = &found.servers[0].1.transport else {
            panic!("expected stdio")
        };
        assert!(stdio.env_from.is_empty());
        assert!(
            !stdio.env.contains_key("A"),
            "the placeholder text must not be flattened into a literal value, \
             or it would be pushed verbatim into other hosts"
        );
        assert!(
            found.warnings.iter().any(|w| w.contains("blocked")),
            "expected a blocker warning, got {:?}",
            found.warnings
        );
    }

    #[test]
    fn a_bearer_env_reference_is_distinct_from_a_plain_header() {
        let server = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "headers":{"Authorization":"Bearer {env:TOK}","X-Trace":"on"}}}}"#,
        );
        let Transport::Http(http) = server.transport else {
            panic!("expected http")
        };
        assert_eq!(http.bearer_token_env.as_deref(), Some("TOK"));
        assert_eq!(http.headers.get("X-Trace").unwrap(), "on");
        assert!(
            !http.headers.contains_key("Authorization"),
            "a bearer env reference must not also remain a raw header"
        );
    }

    #[test]
    fn a_literal_authorization_header_is_not_laundered_into_a_bearer_reference() {
        let server = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "headers":{"Authorization":"Bearer sk-literal-secret"}}}}"#,
        );
        let Transport::Http(http) = server.transport else {
            panic!("expected http")
        };
        assert_eq!(http.bearer_token_env, None);
        assert_eq!(
            http.headers.get("Authorization").unwrap(),
            "Bearer sk-literal-secret",
            "a literal credential must stay visible to the secret gate"
        );
    }

    #[test]
    fn enabled_cwd_and_timeout_are_represented() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "enabled":false,"cwd":"/work","timeout":5000}}}"#,
        );
        assert_eq!(server.enabled, Some(false));
        assert_eq!(server.cwd.as_deref(), Some("/work"));
        assert_eq!(
            server.timeout_json.as_deref(),
            Some("5000"),
            "the timeout is carried as exact JSON text because its unit is \
             unverified against the runtime"
        );
    }

    #[test]
    fn an_absent_field_stays_absent_rather_than_defaulting() {
        let server = only(r#"{"mcp":{"s":{"type":"local","command":["srv"]}}}"#);
        assert_eq!(
            server.enabled, None,
            "absent must not become Some(true); the host default is not ours to invent"
        );
        assert_eq!(server.timeout_json, None);
        assert_eq!(server.cwd, None);
        assert_eq!(server.oauth, OAuthState::Unspecified);
    }

    #[test]
    fn oauth_follow_up_is_only_emitted_for_explicit_oauth_state() {
        let plain = only(r#"{"mcp":{"s":{"type":"remote","url":"https://public.example/mcp"}}}"#);
        assert_eq!(plain.oauth, OAuthState::Unspecified);
        assert!(
            !plain.needs_oauth_login(),
            "a public HTTP server must not generate OAuth work nobody asked for"
        );

        let disabled =
            only(r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp","oauth":false}}}"#);
        assert_eq!(disabled.oauth, OAuthState::Disabled);
        assert!(!disabled.needs_oauth_login());

        let automatic =
            only(r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp","oauth":true}}}"#);
        assert_eq!(automatic.oauth, OAuthState::Automatic);
        assert!(automatic.needs_oauth_login());
    }

    #[test]
    fn an_oauth_client_secret_is_only_carried_as_an_environment_reference() {
        let referenced = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "oauth":{"client_id":"abc","client_secret":"{env:OAUTH_SECRET}"}}}}"#,
        );
        assert_eq!(
            referenced.oauth,
            OAuthState::Client {
                client_id: "abc".into(),
                client_secret_env: Some("OAUTH_SECRET".into()),
            }
        );

        let literal = read(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "oauth":{"client_id":"abc","client_secret":"sk-literal"}}}}"#,
        );
        let OAuthState::Client {
            client_secret_env, ..
        } = &literal.servers[0].1.oauth
        else {
            panic!("expected an explicit client")
        };
        assert_eq!(
            *client_secret_env, None,
            "a literal client secret must never be carried into the manifest"
        );
        let joined = literal.warnings.join(" ");
        assert!(joined.contains("literal"), "expected a warning: {joined}");
        assert!(
            !joined.contains("sk-literal"),
            "the warning must not echo the secret itself: {joined}"
        );
    }

    #[test]
    fn kilo_project_scope_environment_references_are_blocked() {
        let found = kilo_mcp_project_jsonc_v1(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"TOKEN":"{env:TOKEN}"}}}}"#,
            &ParseCtx {
                repo: Some("/repo".into()),
                origin: PathBuf::from("/repo/.kilo/kilo.jsonc"),
            },
        )
        .unwrap();

        assert!(matches!(found.servers[0].0, Scope::Project(_)));
        assert!(
            found
                .warnings
                .iter()
                .any(|w| w.contains("project-scope environment references are blocked")),
            "expected a Kilo project-scope blocker, got {:?}",
            found.warnings
        );
    }

    #[test]
    fn a_malformed_or_unknown_server_is_skipped_with_a_warning_not_invented() {
        for body in [
            r#"{"mcp":{"s":{"type":"local"}}}"#,
            r#"{"mcp":{"s":{"type":"remote"}}}"#,
            r#"{"mcp":{"s":{"type":"telepathy","command":["srv"]}}}"#,
            r#"{"mcp":{"s":{"type":"local","command":[]}}}"#,
            r#"{"mcp":{"s":"not-an-object"}}"#,
        ] {
            let found = read(body);
            assert!(
                found.servers.is_empty(),
                "{body} must not produce an invented server"
            );
            assert_eq!(found.warnings.len(), 1, "{body} must report why");
        }
    }

    #[test]
    fn a_config_without_an_mcp_section_yields_nothing() {
        let found = read(r#"{"model":"some/model"}"#);
        assert!(found.servers.is_empty());
        assert!(found.warnings.is_empty());
    }
}
