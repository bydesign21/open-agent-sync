//! End-to-end differ and planner tests over a synthetic world.
//!
//! These build `World` directly rather than reading the machine. So they assert
//! on behaviour that would otherwise only be visible by running the tool
//! against a real configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentsync::core::diff::{ActionKind, Domain, Row, Severity};
use agentsync::core::model::{
    HookHandler, HookId, HostSnapshot, HttpServer, McpServer, Scope, ScopeKind, StdioServer,
    Transport,
};
use agentsync::core::plan::{FsOp, Step};
use agentsync::domains::World;
use agentsync::hosts::{Host, descriptor};
use agentsync::manifest::{Manifest, McpEntry};

fn host(name: &str) -> Host {
    let text = descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .expect("builtin descriptor")
        .1;
    Host {
        descriptor: descriptor::parse(text, name).unwrap(),
        // Pretend it is installed. Nothing in these tests executes it.
        bin: Some(PathBuf::from(format!("/usr/bin/{name}"))),
    }
}

fn snapshot(name: &str, servers: &[(Scope, McpServer)]) -> HostSnapshot {
    let mut snap = HostSnapshot {
        host: name.to_string(),
        display: name.to_string(),
        detected: true,
        ..Default::default()
    };
    for (scope, server) in servers {
        snap.mcp
            .insert((scope.clone(), server.name.clone()), server.clone());
    }
    snap
}

fn stdio(name: &str, command: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        transport: Transport::Stdio(StdioServer {
            command: command.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn http(name: &str, url: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        transport: Transport::Http(HttpServer {
            url: url.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn http_with_header(name: &str, url: &str, key: &str, value: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        transport: Transport::Http(HttpServer {
            url: url.to_string(),
            headers: BTreeMap::from([(key.to_string(), value.to_string())]),
            bearer_token_env: None,
        }),
        ..Default::default()
    }
}

fn world(manifest: Manifest, claude: HostSnapshot, codex: HostSnapshot) -> World {
    World {
        manifest,
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![host("claude"), host("codex")],
        snapshots: vec![claude, codex],
        repos: vec!["/repos/one".to_string()],
        warnings: Vec::new(),
    }
}

fn find<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.name == name && r.domain == Domain::Mcp)
        .unwrap_or_else(|| panic!("no mcp row named {name:?} in {:?}", names(rows)))
}

fn names(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|r| r.name.clone()).collect()
}

fn accept(rows: &mut [Row], name: &str) {
    for row in rows.iter_mut() {
        row.accepted = row.name == name && row.actionable();
    }
}

// ---------------------------------------------------------------------------

#[test]
fn a_server_on_every_host_is_missing_from_the_manifest_not_from_a_host() {
    let server = stdio("mcp-unity", "node");
    let w = world(
        Manifest::default(),
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[(Scope::User, server.clone())]),
    );
    let rows = w.rows();
    let row = find(&rows, "mcp-unity");

    assert_eq!(row.headline, "not in the manifest yet");
    assert_eq!(row.action().label, "adopt into the manifest");
    // "keep X-only" makes no sense when every host already has it.
    assert!(
        !row.actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::KeepDivergent { .. })),
        "{:?}",
        row.actions.iter().map(|a| &a.label).collect::<Vec<_>>()
    );
}

#[test]
fn a_server_on_one_host_reads_as_only_in_that_host() {
    let w = world(
        Manifest::default(),
        snapshot("claude", &[]),
        snapshot(
            "codex",
            &[(Scope::User, http("atlassian_rovo", "https://a.test/mcp"))],
        ),
    );
    let rows = w.rows();
    let row = find(&rows, "atlassian_rovo");
    assert_eq!(row.headline, "only in codex");
    assert_eq!(row.severity, Severity::Normal);
}

#[test]
fn shadowing_defaults_to_one_global_definition_not_an_arbitrary_scope() {
    let server = stdio("tradingview", "bun");
    let w = world(
        Manifest::default(),
        snapshot(
            "claude",
            &[
                (Scope::User, server.clone()),
                (Scope::Project("/repos/one".into()), server.clone()),
            ],
        ),
        snapshot("codex", &[]),
    );
    let rows = w.rows();
    let row = find(&rows, "tradingview");

    assert_eq!(row.severity, Severity::Warn);
    assert!(row.headline.contains("2 scopes"), "{}", row.headline);
    assert_eq!(
        row.action().kind,
        ActionKind::Adopt {
            push: true,
            promote: true
        },
        "the default must not be whichever scope sorted first"
    );
    // Collapsing to a specific scope is still available.
    assert!(
        row.actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::CollapseScope { .. }))
    );
}

#[test]
fn promoting_removes_the_per_repo_copies_before_adding_the_global_one() {
    let server = stdio("pulumi", "pulumi");
    let w = world(
        Manifest::default(),
        snapshot(
            "claude",
            &[(Scope::Local("/repos/one".into()), server.clone())],
        ),
        snapshot("codex", &[]),
    );
    let mut rows = w.rows();
    accept(&mut rows, "pulumi");
    let plan = w.plan(&rows);

    let host_steps: Vec<(String, Vec<String>)> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.step {
            Step::Host { host, argv, .. } => Some((host.clone(), argv.clone())),
            _ => None,
        })
        .collect();

    let first_remove = host_steps
        .iter()
        .position(|(_, a)| a.contains(&"remove".to_string()));
    let first_add = host_steps
        .iter()
        .position(|(_, a)| a.iter().any(|x| x == "add" || x == "add-json"));
    assert!(
        first_remove.is_some(),
        "expected a remove step: {host_steps:?}"
    );
    assert!(first_add.is_some(), "expected an add step: {host_steps:?}");
    assert!(
        first_remove < first_add,
        "remove must precede add so the name is never at two scopes at once: {host_steps:?}"
    );

    // The add must land at user scope on claude.
    let claude_add = host_steps
        .iter()
        .find(|(h, a)| h == "claude" && a.iter().any(|x| x == "add-json"))
        .expect("claude add");
    assert_eq!(claude_add.1.last().unwrap(), "user");
}

#[test]
fn promoting_does_not_churn_a_host_that_already_holds_it_globally() {
    let server = stdio("mcp-unity", "node");
    let w = world(
        Manifest::default(),
        // claude has a per-repo copy; codex already has the global one.
        snapshot(
            "claude",
            &[(Scope::Project("/repos/one".into()), server.clone())],
        ),
        snapshot("codex", &[(Scope::User, server.clone())]),
    );
    let mut rows = w.rows();
    accept(&mut rows, "mcp-unity");
    let plan = w.plan(&rows);

    let codex_removes = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(&s.step, Step::Host { host, argv, .. }
                if host == "codex" && argv.contains(&"remove".to_string()))
        })
        .count();
    assert_eq!(
        codex_removes,
        0,
        "codex already holds it at user scope; removing and re-adding is churn: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn overwriting_an_existing_definition_removes_it_first() {
    // `claude mcp add-json` is NOT an upsert — it exits 1 with "already exists in
    // user config". So pushing a changed value must remove before it adds, or the
    // push always fails against a host that already has the name.
    let mut manifest = Manifest::default();
    let wanted = http("knowledge", "https://new.example.test/mcp");
    manifest.mcp.insert(
        "knowledge".into(),
        McpEntry::from_server(&wanted, ScopeKind::User, vec![]),
    );

    let stale = http("knowledge", "https://old.example.test/mcp");
    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, stale)]),
        snapshot("codex", &[]),
    );

    let mut rows = w.rows();
    let row = find(&rows, "knowledge");
    assert_eq!(row.headline, "differs on claude");

    accept(&mut rows, "knowledge");
    let plan = w.plan(&rows);

    let claude: Vec<Vec<String>> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.step {
            Step::Host { host, argv, .. } if host == "claude" => Some(argv.clone()),
            _ => None,
        })
        .collect();

    let remove = claude
        .iter()
        .position(|a| a.contains(&"remove".to_string()))
        .unwrap_or_else(|| panic!("expected a remove before the add: {claude:?}"));
    let add = claude
        .iter()
        .position(|a| a.contains(&"add-json".to_string()))
        .unwrap_or_else(|| panic!("expected an add: {claude:?}"));
    assert!(remove < add, "remove must come first: {claude:?}");
}

#[test]
fn an_identical_definition_is_left_completely_alone() {
    let mut manifest = Manifest::default();
    let server = http("same", "https://example.test/mcp");
    manifest.mcp.insert(
        "same".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );
    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[(Scope::User, server)]),
    );
    let mut rows = w.rows();
    assert_eq!(find(&rows, "same").severity, Severity::Synced);

    // Even if forced, there is nothing to do — no remove, no add.
    for row in rows.iter_mut() {
        row.accepted = true;
    }
    let plan = w.plan(&rows);
    assert!(
        plan.steps.is_empty(),
        "an in-sync server must produce no churn: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn pushing_an_oauth_server_says_it_still_needs_a_login() {
    // Credentials are per-host and do not travel with the definition. Reporting
    // the add as done without saying so reports success for a server that cannot
    // connect — which is exactly what happened in practice with sentry.
    let mut manifest = Manifest::default();
    let oauth = http("sentry", "https://mcp.sentry.dev/mcp");
    manifest.mcp.insert(
        "sentry".into(),
        McpEntry::from_server(&oauth, ScopeKind::User, vec![]),
    );
    let with_token = http("knowledge", "https://api.example.test/mcp");
    let mut entry = McpEntry::from_server(&with_token, ScopeKind::User, vec![]);
    entry.bearer_token_env = Some("KNOWLEDGE_TOKEN".into());
    manifest.mcp.insert("knowledge".into(), entry);

    let w = world(manifest, snapshot("claude", &[]), snapshot("codex", &[]));
    let mut rows = w.rows();
    for r in rows.iter_mut() {
        r.accepted = r.actionable();
    }
    let plan = w.plan(&rows);

    let manual: Vec<&String> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.step {
            Step::Manual(text) => Some(text),
            _ => None,
        })
        .collect();

    assert!(
        manual.iter().any(|t| t.contains("claude mcp login sentry")),
        "expected a claude login step: {manual:?}"
    );
    assert!(
        manual.iter().any(|t| t.contains("codex mcp login sentry")),
        "expected a codex login step: {manual:?}"
    );
    // A server that carries its own credential reference needs no login.
    assert!(
        !manual.iter().any(|t| t.contains("knowledge")),
        "a bearer_token_env server must not be told to log in: {manual:?}"
    );
}

#[test]
fn a_local_scoped_server_is_reported_as_blocked_for_a_user_only_host() {
    let server = http("pulumi", "https://mcp.ai.pulumi.com/mcp");
    let w = world(
        Manifest::default(),
        snapshot(
            "claude",
            &[(Scope::Local("/repos/one".into()), server.clone())],
        ),
        snapshot("codex", &[]),
    );
    let rows = w.rows();
    let row = find(&rows, "pulumi");
    assert!(
        row.detail.contains("codex: no local scope"),
        "detail was {:?}",
        row.detail
    );
}

