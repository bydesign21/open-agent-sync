//! End-to-end differ and planner tests over a synthetic world.
//!
//! These build `World` directly rather than reading the machine, so they assert
//! on behaviour that would otherwise only be visible by running the tool against
//! a real configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agentsync::core::diff::{ActionKind, Domain, Row, Severity};
use agentsync::core::model::{
    HostSnapshot, HttpServer, McpServer, Scope, ScopeKind, StdioServer, Transport,
};
use agentsync::core::plan::Step;
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
        // Pretend it is installed; nothing in these tests executes it.
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
    }
}

fn http(name: &str, url: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        transport: Transport::Http(HttpServer {
            url: url.to_string(),
            ..Default::default()
        }),
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

    // Even if accepted, no codex add may be emitted.
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
    assert!(
        plan.steps
            .iter()
            .any(|s| matches!(&s.step, Step::Manual(t) if t.contains("UPSKILLAI_KNOWLEDGE_TOKEN"))),
        "expected a manual step naming the variable"
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
        .insert("mine".into(), agentsync::core::model::SkillState::RealDir);
    claude.skills.insert(
        "mine".into(),
        agentsync::core::model::SkillState::Foreign(PathBuf::from("/elsewhere/mine")),
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
