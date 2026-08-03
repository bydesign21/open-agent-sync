//! End-to-end test of the write path against a fake host CLI.
//!
//! A real `claude`/`codex` invocation would mutate the machine running the
//! tests, so this stands up a throwaway host: a descriptor in a temporary
//! `AGENTSYNC_HOME`, and a shell script on `PATH` that records the argv it was
//! called with. That makes it possible to assert that the commands we *say* we
//! will run are the commands that actually get run.
//!
//! Everything lives in one test function because `PATH` and `AGENTSYNC_HOME` are
//! process-global; splitting it would race.
//!
//! Unix only: the fake CLIs are scripts with shebangs, and the executable bit has
//! no Windows equivalent. The write path itself is platform-neutral — the one
//! platform-specific piece, symlink creation, lives in `platform.rs` and is
//! covered by the unit tests.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentsync::core::apply::{self, Outcome};
use agentsync::core::diff::{ActionKind, Domain};
use agentsync::core::model::HostSnapshot;
use agentsync::core::model::{Scope, ScopeKind, StdioServer, Transport};
use agentsync::core::plan::{FsOp, Plan, Step};
use agentsync::domains::World;
use agentsync::hosts::opencode_family::layers::{Env, Family};
use agentsync::hosts::opencode_family::plugins as ocp;
use agentsync::hosts::{Host, descriptor};
use agentsync::manifest::{Manifest, McpEntry};
use agentsync::manifest::{PluginEntry, PluginTarget};
use agentsync::transaction::{
    ConfigEditOperation, ConfigOrigin, ConfigScope, ConfigTransaction, FilePrecondition,
    FileTransaction, GuardedSource, SourceEdit, compute_sha256,
};
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

/// A stand-in host CLI. It logs its argv and maintains a real config file, so a
/// second pass sees the world the first pass produced. `__LOG__` and `__CFG__`
/// are substituted by plain replacement — Rust's `format!` cannot be used here
/// because the body is full of Python braces.
const FAKE_HOST: &str = r#"#!/usr/bin/env python3
import re, sys

LOG = "__LOG__"
CFG = "__CFG__"

argv = sys.argv[1:]
with open(LOG, "a") as fh:
    fh.write(" ".join(argv) + "\n")


def load():
    try:
        with open(CFG) as fh:
            return fh.read()
    except FileNotFoundError:
        return ""


def without(name, text):
    # Drop a [mcp_servers.NAME] table, up to the next top-level table.
    return re.sub(r"\[mcp_servers\." + re.escape(name) + r"\][^\[]*", "", text)


def without_plugin(name, text):
    # Plugin keys are fully qualified as name@marketplace. The removal command
    # may carry either that id or the bare name, so match the optional suffix.
    pattern = r'\[plugins\."' + re.escape(name) + r'(?:@[^\"]+)?"\][^\[]*'
    return re.sub(pattern, "", text)


if argv[:2] == ["mcp", "add"]:
    name = argv[2]
    rest = argv[3:]
    lines = ["[mcp_servers." + name + "]"]
    if "--url" in rest:
        lines.append('url = "' + rest[rest.index("--url") + 1] + '"')
    if "--bearer-token-env-var" in rest:
        value = rest[rest.index("--bearer-token-env-var") + 1]
        lines.append('bearer_token_env_var = "' + value + '"')
    if "--" in rest:
        cmd = rest[rest.index("--") + 1:]
        lines.append('command = "' + cmd[0] + '"')
        if cmd[1:]:
            lines.append("args = [" + ", ".join('"' + a + '"' for a in cmd[1:]) + "]")
    # Compute the new text BEFORE opening for write: open(..., "w") truncates
    # immediately, so reading inside the with-statement would read nothing.
    text = without(name, load()) + "\n".join(lines) + "\n\n"
    with open(CFG, "w") as fh:
        fh.write(text)
elif argv[:2] == ["mcp", "remove"]:
    text = without(argv[2], load())
    with open(CFG, "w") as fh:
        fh.write(text)
elif argv[:3] == ["plugin", "marketplace", "add"]:
    text = load() + '\n[marketplaces.agentsync-shims]\nsource = "' + argv[3] + '"\nsource_type = "local"\n'
    with open(CFG, "w") as fh:
        fh.write(text)
elif argv[:2] == ["plugin", "add"]:
    text = without_plugin(argv[2], load())
    text += '\n[plugins."' + argv[2] + '"]\nenabled = true\n'
    with open(CFG, "w") as fh:
        fh.write(text)
elif argv[:2] == ["plugin", "remove"]:
    text = without_plugin(argv[2], load())
    with open(CFG, "w") as fh:
        fh.write(text)
elif argv[:1] == ["touch"]:
    # Proof-of-execution for the guard test below: if this process ever
    # runs, the path named in argv[1] exists afterward.
    with open(argv[1], "w") as fh:
        fh.write("")

sys.exit(0)
"#;

/// A stand-in `claude` CLI. Claude's `mcp` write path is `add-json`/`remove`
/// against a single JSON document (`~/.claude.json`), a different shape from
/// [`FAKE_HOST`]'s TOML flags style, so it gets its own fake. Like
/// `FAKE_HOST`, it both logs its argv and maintains the real config file, so a
/// second read genuinely sees what the first write produced.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, sys

LOG = "__LOG__"
CFG = "__CFG__"

argv = sys.argv[1:]
with open(LOG, "a") as fh:
    fh.write(" ".join(argv) + "\n")


def load():
    try:
        with open(CFG) as fh:
            return json.load(fh)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save(data):
    with open(CFG, "w") as fh:
        json.dump(data, fh)


def scope_of(rest):
    return rest[rest.index("--scope") + 1] if "--scope" in rest else "local"


if argv[:2] == ["mcp", "add-json"]:
    name = argv[2]
    blob = json.loads(argv[3])
    scope = scope_of(argv[4:])
    data = load()
    if scope == "user":
        data.setdefault("mcpServers", {})[name] = blob
    else:
        cwd = os.getcwd()
        data.setdefault("projects", {}).setdefault(cwd, {}).setdefault("mcpServers", {})[name] = blob
    save(data)
elif argv[:2] == ["mcp", "remove"]:
    name = argv[2]
    scope = scope_of(argv[3:])
    data = load()
    if scope == "user":
        data.get("mcpServers", {}).pop(name, None)
    else:
        cwd = os.getcwd()
        data.get("projects", {}).get(cwd, {}).get("mcpServers", {}).pop(name, None)
    save(data)

sys.exit(0)
"#;

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

fn apply_transaction_step(step: Step) -> apply::Report {
    let tmp = tempfile::tempdir().unwrap();
    let mut plan = Plan::default();
    plan.push("guarded transaction", step);
    apply::run(
        &plan,
        &mut Manifest::default(),
        &tmp.path().join("manifest.toml"),
        &[],
        |_| {},
    )
}

#[test]
fn config_patch_preserves_unrelated_jsonc_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.jsonc");
    let original = "{\n  // this comment and all spacing are user-owned\n  \"mcp\": { \"enabled\": true },\n  \"plugin\": [\"pkg\", {\"option\": null}]\n}\n";
    std::fs::write(&path, original).unwrap();
    let hash = compute_sha256(original.as_bytes());
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 0, &hash);
    let transaction = ConfigTransaction::new(serde_json::json!({
        "mcp": {"enabled": false},
        "plugin": ["pkg", {"option": null}]
    }))
    .with_source(GuardedSource::with_hash(&path, hash))
    .with_edit(SourceEdit {
        origin,
        config_path: vec!["mcp".into(), "enabled".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(false),
            raw_json: Some("false".into()),
        },
    });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Done);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original.replacen("true", "false", 1),
        "only the owned value token may change"
    );
}

#[test]
fn config_patch_rejects_a_plan_apply_race_without_overwriting_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.jsonc");
    let planned = b"{\"enabled\":true}\n";
    std::fs::write(&path, planned).unwrap();
    let hash = compute_sha256(planned);
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 0, &hash);
    let transaction = ConfigTransaction::new(serde_json::json!({"enabled": false}))
        .with_source(GuardedSource::with_hash(&path, hash))
        .with_edit(SourceEdit {
            origin,
            config_path: vec!["enabled".into()],
            operation: ConfigEditOperation::Set {
                value: serde_json::json!(false),
                raw_json: None,
            },
        });
    let raced = b"{\"enabled\":true,\"added_by_user\":true}\n";
    std::fs::write(&path, raced).unwrap();

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert_eq!(std::fs::read(&path).unwrap(), raced);
}

#[test]
fn config_patch_rolls_back_when_effective_projection_is_wrong() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.jsonc");
    let original = b"{\"enabled\":true}\n";
    std::fs::write(&path, original).unwrap();
    let hash = compute_sha256(original);
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 0, &hash);
    let transaction = ConfigTransaction::new(serde_json::json!({"enabled": "not-the-result"}))
        .with_source(GuardedSource::with_hash(&path, hash))
        .with_edit(SourceEdit {
            origin,
            config_path: vec!["enabled".into()],
            operation: ConfigEditOperation::Set {
                value: serde_json::json!(false),
                raw_json: None,
            },
        });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn config_patch_removal_reveals_the_lower_precedence_value() {
    let tmp = tempfile::tempdir().unwrap();
    let lower = tmp.path().join("global.jsonc");
    let higher = tmp.path().join("project.jsonc");
    let lower_bytes = b"{\"mcp\":{\"url\":\"https://global.test\",\"keep\":1}}\n";
    let higher_bytes = b"{\"mcp\":{\"url\":\"https://project.test\"}}\n";
    std::fs::write(&lower, lower_bytes).unwrap();
    std::fs::write(&higher, higher_bytes).unwrap();
    let higher_hash = compute_sha256(higher_bytes);
    let lower_hash = compute_sha256(lower_bytes);
    let transaction = ConfigTransaction::new(serde_json::json!({
        "mcp": {"url": "https://global.test", "keep": 1}
    }))
    .with_source(GuardedSource::with_hash(&lower, &lower_hash))
    .with_source(GuardedSource::with_hash(&higher, &higher_hash))
    .with_origin(ConfigOrigin::new(
        &lower,
        ConfigScope::Global,
        20,
        lower_hash,
    ))
    .with_edit(SourceEdit {
        origin: ConfigOrigin::new(&higher, ConfigScope::Project, 1, higher_hash),
        config_path: vec!["mcp".into(), "url".into()],
        operation: ConfigEditOperation::Remove,
    });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Done);
    assert_eq!(std::fs::read(&lower).unwrap(), lower_bytes);
    assert_eq!(std::fs::read_to_string(&higher).unwrap(), "{\"mcp\":{}}\n");
}

#[test]
fn config_patch_rejects_missing_origin_precedence_for_a_layered_projection() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global.jsonc");
    let project = tmp.path().join("project.jsonc");
    let global_bytes = b"{\"value\":\"global\"}\n";
    let project_bytes = b"{\"value\":\"project\"}\n";
    std::fs::write(&global, global_bytes).unwrap();
    std::fs::write(&project, project_bytes).unwrap();
    let global_hash = compute_sha256(global_bytes);
    let project_hash = compute_sha256(project_bytes);
    let transaction = ConfigTransaction::new(serde_json::json!({"value": "project"}))
        .with_source(GuardedSource::with_hash(&global, &global_hash))
        .with_source(GuardedSource::with_hash(&project, &project_hash))
        // Project is known, but the global layer's precedence is absent.
        .with_origin(ConfigOrigin::new(
            &project,
            ConfigScope::Project,
            10,
            project_hash,
        ));

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert!(
        report.results[0]
            .message
            .contains("missing origin precedence"),
        "missing precedence must be reported instead of guessed: {}",
        report.results[0].message
    );
    assert_eq!(std::fs::read(&global).unwrap(), global_bytes);
    assert_eq!(std::fs::read(&project).unwrap(), project_bytes);
}