#[test]
fn a_header_carrying_server_is_never_pushed_to_a_host_that_cannot_express_headers() {
    let mut manifest = Manifest::default();
    let server = http_with_header("corridor", "https://a.test/mcp", "X-Key", "${K}");
    manifest.mcp.insert(
        "corridor".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );

    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[]),
    );
    let mut rows = w.rows();
    let row = find(&rows, "corridor");
    assert_eq!(row.severity, Severity::Blocked);
    assert!(
        row.headline.contains("cannot hold it"),
        "headline was {:?}",
        row.headline
    );

    // Even if accepted, the plan must not emit a codex add step.
    accept(&mut rows, "corridor");
    let plan = w.plan(&rows);
    let touches_codex = plan.steps.iter().any(|s| match &s.step {
        Step::Host { host, argv, .. } => {
            host == "codex" && argv.iter().any(|a| a == "add" || a == "add-json")
        }
        _ => false,
    });
    assert!(
        !touches_codex,
        "headers cannot be expressed on codex, so it must be skipped, not lossily pushed"
    );
}

#[test]
fn a_literal_credential_becomes_a_warning_that_moves_it_to_an_env_var() {
    let server = http_with_header(
        "upskillai-knowledge",
        "https://api.example.test/mcp",
        "Authorization",
        "Bearer f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae",
    );
    let w = world(
        Manifest::default(),
        snapshot("claude", &[(Scope::User, server)]),
        snapshot("codex", &[]),
    );
    let mut rows = w.rows();
    let row = find(&rows, "upskillai-knowledge");

    assert_eq!(row.severity, Severity::Warn);
    assert_eq!(
        row.action().kind,
        ActionKind::SecretToEnv {
            var: "UPSKILLAI_KNOWLEDGE_TOKEN".into()
        }
    );

    accept(&mut rows, "upskillai-knowledge");
    let plan = w.plan(&rows);

    // The manifest must receive the reference, never the literal.
    let upsert = plan
        .steps
        .iter()
        .find_map(|s| match &s.step {
            Step::Manifest(agentsync::core::plan::ManifestOp::UpsertMcp { entry, .. }) => {
                Some(entry.clone())
            }
            _ => None,
        })
        .expect("expected a manifest upsert");
    assert_eq!(
        upsert.bearer_token_env.as_deref(),
        Some("UPSKILLAI_KNOWLEDGE_TOKEN")
    );
    assert!(
        upsert.headers.is_empty(),
        "the literal Authorization header must not survive into the manifest"
    );

    // And the user must be told to set the variable, as an explicit manual step.
    let manual = plan
        .steps
        .iter()
        .find_map(|s| match &s.step {
            Step::Manual(t) if t.contains("UPSKILLAI_KNOWLEDGE_TOKEN") => Some(t.clone()),
            _ => None,
        })
        .expect("expected a manual step naming the variable");

    // It must not promise a copy that does not exist. The manifest's secret gate
    // means the literal is never written there. Host config files are never
    // backed up, so the only copy is the one this plan overwrites.
    assert!(
        !manual.contains("backup"),
        "the manual step must not claim the literal is recoverable from a backup: {manual:?}"
    );
    assert!(
        manual.contains("FIRST") && manual.contains("claude"),
        "it must say to copy the value out of the host that holds it, first: {manual:?}"
    );

    // Lifting the token out also makes it representable on codex.
    let codex_add = plan.steps.iter().any(|s| match &s.step {
        Step::Host { host, argv, .. } => host == "codex" && argv.contains(&"add".to_string()),
        _ => false,
    });
    assert!(codex_add, "with the token in an env var, codex can hold it");
}

#[test]
fn recorded_divergence_stops_a_one_sided_entry_being_reported() {
    let server = http("unityMCP", "http://127.0.0.1:8080/mcp");
    let mut manifest = Manifest::default();
    let mut entry = McpEntry::from_server(&server, ScopeKind::User, vec![]);
    entry.hosts = Some(vec!["codex".into()]);
    manifest.mcp.insert("unityMCP".into(), entry);

    let w = world(
        manifest,
        snapshot("claude", &[]),
        snapshot("codex", &[(Scope::User, server)]),
    );
    let rows = w.rows();
    let row = find(&rows, "unityMCP");
    assert_eq!(
        row.severity,
        Severity::Synced,
        "a divergence recorded with hosts = [..] must not keep nagging: {}",
        row.headline
    );
}

#[test]
fn delete_everywhere_removes_from_both_hosts_and_the_manifest() {
    let server = stdio("obsolete", "node");
    let mut manifest = Manifest::default();
    manifest.mcp.insert(
        "obsolete".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );
    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[]),
    );

    let mut rows = w.rows();
    // Pick the delete action explicitly.
    for row in rows.iter_mut() {
        if row.name == "obsolete" {
            let pos = row
                .actions
                .iter()
                .position(|a| matches!(a.kind, ActionKind::Delete { .. }))
                .expect("a delete action");
            row.chosen = pos;
            row.accepted = true;
        }
    }
    let plan = w.plan(&rows);

    assert!(
        plan.steps.iter().any(|s| matches!(
            &s.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemoveMcp(n)) if n == "obsolete"
        )),
        "the manifest entry must go too"
    );
    assert!(
        plan.steps.iter().any(|s| matches!(
            &s.step,
            Step::Host { host, argv, .. } if host == "claude" && argv.contains(&"remove".to_string())
        )),
        "claude holds it, so it must be removed there"
    );
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

#[test]
fn an_in_sync_row_can_be_removed_but_never_by_accepting_everything() {
    let server = http("shared", "https://example.test/mcp");
    let mut manifest = Manifest::default();
    manifest.mcp.insert(
        "shared".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );
    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[(Scope::User, server)]),
    );
    let mut rows = w.rows();
    let row = find(&rows, "shared");

    assert_eq!(row.severity, Severity::Synced);
    // Removal must be reachable: otherwise the only way to delete something is
    // to break it first.
    assert!(
        row.actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::Delete { .. })),
        "{:?}",
        row.actions.iter().map(|a| &a.label).collect::<Vec<_>>()
    );
    // ...but the default must be inert, so `A` / `--yes` cannot delete anything.
    assert_eq!(row.action().kind, ActionKind::Nothing);
    assert!(!row.actionable());

    for r in rows.iter_mut() {
        r.accepted = r.actionable();
    }
    assert!(
        w.plan(&rows).is_empty(),
        "accepting every default must not remove an in-sync entry"
    );
}

#[test]
fn a_manifest_entry_on_no_host_can_still_be_dropped() {
    // Nothing to uninstall, but the entry itself has to be removable — otherwise
    // a server that never installed anywhere is reported forever with no way out.
    let mut manifest = Manifest::default();
    let server = http("linear", "https://mcp.linear.app/mcp");
    manifest.mcp.insert(
        "linear".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );
    let w = world(manifest, snapshot("claude", &[]), snapshot("codex", &[]));

    let mut rows = w.rows();
    let row = find(&rows, "linear");
    assert_eq!(row.headline, "missing from claude and codex");
    // "keep nothing-only" is not a thing.
    assert!(
        !row.actions.iter().any(|a| a.label.contains("nothing-only")),
        "{:?}",
        row.actions.iter().map(|a| &a.label).collect::<Vec<_>>()
    );

    let pos = row
        .actions
        .iter()
        .position(|a| a.label.contains("drop it from the manifest"))
        .unwrap_or_else(|| {
            panic!(
                "expected a manifest-drop action in {:?}",
                row.actions.iter().map(|a| &a.label).collect::<Vec<_>>()
            )
        });
    for r in rows.iter_mut() {
        if r.name == "linear" {
            r.chosen = pos;
            r.accepted = true;
        }
    }
    let plan = w.plan(&rows);
    assert!(
        plan.steps.iter().any(|s| matches!(
            &s.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemoveMcp(n)) if n == "linear"
        )),
        "{:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(&s.step, Step::Host { .. })),
        "there is nothing installed, so no host command should run"
    );
}

#[test]
fn removing_from_one_host_narrows_the_manifest_instead_of_dropping_it() {
    let server = http("shared", "https://example.test/mcp");
    let mut manifest = Manifest::default();
    manifest.mcp.insert(
        "shared".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );
    let w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server.clone())]),
        snapshot("codex", &[(Scope::User, server)]),
    );

    let mut rows = w.rows();
    for row in rows.iter_mut() {
        if row.name != "shared" {
            continue;
        }
        let pos = row
            .actions
            .iter()
            .position(|a| a.label.contains("codex only"))
            .unwrap_or_else(|| {
                panic!(
                    "expected a codex-only removal in {:?}",
                    row.actions.iter().map(|a| &a.label).collect::<Vec<_>>()
                )
            });
        row.chosen = pos;
        row.accepted = true;
    }
    let plan = w.plan(&rows);

    assert!(
        plan.steps.iter().any(|s| matches!(&s.step,
            Step::Host { host, argv, .. } if host == "codex" && argv.contains(&"remove".to_string()))),
        "codex must actually have it removed"
    );
    // Leaving the manifest wanting it everywhere would report it as missing on
    // the very next run and offer to put it straight back.
    let narrowed = plan.steps.iter().find_map(|s| match &s.step {
        Step::Manifest(agentsync::core::plan::ManifestOp::SetMcpHosts { hosts, .. }) => {
            hosts.clone()
        }
        _ => None,
    });
    assert_eq!(
        narrowed,
        Some(vec!["claude".to_string()]),
        "the manifest must be narrowed to the hosts that keep it: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        !plan.steps.iter().any(|s| matches!(
            &s.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemoveMcp(_))
        )),
        "a partial removal must not drop the entry entirely"
    );
}

#[test]
fn a_skill_removal_says_when_it_destroys_the_only_copy() {
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    // codex owns the content; claude merely links somewhere else.
    codex
        .skills
        .insert("mine".into(), agentsync::core::model::LinkState::Owned);
    claude.skills.insert(
        "mine".into(),
        agentsync::core::model::LinkState::Foreign(PathBuf::from("/elsewhere/mine")),
    );

    let w = world(Manifest::default(), claude, codex);
    let rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "mine" && r.domain == Domain::Skills)
        .expect("a skills row");

    let labels: Vec<&String> = row.actions.iter().map(|a| &a.label).collect();
    assert!(
        labels.iter().any(|l| l.contains("destroys the only copy")),
        "the all-hosts removal must say what it destroys: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("unlink from claude only")),
        "removing a mere link must not be called a delete: {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|l| l.contains("delete the only copy on codex")),
        "removing the content must say so: {labels:?}"
    );
}

// ---------------------------------------------------------------------------
// Instruction files
// ---------------------------------------------------------------------------

