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

use std::path::Path;

use agentsync::core::apply::{self, Outcome};
use agentsync::core::diff::{ActionKind, Domain};
use agentsync::domains::World;

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

#[test]
fn the_plan_is_what_actually_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let home = root.join("agentsync-home");
    let bindir = root.join("bin");
    let hostcfg = root.join("fake-config.toml");
    let hostskills = root.join("fake-skills");
    let log = root.join("calls.log");
    std::fs::create_dir_all(home.join("hosts")).unwrap();
    std::fs::create_dir_all(&bindir).unwrap();
    std::fs::create_dir_all(&hostskills).unwrap();

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
         [mcp_servers.adoptme]\ncommand = \"keeper\"\nargs = [\"--serve\"]\n",
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
    std::fs::write(
        home.join("hosts/fakehost.toml"),
        descriptor("fakehost", "fakehost"),
    )
    .unwrap();
    std::fs::write(
        home.join("hosts/brokenhost.toml"),
        descriptor("brokenhost", "brokenhost"),
    )
    .unwrap();

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
    // link `my-skill`.
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
    let rows2 = world2.rows();
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