#[test]
fn config_patch_blocks_an_externally_controlled_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("managed.jsonc");
    let original = b"{\"enabled\":true}\n";
    std::fs::write(&path, original).unwrap();
    let hash = compute_sha256(original);
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 0, &hash)
        .externally_controlled("managed policy");
    let transaction = ConfigTransaction::new(serde_json::json!({"enabled": false}))
        .with_source(GuardedSource::with_hash(&path, hash))
        .with_edit(SourceEdit {
            origin,
            config_path: vec!["enabled".into()],
            operation: ConfigEditOperation::Set {
                value: serde_json::json!(false),
                raw_json: None,
            },
        });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert!(report.results[0].message.contains("managed policy"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn config_patch_allows_a_writable_higher_origin_over_a_shadowed_read_only_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let lower = tmp.path().join("managed.jsonc");
    let higher = tmp.path().join("project.jsonc");
    let lower_bytes = b"{\"value\":\"managed\"}\n";
    let higher_bytes = b"{\"value\":\"project\",\"enabled\":true}\n";
    std::fs::write(&lower, lower_bytes).unwrap();
    std::fs::write(&higher, higher_bytes).unwrap();
    let lower_hash = compute_sha256(lower_bytes);
    let higher_hash = compute_sha256(higher_bytes);
    let transaction = ConfigTransaction::new(serde_json::json!({
        "value": "project",
        "enabled": false
    }))
    .with_source(GuardedSource::with_hash(&lower, &lower_hash))
    .with_source(GuardedSource::with_hash(&higher, &higher_hash))
    .with_origin(
        ConfigOrigin::new(&lower, ConfigScope::Global, 20, &lower_hash)
            .externally_controlled("managed policy"),
    )
    .with_edit(SourceEdit {
        origin: ConfigOrigin::new(&higher, ConfigScope::Project, 10, &higher_hash),
        config_path: vec!["enabled".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(false),
            raw_json: None,
        },
    });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(
        report.results[0].outcome,
        Outcome::Done,
        "only the edited controlling origin must be writable: {}",
        report.results[0].message
    );
    assert_eq!(std::fs::read(&lower).unwrap(), lower_bytes);
    assert_eq!(
        std::fs::read_to_string(&higher).unwrap(),
        "{\"value\":\"project\",\"enabled\":false}\n"
    );
}

#[cfg(unix)]
#[test]
fn config_patch_rollback_never_rewrites_a_shadowed_read_only_projection_source() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let tmp = tempfile::tempdir().unwrap();
    let lower = tmp.path().join("managed.jsonc");
    let higher = tmp.path().join("project.jsonc");
    let lower_bytes = b"{\"value\":\"managed\"}\n";
    let higher_bytes = b"{\"value\":\"project\",\"enabled\":true}\n";
    std::fs::write(&lower, lower_bytes).unwrap();
    std::fs::write(&higher, higher_bytes).unwrap();
    std::fs::set_permissions(&lower, std::fs::Permissions::from_mode(0o444)).unwrap();
    let lower_before = std::fs::metadata(&lower).unwrap();
    let lower_hash = compute_sha256(lower_bytes);
    let higher_hash = compute_sha256(higher_bytes);
    let transaction = ConfigTransaction::new(serde_json::json!({
        "value": "deliberately wrong projection",
        "enabled": false
    }))
    .with_source(GuardedSource::with_hash(&lower, &lower_hash))
    .with_source(GuardedSource::with_hash(&higher, &higher_hash))
    .with_origin(
        ConfigOrigin::new(&lower, ConfigScope::Global, 20, &lower_hash)
            .externally_controlled("managed policy"),
    )
    .with_edit(SourceEdit {
        origin: ConfigOrigin::new(&higher, ConfigScope::Project, 10, &higher_hash),
        config_path: vec!["enabled".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(false),
            raw_json: None,
        },
    });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert_eq!(std::fs::read(&higher).unwrap(), higher_bytes);
    let lower_after = std::fs::metadata(&lower).unwrap();
    assert_eq!(std::fs::read(&lower).unwrap(), lower_bytes);
    assert_eq!(
        lower_after.ino(),
        lower_before.ino(),
        "projection-only sources must not be replaced during apply or rollback"
    );
    assert_eq!(
        lower_after.permissions().mode() & 0o777,
        lower_before.permissions().mode() & 0o777,
        "projection-only source permissions must remain unchanged"
    );
}

#[test]
fn config_patch_composes_mcp_and_plugin_edits_across_origin_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global.jsonc");
    let project = tmp.path().join("project.jsonc");
    let global_bytes =
        b"{\"mcp\":{\"demo\":{\"url\":\"https://global.test\"}},\"plugin\":[\"global-plugin\"]}\n";
    let project_bytes = b"{\"mcp\":{\"demo\":{\"url\":\"https://project.test\"}},\"plugin\":[\"project-plugin\"]}\n";
    std::fs::write(&global, global_bytes).unwrap();
    std::fs::write(&project, project_bytes).unwrap();
    let global_hash = compute_sha256(global_bytes);
    let project_hash = compute_sha256(project_bytes);
    let global_origin = ConfigOrigin::new(&global, ConfigScope::Global, 20, &global_hash);
    let project_origin = ConfigOrigin::new(&project, ConfigScope::Project, 10, &project_hash);

    // Deliberately list the higher-precedence project source first. Resolution
    // must use origin precedence, not incidental source insertion order.
    let transaction = ConfigTransaction::new(serde_json::json!({
        "mcp": {"demo": {"url": "https://project.test", "timeout": 5000}},
        "plugin": ["project-plugin", {"enabled": null}]
    }))
    .with_source(GuardedSource::with_hash(&project, &project_hash))
    .with_source(GuardedSource::with_hash(&global, &global_hash))
    .with_edit(SourceEdit {
        origin: global_origin,
        config_path: vec!["mcp".into(), "demo".into(), "timeout".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(5000),
            raw_json: Some("5000".into()),
        },
    })
    .with_edit(SourceEdit {
        origin: project_origin,
        config_path: vec!["plugin".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(["project-plugin", {"enabled": null}]),
            raw_json: Some(r#"["project-plugin", {"enabled": null}]"#.into()),
        },
    });

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(
        report.results[0].outcome,
        Outcome::Done,
        "split-origin resolution must honor explicit precedence: {}",
        report.results[0].message
    );
    let global_value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&global).unwrap()).unwrap();
    assert_eq!(global_value["mcp"]["demo"]["timeout"], 5000);
    assert!(
        std::fs::read_to_string(&project)
            .unwrap()
            .contains(r#"["project-plugin", {"enabled": null}]"#)
    );
}

#[test]
fn config_patch_resolves_environment_placeholders_from_resolver_context() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.jsonc");
    let original = b"{\"endpoint\":\"{env:DEMO_ENDPOINT}\",\"enabled\":true}\n";
    std::fs::write(&path, original).unwrap();
    let hash = compute_sha256(original);
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 10, &hash);
    let mut transaction = ConfigTransaction::new(serde_json::json!({
        "endpoint": "https://resolved.test",
        "enabled": false
    }))
    .with_source(GuardedSource::with_hash(&path, &hash))
    .with_edit(SourceEdit {
        origin,
        config_path: vec!["enabled".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(false),
            raw_json: None,
        },
    });
    transaction
        .resolver_context
        .env_vars
        .insert("DEMO_ENDPOINT".into(), "https://resolved.test".into());

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(
        report.results[0].outcome,
        Outcome::Done,
        "effective projection must use the supplied resolver context: {}",
        report.results[0].message
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"endpoint\":\"{env:DEMO_ENDPOINT}\",\"enabled\":false}\n",
        "resolver expansion verifies the projection without rewriting user placeholders"
    );
}

#[test]
fn config_patch_resolves_file_placeholders_from_resolver_search_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.jsonc");
    let include_dir = tmp.path().join("includes");
    std::fs::create_dir_all(&include_dir).unwrap();
    std::fs::write(
        include_dir.join("instructions.md"),
        "Keep this exact text.\n",
    )
    .unwrap();
    let original = b"{\"instructions\":\"{file:instructions.md}\",\"enabled\":true}\n";
    std::fs::write(&path, original).unwrap();
    let hash = compute_sha256(original);
    let origin = ConfigOrigin::new(&path, ConfigScope::Global, 10, &hash);
    let mut transaction = ConfigTransaction::new(serde_json::json!({
        "instructions": "Keep this exact text.\n",
        "enabled": false
    }))
    .with_source(GuardedSource::with_hash(&path, &hash))
    .with_edit(SourceEdit {
        origin,
        config_path: vec!["enabled".into()],
        operation: ConfigEditOperation::Set {
            value: serde_json::json!(false),
            raw_json: None,
        },
    });
    transaction.resolver_context.search_paths.push(include_dir);

    let report = apply_transaction_step(Step::ConfigTransaction(transaction));

    assert_eq!(
        report.results[0].outcome,
        Outcome::Done,
        "effective projection must resolve files through search paths: {}",
        report.results[0].message
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"instructions\":\"{file:instructions.md}\",\"enabled\":false}\n"
    );
}

#[test]
fn file_transaction_rejects_unowned_destinations() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("user-owned.ts");
    let transaction = FileTransaction::new().write(&path, b"generated", FilePrecondition::Absent);

    let report = apply_transaction_step(Step::FileTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert!(!path.exists());
}

#[test]
fn file_transaction_rejects_a_plan_apply_race_without_overwriting_it() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(".agentsync-owned"), b"").unwrap();
    let path = tmp.path().join("bridge.ts");
    std::fs::write(&path, b"planned bytes").unwrap();
    let transaction = FileTransaction::new().write(
        &path,
        b"new generated bytes",
        FilePrecondition::Sha256(compute_sha256(b"planned bytes")),
    );
    std::fs::write(&path, b"user changed this").unwrap();

    let report = apply_transaction_step(Step::FileTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Failed);
    assert_eq!(std::fs::read(&path).unwrap(), b"user changed this");
}

#[test]
fn file_transaction_creates_an_absent_owned_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(".agentsync-owned"), b"").unwrap();
    let path = tmp.path().join("bridge.ts");
    let transaction =
        FileTransaction::new().write(&path, b"export default {};\n", FilePrecondition::Absent);

    let report = apply_transaction_step(Step::FileTransaction(transaction));

    assert_eq!(report.results[0].outcome, Outcome::Done);
    assert_eq!(std::fs::read(&path).unwrap(), b"export default {};\n");
}