use agentsync::core::model::{InstructionFile, LinkState};

fn with_instruction(snap: &mut HostSnapshot, scope: Scope, path: &str, state: LinkState) {
    snap.instructions.insert(
        scope,
        InstructionFile {
            path: PathBuf::from(path),
            state,
        },
    );
}

fn instruction_row<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.name == name && r.domain == Domain::Instructions)
        .unwrap_or_else(|| panic!("no instruction row {name:?} in {:?}", names(rows)))
}

#[test]
fn an_instruction_file_on_one_host_is_adopted_and_linked_into_the_other() {
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    with_instruction(
        &mut claude,
        Scope::User,
        "/home/me/.claude/CLAUDE.md",
        LinkState::Owned,
    );
    with_instruction(
        &mut codex,
        Scope::User,
        "/home/me/.codex/AGENTS.md",
        LinkState::Absent,
    );

    let w = world(Manifest::default(), claude, codex);
    let rows = w.rows();
    let row = instruction_row(&rows, "user");
    assert_eq!(row.headline, "only in claude");
    assert_eq!(row.action().label, "adopt + link into codex");
}

#[test]
fn two_hosts_with_their_own_file_must_be_told_whose_wins() {
    // Picking one silently discards the other's wording, so there is no
    // defensible default: every offer names a host.
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    with_instruction(
        &mut claude,
        Scope::Project("/repos/one".into()),
        "/repos/one/CLAUDE.md",
        LinkState::Owned,
    );
    with_instruction(
        &mut codex,
        Scope::Project("/repos/one".into()),
        "/repos/one/AGENTS.md",
        LinkState::Owned,
    );

    let w = world(Manifest::default(), claude, codex);
    let rows = w.rows();
    let row = instruction_row(&rows, "repos-one");

    assert_eq!(row.severity, Severity::Warn);
    assert!(
        row.headline.contains("each have their own"),
        "{}",
        row.headline
    );
    let labels: Vec<&String> = row.actions.iter().map(|a| &a.label).collect();
    assert!(
        labels.iter().any(|l| l.contains("adopt claude's version")),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("adopt codex's version")),
        "{labels:?}"
    );
    assert!(
        !labels
            .iter()
            .any(|l| l.as_str() == "adopt into the manifest"),
        "an ambiguous default would silently pick one: {labels:?}"
    );
}

#[test]
fn a_scope_a_host_cannot_hold_is_reported_not_invented() {
    // Codex has no counterpart to CLAUDE.local.md, so its descriptor declares no
    // `local` path and the row must say so rather than fabricate one.
    let mut claude = snapshot("claude", &[]);
    let codex = snapshot("codex", &[]);
    with_instruction(
        &mut claude,
        Scope::Local("/repos/one".into()),
        "/repos/one/CLAUDE.local.md",
        LinkState::Owned,
    );

    let w = world(Manifest::default(), claude, codex);
    let rows = w.rows();
    let row = instruction_row(&rows, "repos-one.local");
    assert!(
        row.detail.contains("codex has no location for local scope"),
        "detail was {:?}",
        row.detail
    );

    // And nothing must be planned for codex.
    let mut rows = rows;
    for r in rows.iter_mut() {
        r.accepted = r.name == "repos-one.local" && r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        !plan.steps.iter().any(|s| matches!(&s.step,
            Step::Fs(agentsync::core::plan::FsOp::Link { link, .. })
                if link.to_string_lossy().contains("AGENTS"))),
        "codex has nowhere to link it: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Plugins: marketplace resolution
// ---------------------------------------------------------------------------

fn with_plugins(snap: &mut HostSnapshot, installed: &[(&str, &str)], catalog: &[(&str, &[&str])]) {
    for (name, market) in installed {
        snap.plugins.insert(
            name.to_string(),
            agentsync::core::model::InstalledPlugin {
                name: name.to_string(),
                marketplace: market.to_string(),
            },
        );
    }
    for (market, plugins) in catalog {
        snap.catalog.insert(
            market.to_string(),
            plugins.iter().map(|p| p.to_string()).collect(),
        );
    }
}

fn plugin_row<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.name == name && r.domain == Domain::Plugins)
        .unwrap_or_else(|| panic!("no plugin row named {name:?} in {:?}", names(rows)))
}

#[test]
fn a_plugin_no_other_host_offers_is_blocked_not_installed() {
    // `atlassian-rovo` exists only in Codex's curated registry. Offering to
    // "install in the others" made `claude plugin install` fail with
    // "not found in any configured marketplace".
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    with_plugins(
        &mut claude,
        &[],
        &[("claude-plugins-official", &["superpowers"])],
    );
    with_plugins(
        &mut codex,
        &[("atlassian-rovo", "openai-curated")],
        &[("openai-curated", &["atlassian-rovo"])],
    );

    let w = world(Manifest::default(), claude, codex);
    let mut rows = w.rows();
    let row = plugin_row(&rows, "atlassian-rovo");

    assert_eq!(row.severity, Severity::Blocked);
    assert!(
        row.headline.contains("no other host offers it"),
        "headline was {:?}",
        row.headline
    );

    // Accepting the default records the divergence and installs nothing.
    for r in rows.iter_mut() {
        r.accepted = r.name == "atlassian-rovo" && r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        !plan.steps.iter().any(|s| matches!(&s.step,
            Step::Host { argv, .. } if argv.iter().any(|a| a == "install" || a == "add"))),
        "nothing installable, so no install may be attempted: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn installs_always_carry_the_marketplace() {
    // `codex plugin add superpowers` exits 1: "requires --marketplace unless
    // passed as <plugin>@<marketplace>". A bare id is never correct.
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    with_plugins(
        &mut claude,
        &[("hookify", "claude-plugins-official")],
        &[("claude-plugins-official", &["hookify"])],
    );
    with_plugins(
        &mut codex,
        &[],
        &[("claude-plugins-official", &["hookify"])],
    );

    let w = world(Manifest::default(), claude, codex);
    let mut rows = w.rows();
    for r in rows.iter_mut() {
        r.accepted = r.name == "hookify" && r.actionable();
    }
    let plan = w.plan(&rows);

    let install = plan
        .steps
        .iter()
        .find_map(|s| match &s.step {
            Step::Host { host, argv, .. } if host == "codex" => Some(argv.clone()),
            _ => None,
        })
        .expect("an install on codex");
    assert_eq!(
        install.last().unwrap(),
        "hookify@claude-plugins-official",
        "the id must be fully qualified: {install:?}"
    );
}

#[test]
fn an_ambiguous_plugin_asks_for_a_pin_instead_of_guessing() {
    let mut claude = snapshot("claude", &[]);
    let mut codex = snapshot("codex", &[]);
    with_plugins(
        &mut claude,
        &[("superpowers", "claude-plugins-official")],
        &[("claude-plugins-official", &["superpowers"])],
    );
    // Three of codex's marketplaces carry the same name.
    with_plugins(
        &mut codex,
        &[],
        &[
            ("claude-plugins-official", &["superpowers"]),
            ("openai-api-curated", &["superpowers"]),
            ("openai-curated", &["superpowers"]),
        ],
    );

    let mut manifest = Manifest::default();
    manifest
        .plugins
        .insert("superpowers".into(), Default::default());

    let w = world(manifest, claude, codex);
    let mut rows = w.rows();
    let row = plugin_row(&rows, "superpowers");
    assert_eq!(row.severity, Severity::Warn);
    assert!(
        row.headline.contains("3 marketplaces"),
        "headline was {:?}",
        row.headline
    );
    assert!(matches!(
        row.action().kind,
        ActionKind::PinMarketplace { .. }
    ));

    for r in rows.iter_mut() {
        r.accepted = r.name == "superpowers" && r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        plan.steps.iter().any(|s| matches!(&s.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::UpsertPlugin {
                marketplace: Some(m), ..
            }) if m == "claude-plugins-official")),
        "pinning must be recorded in the manifest: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn an_uninstalled_host_produces_no_rows_and_no_steps() {
    let server = stdio("kicad", "node");
    let mut manifest = Manifest::default();
    manifest.mcp.insert(
        "kicad".into(),
        McpEntry::from_server(&server, ScopeKind::User, vec![]),
    );

    let mut w = world(
        manifest,
        snapshot("claude", &[(Scope::User, server)]),
        snapshot("codex", &[]),
    );
    // Codex is not on PATH.
    w.hosts[1].bin = None;
    w.snapshots[1].detected = false;

    let rows = w.rows();
    let row = find(&rows, "kicad");
    assert_eq!(
        row.severity,
        Severity::Synced,
        "an absent host is not a divergence: {}",
        row.headline
    );
}

// ---------------------------------------------------------------------------
// OpenCode-family npm/local plugin targets
// ---------------------------------------------------------------------------

use agentsync::core::model::PluginOccurrence;
use agentsync::manifest::{PluginEntry, PluginTarget};
use agentsync::transaction::{ConfigOrigin, ConfigScope};

fn oc_world(manifest: Manifest, opencode: HostSnapshot, kilo: HostSnapshot) -> World {
    World {
        manifest,
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![host("opencode"), host("kilo")],
        snapshots: vec![opencode, kilo],
        repos: vec![],
        warnings: Vec::new(),
    }
}

fn npm_target(spec: &str, scope: ScopeKind) -> PluginTarget {
    PluginTarget {
        npm: Some(spec.to_string()),
        local: None,
        scope,
    }
}

fn local_target(source: &str, scope: ScopeKind) -> PluginTarget {
    PluginTarget {
        npm: None,
        local: Some(source.to_string()),
        scope,
    }
}

fn plugin_entry_with_target(host: &str, target: PluginTarget) -> PluginEntry {
    PluginEntry {
        marketplace: None,
        hosts: None,
        targets: BTreeMap::from([(host.to_string(), target)]),
    }
}

#[test]
fn opencode_plugins_a_declared_npm_target_missing_from_the_host_is_reported_and_plans_an_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg/opencode");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("opencode.jsonc");
    std::fs::write(&cfg_path, "{}\n").unwrap();

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "security-guidance".into(),
        plugin_entry_with_target(
            "opencode",
            npm_target("@company/opencode-security@1.4.2", ScopeKind::User),
        ),
    );

    let mut opencode = snapshot("opencode", &[]);
    let hash = agentsync::transaction::compute_sha256(b"{}\n");
    opencode.plugin_targets.config.insert(
        ScopeKind::User,
        agentsync::core::model::PluginConfigSource {
            origin: ConfigOrigin::new(&cfg_path, ConfigScope::Global, 70, hash),
            entries: Vec::new(),
        },
    );
    let kilo = snapshot("kilo", &[]);
    let w = oc_world(manifest, opencode, kilo);
    // Nothing in the opencode config yet, so the target is missing.

    let mut rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "security-guidance" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert_eq!(row.severity, Severity::Normal);
    assert!(row.headline.contains("missing"), "{}", row.headline);

    for r in rows.iter_mut() {
        r.accepted = r.name == "security-guidance" && r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::ConfigTransaction(_))),
        "a missing npm target must plan a config transaction: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn opencode_plugins_a_present_npm_target_is_in_sync() {
    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "security-guidance".into(),
        plugin_entry_with_target(
            "opencode",
            npm_target("@company/opencode-security@1.4.2", ScopeKind::User),
        ),
    );

    let mut opencode = snapshot("opencode", &[]);
    opencode.plugin_targets.occurrences.insert(
        "@company/opencode-security@1.4.2".to_string(),
        vec![PluginOccurrence::Config(ConfigOrigin::new(
            "/cfg/opencode/opencode.jsonc",
            ConfigScope::Global,
            70,
            "deadbeef",
        ))],
    );
    let kilo = snapshot("kilo", &[]);
    let w = oc_world(manifest, opencode, kilo);

    let rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "security-guidance" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert_eq!(row.severity, Severity::Synced, "{}", row.headline);
}

