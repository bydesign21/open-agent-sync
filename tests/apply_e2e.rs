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
use agentsync::core::model::{Scope, ScopeKind, StdioServer, Transport};
use agentsync::core::plan::{Plan, Step};
use agentsync::domains::World;
use agentsync::hosts::{Host, descriptor};
use agentsync::manifest::{Manifest, McpEntry};
use agentsync::transaction::{
    ConfigEditOperation, ConfigOrigin, ConfigScope, ConfigTransaction, FilePrecondition,
    FileTransaction, GuardedSource, SourceEdit, compute_sha256,
};

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