#[test]
fn shim_reconciliation_converges_after_two_full_passes() {
    let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let _env_restore = EnvRestore::capture(&["AGENTSYNC_HOME", "PATH"]);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let home = root.join("agentsync-home");
    let bindir = root.join("bin");
    let hostcfg = root.join("fake-config.toml");
    let hostskills = root.join("fake-skills");
    let shim_marketplace = home.join("shims/fakehost");
    let plugin_catalog = home.join("catalog/marketplace.json");
    let source_hook =
        home.join("plugin-cache/claude-plugins-official/security-guidance/1.0.0/hooks/hooks.json");
    let log = root.join("calls.log");
    std::fs::create_dir_all(home.join("hosts")).unwrap();
    std::fs::create_dir_all(&bindir).unwrap();
    std::fs::create_dir_all(&hostskills).unwrap();
    std::fs::create_dir_all(plugin_catalog.parent().unwrap()).unwrap();
    std::fs::create_dir_all(source_hook.parent().unwrap()).unwrap();
    std::fs::write(
        &plugin_catalog,
        r#"{"name":"claude-plugins-official","plugins":[{"name":"security-guidance"}]}"#,
    )
    .unwrap();
    std::fs::write(
        &source_hook,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo review","if":"Bash(git commit:*)"}]}]}}"#,
    )
    .unwrap();

    // A host CLI that records how it was called *and* actually maintains its
    // config file. Recording alone would not prove convergence: a second run has
    // to see the world the first run produced.
    // Resolve the interpreter before PATH is narrowed: the fake host's shebang
    // must not depend on a PATH we are about to replace.
    let python = which::which("python3").expect("python3 is needed to run this test");
    write_exec(
        &bindir.join("fakehost"),
        &FAKE_HOST
            .replace("#!/usr/bin/env python3", &format!("#!{}", python.display()))
            .replace("__LOG__", &log.display().to_string())
            .replace("__CFG__", &hostcfg.display().to_string()),
    );

    // A second host that always fails, to prove a failure does not abort the run.
    write_exec(
        &bindir.join("brokenhost"),
        "#!/bin/sh\necho 'boom: no such marketplace' >&2\nexit 3\n",
    );

    // The fake host starts out holding one server we want to adopt and one we
    // want gone.
    std::fs::write(
        &hostcfg,
        "[mcp_servers.obsolete]\ncommand = \"old-thing\"\n\n\
         [mcp_servers.adoptme]\ncommand = \"keeper\"\nargs = [\"--serve\"]\n\n\
         [plugins.\"security-guidance@claude-plugins-official\"]\nenabled = true\n",
    )
    .unwrap();

    let descriptor = |name: &str, bin: &str| {
        format!(
            r#"
name = "{name}"
display = "Fake {name}"
detect = {{ bin = "{bin}" }}

[mcp]
scopes = ["user"]
caps = ["stdio", "http", "env", "bearer_env"]

[[mcp.read]]
file = "{cfg}"
parser = "codex_toml_v1"

[mcp.add]
style = "flags"
argv_stdio = ["mcp", "add", "{{name}}", "{{env_flags...}}", "--", "{{command}}", "{{args...}}"]
argv_http = ["mcp", "add", "{{name}}", "--url", "{{url}}", "{{bearer_flags...}}"]
env_flag = "--env"
env_format = "{{key}}={{value}}"
bearer_env_flag = "--bearer-token-env-var"

[mcp.remove]
argv = ["mcp", "remove", "{{name}}"]

[skills]
dirs = ["{skills}"]
"#,
            cfg = hostcfg.display(),
            skills = hostskills.display(),
        )
    };
    let fakehost_descriptor = format!(
        r#"{}

[plugins]

[[plugins.read]]
file = "{}"
parser = "codex_plugins_toml_v1"

[[plugins.catalog]]
glob = "{}"
parser = "marketplace_manifest_v1"

[plugins.install]
argv = ["plugin", "add", "{{id}}"]

[plugins.remove]
argv = ["plugin", "remove", "{{id}}"]

[plugins.marketplace_add]
argv = ["plugin", "marketplace", "add", "{{source}}"]

[plugins.marketplace_remove]
argv = ["plugin", "marketplace", "remove", "{{name}}"]

[hooks]
events = ["PreToolUse"]
caps = ["matcher"]
output = ["system_message"]

[hooks.shim]
marketplace = "{}"
"#,
        descriptor("fakehost", "fakehost"),
        hostcfg.display(),
        plugin_catalog.display(),
        shim_marketplace.display(),
    );
    std::fs::write(home.join("hosts/fakehost.toml"), fakehost_descriptor).unwrap();

    let brokenhost_descriptor = format!(
        r#"{}

[hooks]
events = ["PreToolUse"]
caps = ["matcher", "if"]
output = ["system_message"]

[[hooks.read]]
file = "{}"
parser = "claude_hooks_json_v1"
"#,
        descriptor("brokenhost", "brokenhost"),
        source_hook.display(),
    );
    std::fs::write(home.join("hosts/brokenhost.toml"), brokenhost_descriptor).unwrap();

    // Canonical skill content that should get linked into the host.
    let canonical = home.join("skills/my-skill");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::write(
        canonical.join("SKILL.md"),
        "---\nname: my-skill\ndescription: test\n---\n",
    )
    .unwrap();

    let manifest_path = home.join("manifest.toml");
    std::fs::write(
        &manifest_path,
        r#"
[mcp.wanted]
transport = "http"
url = "https://example.test/mcp"
bearer_token_env = "WANTED_TOKEN"
hosts = ["fakehost"]

[skills.my-skill]
source = "skills/my-skill"
hosts = ["fakehost"]

[plugins.security-guidance]
hosts = ["fakehost"]
"#,
    )
    .unwrap();

    // Point the tool at the throwaway home, and hide the real CLIs by replacing
    // PATH with the directory holding the fakes.
    unsafe {
        std::env::set_var("AGENTSYNC_HOME", &home);
        std::env::set_var("PATH", &bindir);
    }

    let world = World::load(&manifest_path, &[]).expect("world loads");

    // Both fakes are detected; the real hosts are not on PATH, so they are not.
    let detected: Vec<String> = world
        .detected()
        .map(|(h, _)| h.name().to_string())
        .collect();
    assert!(detected.contains(&"fakehost".to_string()), "{detected:?}");
    assert!(
        !detected.contains(&"claude".to_string()),
        "the real claude must not be reachable from this test: {detected:?}"
    );

    let mut rows = world.rows();

    // `wanted` is in the manifest but not on the host.
    let wanted = rows
        .iter()
        .find(|r| r.name == "wanted" && r.domain == Domain::Mcp)
        .expect("a row for wanted");
    assert_eq!(wanted.headline, "missing from fakehost");

    // `obsolete` is on both hosts (they share a config file) but in neither the
    // manifest — so the thing that is missing is the manifest entry.
    let obsolete = rows
        .iter()
        .find(|r| r.name == "obsolete" && r.domain == Domain::Mcp)
        .expect("a row for obsolete");
    assert_eq!(obsolete.headline, "not in the manifest yet");

    // `my-skill` is in the manifest but not linked.
    let skill = rows
        .iter()
        .find(|r| r.name == "my-skill" && r.domain == Domain::Skills)
        .expect("a row for my-skill");
    assert_eq!(skill.headline, "missing from fakehost");

    // Accept: push `wanted`, adopt `adoptme`, delete `obsolete` everywhere,
    // link `my-skill`, and replace the original security plugin with its shim.
    for row in rows.iter_mut() {
        match row.name.as_str() {
            "wanted" | "my-skill" | "adoptme" => row.accepted = true,
            "obsolete" => {
                row.chosen = row
                    .actions
                    .iter()
                    .position(|a| matches!(a.kind, ActionKind::Delete { .. }))
                    .expect("a delete action");
                row.accepted = true;
            }
            _ if row.domain == Domain::Hooks && row.actionable() => row.accepted = true,
            _ => {}
        }
    }

    let plan = world.plan(&rows);
    let mut manifest = world.manifest.clone();
    let report = apply::run(&plan, &mut manifest, &manifest_path, &world.hosts, |_| {});

    // ---- the commands that actually ran ----
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains(
            "mcp add wanted --url https://example.test/mcp --bearer-token-env-var WANTED_TOKEN"
        ),
        "the http add, with the bearer flag, must have reached the CLI. log:\n{calls}"
    );
    assert!(
        calls.contains("mcp remove obsolete"),
        "the removal must have reached the CLI. log:\n{calls}"
    );
    let shim =
        agentsync::shim::generate::shim_plugin_name("claude-plugins-official", "security-guidance");
    assert!(
        calls.contains(&format!("plugin add {shim}@agentsync-shims")),
        "the shim install must have reached the CLI. log:\n{calls}"
    );
    assert!(
        calls.contains("plugin remove security-guidance@claude-plugins-official"),
        "the original removal must have reached the CLI. log:\n{calls}"
    );

    // ---- the filesystem side ----
    let link = hostskills.join("my-skill");
    let meta = link.symlink_metadata().expect("the skill link exists");
    assert!(meta.file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), canonical);
    assert!(
        link.join("SKILL.md").exists(),
        "the link must resolve to real content"
    );

    // ---- the manifest side ----
    assert!(report.manifest_written, "{:?}", report.manifest_error);
    let reloaded = agentsync::manifest::Manifest::load(&manifest_path).unwrap();
    assert!(
        reloaded.mcp.contains_key("adoptme"),
        "the adopted server must be in the rewritten manifest: {:?}",
        reloaded.mcp.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        reloaded.mcp["adoptme"].command.as_deref(),
        Some("keeper"),
        "the adopted definition must match what the host held"
    );
    assert_eq!(reloaded.mcp["adoptme"].args, vec!["--serve"]);
    assert!(
        reloaded.mcp.contains_key("wanted"),
        "the pre-existing entry must survive the rewrite"
    );
    assert!(
        reloaded.skills.contains_key("my-skill"),
        "the skills section must survive the rewrite"
    );

    // ---- a failing step is reported, not fatal ----
    assert!(
        report.count(Outcome::Done) > 0,
        "some steps must have succeeded: {:?}",
        report.results
    );
    assert!(
        report.count(Outcome::Failed) > 0,
        "brokenhost always exits 3, so at least one step must be recorded as failed"
    );
    assert!(
        report
            .results
            .iter()
            .any(|r| r.outcome == Outcome::Failed && r.message.contains("boom")),
        "the failure must carry the host's own stderr: {:?}",
        report.results
    );

    // A second pass must converge: the things we just fixed stop being reported.
    let world2 = World::load(&manifest_path, &[]).unwrap();
    let mut rows2 = world2.rows();
    let still_open: Vec<String> = rows2
        .iter()
        .filter(|r| {
            r.severity != agentsync::core::diff::Severity::Synced
                && matches!(
                    r.name.as_str(),
                    "wanted" | "my-skill" | "obsolete" | "adoptme"
                )
        })
        .map(|r| format!("{} ({})", r.name, r.headline))
        .collect();
    assert!(
        still_open.is_empty(),
        "a second run must not re-report work already done: {still_open:?}"
    );

    for row in rows2.iter_mut() {
        if row.actionable() {
            row.accepted = true;
        }
    }
    let plan2 = world2.plan(&rows2);
    let plugin_steps: Vec<_> = plan2
        .steps
        .iter()
        .filter(|step| matches!(&step.step,
            agentsync::core::plan::Step::Host { argv, .. }
                if argv.iter().any(|arg| arg.contains("security-guidance") || arg == &shim)
                    && argv.iter().any(|arg| {
                        arg == "add" || arg == "install" || arg == "remove" || arg == "uninstall"
                    })
        ))
        .map(|step| step.label.as_str())
        .collect();
    assert!(
        plugin_steps.is_empty(),
        "the second complete plan must not install or remove either copy: {plugin_steps:?}"
    );

    let mut manifest2 = world2.manifest.clone();
    let report2 = apply::run(
        &plan2,
        &mut manifest2,
        &manifest_path,
        &world2.hosts,
        |_| {},
    );
    assert_eq!(
        report2.count(Outcome::Failed),
        0,
        "the converged second plan must apply cleanly: {:?}",
        report2.results
    );

    // ---- a guarded step whose guard failed must never be spawned ----
    //
    // This reuses `brokenhost` (always exits 3) as the guarded install and
    // `fakehost` (the `touch` branch above) as the guarded removal, both
    // already wired to real processes on `PATH`. `Step::Manual` never spawns
    // anything, so proving the guard on a manual step only proves the
    // *label* says "Skipped" — it does not prove non-execution. A marker
    // file left behind by an actual process does.
    let marker = root.join("removal-ran");
    let mut guard_plan = agentsync::core::plan::Plan::default();
    guard_plan.push_guarded(
        "install the shim",
        agentsync::core::plan::Step::Host {
            host: "brokenhost".into(),
            argv: vec!["plugin".into(), "add".into(), "shim".into()],
            cwd: None,
        },
        1,
        "shim:test:example",
    );
    guard_plan.push_guarded(
        "remove the original",
        agentsync::core::plan::Step::Host {
            host: "fakehost".into(),
            argv: vec!["touch".into(), marker.display().to_string()],
            cwd: None,
        },
        2,
        "shim:test:example",
    );
    let mut guard_manifest = agentsync::manifest::Manifest::default();
    let guard_report = apply::run(
        &guard_plan,
        &mut guard_manifest,
        &root.join("unused-manifest.toml"),
        &world.hosts,
        |_| {},
    );
    assert_eq!(guard_report.results[0].outcome, Outcome::Failed);
    assert_eq!(guard_report.results[1].outcome, Outcome::Skipped);
    assert!(
        !marker.exists(),
        "the guarded removal must never have been spawned, but its marker file exists"
    );
}