#[test]
fn opencode_plugins_duplicate_occurrences_are_reported_not_collapsed() {
    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "security-guidance".into(),
        plugin_entry_with_target(
            "opencode",
            npm_target("@company/opencode-security@1.4.2", ScopeKind::User),
        ),
    );

    let mut opencode = snapshot("opencode", &[]);
    opencode.plugin_targets.occurrences.insert(
        "@company/opencode-security@1.4.2".to_string(),
        vec![
            PluginOccurrence::Config(ConfigOrigin::new(
                "/cfg/opencode/opencode.jsonc",
                ConfigScope::Global,
                70,
                "global-hash",
            )),
            PluginOccurrence::Config(ConfigOrigin::new(
                "/repo/.opencode/opencode.jsonc",
                ConfigScope::Project,
                20,
                "project-hash",
            )),
        ],
    );
    let kilo = snapshot("kilo", &[]);
    let w = oc_world(manifest, opencode, kilo);

    let rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "security-guidance" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert_eq!(row.severity, Severity::Warn);
    assert!(row.headline.contains("duplicate"), "{}", row.headline);
}

#[test]
fn kilo_plugins_a_declared_local_target_missing_from_the_host_is_reported_and_plans_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(
        tmp.path().join("plugins/local-policy.ts"),
        "export async function AgentsyncHooks() {}\n",
    )
    .unwrap();

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "local-policy".into(),
        plugin_entry_with_target(
            "kilo",
            local_target("plugins/local-policy.ts", ScopeKind::Project),
        ),
    );

    let mut kilo = snapshot("kilo", &[]);
    kilo.plugin_targets
        .profile_dir
        .insert(ScopeKind::Project, tmp.path().join("repo/.kilo"));
    let opencode = snapshot("opencode", &[]);
    let mut w = oc_world(manifest, opencode, kilo);
    w.manifest_path = tmp.path().join(".agentsync.toml");

    let mut rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "local-policy" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert!(row.headline.contains("missing"), "{}", row.headline);

    for r in rows.iter_mut() {
        r.accepted = r.name == "local-policy" && r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "a missing local target must plan a file copy: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn kilo_plugins_an_existing_unowned_local_directory_blocks_rather_than_claims_it() {
    // A directory the user already has, with their own file inside and no
    // agentsync ownership marker, must never be silently claimed. The row
    // must report it as blocked, and even "accept everything" must produce
    // no mutation for it.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
    std::fs::write(
        tmp.path().join("plugins/local-policy.ts"),
        "export async function AgentsyncHooks() {}\n",
    )
    .unwrap();
    let dest_dir = tmp.path().join("repo/.kilo/plugin");
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::write(dest_dir.join("users-own-plugin.ts"), b"user content").unwrap();

    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "local-policy".into(),
        plugin_entry_with_target(
            "kilo",
            local_target("plugins/local-policy.ts", ScopeKind::Project),
        ),
    );

    let mut kilo = snapshot("kilo", &[]);
    kilo.plugin_targets
        .profile_dir
        .insert(ScopeKind::Project, tmp.path().join("repo/.kilo"));
    // This is exactly what a real read would find: the directory exists and
    // carries no marker, so it is not claimable.
    kilo.plugin_targets
        .local_dir_claimable
        .insert(ScopeKind::Project, false);
    let opencode = snapshot("opencode", &[]);
    let mut w = oc_world(manifest, opencode, kilo);
    w.manifest_path = tmp.path().join(".agentsync.toml");

    let mut rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "local-policy" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert_eq!(row.severity, Severity::Blocked, "{}", row.headline);
    assert!(
        row.headline.contains("unowned"),
        "headline was {:?}",
        row.headline
    );
    assert!(
        !row.actionable(),
        "a blocked row must offer nothing to accept"
    );

    for r in rows.iter_mut() {
        r.accepted = r.actionable();
    }
    let plan = w.plan(&rows);
    assert!(
        !plan.steps.iter().any(|s| matches!(
            &s.step,
            Step::FileTransaction(_) | Step::ConfigTransaction(_)
        )),
        "accepting everything must not plant a marker or write into an unowned directory: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        !dest_dir.join(".agentsync-owned").exists(),
        "no marker may appear in a pre-existing, unowned directory"
    );
    assert_eq!(
        std::fs::read(dest_dir.join("users-own-plugin.ts")).unwrap(),
        b"user content",
        "the user's own file must be completely untouched"
    );
}

#[test]
fn kilo_plugins_npm_and_local_identities_never_collide() {
    // An npm spec and a local host-owned destination live in different
    // identity namespaces, even when their text happens to coincide.
    let mut manifest = Manifest::default();
    manifest.plugins.insert(
        "same-name".into(),
        plugin_entry_with_target("kilo", npm_target("same-name", ScopeKind::User)),
    );

    let mut kilo = snapshot("kilo", &[]);
    // A *local* occurrence recorded under the destination file-stem identity
    // must not satisfy the npm target above, which is looked up by its own
    // spec text.
    kilo.plugin_targets.occurrences.insert(
        "agentsync-same-name".to_string(),
        vec![PluginOccurrence::File {
            path: PathBuf::from("/profile/plugin/agentsync-same-name.ts"),
            sha256: "abc".into(),
            scope: ScopeKind::User,
        }],
    );
    let opencode = snapshot("opencode", &[]);
    let w = oc_world(manifest, opencode, kilo);

    let rows = w.rows();
    let row = rows
        .iter()
        .find(|r| r.name == "same-name" && r.domain == Domain::Plugins)
        .expect("target row present");
    assert!(
        row.headline.contains("missing"),
        "the local occurrence must not satisfy the distinct npm identity: {}",
        row.headline
    );
}

// ---------------------------------------------------------------------------
// Hooks domain
// ---------------------------------------------------------------------------

fn hook_id(source: &str, event: &str) -> HookId {
    HookId {
        source: source.to_string(),
        event: event.to_string(),
        group: 0,
        index: 0,
    }
}

fn hooks_snapshot(name: &str, hooks: Vec<HookHandler>) -> HostSnapshot {
    let mut snap = HostSnapshot {
        host: name.to_string(),
        display: name.to_string(),
        detected: true,
        ..Default::default()
    };
    for h in hooks {
        snap.hooks.insert(h.id.clone(), h);
    }
    snap
}

/// A detected host whose descriptor has no `[hooks]` section at all. This is
/// the shape a user descriptor takes when it replaces a builtin wholesale
/// without carrying the hooks table forward.
fn host_without_hooks(name: &str) -> Host {
    let text =
        format!("name = \"{name}\"\ndisplay = \"{name}\"\ndetect = {{ bin = \"{name}\" }}\n");
    Host {
        descriptor: descriptor::parse(&text, name).unwrap(),
        bin: Some(PathBuf::from(format!("/usr/bin/{name}"))),
    }
}

fn hooks_world(hosts: Vec<Host>, snapshots: Vec<HostSnapshot>) -> World {
    World {
        manifest: Manifest::default(),
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts,
        snapshots,
        repos: vec!["/repos/one".to_string()],
        warnings: Vec::new(),
    }
}

fn hook_rows(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| r.domain == Domain::Hooks).collect()
}

fn write_fs_op(op: agentsync::core::plan::FsOp) {
    match op {
        agentsync::core::plan::FsOp::WriteFile { path, contents } => {
            std::fs::create_dir_all(path.parent().expect("generated file parent")).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        other => panic!("the substitution fixture has no vendored links: {other:?}"),
    }
}

fn materialize_valid_shim(world: &World, marketplace_dir: &std::path::Path) {
    let source = world.snapshot("claude").expect("claude snapshot");
    let target = world.host("codex").expect("codex host");
    let declared = target.descriptor.hooks.as_ref().expect("codex hooks");
    let effective = world.manifest.hooks_for("codex", declared);
    let shim = effective.shim.as_ref().expect("codex shim config");
    let handlers: Vec<_> = source.hooks.values().cloned().collect();
    let input = agentsync::shim::generate::ShimInput {
        marketplace_dir: marketplace_dir.to_path_buf(),
        plugin: "security-guidance".into(),
        marketplace: "claude-plugins-official".into(),
        handlers,
        allowed_output: effective
            .output
            .iter()
            .map(|field| field.json_key().to_string())
            .collect(),
        fold_into_system_message: vec!["rewakeMessage".into()],
        output_strategy: shim.output_strategy,
        agentsync_bin: std::env::current_exe().expect("test binary path"),
        target_caps: effective.caps,
        vendor: vec![],
    };
    let generated = agentsync::shim::generate::plan_shim(&input).unwrap();
    let shim_plugin = generated.shim_plugin.clone();
    for op in generated.ops {
        write_fs_op(op);
    }
    write_fs_op(
        agentsync::shim::generate::marketplace_manifest_op(
            marketplace_dir,
            std::slice::from_ref(&shim_plugin),
        )
        .unwrap(),
    );
}

fn shim_substitution_world(
    marketplace_dir: &std::path::Path,
    original_on_codex: bool,
    internal_manifest_entries: bool,
) -> World {
    let plugin = "security-guidance";
    let marketplace = "claude-plugins-official";
    let shim = agentsync::shim::generate::shim_plugin_name(marketplace, plugin);

    let mut hook = HookHandler::new(
        hook_id(
            "security-guidance@claude-plugins-official:hooks/hooks.json",
            "PreToolUse",
        ),
        "PreToolUse",
        "echo hi",
    );
    hook.if_pattern = Some("Bash(git commit:*)".into());

    let mut claude = hooks_snapshot("claude", vec![hook]);
    with_plugins(
        &mut claude,
        &[(plugin, marketplace)],
        &[(marketplace, &[plugin])],
    );

    let mut codex = hooks_snapshot("codex", vec![]);
    with_plugins(
        &mut codex,
        &[(shim.as_str(), "agentsync-shims")],
        &[(marketplace, &[plugin])],
    );
    if original_on_codex {
        with_plugins(&mut codex, &[(plugin, marketplace)], &[]);
    }
    codex.marketplaces.insert(
        "agentsync-shims".into(),
        agentsync::core::model::MarketplaceSource::Directory(
            marketplace_dir.to_string_lossy().into_owned(),
        ),
    );

    let mut manifest = Manifest::default();
    manifest.plugins.insert(plugin.into(), Default::default());
    if internal_manifest_entries {
        manifest.plugins.insert(shim, Default::default());
        manifest.marketplaces.insert(
            "agentsync-shims".into(),
            agentsync::manifest::MarketplaceEntry {
                directory: Some(marketplace_dir.to_string_lossy().into_owned()),
                github: None,
                url: None,
                hosts: None,
            },
        );
    }

    let mut codex_host = host("codex");
    codex_host
        .descriptor
        .hooks
        .as_mut()
        .expect("codex hooks")
        .shim
        .as_mut()
        .expect("codex shim config")
        .marketplace = marketplace_dir.to_string_lossy().into_owned();
    let world = World {
        manifest,
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![host("claude"), codex_host],
        snapshots: vec![claude, codex],
        repos: vec!["/repos/one".to_string()],
        warnings: Vec::new(),
    };
    materialize_valid_shim(&world, marketplace_dir);
    world
}

#[test]
fn shim_substitution_satisfies_the_original_plugin_on_its_target_host() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), false, false);
    let rows = world.rows();
    let original = plugin_row(&rows, "security-guidance");

    assert_eq!(
        original.severity,
        Severity::Synced,
        "the installed shim must satisfy the original on codex: {}",
        original.headline
    );
}

