//! End-to-end differ and planner tests over a synthetic world.
//!
//! These build `World` directly rather than reading the machine. So they assert
//! on behaviour that would otherwise only be visible by running the tool
//! against a real configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