// ---------------------------------------------------------------------------
// OpenCode-family MCP write path: two full applies must converge.
//
// Unlike the CLI-based hosts above, this needs no fake host process at all —
// the whole write path is a guarded JSONC edit against a real temp file, so
// the real `hosts::opencode_family` parsers and `transaction::ConfigTransaction`
// machinery run end to end. The descriptor points at an isolated temp path
// (never `{xdg_config}`), so this never touches the machine running the test.
// ---------------------------------------------------------------------------

fn opencode_family_descriptor(
    name: &str,
    parser_user: &str,
    parser_project: &str,
    user_file: &Path,
    project_file_template: &str,
) -> String {
    format!(
        r#"
name = "{name}"
display = "{name}"
detect = {{ bin = "{name}" }}

[mcp]
scopes = ["user", "project"]
caps = ["stdio", "http", "env", "env_from", "headers", "bearer_env"]

[[mcp.read]]
file = "{user_file}"
parser = "{parser_user}"

[[mcp.read]]
file = "{project_file_template}"
parser = "{parser_project}"

[mcp.jsonc]
user_file = "{user_file}"
project_file = "{project_file_template}"
"#,
        user_file = user_file.display(),
    )
}

/// Add `demo` to a host that writes `mcp` through guarded JSONC edits, apply
/// it, then prove a second full pass reports and does nothing further.
fn mcp_family_converges_after_two_passes(name: &str, parser_user: &str, parser_project: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let user_file = root.join(format!("{name}.jsonc"));
    let project_file_template = format!("{{repo}}/.{name}/{name}.jsonc");

    let descriptor_text = opencode_family_descriptor(
        name,
        parser_user,
        parser_project,
        &user_file,
        &project_file_template,
    );
    let make_host = || Host {
        descriptor: descriptor::parse(&descriptor_text, name).unwrap(),
        bin: Some(PathBuf::from(format!("/usr/bin/{name}"))),
    };

    let repos = vec![repo.display().to_string()];
    let manifest_path = root.join("manifest.toml");

    let mut manifest = Manifest::default();
    manifest.mcp.insert(
        "demo".into(),
        McpEntry {
            transport: "stdio".into(),
            command: Some("node".into()),
            args: vec!["/x/index.js".into()],
            env: BTreeMap::from([("LEVEL".to_string(), "info".to_string())]),
            env_from: vec![],
            url: None,
            headers: BTreeMap::new(),
            bearer_token_env: None,
            scope: ScopeKind::User,
            repos: vec![],
            hosts: None,
        },
    );

    // ---- pass 1: the host has nothing yet ----
    let host1 = make_host();
    let snap1 = host1
        .read(&repos)
        .expect("reading an absent config is not an error");
    assert!(snap1.mcp.is_empty(), "the host file does not exist yet");

    let world1 = World {
        manifest: manifest.clone(),
        manifest_path: manifest_path.clone(),
        hosts: vec![host1],
        snapshots: vec![snap1],
        repos: repos.clone(),
        warnings: Vec::new(),
    };
    let mut rows1 = world1.rows();
    let row = rows1
        .iter_mut()
        .find(|r| r.name == "demo" && r.domain == Domain::Mcp)
        .expect("a row for demo");
    assert_eq!(row.headline, format!("missing from {name}"));
    row.accepted = true;

    let plan1 = world1.plan(&rows1);
    let tx_steps = plan1
        .steps
        .iter()
        .filter(|s| matches!(s.step, Step::ConfigTransaction(_)))
        .count();
    assert_eq!(
        tx_steps,
        1,
        "adding demo must be exactly one guarded JSONC edit, not a host CLI call: {:?}",
        plan1.steps.iter().map(|s| &s.step).collect::<Vec<_>>()
    );
    assert!(
        !plan1
            .steps
            .iter()
            .any(|s| matches!(s.step, Step::Host { .. })),
        "there is no `{name} mcp remove`, so the write path must never build a host command: {:?}",
        plan1.steps.iter().map(|s| &s.step).collect::<Vec<_>>()
    );

    let mut manifest_after_1 = world1.manifest.clone();
    let report1 = apply::run(
        &plan1,
        &mut manifest_after_1,
        &manifest_path,
        &world1.hosts,
        |_| {},
    );
    assert_eq!(report1.count(Outcome::Failed), 0, "{:?}", report1.results);
    assert!(user_file.is_file(), "the write path must create the file");
    let contents = std::fs::read_to_string(&user_file).unwrap();
    assert!(contents.contains("\"demo\""), "{contents}");

    // ---- pass 2: reading the file back must round-trip what we wrote ----
    let host2 = make_host();
    let snap2 = host2.read(&repos).unwrap();
    let demo2 = snap2
        .mcp
        .get(&(Scope::User, "demo".to_string()))
        .expect("demo round-trips from disk");
    assert_eq!(
        demo2.transport,
        Transport::Stdio(StdioServer {
            command: "node".into(),
            args: vec!["/x/index.js".into()],
            env: BTreeMap::from([("LEVEL".to_string(), "info".to_string())]),
            env_from: vec![],
        })
    );

    let world2 = World {
        manifest: manifest_after_1.clone(),
        manifest_path: manifest_path.clone(),
        hosts: vec![host2],
        snapshots: vec![snap2],
        repos: repos.clone(),
        warnings: Vec::new(),
    };
    let mut rows2 = world2.rows();
    let demo_row2 = rows2
        .iter()
        .find(|r| r.name == "demo" && r.domain == Domain::Mcp)
        .expect("still a row for demo");
    assert_eq!(
        demo_row2.severity,
        agentsync::core::diff::Severity::Synced,
        "a second run must not re-report work already done: {} ({})",
        demo_row2.name,
        demo_row2.headline
    );

    for row in rows2.iter_mut() {
        if row.actionable() {
            row.accepted = true;
        }
    }
    let plan2 = world2.plan(&rows2);
    let mutations: Vec<&str> = plan2
        .steps
        .iter()
        .filter(|s| matches!(s.step, Step::ConfigTransaction(_) | Step::Host { .. }))
        .map(|s| s.label.as_str())
        .collect();
    assert!(
        mutations.is_empty(),
        "the second full plan must not touch mcp again: {mutations:?}"
    );

    let mut manifest_after_2 = world2.manifest.clone();
    let report2 = apply::run(
        &plan2,
        &mut manifest_after_2,
        &manifest_path,
        &world2.hosts,
        |_| {},
    );
    assert_eq!(
        report2.count(Outcome::Failed),
        0,
        "the converged second plan must apply cleanly: {:?}",
        report2.results
    );
}

#[test]
fn opencode_mcp_converges_after_two_passes() {
    mcp_family_converges_after_two_passes(
        "opencode",
        "opencode_mcp_jsonc_v1",
        "opencode_mcp_project_jsonc_v1",
    );
}