#[test]
fn shim_substitution_removes_an_original_that_is_still_installed() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), true, false);
    let plan = world.plan(&[]);

    assert!(
        plan.steps.iter().any(|step| matches!(&step.step,
            Step::Host { host, argv, .. }
                if host == "codex"
                    && argv.iter().any(|arg| arg == "remove" || arg == "uninstall")
                    && argv.iter().any(|arg| arg.contains("security-guidance"))
        )),
        "finding both copies must remove the original: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
}

#[test]
fn shim_substitution_keeps_a_same_named_plugin_from_another_marketplace() {
    let dir = tempfile::tempdir().unwrap();
    let mut world = shim_substitution_world(dir.path(), true, false);
    world.snapshots[1]
        .plugins
        .get_mut("security-guidance")
        .expect("the same-named plugin is installed")
        .marketplace = "market-b".into();

    let plan = world.plan(&[]);

    assert!(
        !plan.steps.iter().any(|step| matches!(&step.step,
            Step::Host { host, argv, .. }
                if host == "codex"
                    && argv.iter().any(|arg| arg == "remove" || arg == "uninstall")
                    && argv.iter().any(|arg| arg.contains("security-guidance"))
        )),
        "the shim for market-a must not remove the same name from market-b: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
}

#[test]
fn shim_substitution_cleans_internal_entries_out_of_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), false, true);
    let shim =
        agentsync::shim::generate::shim_plugin_name("claude-plugins-official", "security-guidance");
    let plan = world.plan(&[]);

    assert!(
        plan.steps.iter().any(|step| matches!(&step.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemovePlugin(name))
                if name == &shim
        )),
        "the generated plugin is runtime state, not manifest state: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
    assert!(
        plan.steps.iter().any(|step| matches!(&step.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemoveMarketplace(name))
                if name == "agentsync-shims"
        )),
        "the generated marketplace is runtime state, not manifest state: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
}

#[test]
fn shim_substitution_sweeps_stale_internal_manifest_entries_without_runtime_state() {
    let shim = agentsync::shim::generate::shim_plugin_name("market-a", "plugin-with-hyphens");
    let mut manifest = Manifest::default();
    manifest.plugins.insert(shim.clone(), Default::default());
    manifest.marketplaces.insert(
        "agentsync-shims".into(),
        agentsync::manifest::MarketplaceEntry {
            directory: Some("/tmp/agentsync-test/shims/codex".into()),
            github: None,
            url: None,
            hosts: None,
        },
    );
    let world = world(manifest, snapshot("claude", &[]), snapshot("codex", &[]));

    let plan = world.plan(&[]);

    assert!(
        plan.steps.iter().any(|step| matches!(&step.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemovePlugin(name))
                if name == &shim
        )),
        "a stale generated plugin must be swept without a detected shim: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
    assert!(
        plan.steps.iter().any(|step| matches!(&step.step,
            Step::Manifest(agentsync::core::plan::ManifestOp::RemoveMarketplace(name))
                if name == "agentsync-shims"
        )),
        "the stale internal marketplace must be swept without a detected shim: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
    assert!(
        plan.steps
            .iter()
            .all(|step| matches!(&step.step, Step::Manifest(_))),
        "manifest cleanup must not remove installed or on-disk shim state: {:?}",
        plan.steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );
}

#[test]
fn shim_substitution_internal_state_never_becomes_an_adoption_row() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), false, false);
    let rows = world.rows();

    assert!(
        !rows.iter().any(|row| {
            row.domain == Domain::Plugins
                && (row.name.starts_with("agentsync-shim-")
                    || row.name == "marketplace agentsync-shims")
        }),
        "internal shim state must stay out of ordinary plugin reconciliation: {:?}",
        rows.iter()
            .filter(|row| row.domain == Domain::Plugins)
            .map(|row| (&row.name, &row.headline))
            .collect::<Vec<_>>()
    );
}

fn assert_invalid_shim_plans_guarded_regeneration(world: World) {
    assert!(
        agentsync::domains::hooks::shim_substitutions(&world).is_empty(),
        "a broken artifact must not satisfy the source plugin"
    );

    let cleanup = world.plan(&[]);
    assert!(
        !cleanup.steps.iter().any(|step| matches!(&step.step,
            Step::Host { host, argv, .. }
                if host == "codex"
                    && argv.iter().any(|arg| arg == "remove" || arg == "uninstall")
                    && argv.iter().any(|arg| arg.contains("security-guidance"))
        )),
        "an invalid shim must not remove the working original: {:?}",
        cleanup
            .steps
            .iter()
            .map(|step| &step.label)
            .collect::<Vec<_>>()
    );

    let mut rows = world.rows();
    let row = rows
        .iter_mut()
        .find(|row| row.domain == Domain::Hooks && row.actionable())
        .expect("the broken shim must plan regeneration under the existing hook gap");
    row.accepted = true;
    let plan = world.plan(&rows);
    let install = plan
        .steps
        .iter()
        .find(|step| {
            matches!(&step.step,
                Step::Host { argv, .. }
                    if !argv.iter().any(|arg| arg == "marketplace")
                        && argv.iter().any(|arg| arg.starts_with("agentsync-shim-"))
            )
        })
        .expect("regeneration must reinstall the shim");
    let remove = plan
        .steps
        .iter()
        .find(|step| {
            matches!(&step.step,
                Step::Host { argv, .. }
                    if argv.iter().any(|arg| arg == "remove" || arg == "uninstall")
            )
        })
        .expect("the original is removed only after regeneration succeeds");
    assert!(install.guard.is_some());
    assert_eq!(
        install.guard, remove.guard,
        "install and removal must share the guard"
    );
}

#[test]
fn a_shim_registered_from_the_wrong_host_path_does_not_substitute() {
    let dir = tempfile::tempdir().unwrap();
    let mut world = shim_substitution_world(dir.path(), true, false);
    world.snapshots[1].marketplaces.insert(
        "agentsync-shims".into(),
        agentsync::core::model::MarketplaceSource::Directory(
            dir.path()
                .join("wrong-host-path")
                .to_string_lossy()
                .into_owned(),
        ),
    );

    assert_invalid_shim_plans_guarded_regeneration(world);
}

#[test]
fn a_shim_with_changed_handler_content_does_not_substitute() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), true, false);
    let sidecar = dir.path().join(
        "agentsync-shim-claude-plugins-official-security-guidance/specs/pre_tool_use-0-0.json",
    );
    let changed = std::fs::read_to_string(&sidecar)
        .unwrap()
        .replace("echo hi", "echo stale review");
    std::fs::write(sidecar, changed).unwrap();

    assert_invalid_shim_plans_guarded_regeneration(world);
}

#[test]
fn a_shim_missing_a_required_sidecar_does_not_substitute() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), true, false);
    std::fs::remove_file(dir.path().join(
        "agentsync-shim-claude-plugins-official-security-guidance/specs/pre_tool_use-0-0.json",
    ))
    .unwrap();

    assert_invalid_shim_plans_guarded_regeneration(world);
}

#[test]
fn a_shim_recording_a_stale_agentsync_binary_does_not_substitute() {
    let dir = tempfile::tempdir().unwrap();
    let world = shim_substitution_world(dir.path(), true, false);
    let hooks = dir
        .path()
        .join("agentsync-shim-claude-plugins-official-security-guidance/hooks/hooks.json");
    let current = std::env::current_exe().unwrap();
    let stale = std::fs::read_to_string(&hooks)
        .unwrap()
        .replace(&current.to_string_lossy().into_owned(), "/stale/agentsync");
    std::fs::write(hooks, stale).unwrap();

    assert_invalid_shim_plans_guarded_regeneration(world);
}

#[test]
fn an_if_only_gap_from_claude_to_codex_is_a_single_normal_row() {
    // Codex declares `caps` without `if`, but does declare a shim target, so
    // the gap is exactly the "actionable" case: shimmable and hostable.
    let mut h = HookHandler::new(
        hook_id("claude-settings", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );
    h.if_pattern = Some("Bash(git commit:*)".into());

    let w = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert_eq!(
        hook_rows.len(),
        1,
        "{:?}",
        hook_rows.iter().map(|r| &r.headline).collect::<Vec<_>>()
    );
    assert_eq!(hook_rows[0].severity, Severity::Normal);
}

#[test]
fn an_event_the_target_cannot_express_is_blocked_and_names_the_event() {
    // Codex has no `PreCompact` event at all; claude does.
    let h = HookHandler::new(
        hook_id("claude-settings", "PreCompact"),
        "PreCompact",
        "echo bye",
    );

    let w = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert_eq!(hook_rows.len(), 1);
    assert_eq!(hook_rows[0].severity, Severity::Blocked);
    assert!(
        hook_rows[0].headline.contains("PreCompact"),
        "{}",
        hook_rows[0].headline
    );
}

#[test]
fn a_handler_with_no_gaps_produces_no_row() {
    // Plain handler: no matcher, if, timeout, or rewake fields, so
    // `required_caps()` is empty and both hosts support `PreToolUse`.
    let h = HookHandler::new(
        hook_id("claude-settings", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );

    let w = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert!(
        hook_rows.is_empty(),
        "{:?}",
        hook_rows.iter().map(|r| &r.headline).collect::<Vec<_>>()
    );
}

#[test]
fn a_target_with_no_hooks_section_is_blocked_not_silently_skipped() {
    // Regression for the case where a descriptor with `hooks = None` produced
    // zero rows — output byte-identical to full compatibility.
    let h = HookHandler::new(
        hook_id("claude-settings", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );
    let bare = host_without_hooks("bare");

    let w = hooks_world(
        vec![host("claude"), bare],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("bare", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert_eq!(hook_rows.len(), 1);
    assert_eq!(hook_rows[0].severity, Severity::Blocked);
    assert!(
        hook_rows[0].headline.contains("no hook engine"),
        "{}",
        hook_rows[0].headline
    );
}

#[test]
fn hook_fidelity_only_exact_events_are_severity_normal() {
    use agentsync::core::model::HookFidelity;
    use agentsync::domains::hooks::severity_for_fidelity;

    assert_eq!(severity_for_fidelity(HookFidelity::Exact), Severity::Normal);
}

#[test]
fn hook_fidelity_side_effect_only_and_best_effort_events_require_explicit_acceptance() {
    // OW-007: a bridged event that is only a side effect, or whose 1:1
    // behaviour was never confirmed, must never be applied by the same
    // silent default an exact row gets. It must come back as a warning, and
    // a freshly built row always starts unaccepted — so the planner (see
    // `agentsync::domains::plan`, which only ever plans `r.accepted &&
    // r.actionable()` rows) cannot apply it until a user explicitly accepts.
    use agentsync::core::model::HookFidelity;
    use agentsync::domains::hooks::severity_for_fidelity;

    for fidelity in [HookFidelity::SideEffectOnly, HookFidelity::BestEffort] {
        let severity = severity_for_fidelity(fidelity);
        assert_eq!(severity, Severity::Warn, "{fidelity:?} must be a warning");

        let row = Row {
            domain: Domain::Hooks,
            name: "demo".into(),
            headline: "bridged with reduced fidelity".into(),
            detail: format!("{fidelity:?}"),
            severity,
            actions: vec![agentsync::core::diff::Action::new(
                "bridge it anyway",
                ActionKind::Push {
                    hosts: vec!["opencode".into()],
                },
            )],
            chosen: 0,
            accepted: false,
            key: Default::default(),
        };
        assert!(
            !row.accepted,
            "a fresh row must never start pre-accepted: {fidelity:?}"
        );
    }
}

#[test]
fn hook_fidelity_unmeasured_callbacks_never_get_a_fidelity_to_apply() {
    use agentsync::core::model::opencode_family_hook_fidelity;

    // config/auth/event were measured to fire with no output channel a
    // bridged action could travel through. Never claim delivery for them.
    for callback in ["config", "auth", "event", "something.never.measured"] {
        assert_eq!(
            opencode_family_hook_fidelity(callback),
            None,
            "{callback} must not be assigned a fidelity to build a row from"
        );
    }
}

#[test]
fn unmodelled_fields_are_reported_even_with_no_other_gap() {
    // Regression for `unknown_fields` being collected but never surfaced: a
    // handler with a field agentsync does not model must never look portable.
    let mut h = HookHandler::new(
        hook_id("claude-settings", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );
    h.unknown_fields.insert("futureThing".into());

    let w = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert_eq!(hook_rows.len(), 1);
    assert_eq!(hook_rows[0].severity, Severity::Blocked);
    assert!(
        hook_rows[0].headline.contains("futureThing")
            || hook_rows[0].detail.contains("futureThing"),
        "{}: {}",
        hook_rows[0].headline,
        hook_rows[0].detail
    );
}

#[test]
fn unmodelled_fields_are_folded_into_an_existing_gap_row_not_a_second_row() {
    // There is exactly one row per name per domain. An unmodelled field found
    // alongside a real capability gap must append to that row's detail,
    // rather than emit a second row for the same handler/target pair.
    let mut h = HookHandler::new(
        hook_id("claude-settings", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );
    h.if_pattern = Some("Bash(git commit:*)".into());
    h.unknown_fields.insert("futureThing".into());

    let w = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = w.rows();
    let hook_rows = hook_rows(&rows);
    assert_eq!(
        hook_rows.len(),
        1,
        "{:?}",
        hook_rows.iter().map(|r| &r.headline).collect::<Vec<_>>()
    );
    // The unmodelled field carries strictly more unknown risk than the `if`
    // gap it was folded into. So the merged row cannot end up at a lighter
    // severity than an unmodelled field alone would produce.
    assert_eq!(hook_rows[0].severity, Severity::Blocked);
    assert!(
        hook_rows[0].detail.contains("futureThing"),
        "{}",
        hook_rows[0].detail
    );
}

#[test]
fn a_shimmable_gap_offers_to_generate_a_shim_and_planning_it_emits_real_steps() {
    // `if` is shimmable, and codex declares `[hooks.shim]`, so this gap is the
    // "actionable" case: shimmable and hostable.
    let mut h = HookHandler::new(
        hook_id(
            "security-guidance@claude-plugins-official:hooks/hooks.json",
            "PreToolUse",
        ),
        "PreToolUse",
        "echo hi",
    );
    h.if_pattern = Some("Bash(git commit:*)".into());

    let mut codex_snap = hooks_snapshot("codex", vec![]);
    // The original plugin is already installed on the target, so the plan must
    // also remove it once the shim replaces it.
    codex_snap.plugins.insert(
        "security-guidance".to_string(),
        agentsync::core::model::InstalledPlugin {
            name: "security-guidance".to_string(),
            marketplace: "claude-plugins-official".to_string(),
        },
    );

    let world = hooks_world(
        vec![host("claude"), host("codex")],
        vec![hooks_snapshot("claude", vec![h]), codex_snap],
    );

    let mut rows = world.rows();
    let row = rows
        .iter_mut()
        .find(|r| r.domain == Domain::Hooks && r.severity == Severity::Normal)
        .expect("a shimmable gap");

    assert!(
        row.actionable(),
        "a gap a shim can close must offer to close it, got {:?}",
        row.action().kind
    );
    row.accepted = true;

    let plan = world.plan(&rows);
    let labels: Vec<&str> = plan.steps.iter().map(|s| s.label.as_str()).collect();
    assert!(!plan.steps.is_empty(), "accepting must produce steps");

    let write_at = plan
        .steps
        .iter()
        .position(|s| matches!(s.step, Step::Fs(FsOp::WriteFile { .. })))
        .expect("the shim content must be written");
    // `argv.contains("add")` alone is ambiguous: the marketplace-add step
    // (`["plugin","marketplace","add",dir]`) matches it too, and comes before
    // the actual install regardless of whether the install is ordered ahead of
    // the removal. Keying on the generated shim's own name, while excluding
    // the marketplace step, matches the install unambiguously on every host.
    let install_at = plan
        .steps
        .iter()
        .position(|s| {
            matches!(&s.step, Step::Host { argv, .. }
                if !argv.iter().any(|a| a == "marketplace")
                    && argv.iter().any(|a| a.starts_with("agentsync-shim-")))
        })
        .expect("the shim must be installed");
    let remove_at = plan
        .steps
        .iter()
        .position(|s| {
            matches!(&s.step, Step::Host { argv, .. }
                if argv.iter().any(|a| a == "remove" || a == "uninstall"))
        })
        .expect("the original must be removed");

    assert!(write_at < install_at, "content before install: {labels:?}");
    assert!(
        install_at < remove_at,
        "install BEFORE remove: a failed removal leaves a duplicate hook, \
         which is visible. The other order fails into no security review at \
         all, which reads as health. Got {labels:?}"
    );

    // Ordering alone does not stop a failed install from being followed by
    // the removal: only the guard does. Both steps must carry one, and it
    // must be the SAME key, or a failed install cannot skip the removal.
    let install_guard = plan.steps[install_at].guard.clone();
    let remove_guard = plan.steps[remove_at].guard.clone();
    assert!(
        install_guard.is_some(),
        "the install step must carry a guard key: {labels:?}"
    );
    assert_eq!(
        install_guard, remove_guard,
        "the install and the removal must share the same guard key, or a \
         failed install cannot skip the removal that depends on it"
    );
}

#[test]
fn accepting_two_shimmable_plugins_writes_the_marketplace_manifest_only_once() {
    // The manifest lists every shim plugin at once, and applying it is a
    // whole-file write. Writing it per row would leave only the last row's
    // plugin registered, so this pins the fix from the plan side too.
    let mut h1 = HookHandler::new(
        hook_id(
            "security-guidance@claude-plugins-official:hooks/hooks.json",
            "PreToolUse",
        ),
        "PreToolUse",
        "echo hi",
    );
    h1.if_pattern = Some("Bash(git commit:*)".into());

    let mut h2 = HookHandler::new(
        hook_id(
            "other-plugin@claude-plugins-official:hooks/hooks.json",
            "PreToolUse",
        ),
        "PreToolUse",
        "echo bye",
    );
    h2.if_pattern = Some("Bash(git push:*)".into());

    let world = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h1, h2]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let mut rows = world.rows();
    for row in rows
        .iter_mut()
        .filter(|r| r.domain == Domain::Hooks && r.severity == Severity::Normal)
    {
        row.accepted = true;
    }

    let plan = world.plan(&rows);
    let manifest_writes: Vec<&String> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.step {
            Step::Fs(FsOp::WriteFile { path, contents })
                if path
                    .to_string_lossy()
                    .ends_with(".claude-plugin/marketplace.json") =>
            {
                Some(contents)
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        manifest_writes.len(),
        1,
        "the manifest must be written exactly once, or the last write wins \
         and silently unregisters the earlier plugin: {:?}",
        manifest_writes
    );
    let manifest: serde_json::Value = serde_json::from_str(manifest_writes[0]).unwrap();
    let names: Vec<&str> = manifest["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("security-guidance")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("other-plugin")),
        "{names:?}"
    );
}

#[test]
fn a_plugin_with_two_shimmable_handlers_plans_the_shim_only_once() {
    // `rows()` emits one row per handler, and a plugin with two shimmable
    // handlers (for example a PreToolUse and a PostToolUse handler, the
    // ordinary case) produces two rows. Planning must not repeat the shim's
    // install and the original's removal once per row: the second of each
    // pair would fail at apply time.
    let mut h1 = HookHandler::new(
        hook_id(
            "security-guidance@claude-plugins-official:hooks/hooks.json",
            "PreToolUse",
        ),
        "PreToolUse",
        "echo hi",
    );
    h1.if_pattern = Some("Bash(git commit:*)".into());

    let mut h2 = HookHandler::new(
        hook_id(
            "security-guidance@claude-plugins-official:hooks/hooks.json",
            "PostToolUse",
        ),
        "PostToolUse",
        "echo bye",
    );
    h2.if_pattern = Some("Bash(git push:*)".into());

    let mut codex_snap = hooks_snapshot("codex", vec![]);
    codex_snap.plugins.insert(
        "security-guidance".to_string(),
        agentsync::core::model::InstalledPlugin {
            name: "security-guidance".to_string(),
            marketplace: "claude-plugins-official".to_string(),
        },
    );

    let world = hooks_world(
        vec![host("claude"), host("codex")],
        vec![hooks_snapshot("claude", vec![h1, h2]), codex_snap],
    );

    let mut rows = world.rows();
    let shimmable: Vec<_> = rows
        .iter_mut()
        .filter(|r| r.domain == Domain::Hooks && r.severity == Severity::Normal)
        .collect();
    assert_eq!(shimmable.len(), 2, "one row per handler");
    for row in shimmable {
        row.accepted = true;
    }

    let plan = world.plan(&rows);
    let labels: Vec<&str> = plan.steps.iter().map(|s| s.label.as_str()).collect();

    let installs = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(&s.step, Step::Host { argv, .. }
                if !argv.iter().any(|a| a == "marketplace")
                    && argv.iter().any(|a| a.starts_with("agentsync-shim-")))
        })
        .count();
    let removes = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(&s.step, Step::Host { argv, .. }
                if argv.iter().any(|a| a == "remove" || a == "uninstall"))
        })
        .count();

    assert_eq!(
        installs, 1,
        "the shim must be installed exactly once: {labels:?}"
    );
    assert_eq!(
        removes, 1,
        "the original must be removed exactly once: {labels:?}"
    );
}

#[test]
fn a_settings_file_path_containing_an_at_sign_is_not_offered_a_shim() {
    // A directory-joined macOS machine can have a home directory like
    // /Users/logan@corp.com, which is an ordinary settings-file path, not a
    // <plugin>@<marketplace> source. A plugin id never contains a path
    // separator; this does, so the split must be rejected rather than
    // manufacturing a plugin name out of half a path.
    let mut h = HookHandler::new(
        hook_id("/Users/logan@corp.com/.claude/settings.json", "PreToolUse"),
        "PreToolUse",
        "echo hi",
    );
    h.if_pattern = Some("Bash(git commit:*)".into());

    let world = hooks_world(
        vec![host("claude"), host("codex")],
        vec![
            hooks_snapshot("claude", vec![h]),
            hooks_snapshot("codex", vec![]),
        ],
    );

    let rows = world.rows();
    let row = rows
        .iter()
        .find(|r| r.domain == Domain::Hooks && r.severity == Severity::Normal)
        .expect("still a shimmable-severity gap: only the action must change");

    assert!(
        !row.actionable(),
        "a source that only looks like <plugin>@<marketplace> must not \
         advertise a fix it cannot deliver: {:?}",
        row.action().kind
    );
}

// ---------------------------------------------------------------------------
// Instructions: OpenCode and Kilo
// ---------------------------------------------------------------------------

#[test]
fn opencode_instructions() {
    // OpenCode instructions must resolve to XDG-rooted paths via {xdg_config},
    // never hardcoded HOME paths. Verify the descriptor template and expansion.
    let text = agentsync::hosts::descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "opencode")
        .expect("opencode builtin")
        .1;
    let desc = agentsync::hosts::descriptor::parse(text, "opencode").unwrap();
    let instructions = desc.instructions.expect("opencode has instructions");

    // User scope must use {xdg_config} placeholder, not hardcoded ~/.
    let user_path = instructions
        .user
        .as_ref()
        .expect("opencode declares user scope");
    assert!(
        user_path.contains("{xdg_config}"),
        "user path must use XDG placeholder: {user_path}"
    );
    assert!(
        user_path.contains("opencode"),
        "user path must reference opencode: {user_path}"
    );
    assert_eq!(
        user_path.as_str(),
        "{xdg_config}/opencode/AGENTS.md",
        "user path template must match exactly"
    );

    // Verify {xdg_config} expansion works correctly.
    // The path template uses {xdg_config}, which means it will resolve through
    // XDG_CONFIG_HOME when expanded, not a hardcoded ~/.config.
    let expanded = agentsync::paths::expand(user_path);
    assert!(
        expanded.to_string_lossy().contains("opencode/AGENTS.md"),
        "expanded path must contain opencode/AGENTS.md: {}",
        expanded.display()
    );
    // Verify it did NOT resolve to a hardcoded ~/ path with expansion.
    // If the code was wrong and used hardcoded ~/.config, it would still work,
    // but we've already asserted the template contains {xdg_config} above.

    // Project scope must use {repo} placeholder.
    let project_path = instructions
        .project
        .as_ref()
        .expect("opencode declares project scope");
    assert_eq!(project_path.as_str(), "{repo}/AGENTS.md");

    // Local scope must NOT exist (OpenCode has no counterpart to CLAUDE.local.md).
    assert!(
        instructions.local.is_none(),
        "opencode must block local scope, not invent a path"
    );

    // Verify scopes match expectations.
    let scopes = instructions.scopes();
    assert_eq!(scopes, vec![ScopeKind::User, ScopeKind::Project]);
}

#[test]
fn kilo_instructions() {
    // Kilo instructions must also use XDG-rooted paths, independent of OpenCode.
    let text = agentsync::hosts::descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "kilo")
        .expect("kilo builtin")
        .1;
    let desc = agentsync::hosts::descriptor::parse(text, "kilo").unwrap();
    let instructions = desc.instructions.expect("kilo has instructions");

    // User scope must use {xdg_config} and reference "kilo", not "opencode".
    let user_path = instructions
        .user
        .as_ref()
        .expect("kilo declares user scope");
    assert!(
        user_path.contains("{xdg_config}"),
        "user path must use XDG placeholder: {user_path}"
    );
    assert!(
        user_path.contains("kilo"),
        "user path must reference kilo, not opencode: {user_path}"
    );
    assert!(
        !user_path.contains("opencode"),
        "kilo path must never reference opencode: {user_path}"
    );
    assert_eq!(
        user_path.as_str(),
        "{xdg_config}/kilo/AGENTS.md",
        "kilo user path must match exactly"
    );

    // Verify {xdg_config} expansion works for Kilo too.
    let expanded = agentsync::paths::expand(user_path);
    assert!(
        expanded.to_string_lossy().contains("kilo/AGENTS.md"),
        "expanded kilo path must contain kilo/AGENTS.md: {}",
        expanded.display()
    );

    // Project scope must use {repo} placeholder.
    let project_path = instructions
        .project
        .as_ref()
        .expect("kilo declares project scope");
    assert_eq!(project_path.as_str(), "{repo}/AGENTS.md");

    // Local scope must NOT exist.
    assert!(instructions.local.is_none(), "kilo must block local scope");

    // Verify scopes match expectations.
    let scopes = instructions.scopes();
    assert_eq!(scopes, vec![ScopeKind::User, ScopeKind::Project]);
}

#[test]
fn opencode_and_kilo_never_cross_read_instruction_paths() {
    // Critical isolation: OpenCode must never read Kilo paths, and vice versa.
    let opencode_text = agentsync::hosts::descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "opencode")
        .expect("opencode builtin")
        .1;
    let opencode_desc = agentsync::hosts::descriptor::parse(opencode_text, "opencode").unwrap();
    let opencode_instructions = opencode_desc.instructions.expect("opencode instructions");

    let kilo_text = agentsync::hosts::descriptor::BUILTIN
        .iter()
        .find(|(n, _)| *n == "kilo")
        .expect("kilo builtin")
        .1;
    let kilo_desc = agentsync::hosts::descriptor::parse(kilo_text, "kilo").unwrap();
    let kilo_instructions = kilo_desc.instructions.expect("kilo instructions");

    // Collect all OpenCode paths.
    let opencode_paths: Vec<String> = opencode_instructions
        .user
        .iter()
        .chain(opencode_instructions.project.iter())
        .chain(opencode_instructions.local.iter())
        .cloned()
        .collect();

    // Collect all Kilo paths.
    let kilo_paths: Vec<String> = kilo_instructions
        .user
        .iter()
        .chain(kilo_instructions.project.iter())
        .chain(kilo_instructions.local.iter())
        .cloned()
        .collect();

    // OpenCode paths must never mention "kilo".
    for path in &opencode_paths {
        assert!(
            !path.contains("kilo"),
            "OpenCode path must never reference kilo: {path}"
        );
    }

    // Kilo paths must never mention "opencode".
    for path in &kilo_paths {
        assert!(
            !path.contains("opencode"),
            "Kilo path must never reference opencode: {path}"
        );
    }
}
// OpenCode-family MCP write path: a guarded JSONC ConfigTransaction, never a
// host CLI call — measured, `opencode mcp` has no `remove` subcommand at all,
// so add/update/remove all go through the same mechanism.
//
// These build a host descriptor pointing at an isolated temp file rather than
// using the shared `host()` helper's BUILTIN descriptor: building the write
// step reads the target file's current bytes to compute a guarded
// precondition, so a real descriptor's `{xdg_config}` path would read the
// machine running these tests.
// ---------------------------------------------------------------------------