#[test]
fn kilo_mcp_converges_after_two_passes() {
    mcp_family_converges_after_two_passes("kilo", "kilo_mcp_jsonc_v1", "kilo_mcp_project_jsonc_v1");
}

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Captures the current value of each named environment variable and puts it
/// back when dropped (Rust runs destructors during an unwinding panic, so a
/// failed assertion mid-test still restores them). Holding the lock alone is
/// not enough: it only serializes *concurrent* mutation, it does not undo a
/// mutation once the lock is released. Without this, whichever of these tests
/// happens to run second inherits the first one's `PATH` (missing `python3`)
/// or `HOME` (a directory that no longer exists once its `TempDir` drops).
struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl EnvRestore {
    fn capture(names: &[&'static str]) -> Self {
        EnvRestore(names.iter().map(|&n| (n, std::env::var(n).ok())).collect())
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[test]
fn shared_agent_paths_converge() {
    let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let _env_restore = EnvRestore::capture(&["HOME", "XDG_CONFIG_HOME", "AGENTSYNC_HOME", "PATH"]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Everything `~` and `{xdg_config}` can expand to lives under `root`, so a
    // bug in this test (or in the code under test) cannot touch the real
    // machine's `~/.agents/skills` or `~/.config`.
    let fake_home = root.join("home");
    let fake_xdg = root.join("xdg-config");
    let agentsync_home = root.join("agentsync-home");
    let bindir = root.join("bin");
    let repo = root.join("repo");
    for dir in [&fake_home, &fake_xdg, &agentsync_home, &bindir, &repo] {
        std::fs::create_dir_all(dir).unwrap();
    }

    // Fake `codex`, `opencode` and `kilo` binaries so all three hosts are
    // detected. Skills and instructions never invoke the host CLI (they are
    // pure filesystem operations), so these need not do anything.
    for bin in ["codex", "opencode", "kilo"] {
        write_exec(&bindir.join(bin), "#!/bin/sh\nexit 0\n");
    }

    // Redirect every path root this test touches BEFORE calling any
    // `paths::` helper (directly or through `instructions::canonical_for`
    // below) — those helpers read these variables live, so computing a
    // "canonical" path before this point would resolve against whatever the
    // process's real or previously-test-set environment happens to be.
    unsafe {
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("XDG_CONFIG_HOME", &fake_xdg);
        std::env::set_var("AGENTSYNC_HOME", &agentsync_home);
        std::env::set_var("PATH", &bindir);
    }

    // Canonical skill content that all three hosts should end up linking to.
    let canonical_skill = agentsync_home.join("skills/demo-skill");
    std::fs::create_dir_all(&canonical_skill).unwrap();
    std::fs::write(
        canonical_skill.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: test\n---\nversion one\n",
    )
    .unwrap();

    // Canonical project instructions content. The read path (`Host::read` in
    // `src/hosts/mod.rs`) classifies a host's link against
    // `instructions::canonical_for(&scope)` unconditionally — it does not
    // consult the manifest's `source` override — so the manifest entry's name
    // and the file it points at must agree with the tool's own default
    // naming, or a correctly-synced link reads back as "foreign" on the next
    // load. Using the product's own naming functions here (rather than an
    // arbitrary name picked for the test) is what keeps this test honest
    // about that coupling instead of accidentally dodging it.
    let scope = agentsync::core::model::Scope::Project(repo.display().to_string());
    let instructions_name = agentsync::domains::instructions::default_name(&scope);
    let canonical_instructions = agentsync::domains::instructions::canonical_for(&scope);
    std::fs::create_dir_all(canonical_instructions.parent().unwrap()).unwrap();
    std::fs::write(&canonical_instructions, "shared project instructions\n").unwrap();

    let manifest_path = agentsync_home.join("manifest.toml");
    std::fs::write(
        &manifest_path,
        format!(
            r#"
[skills.demo-skill]
source = "skills/demo-skill"

[instructions."{instructions_name}"]
source = "{source}"
scope = "project"
repos = ["{repo}"]
"#,
            instructions_name = instructions_name,
            source = canonical_instructions.display(),
            repo = repo.display()
        ),
    )
    .unwrap();

    let world = World::load(&manifest_path, &[repo.display().to_string()]).expect("world loads");

    let detected: Vec<String> = world
        .detected()
        .map(|(h, _)| h.name().to_string())
        .collect();
    for name in ["codex", "opencode", "kilo"] {
        assert!(detected.contains(&name.to_string()), "{detected:?}");
    }
    assert!(
        !detected.contains(&"claude".to_string()),
        "the real claude must not be reachable from this test: {detected:?}"
    );

    // ---- proof 1: the shared skills write target is genuinely shared ----
    let expected_skills_dir = fake_home.join(".agents/skills");
    let mut skills_link_dirs = Vec::new();
    for name in ["codex", "opencode", "kilo"] {
        let dir = world
            .host(name)
            .unwrap()
            .skills_link_dir()
            .unwrap_or_else(|| panic!("{name} has no skills_link_dir"));
        assert_eq!(
            dir, expected_skills_dir,
            "{name} must resolve dirs[0] to the shared skills directory"
        );
        skills_link_dirs.push(dir);
    }

    // ---- proof 1b: the shared project instructions path is genuinely shared ----
    let expected_agents_md = repo.join("AGENTS.md");
    for name in ["codex", "opencode", "kilo"] {
        let path = world
            .host(name)
            .unwrap()
            .instruction_path(&agentsync::core::model::Scope::Project(
                repo.display().to_string(),
            ))
            .unwrap_or_else(|| panic!("{name} has no project instruction path"));
        assert_eq!(
            path, expected_agents_md,
            "{name} must resolve the project instructions path to the shared AGENTS.md"
        );
    }

    let mut rows = world.rows();
    let skill_row = rows
        .iter()
        .find(|r| r.name == "demo-skill" && r.domain == Domain::Skills)
        .expect("a row for demo-skill");
    assert_eq!(
        skill_row.headline, "missing from codex, kilo and opencode",
        "unexpected headline: {}",
        skill_row.headline
    );
    let instructions_row = rows
        .iter()
        .find(|r| r.name == instructions_name && r.domain == Domain::Instructions)
        .expect("a row for repo-agents");
    assert_eq!(
        instructions_row.headline, "missing from codex, kilo and opencode",
        "unexpected headline: {}",
        instructions_row.headline
    );

    for row in rows.iter_mut() {
        if (row.name == "demo-skill" && row.domain == Domain::Skills)
            || (row.name == instructions_name && row.domain == Domain::Instructions)
        {
            row.accepted = true;
        }
    }

    let plan = world.plan(&rows);

    // ---- proof 2 & 3: one filesystem operation, not three, per shared path ----
    //
    // Counting steps in the *plan* is a real-effect count, not a precondition:
    // `apply::run` executes `plan.steps` one-for-one with no deduplication of
    // its own (see `src/core/apply.rs`), so the number of `FsOp::Link` steps
    // that name a given path is exactly the number of `symlink()` calls that
    // will happen against that path.
    let links_to = |target: &Path| -> Vec<&Path> {
        plan.steps
            .iter()
            .filter_map(|s| match &s.step {
                Step::Fs(FsOp::Link { link, .. }) if link == target => Some(link.as_path()),
                _ => None,
            })
            .collect()
    };
    let skill_link_path = expected_skills_dir.join("demo-skill");
    assert_eq!(
        links_to(&skill_link_path).len(),
        1,
        "syncing demo-skill to codex, opencode and kilo (all sharing {}) must be one \
         filesystem operation, not three: {:#?}",
        skill_link_path.display(),
        plan.steps
    );
    assert_eq!(
        links_to(&expected_agents_md).len(),
        1,
        "linking AGENTS.md into codex, opencode and kilo (all sharing {}) must be one \
         filesystem operation, not three: {:#?}",
        expected_agents_md.display(),
        plan.steps
    );

    let mut manifest = world.manifest.clone();
    let report = apply::run(&plan, &mut manifest, &manifest_path, &world.hosts, |_| {});
    assert!(
        !report.any_failed(),
        "pass 1 must apply cleanly: {:?}",
        report.results
    );

    // ---- the filesystem side, pass 1 ----
    let skill_meta = skill_link_path
        .symlink_metadata()
        .expect("the skill link exists");
    assert!(skill_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&skill_link_path).unwrap(),
        canonical_skill
    );
    let agents_meta = expected_agents_md
        .symlink_metadata()
        .expect("the AGENTS.md link exists");
    assert!(agents_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&expected_agents_md).unwrap(),
        canonical_instructions
    );

    // Concrete state captured after pass 1: path, inode of the link itself,
    // and byte content read through it. This is what pass 2 must reproduce
    // exactly for convergence to be proven rather than assumed.
    let skill_ino_1 = skill_meta.ino();
    let agents_ino_1 = agents_meta.ino();
    let skill_bytes_1 = std::fs::read(skill_link_path.join("SKILL.md")).unwrap();
    let agents_bytes_1 = std::fs::read(&expected_agents_md).unwrap();

    // ---- proof 4: a second full pass mutates nothing further ----
    let world2 = World::load(&manifest_path, &[repo.display().to_string()]).unwrap();
    let mut rows2 = world2.rows();
    let still_open: Vec<String> = rows2
        .iter()
        .filter(|r| {
            r.severity != agentsync::core::diff::Severity::Synced
                && matches!(r.name.as_str(), n if n == "demo-skill" || n == instructions_name)
        })
        .map(|r| format!("{} ({})", r.name, r.headline))
        .collect();
    assert!(
        still_open.is_empty(),
        "a second run must not re-report work pass 1 already did: {still_open:?}"
    );

    for row in rows2.iter_mut() {
        if row.actionable() {
            row.accepted = true;
        }
    }
    let plan2 = world2.plan(&rows2);
    let leftover_links: Vec<_> = plan2
        .steps
        .iter()
        .filter(|s| {
            matches!(&s.step, Step::Fs(FsOp::Link { link, .. })
                if link == &skill_link_path || link == &expected_agents_md)
        })
        .map(|s| s.label.clone())
        .collect();
    assert!(
        leftover_links.is_empty(),
        "a converged second plan must contain no further link steps for the shared paths: \
         {leftover_links:?}"
    );

    let mut manifest2 = world2.manifest.clone();
    let report2 = apply::run(
        &plan2,
        &mut manifest2,
        &manifest_path,
        &world2.hosts,
        |_| {},
    );
    assert!(
        !report2.any_failed(),
        "pass 2 must apply cleanly: {:?}",
        report2.results
    );

    // The real convergence proof: identical path, identical inode, identical
    // bytes. Not "apply returned Ok" — the actual link and its target must be
    // untouched by the second pass.
    let skill_meta_2 = skill_link_path
        .symlink_metadata()
        .expect("the skill link must still exist after pass 2");
    let agents_meta_2 = expected_agents_md
        .symlink_metadata()
        .expect("the AGENTS.md link must still exist after pass 2");
    assert_eq!(
        skill_meta_2.ino(),
        skill_ino_1,
        "pass 2 must not have recreated the skill symlink"
    );
    assert_eq!(
        agents_meta_2.ino(),
        agents_ino_1,
        "pass 2 must not have recreated the AGENTS.md symlink"
    );
    assert_eq!(
        std::fs::read(skill_link_path.join("SKILL.md")).unwrap(),
        skill_bytes_1,
        "the skill content reached through the link must be byte-identical after pass 2"
    );
    assert_eq!(
        std::fs::read(&expected_agents_md).unwrap(),
        agents_bytes_1,
        "the AGENTS.md content reached through the link must be byte-identical after pass 2"
    );

    // ---- no artifact escapes the temp dir: the shared skills dir must hold
    // exactly the one entry this test created ----
    let entries: Vec<_> = std::fs::read_dir(&expected_skills_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("demo-skill")],
        "the shared skills directory must contain exactly the one linked skill: {entries:?}"
    );
}

fn family_host(name: &str) -> Host {
    let text = descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .expect("builtin descriptor")
        .1;
    Host {
        descriptor: descriptor::parse(text, name).unwrap(),
        bin: Some(std::path::PathBuf::from(format!("/usr/bin/{name}"))),
    }
}

fn family_snapshot(name: &str, family: Family, env: &Env, repo: Option<&Path>) -> HostSnapshot {
    HostSnapshot {
        host: name.to_string(),
        display: name.to_string(),
        detected: true,
        plugin_targets: ocp::read_full_state(family, env, repo),
        ..Default::default()
    }
}

fn family_world(
    manifest: Manifest,
    manifest_path: std::path::PathBuf,
    env: &Env,
    repo: Option<&Path>,
) -> World {
    World {
        manifest,
        manifest_path,
        hosts: vec![family_host("opencode"), family_host("kilo")],
        snapshots: vec![
            family_snapshot("opencode", Family::OpenCode, env, repo),
            family_snapshot("kilo", Family::Kilo, env, repo),
        ],
        repos: repo
            .map(|r| vec![r.display().to_string()])
            .unwrap_or_default(),
        warnings: Vec::new(),
    }
}

#[test]
fn opencode_plugins_converge_after_two_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_home = tmp.path().join("cfg");
    let env =
        Env::new(tmp.path().join("home")).set("XDG_CONFIG_HOME", cfg_home.display().to_string());

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "security-guidance".into(),
        PluginEntry {
            marketplace: None,
            hosts: None,
            targets: BTreeMap::from([(
                "opencode".to_string(),
                PluginTarget {
                    npm: Some("@company/opencode-security@1.4.2".into()),
                    local: None,
                    scope: ScopeKind::User,
                },
            )]),
        },
    );
    let manifest_path = tmp.path().join("manifest.toml");

    // Pass 1: nothing on disk yet, so the target is missing and a real
    // config transaction must be planned and applied.
    let world = family_world(manifest.clone(), manifest_path.clone(), &env, None);
    let mut rows = world.rows();
    for row in rows.iter_mut() {
        row.accepted = row.name == "security-guidance" && row.actionable();
    }
    let plan = world.plan(&rows);
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::ConfigTransaction(_))),
        "pass 1 must plan a config transaction: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    let mut manifest_for_apply = Manifest::default();
    let report = apply::run(&plan, &mut manifest_for_apply, &manifest_path, &[], |_| {});
    assert!(
        report
            .results
            .iter()
            .all(|r| r.outcome == Outcome::Done || r.outcome == Outcome::Skipped),
        "pass 1 must apply cleanly: {:?}",
        report.results
    );

    let config_path = cfg_home.join("opencode/opencode.jsonc");
    let written = std::fs::read_to_string(&config_path).expect("opencode config was written");
    assert!(
        written.contains("@company/opencode-security@1.4.2"),
        "the npm spec must actually land in the plugin array: {written}"
    );

    // Pass 2: read the real file the first pass just wrote. The plan must
    // contain no plugin mutation at all — that is the actual proof of
    // convergence, not a second assertion about the file's contents.
    let world2 = family_world(manifest, manifest_path.clone(), &env, None);
    let rows2 = world2.rows();
    let row2 = rows2
        .iter()
        .find(|r| r.name == "security-guidance" && r.domain == Domain::Plugins)
        .expect("target row still present");
    assert!(
        !row2.actionable(),
        "a converged target must have nothing left to do: {}",
        row2.headline
    );
    let mut rows2_accept_everything = rows2;
    for row in rows2_accept_everything.iter_mut() {
        row.accepted = row.actionable();
    }
    let plan2 = world2.plan(&rows2_accept_everything);
    assert!(
        !plan2.steps.iter().any(|s| matches!(
            &s.step,
            Step::ConfigTransaction(_) | Step::FileTransaction(_)
        )),
        "the second plan must contain no plugin mutation: {:?}",
        plan2.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn kilo_plugins_converge_after_two_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_home = tmp.path().join("cfg");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(tmp.path().join("manifest-dir/plugins")).unwrap();
    std::fs::write(
        tmp.path().join("manifest-dir/plugins/local-policy.ts"),
        "export async function AgentsyncHooks(ctx) { return {}; }\n",
    )
    .unwrap();
    let env =
        Env::new(tmp.path().join("home")).set("XDG_CONFIG_HOME", cfg_home.display().to_string());

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "local-policy".into(),
        PluginEntry {
            marketplace: None,
            hosts: None,
            targets: BTreeMap::from([(
                "kilo".to_string(),
                PluginTarget {
                    npm: None,
                    local: Some("plugins/local-policy.ts".into()),
                    scope: ScopeKind::Project,
                },
            )]),
        },
    );
    let manifest_path = tmp.path().join("manifest-dir/manifest.toml");

    // Pass 1: the local plugin file exists at its source path, but has not
    // been copied to Kilo's host-owned location yet.
    let world = family_world(manifest.clone(), manifest_path.clone(), &env, Some(&repo));
    let mut rows = world.rows();
    for row in rows.iter_mut() {
        row.accepted = row.name == "local-policy" && row.actionable();
    }
    let plan = world.plan(&rows);
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "pass 1 must plan a file copy: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    let mut manifest_for_apply = Manifest::default();
    let report = apply::run(&plan, &mut manifest_for_apply, &manifest_path, &[], |_| {});
    assert!(
        report
            .results
            .iter()
            .all(|r| r.outcome == Outcome::Done || r.outcome == Outcome::Skipped),
        "pass 1 must apply cleanly: {:?}",
        report.results
    );

    let destination = repo.join(".kilo/plugin/agentsync-local-policy.ts");
    assert!(
        destination.is_file(),
        "the local plugin must be copied to Kilo's host-owned location"
    );

    // Pass 2: read the real destination the first pass just wrote.
    let world2 = family_world(manifest, manifest_path.clone(), &env, Some(&repo));
    let rows2 = world2.rows();
    let row2 = rows2
        .iter()
        .find(|r| r.name == "local-policy" && r.domain == Domain::Plugins)
        .expect("target row still present");
    assert!(
        !row2.actionable(),
        "a converged local target must have nothing left to do: {}",
        row2.headline
    );
    let mut rows2_accept_everything = rows2;
    for row in rows2_accept_everything.iter_mut() {
        row.accepted = row.actionable();
    }
    let plan2 = world2.plan(&rows2_accept_everything);
    assert!(
        !plan2.steps.iter().any(|s| matches!(
            &s.step,
            Step::ConfigTransaction(_) | Step::FileTransaction(_)
        )),
        "the second plan must contain no plugin mutation: {:?}",
        plan2.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn kilo_plugins_never_claims_a_preexisting_unowned_local_plugin_directory() {
    // Real read (`read_full_state`), real files, real plan/apply — proving
    // the guard end to end, not just against a hand-set snapshot field.
    let tmp = tempfile::tempdir().unwrap();
    let cfg_home = tmp.path().join("cfg");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(tmp.path().join("manifest-dir/plugins")).unwrap();
    std::fs::write(
        tmp.path().join("manifest-dir/plugins/local-policy.ts"),
        "export async function AgentsyncHooks(ctx) { return {}; }\n",
    )
    .unwrap();
    // The destination directory already exists, holds a file of the user's
    // own, and was never touched by agentsync — no `.agentsync-owned`
    // marker anywhere in it.
    let dest_dir = repo.join(".kilo/plugin");
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::write(dest_dir.join("users-own-plugin.ts"), b"user content").unwrap();

    let env =
        Env::new(tmp.path().join("home")).set("XDG_CONFIG_HOME", cfg_home.display().to_string());

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "local-policy".into(),
        PluginEntry {
            marketplace: None,
            hosts: None,
            targets: BTreeMap::from([(
                "kilo".to_string(),
                PluginTarget {
                    npm: None,
                    local: Some("plugins/local-policy.ts".into()),
                    scope: ScopeKind::Project,
                },
            )]),
        },
    );
    let manifest_path = tmp.path().join("manifest-dir/manifest.toml");

    let world = family_world(manifest, manifest_path.clone(), &env, Some(&repo));
    let mut rows = world.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "local-policy" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert_eq!(
        row.severity,
        agentsync::core::diff::Severity::Blocked,
        "a real, pre-existing unowned directory must be read as blocked: {}",
        row.headline
    );
    assert!(!row.actionable());

    for r in rows.iter_mut() {
        r.accepted = r.actionable();
    }
    let plan = world.plan(&rows);
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "no file transaction may be planned against an unowned directory: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    let mut manifest_for_apply = Manifest::default();
    let report = apply::run(&plan, &mut manifest_for_apply, &manifest_path, &[], |_| {});
    assert!(
        report.results.iter().all(|r| r.outcome != Outcome::Failed),
        "an empty accepted plan must not report a failure: {:?}",
        report.results
    );

    assert!(
        !dest_dir.join(".agentsync-owned").exists(),
        "no ownership marker may appear in a directory agentsync did not create"
    );
    assert!(
        !dest_dir.join("agentsync-local-policy.ts").exists(),
        "nothing may be written into an unowned directory"
    );
    assert_eq!(
        std::fs::read(dest_dir.join("users-own-plugin.ts")).unwrap(),
        b"user content",
        "the user's own file must be completely untouched"
    );
}