fn opencode_family_host(name: &str, parser_user: &str, user_file: &Path) -> Host {
    let text = format!(
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

[mcp.jsonc]
user_file = "{user_file}"
"#,
        user_file = user_file.display(),
    );
    Host {
        descriptor: descriptor::parse(&text, name).unwrap(),
        bin: Some(PathBuf::from(format!("/usr/bin/{name}"))),
    }
}

fn mcp_entry(command: &str) -> McpEntry {
    McpEntry {
        transport: "stdio".into(),
        command: Some(command.into()),
        args: vec![],
        env: BTreeMap::new(),
        env_from: vec![],
        url: None,
        headers: BTreeMap::new(),
        bearer_token_env: None,
        scope: ScopeKind::User,
        repos: vec![],
        hosts: None,
    }
}

fn one_host_world(host: Host, snapshot: HostSnapshot, manifest: Manifest) -> World {
    World {
        manifest,
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![host],
        snapshots: vec![snapshot],
        repos: vec!["/repos/one".to_string()],
        warnings: Vec::new(),
    }
}

fn assert_mcp_write_is_config_transaction_never_host(plan: &agentsync::core::plan::Plan) {
    let mcp_steps: Vec<_> = plan.steps.iter().map(|s| &s.step).collect();
    assert!(
        !mcp_steps.is_empty(),
        "expected at least one step in the plan"
    );
    assert!(
        mcp_steps
            .iter()
            .all(|s| matches!(s, Step::ConfigTransaction(_)) || matches!(s, Step::Manual(_))),
        "the OpenCode-family MCP write path must be a guarded JSONC edit, \
         never a host CLI call: {mcp_steps:?}"
    );
    assert!(
        mcp_steps
            .iter()
            .any(|s| matches!(s, Step::ConfigTransaction(_))),
        "expected a ConfigTransaction step: {mcp_steps:?}"
    );
}

#[test]
fn opencode_mcp_add_is_a_guarded_jsonc_transaction_not_a_host_command() {
    let tmp = tempfile::tempdir().unwrap();
    let user_file = tmp.path().join("opencode.jsonc");
    let opencode = opencode_family_host("opencode", "opencode_mcp_jsonc_v1", &user_file);

    let mut manifest = Manifest::default();
    manifest.mcp.insert("demo".into(), mcp_entry("node"));

    let snap = snapshot("opencode", &[]);
    let world = one_host_world(opencode, snap, manifest);

    let mut rows = world.rows();
    assert_eq!(find(&rows, "demo").headline, "missing from opencode");

    accept(&mut rows, "demo");
    let plan = world.plan(&rows);
    assert_mcp_write_is_config_transaction_never_host(&plan);
}

#[test]
fn kilo_mcp_add_is_a_guarded_jsonc_transaction_not_a_host_command() {
    let tmp = tempfile::tempdir().unwrap();
    let user_file = tmp.path().join("kilo.jsonc");
    let kilo = opencode_family_host("kilo", "kilo_mcp_jsonc_v1", &user_file);

    let mut manifest = Manifest::default();
    manifest.mcp.insert("demo".into(), mcp_entry("node"));

    let snap = snapshot("kilo", &[]);
    let world = one_host_world(kilo, snap, manifest);

    let mut rows = world.rows();
    assert_eq!(find(&rows, "demo").headline, "missing from kilo");

    accept(&mut rows, "demo");
    let plan = world.plan(&rows);
    assert_mcp_write_is_config_transaction_never_host(&plan);
}

#[test]
fn opencode_mcp_removal_is_an_exact_jsonc_origin_removal_not_a_host_command() {
    let tmp = tempfile::tempdir().unwrap();
    let user_file = tmp.path().join("opencode.jsonc");
    std::fs::write(
        &user_file,
        r#"{"mcp":{"demo":{"type":"local","command":["node"]},"keep":{"type":"local","command":["x"]}}}"#,
    )
    .unwrap();
    let opencode = opencode_family_host("opencode", "opencode_mcp_jsonc_v1", &user_file);

    // Unmanaged: present on the host, absent from the manifest, so the
    // offered action is a removal.
    let snap = snapshot("opencode", &[(Scope::User, stdio("demo", "node"))]);
    let world = one_host_world(opencode, snap, Manifest::default());

    let mut rows = world.rows();
    let row = rows
        .iter_mut()
        .find(|r| r.name == "demo" && r.domain == Domain::Mcp)
        .expect("a row for demo");
    row.chosen = row
        .actions
        .iter()
        .position(|a| matches!(a.kind, ActionKind::Delete { .. }))
        .expect("a delete action");
    row.accepted = true;

    let plan = world.plan(&rows);
    assert_mcp_write_is_config_transaction_never_host(&plan);
    assert!(
        plan.steps.iter().all(
            |s| !matches!(&s.step, Step::Host { argv, .. } if argv.iter().any(|a| a == "remove"))
        ),
        "there is no `opencode mcp remove`, so no removal argv may ever be built: {:?}",
        plan.steps
    );
}

#[test]
fn kilo_mcp_removal_is_an_exact_jsonc_origin_removal_not_a_host_command() {
    let tmp = tempfile::tempdir().unwrap();
    let user_file = tmp.path().join("kilo.jsonc");
    std::fs::write(
        &user_file,
        r#"{"mcp":{"demo":{"type":"local","command":["node"]},"keep":{"type":"local","command":["x"]}}}"#,
    )
    .unwrap();
    let kilo = opencode_family_host("kilo", "kilo_mcp_jsonc_v1", &user_file);

    let snap = snapshot("kilo", &[(Scope::User, stdio("demo", "node"))]);
    let world = one_host_world(kilo, snap, Manifest::default());

    let mut rows = world.rows();
    let row = rows
        .iter_mut()
        .find(|r| r.name == "demo" && r.domain == Domain::Mcp)
        .expect("a row for demo");
    row.chosen = row
        .actions
        .iter()
        .position(|a| matches!(a.kind, ActionKind::Delete { .. }))
        .expect("a delete action");
    row.accepted = true;

    let plan = world.plan(&rows);
    assert_mcp_write_is_config_transaction_never_host(&plan);
}

// ---------------------------------------------------------------------------
// OpenCode hook bridge (OW-008)
// ---------------------------------------------------------------------------
//
// OpenCode has no marketplace and no native hook config format at all
// (measured; see `docs/open-work.md`), so a bridged handler never goes
// through the Claude-plugin shim path (`src/shim/generate.rs`) the Codex
// tests above exercise. `crate::paths::xdg_config_home()`/`state_dir()` read
// process-global env vars, so this section guards them with its own lock —
// no other test in this file reads either variable.
static OPENCODE_HOOKS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct OpenCodeHooksEnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous_xdg: Option<String>,
    previous_state: Option<String>,
}

impl OpenCodeHooksEnvGuard {
    fn set(xdg_config_home: &Path, state_home: &Path) -> Self {
        let guard = OPENCODE_HOOKS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let previous_state = std::env::var("AGENTSYNC_STATE_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", xdg_config_home);
            std::env::set_var("AGENTSYNC_STATE_HOME", state_home);
        }
        OpenCodeHooksEnvGuard {
            _guard: guard,
            previous_xdg,
            previous_state,
        }
    }
}

impl Drop for OpenCodeHooksEnvGuard {
    fn drop(&mut self) {
        match &self.previous_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match &self.previous_state {
            Some(v) => unsafe { std::env::set_var("AGENTSYNC_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("AGENTSYNC_STATE_HOME") },
        }
    }
}

fn claude_source_host() -> Host {
    host("claude")
}

fn opencode_hooks_world(handler: HookHandler, manifest: Manifest) -> World {
    let mut claude_snap = HostSnapshot {
        host: "claude".to_string(),
        display: "claude".to_string(),
        detected: true,
        ..Default::default()
    };
    claude_snap.hooks.insert(handler.id.clone(), handler);

    let opencode_snap = HostSnapshot {
        host: "opencode".to_string(),
        display: "opencode".to_string(),
        detected: true,
        ..Default::default()
    };

    World {
        manifest,
        manifest_path: PathBuf::from("/tmp/agentsync-test/manifest.toml"),
        hosts: vec![claude_source_host(), host("opencode")],
        snapshots: vec![claude_snap, opencode_snap],
        repos: Vec::new(),
        warnings: Vec::new(),
    }
}

fn pre_tool_use_handler() -> HookHandler {
    let id = HookId {
        source: "demo-plugin@demo-marketplace:hooks/hooks.json".to_string(),
        event: "PreToolUse".to_string(),
        group: 0,
        index: 0,
    };
    let mut h = HookHandler::new(id, "PreToolUse", "true");
    h.matcher = Some("Bash".to_string());
    h
}

#[test]
fn opencode_hooks_maps_pre_and_post_tool_use_to_the_measured_callbacks_and_blocks_everything_else()
{
    // PreToolUse/PostToolUse have an honest structural mapping onto OpenCode's
    // measured tool.execute.before/after. Every other Claude event is left
    // unmapped, so it must be blocked rather than silently ignored.
    let tmp = tempfile::tempdir().unwrap();
    let _env = OpenCodeHooksEnvGuard::set(&tmp.path().join("cfg"), &tmp.path().join("state"));

    let world = opencode_hooks_world(pre_tool_use_handler(), Manifest::default());
    let rows = world.rows();
    let row = rows
        .iter()
        .find(|r| r.domain == Domain::Hooks && r.name.starts_with("demo-plugin"))
        .expect("a hooks row for the PreToolUse handler");
    assert_eq!(row.severity, Severity::Normal, "{row:?}");
    assert!(
        row.actions
            .iter()
            .any(|a| matches!(&a.kind, ActionKind::Push { hosts } if hosts == &vec!["opencode".to_string()])),
        "must offer to generate the bridge on opencode: {row:?}"
    );

    // An event with no honest mapping (Stop has no OpenCode analog) must be
    // blocked, never silently dropped or guessed at.
    let mut stop_id = HookId {
        source: "demo-plugin@demo-marketplace:hooks/hooks.json".to_string(),
        event: "Stop".to_string(),
        group: 0,
        index: 0,
    };
    stop_id.index = 1;
    let stop_handler = HookHandler::new(stop_id, "Stop", "true");
    let world2 = opencode_hooks_world(stop_handler, Manifest::default());
    let rows2 = world2.rows();
    let row2 = rows2
        .iter()
        .find(|r| r.domain == Domain::Hooks)
        .expect("a row for the unmapped Stop handler");
    assert_eq!(row2.severity, Severity::Blocked, "{row2:?}");
    assert!(
        !row2.actionable(),
        "an unmapped event must not be actionable"
    );
}

#[test]
fn opencode_hooks_plans_a_guarded_file_transaction_never_a_host_command() {
    let tmp = tempfile::tempdir().unwrap();
    let _env = OpenCodeHooksEnvGuard::set(&tmp.path().join("cfg"), &tmp.path().join("state"));

    let world = opencode_hooks_world(pre_tool_use_handler(), Manifest::default());
    let mut rows = world.rows();
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
        "the OpenCode bridge must be a guarded FileTransaction: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        plan.steps
            .iter()
            .all(|s| !matches!(&s.step, Step::Host { .. })),
        "OpenCode has no marketplace/plugin install command to invoke: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
}

#[test]
fn opencode_hooks_a_rewake_configured_handler_is_reported_not_silently_bridged() {
    // The bridge has no channel to deliver rewakeMessage/rewakeSummary
    // through (see src/shim/bridge_output.rs); generating it anyway would
    // silently promise an emulation that never happens.
    let tmp = tempfile::tempdir().unwrap();
    let _env = OpenCodeHooksEnvGuard::set(&tmp.path().join("cfg"), &tmp.path().join("state"));

    let mut handler = pre_tool_use_handler();
    handler.rewake_message = Some("would rewake here".to_string());
    let world = opencode_hooks_world(handler, Manifest::default());
    let mut rows = world.rows();
    for row in rows.iter_mut() {
        if row.domain == Domain::Hooks {
            row.accepted = row.actionable();
        }
    }
    let plan = world.plan(&rows);
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(&s.step, Step::FileTransaction(_))),
        "a handler the bridge cannot faithfully deliver must not produce a write: {:?}",
        plan.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    assert!(
        plan.notes.iter().any(|n| n.contains("rewakeMessage")),
        "the refusal must be said out loud, not silently skipped: {:?}",
        plan.notes
    );
}