#[test]
fn host_read_exercises_the_real_plugin_target_wiring_for_opencode() {
    // A synthetic minimal descriptor (no `[skills]`/`[instructions]`
    // sections) isolates exactly the line added to `Host::read`: the
    // `Family::from_host_name` gate that calls into
    // `opencode_family::plugins::read_full_state`. Using the full shipped
    // `opencode.toml` here would also exercise the `[skills]` section, whose
    // `~/.agents/skills` write target is deliberately real-HOME-rooted (see
    // the comment in `opencode.toml`) — and reading the real developer
    // machine's home directory is exactly what this test must never do.
    let descriptor_text = "\
name = \"opencode\"
display = \"OpenCode\"
detect = { bin = \"opencode\" }
";
    let descriptor = agentsync::hosts::descriptor::parse(descriptor_text, "opencode").unwrap();
    let host = agentsync::hosts::Host {
        descriptor,
        bin: Some(std::path::PathBuf::from("/usr/bin/opencode")),
    };

    let tmp = tempfile::tempdir().unwrap();
    let cfg_home = tmp.path().join("cfg");
    std::fs::create_dir_all(cfg_home.join("opencode")).unwrap();
    std::fs::write(
        cfg_home.join("opencode/opencode.jsonc"),
        r#"{"plugin": ["@company/opencode-security@1.4.2"]}"#,
    )
    .unwrap();

    // `Host::read` calls `Env::from_process()` internally for the OpenCode
    // family, so proving the real wiring requires a real (but fully
    // isolated, tempdir-backed) `XDG_CONFIG_HOME`. No other test in this
    // suite reads or sets this variable, and this test does not touch `HOME`
    // or `PATH`, so it does not race with anything else here.
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
    let result = host.read(&[]);
    match previous {
        Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
    let snap = result.expect("read succeeds");

    assert!(
        snap.plugin_targets
            .occurrences
            .contains_key("@company/opencode-security@1.4.2"),
        "Host::read must exercise the real opencode_family::plugins::read_full_state \
         path for opencode, not just the helper called directly by other tests"
    );
}

// ---------------------------------------------------------------------------
// OW-009: the generated Kilo hook bridge — two full applies must converge.
//
// Every write here goes through the real `transaction::FileTransaction` and
// `apply::run`, exactly like the rest of the guarded-write suite above. The
// generated bridge, its index, and its sidecar are the one validity contract
// (`GeneratedBridge::verify_on_disk`), proven by actually reading back the
// files pass 1 wrote and confirming pass 2 plans nothing further.
// ---------------------------------------------------------------------------

fn kilo_bridge_test_spec() -> agentsync::shim::ShimSpec {
    agentsync::shim::ShimSpec {
        source_id: "demo@mkt:hooks/hooks.json:pre_tool_use:0:0".into(),
        command: "echo '{\"systemMessage\":\"ok\"}'".into(),
        plugin_root: None,
        if_pattern: None,
        event: Some("PreToolUse".into()),
        output_strategy: agentsync::core::model::HookOutputStrategy::KiloV1,
        allowed_output: vec!["systemMessage".into()],
        fold_into_system_message: vec![],
        rewake_message: None,
        rewake_summary: None,
        timeout_seconds: None,
    }
}

#[test]
fn kilo_hooks_converge_after_two_passes() {
    // `state_home` is injected explicitly below, but the ownership guard
    // resolves `paths::state_dir()`, which reads AGENTSYNC_STATE_HOME from the
    // *process* environment. Without this lock a concurrently running hook test
    // that sets that variable steers this test's writes into its fixture, and
    // the convergence assertion then fails for a reason that has nothing to do
    // with convergence.
    let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let _env_restore = EnvRestore::capture(&["XDG_CONFIG_HOME", "AGENTSYNC_STATE_HOME"]);
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path().join("profile");
    let state = tmp.path().join("state");
    unsafe {
        std::env::set_var("AGENTSYNC_STATE_HOME", &state);
    }
    let bin = tmp.path().join("bin/agentsync");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"fake-agentsync-binary-bytes").unwrap();

    let bridge_input = agentsync::shim::bridges::kilo::KiloBridgeInput {
        active_profile_dir: profile.clone(),
        state_home: state.clone(),
        agentsync_bin: bin.clone(),
        handlers: vec![agentsync::shim::bridges::kilo::BridgedHandler {
            callback: "tool.execute.before".into(),
            spec: kilo_bridge_test_spec(),
        }],
    };

    // ---- pass 1: nothing on disk yet ----
    let g1 = agentsync::shim::bridges::kilo::generate(&bridge_input).unwrap();
    let tx1 = g1
        .transaction(false, false, None, None, &[])
        .unwrap()
        .expect("a fresh bridge must be planned");

    let mut plan1 = Plan::default();
    plan1.push("generate the Kilo hook bridge", Step::FileTransaction(tx1));

    let manifest_path = tmp.path().join("manifest.toml");
    let mut manifest = agentsync::manifest::Manifest::default();
    let report1 = apply::run(&plan1, &mut manifest, &manifest_path, &[], |_| {});
    assert_eq!(
        report1.count(Outcome::Failed),
        0,
        "pass 1 must apply cleanly: {:?}",
        report1.results
    );
    assert!(g1.bridge_path.is_file(), "the bridge file must be written");
    assert!(g1.index_path.is_file(), "the index must be written");
    assert_eq!(g1.sidecars.len(), 1);
    assert!(g1.sidecars[0].0.is_file(), "the sidecar must be written");
    g1.verify_on_disk()
        .expect("everything just written must satisfy the validity contract");

    // ---- pass 2: read the real files pass 1 wrote and regenerate ----
    let g2 = agentsync::shim::bridges::kilo::generate(&bridge_input).unwrap();
    assert_eq!(
        g2.bridge_contents, g1.bridge_contents,
        "generation must be byte-stable across passes"
    );
    let existing_bridge = std::fs::read(&g2.bridge_path).unwrap();
    let existing_index = std::fs::read(&g2.index_path).unwrap();
    let existing_sidecars: Vec<Option<Vec<u8>>> = g2
        .sidecars
        .iter()
        .map(|(path, _)| std::fs::read(path).ok())
        .collect();

    let tx2 = g2
        .transaction(
            true,
            true,
            Some(&existing_bridge),
            Some(&existing_index),
            &existing_sidecars,
        )
        .unwrap();
    assert!(
        tx2.is_none(),
        "the second pass must plan no mutation at all, proving convergence"
    );

    // Applying an empty second plan must still succeed cleanly.
    let plan2 = Plan::default();
    let mut manifest2 = manifest.clone();
    let report2 = apply::run(&plan2, &mut manifest2, &manifest_path, &[], |_| {});
    assert_eq!(
        report2.count(Outcome::Failed),
        0,
        "the converged second (empty) plan must apply cleanly: {:?}",
        report2.results
    );
}

#[test]
fn kilo_hooks_apply_time_race_is_rejected_and_the_existing_bridge_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path().join("profile");
    let state = tmp.path().join("state");
    let bin = tmp.path().join("bin/agentsync");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"fake-agentsync-binary-bytes").unwrap();

    let bridge_input = agentsync::shim::bridges::kilo::KiloBridgeInput {
        active_profile_dir: profile.clone(),
        state_home: state.clone(),
        agentsync_bin: bin.clone(),
        handlers: vec![],
    };
    let g = agentsync::shim::bridges::kilo::generate(&bridge_input).unwrap();
    std::fs::create_dir_all(&g.plugin_dir).unwrap();
    std::fs::write(g.plugin_dir.join(".agentsync-owned"), b"").unwrap();
    std::fs::create_dir_all(&g.state_dir).unwrap();
    std::fs::write(g.state_dir.join(".agentsync-owned"), b"").unwrap();
    // A previous generation is on disk, but the plan was built against a
    // now-stale hash (someone else's apply already ran, or the file was
    // hand-edited between plan and apply).
    std::fs::write(&g.bridge_path, "// changed since the plan was built\n").unwrap();

    let stale_precondition_hash =
        agentsync::transaction::compute_sha256(b"the hash the plan was built against");
    let tx = agentsync::transaction::FileTransaction::new().write_generated(
        &g.bridge_path,
        g.bridge_contents.clone(),
        agentsync::transaction::FilePrecondition::Sha256(stale_precondition_hash),
    );

    let mut plan = Plan::default();
    plan.push("regenerate the Kilo hook bridge", Step::FileTransaction(tx));
    let manifest_path = tmp.path().join("manifest.toml");
    let mut manifest = agentsync::manifest::Manifest::default();
    let report = apply::run(&plan, &mut manifest, &manifest_path, &[], |_| {});
    assert_eq!(
        report.count(Outcome::Failed),
        1,
        "a plan/apply race must be reported as a failure: {:?}",
        report.results
    );
    assert_eq!(
        std::fs::read_to_string(&g.bridge_path).unwrap(),
        "// changed since the plan was built\n",
        "a rejected race must never overwrite what is actually on disk"
    );
}

fn opencode_hooks_pre_tool_use_handler() -> agentsync::core::model::HookHandler {
    let id = agentsync::core::model::HookId {
        source: "demo-plugin@demo-marketplace:hooks/hooks.json".to_string(),
        event: "PreToolUse".to_string(),
        group: 0,
        index: 0,
    };
    let mut h = agentsync::core::model::HookHandler::new(id, "PreToolUse", "true");
    h.matcher = Some("Bash".to_string());
    h
}

fn opencode_hooks_world(cfg_home: &Path) -> World {
    let mut claude_snap = HostSnapshot {
        host: "claude".to_string(),
        display: "claude".to_string(),
        detected: true,
        ..Default::default()
    };
    let handler = opencode_hooks_pre_tool_use_handler();
    claude_snap.hooks.insert(handler.id.clone(), handler);

    let opencode_snap = HostSnapshot {
        host: "opencode".to_string(),
        display: "opencode".to_string(),
        detected: true,
        ..Default::default()
    };

    let claude_text = descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "claude")
        .expect("claude builtin")
        .1;
    let claude_host = Host {
        descriptor: descriptor::parse(claude_text, "claude").unwrap(),
        bin: Some(PathBuf::from("/usr/bin/claude")),
    };
    let opencode_text = descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "opencode")
        .expect("opencode builtin")
        .1;
    let opencode_host = Host {
        descriptor: descriptor::parse(opencode_text, "opencode").unwrap(),
        bin: Some(PathBuf::from("/usr/bin/opencode")),
    };
    // `cfg_home` doubles as this test's manifest directory too — it never
    // collides with the XDG config tree below, and keeping everything under
    // one temp root makes the fixture easy to read.
    let _ = cfg_home;

    World {
        manifest: Manifest::default(),
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![claude_host, opencode_host],
        snapshots: vec![claude_snap, opencode_snap],
        repos: Vec::new(),
        warnings: Vec::new(),
    }
}

#[test]
fn opencode_hooks_converge_after_two_passes() {
    let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let _env_restore = EnvRestore::capture(&["XDG_CONFIG_HOME", "AGENTSYNC_STATE_HOME"]);

    let tmp = tempfile::tempdir().unwrap();
    let cfg_home = tmp.path().join("cfg");
    let state_home = tmp.path().join("state");
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &cfg_home);
        std::env::set_var("AGENTSYNC_STATE_HOME", &state_home);
    }

    // ---- pass 1: nothing generated yet, so the row is actionable and the
    // plan carries exactly one guarded FileTransaction, never a host command
    // (OpenCode has none to invoke) ----
    let world = opencode_hooks_world(&cfg_home);
    let mut rows = world.rows();
    let row = rows
        .iter()
        .find(|r| r.domain == Domain::Hooks && r.name.starts_with("demo-plugin"))
        .expect("a hooks row for the bridged handler");
    assert!(
        row.actionable(),
        "pass 1 must have something to do: {row:?}"
    );
    for row in rows.iter_mut() {
        if row.domain == Domain::Hooks {
            row.accepted = row.actionable();
        }
    }
    let plan = world.plan(&rows);
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "pass 1 must plan a guarded file transaction: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        plan.steps
            .iter()
            .all(|s| !matches!(&s.step, Step::Host { .. })),
        "OpenCode has no install command to invoke: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    let manifest_path = tmp.path().join("manifest.toml");
    let mut manifest_for_apply = Manifest::default();
    let report = apply::run(&plan, &mut manifest_for_apply, &manifest_path, &[], |_| {});
    assert!(
        !report.any_failed(),
        "pass 1 must apply cleanly: {:?}",
        report.results
    );

    let bridge_path = cfg_home.join("opencode/plugins/agentsync-hooks.ts");
    let index_path = state_home.join("shims/opencode/index.json");
    assert!(bridge_path.is_file(), "bridge script must be written");
    assert!(index_path.is_file(), "bridge index must be written");
    let index_text = std::fs::read_to_string(&index_path).unwrap();
    assert!(
        index_text.contains("tool.execute.before"),
        "the index must map PreToolUse to the measured OpenCode callback: {index_text}"
    );

    // ---- pass 2: read the real files pass 1 just wrote. The row must have
    // nothing left to do, and the plan must contain no hooks mutation at all
    // — that is the actual proof of convergence, not a second assertion about
    // file contents. ----
    let world2 = opencode_hooks_world(&cfg_home);
    let rows2 = world2.rows();
    let row2 = rows2
        .iter()
        .find(|r| r.domain == Domain::Hooks && r.name.starts_with("demo-plugin"))
        .expect("target row still present");
    assert!(
        !row2.actionable(),
        "a converged bridge must have nothing left to do: {}",
        row2.headline
    );
    let mut rows2_accept_everything = rows2;
    for row in rows2_accept_everything.iter_mut() {
        row.accepted = row.actionable();
    }
    let plan2 = world2.plan(&rows2_accept_everything);
    assert!(
        !plan2
            .steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "the second plan must contain no hooks mutation: {:?}",
        plan2.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

/// The exact bytes of a regular file, or the exact link target of a symlink,
/// alongside `(dev, ino)`. Used to prove a second, fully-converged pass
/// leaves every artifact byte-for-byte and inode-for-inode untouched — not
/// merely "still present with the same content", which a delete-and-recreate
/// would also satisfy.
#[derive(Debug, PartialEq)]
enum Fingerprint {
    File { dev: u64, ino: u64, bytes: Vec<u8> },
    Symlink { dev: u64, ino: u64, target: PathBuf },
}

fn fingerprint(path: &Path) -> Fingerprint {
    let meta =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    if meta.file_type().is_symlink() {
        Fingerprint::Symlink {
            dev: meta.dev(),
            ino: meta.ino(),
            target: std::fs::read_link(path).unwrap(),
        }
    } else {
        Fingerprint::File {
            dev: meta.dev(),
            ino: meta.ino(),
            bytes: std::fs::read(path).unwrap(),
        }
    }
}

/// A complete fake world with all four hosts agentsync supports — Claude,
/// Codex, OpenCode, and Kilo — proving they converge together, not merely
/// each in isolation (every other two-pass test in this file exercises one
/// host, or one OpenCode-family host, at a time).
///
/// Covers three domains that are meaningful across all four host shapes at
/// once: MCP (a CLI `add-json`/`add`/`remove` write for Claude/Codex, a
/// guarded JSONC edit for OpenCode/Kilo — measured: neither has an `mcp
/// remove` command), skills (a shared canonical file, symlinked from each
/// host's own directory), and project instructions (the same canonical file,
/// symlinked as `AGENTS.md` for Codex/OpenCode/Kilo and `CLAUDE.md` for
/// Claude). Plugins and hooks already have dedicated per-host two-pass gates
/// elsewhere in this file; this test's job is the four-host combination, not
/// re-covering every domain again.
///
/// Pass 2 asserts on the plan's concrete step count and on file
/// bytes/inodes captured after pass 1 — never on a precondition the test
/// itself supplied — because two earlier attempts at a convergence gate in
/// this codebase were rejected for exactly that (see `docs/open-work.md`,
/// OW-004: one asserted an empty manifest had zero servers, the other
/// compared three identical strings the test had built itself).
#[test]
fn four_host_world_converges_after_two_passes() {
    let _env_guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let _env_restore = EnvRestore::capture(&[
        "HOME",
        "XDG_CONFIG_HOME",
        "AGENTSYNC_HOME",
        "AGENTSYNC_STATE_HOME",
        "PATH",
    ]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Everything `~`, `{xdg_config}`, and `{agentsync-state}` can expand to
    // lives under `root`, so a bug here (or in the code under test) cannot
    // reach the real machine's `~/.claude.json`, `~/.config`, or
    // `~/.agents/skills`.
    let fake_home = root.join("home");
    let fake_xdg = root.join("xdg-config");
    let agentsync_home = root.join("agentsync-home");
    let state_home = root.join("agentsync-state");
    let bindir = root.join("bin");
    let repo = root.join("repo");
    for dir in [
        &fake_home,
        &fake_xdg,
        &agentsync_home,
        &state_home,
        &bindir,
        &repo,
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let python = which::which("python3").expect("python3 is needed to run this test");
    let claude_cfg = fake_home.join(".claude.json");
    let claude_log = root.join("claude-calls.log");
    write_exec(
        &bindir.join("claude"),
        &FAKE_CLAUDE
            .replace("#!/usr/bin/env python3", &format!("#!{}", python.display()))
            .replace("__LOG__", &claude_log.display().to_string())
            .replace("__CFG__", &claude_cfg.display().to_string()),
    );

    let codex_cfg = fake_home.join(".codex/config.toml");
    std::fs::create_dir_all(codex_cfg.parent().unwrap()).unwrap();
    let codex_log = root.join("codex-calls.log");
    write_exec(
        &bindir.join("codex"),
        &FAKE_HOST
            .replace("#!/usr/bin/env python3", &format!("#!{}", python.display()))
            .replace("__LOG__", &codex_log.display().to_string())
            .replace("__CFG__", &codex_cfg.display().to_string()),
    );

    // OpenCode and Kilo write mcp through guarded JSONC edits, never a CLI
    // call (measured: neither has an `mcp remove` command), so their fake
    // binaries only need to exist for host detection.
    write_exec(&bindir.join("opencode"), "#!/bin/sh\nexit 0\n");
    write_exec(&bindir.join("kilo"), "#!/bin/sh\nexit 0\n");

    // Redirect every path root this test touches BEFORE calling any `paths::`
    // helper (directly or through `World::load` below) — those helpers read
    // these variables live.
    unsafe {
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("XDG_CONFIG_HOME", &fake_xdg);
        std::env::set_var("AGENTSYNC_HOME", &agentsync_home);
        std::env::set_var("AGENTSYNC_STATE_HOME", &state_home);
        std::env::set_var("PATH", &bindir);
    }

    // Canonical skill content shared by Codex, OpenCode, and Kilo
    // (`~/.agents/skills`); Claude links it from its own `~/.claude/skills`.
    let canonical_skill = agentsync_home.join("skills/demo-skill");
    std::fs::create_dir_all(&canonical_skill).unwrap();
    std::fs::write(
        canonical_skill.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: test\n---\nversion one\n",
    )
    .unwrap();

    // Canonical project instructions: one file, symlinked as `AGENTS.md` for
    // Codex/OpenCode/Kilo and `CLAUDE.md` for Claude.
    let scope = Scope::Project(repo.display().to_string());
    let instructions_name = agentsync::domains::instructions::default_name(&scope);
    let canonical_instructions = agentsync::domains::instructions::canonical_for(&scope);
    std::fs::create_dir_all(canonical_instructions.parent().unwrap()).unwrap();
    std::fs::write(&canonical_instructions, "shared project instructions\n").unwrap();

    let manifest_path = agentsync_home.join("manifest.toml");
    std::fs::write(
        &manifest_path,
        format!(
            r#"
[mcp.demo]
transport = "stdio"
command = "node"
args = ["/x/index.js"]

[skills.demo-skill]
source = "skills/demo-skill"

[instructions."{instructions_name}"]
source = "{source}"
scope = "project"
repos = ["{repo}"]
"#,
            source = canonical_instructions.display(),
            repo = repo.display(),
        ),
    )
    .unwrap();

    // ---- pass 1: nothing exists on any host yet ----
    let world1 = World::load(&manifest_path, &[repo.display().to_string()]).expect("world loads");
    let detected: Vec<String> = world1
        .detected()
        .map(|(h, _)| h.name().to_string())
        .collect();
    for name in ["claude", "codex", "opencode", "kilo"] {
        assert!(
            detected.contains(&name.to_string()),
            "{name} must be a detected host in this fake world: {detected:?}"
        );
    }

    let mut rows1 = world1.rows();
    assert!(
        rows1
            .iter()
            .any(|r| r.domain == Domain::Mcp && r.actionable()),
        "pass 1 must have mcp work to do"
    );
    assert!(
        rows1
            .iter()
            .any(|r| r.domain == Domain::Skills && r.actionable()),
        "pass 1 must have a skills row to do"
    );
    assert!(
        rows1
            .iter()
            .any(|r| r.domain == Domain::Instructions && r.actionable()),
        "pass 1 must have an instructions row to do"
    );
    for row in rows1.iter_mut() {
        row.accepted = row.actionable();
    }

    let plan1 = world1.plan(&rows1);
    let host_steps = plan1
        .steps
        .iter()
        .filter(|s| matches!(s.step, Step::Host { .. }))
        .count();
    let config_tx_steps = plan1
        .steps
        .iter()
        .filter(|s| matches!(s.step, Step::ConfigTransaction(_)))
        .count();
    let link_steps = plan1
        .steps
        .iter()
        .filter(|s| matches!(s.step, Step::Fs(FsOp::Link { .. })))
        .count();
    assert_eq!(
        host_steps,
        2,
        "exactly one `claude mcp add-json` and one `codex mcp add`, no more: {:?}",
        plan1.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert_eq!(
        config_tx_steps,
        2,
        "exactly one guarded JSONC edit for OpenCode and one for Kilo: {:?}",
        plan1.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert_eq!(
        link_steps,
        4,
        "one skill symlink shared by codex/opencode/kilo, one skill symlink \
         for claude's own directory, one AGENTS.md symlink shared by \
         codex/opencode/kilo, and one CLAUDE.md symlink for claude: {:?}",
        plan1.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    let mut manifest_after_1 = world1.manifest.clone();
    let report1 = apply::run(
        &plan1,
        &mut manifest_after_1,
        &manifest_path,
        &world1.hosts,
        |_| {},
    );
    assert!(
        !report1.any_failed(),
        "pass 1 must apply cleanly across all four hosts: {:?}",
        report1.results
    );

    let opencode_jsonc = fake_xdg.join("opencode/opencode.jsonc");
    let kilo_jsonc = fake_xdg.join("kilo/kilo.jsonc");
    let shared_skill_link = fake_home.join(".agents/skills/demo-skill");
    let claude_skill_link = fake_home.join(".claude/skills/demo-skill");
    let shared_agents_md = repo.join("AGENTS.md");
    let claude_md = repo.join("CLAUDE.md");

    let watched: Vec<PathBuf> = vec![
        claude_cfg.clone(),
        codex_cfg.clone(),
        opencode_jsonc.clone(),
        kilo_jsonc.clone(),
        canonical_instructions.clone(),
        canonical_skill.join("SKILL.md"),
        shared_skill_link.clone(),
        claude_skill_link.clone(),
        shared_agents_md.clone(),
        claude_md.clone(),
    ];
    for path in &watched {
        assert!(
            path.exists() || path.symlink_metadata().is_ok(),
            "pass 1 must have produced {}",
            path.display()
        );
    }
    let fingerprints_after_pass_1: Vec<Fingerprint> =
        watched.iter().map(|p| fingerprint(p)).collect();

    assert!(
        std::fs::read_to_string(&claude_cfg)
            .unwrap()
            .contains("\"demo\""),
        "claude's config must actually contain the added server"
    );
    assert!(
        std::fs::read_to_string(&codex_cfg)
            .unwrap()
            .contains("mcp_servers.demo"),
        "codex's config must actually contain the added server"
    );
    assert!(
        std::fs::read_to_string(&opencode_jsonc)
            .unwrap()
            .contains("\"demo\""),
        "opencode's config must actually contain the added server"
    );
    assert!(
        std::fs::read_to_string(&kilo_jsonc)
            .unwrap()
            .contains("\"demo\""),
        "kilo's config must actually contain the added server"
    );

    // ---- pass 2: read the real state pass 1 produced. Nothing may be left
    // to do, and applying "every accepted action" a second time must mutate
    // nothing at all. ----
    let world2 = World::load(&manifest_path, &[repo.display().to_string()]).expect("world loads");
    let rows2 = world2.rows();
    let expected_row_name = |domain: Domain| match domain {
        Domain::Mcp => "demo".to_string(),
        Domain::Skills => "demo-skill".to_string(),
        Domain::Instructions => instructions_name.clone(),
        Domain::Plugins | Domain::Hooks => unreachable!("not exercised by this test"),
    };
    for domain in [Domain::Mcp, Domain::Skills, Domain::Instructions] {
        let name = expected_row_name(domain);
        let touched: Vec<&agentsync::core::diff::Row> = rows2
            .iter()
            .filter(|r| r.domain == domain && r.name == name)
            .collect();
        assert!(
            !touched.is_empty(),
            "domain {domain:?} must still carry the row named {name:?} that pass 1 touched: {:?}",
            rows2
                .iter()
                .filter(|r| r.domain == domain)
                .map(|r| &r.name)
                .collect::<Vec<_>>()
        );
        for row in &touched {
            assert!(
                !row.actionable(),
                "a converged {domain:?} row must have nothing left to do: {} ({})",
                row.name,
                row.headline
            );
        }
    }

    let mut rows2_accept_everything = rows2;
    for row in rows2_accept_everything.iter_mut() {
        row.accepted = row.actionable();
    }
    let plan2 = world2.plan(&rows2_accept_everything);
    assert_eq!(
        plan2.steps.len(),
        0,
        "the second plan, built from a world that just re-read what pass 1 \
         wrote, must contain no config, plugin, hook, skill, instruction, or \
         manifest mutation at all: {:?}",
        plan2.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );

    let mut manifest_after_2 = world2.manifest.clone();
    let report2 = apply::run(
        &plan2,
        &mut manifest_after_2,
        &manifest_path,
        &world2.hosts,
        |_| {},
    );
    assert!(
        !report2.any_failed(),
        "applying the empty converged plan must still succeed: {:?}",
        report2.results
    );

    let fingerprints_after_pass_2: Vec<Fingerprint> =
        watched.iter().map(|p| fingerprint(p)).collect();
    assert_eq!(
        fingerprints_after_pass_1, fingerprints_after_pass_2,
        "every watched artifact must be byte-for-byte and inode-for-inode \
         identical after the converged second pass — not merely present with \
         the same content, which a delete-and-recreate would also satisfy"
    );
}
