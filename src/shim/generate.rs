//! Building a shim plugin as a list of filesystem operations.
//!
//! The generator writes data only. Every generated command invokes the
//! agentsync binary, so a fix to a translation strategy ships with the binary
//! instead of requiring every shim on disk to be rewritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::core::model::{HookCap, HookHandler, HookOutputStrategy, event_key};
use crate::core::plan::FsOp;
use crate::shim::ShimSpec;

/// Everything needed to generate one shim plugin.
pub struct ShimInput {
    /// Directory agentsync owns and registers as a local marketplace.
    pub marketplace_dir: PathBuf,
    /// The original plugin being stood in for.
    pub plugin: String,
    /// The marketplace the original came from.
    pub marketplace: String,
    /// Handlers to emulate, in their original order. All handlers must come
    /// from one source file. Two sources would collide on a sidecar name,
    /// since a sidecar's name is keyed by event, group, and index alone.
    pub handlers: Vec<HookHandler>,
    /// Top-level stdout keys the target accepts.
    pub allowed_output: Vec<String>,
    /// Keys whose human-readable text moves into `systemMessage`.
    pub fold_into_system_message: Vec<String>,
    /// The target host's event-aware output contract.
    pub output_strategy: HookOutputStrategy,
    /// Absolute path of the agentsync binary the generated commands invoke.
    pub agentsync_bin: PathBuf,
    /// The target host's declared hook capabilities. A handler config field is
    /// re-emitted into the generated manifest only when the target actually
    /// declares support for it — otherwise `plan_shim` would either write a key
    /// the target ignores, or silently drop one it does honour. `asyncRewake`
    /// is the motivating case: Codex supports it, but nothing re-emitted it
    /// before this field existed.
    pub target_caps: Vec<HookCap>,
    /// Directories in the ORIGINAL plugin to carry over by symlink, for example
    /// its `skills` and `commands`. The shim replaces the original outright, so
    /// without these that content would disappear. Linking rather than copying
    /// means the shim cannot drift from the original.
    pub vendor: Vec<PathBuf>,
}

pub struct Generated {
    pub ops: Vec<FsOp>,
    /// Name of the generated plugin, for `plugin add`.
    pub shim_plugin: String,
    /// Name of the marketplace it lives in, for `marketplace add`.
    pub marketplace_name: String,
}

/// The marketplace agentsync owns. Named so it cannot collide with a real one.
pub const MARKETPLACE_NAME: &str = "agentsync-shims";

/// Whether a marketplace is generated and owned by agentsync itself.
pub fn is_internal_marketplace(name: &str) -> bool {
    name == MARKETPLACE_NAME
}

/// The generated plugin's name. Keyed by marketplace as well as plugin name,
/// so two plugins named the same from different marketplaces do not resolve
/// to the same directory and overwrite each other's `hooks.json`.
pub fn shim_plugin_name(marketplace: &str, plugin: &str) -> String {
    format!("agentsync-shim-{marketplace}-{plugin}")
}

/// A sidecar file name that is unique per handler and stable across runs.
fn sidecar_name(handler: &HookHandler) -> String {
    format!(
        "{}-{}-{}.json",
        event_key(&handler.event),
        handler.id.group,
        handler.id.index
    )
}

pub fn plan_shim(input: &ShimInput) -> Result<Generated> {
    let shim_plugin = shim_plugin_name(&input.marketplace, &input.plugin);
    let plugin_dir = input.marketplace_dir.join(&shim_plugin);
    let specs_dir = plugin_dir.join("specs");
    let mut ops = Vec::new();

    // One sidecar per handler. Identity is positional, so two handlers with
    // byte-identical commands still get their own file.
    for handler in &input.handlers {
        let spec = ShimSpec {
            source_id: handler.id.to_string(),
            command: handler.command.clone(),
            plugin_root: handler.plugin_root.clone(),
            if_pattern: handler.if_pattern.clone(),
            event: Some(handler.event.clone()),
            output_strategy: input.output_strategy,
            allowed_output: input.allowed_output.clone(),
            fold_into_system_message: input.fold_into_system_message.clone(),
            // Only carried into the spec (and folded into `systemMessage` at
            // run time) when the target cannot represent the field itself. A
            // target that declares the cap gets it as a real manifest field
            // below, and does not need a second, textual copy of it.
            rewake_message: (!input.target_caps.contains(&HookCap::RewakeMessage))
                .then(|| handler.rewake_message.clone())
                .flatten(),
            rewake_summary: (!input.target_caps.contains(&HookCap::RewakeSummary))
                .then(|| handler.rewake_summary.clone())
                .flatten(),
        };
        ops.push(FsOp::WriteFile {
            path: specs_dir.join(sidecar_name(handler)),
            contents: format!("{}\n", spec.to_json()?),
        });
    }

    // The manifest. Groups are rebuilt from the handlers' own group indices, but
    // only groups a handler actually occupies are emitted. An empty padding
    // group would carry no matcher and no hooks, so leaving it out loses
    // nothing while keeping the manifest free of dead entries. Groups keep the
    // relative order of their original index.
    let mut events: BTreeMap<String, BTreeMap<usize, Value>> = BTreeMap::new();
    for handler in &input.handlers {
        let groups = events.entry(handler.event.clone()).or_default();
        let group = groups
            .entry(handler.id.group)
            .or_insert_with(|| json!({ "hooks": [] }));
        if let Some(matcher) = &handler.matcher {
            group["matcher"] = Value::String(matcher.clone());
        }
        let command = format!(
            "{} hook-shim --spec {}",
            shell_quote(&input.agentsync_bin.to_string_lossy()),
            shell_quote(&specs_dir.join(sidecar_name(handler)).to_string_lossy()),
        );
        let mut entry = json!({ "type": "command", "command": command });
        if let Some(timeout) = handler.timeout {
            entry["timeout"] = json!(timeout);
        }
        // Re-emit every config field the target actually declares support
        // for. Without this, a handler field the target supports natively
        // still gets lost, because the whole plugin travels together as one
        // shim and this loop is the only place that writes the manifest entry.
        if handler.async_rewake && input.target_caps.contains(&HookCap::AsyncRewake) {
            entry["asyncRewake"] = json!(true);
        }
        group["hooks"]
            .as_array_mut()
            .expect("groups are built with a hooks array")
            .push(entry);
    }
    let events: Map<String, Value> = events
        .into_iter()
        .map(|(event, groups)| (event, Value::Array(groups.into_values().collect())))
        .collect();

    let manifest = json!({
        "description": format!(
            "Generated by agentsync so this host can run hooks from {}@{}.",
            input.plugin, input.marketplace
        ),
        "hooks": Value::Object(events),
    });
    ops.push(FsOp::WriteFile {
        path: plugin_dir.join("hooks/hooks.json"),
        contents: format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    });

    // The host's install path reads this file before it looks at anything
    // else. With no `plugin.json`, `codex plugin add` fails on a missing
    // manifest before it ever reaches `hooks/hooks.json`. The version must be
    // a real semver string, not a bare integer, or the host rejects it too.
    let plugin_manifest = json!({
        "name": shim_plugin,
        "description": format!(
            "Generated by agentsync so this host can run hooks from {}@{}.",
            input.plugin, input.marketplace
        ),
        "version": "0.1.0",
    });
    ops.push(FsOp::WriteFile {
        path: plugin_dir.join(".claude-plugin/plugin.json"),
        contents: format!("{}\n", serde_json::to_string_pretty(&plugin_manifest)?),
    });

    // Carry the original's non-hook content across by symlink. The shim
    // supersedes the original, so anything not carried over is lost.
    for source in &input.vendor {
        if let Some(name) = source.file_name() {
            ops.push(FsOp::Link {
                target: source.clone(),
                link: plugin_dir.join(name),
            });
        }
    }

    Ok(Generated {
        ops,
        shim_plugin,
        marketplace_name: MARKETPLACE_NAME.to_string(),
    })
}

/// The marketplace manifest, listing every shim plugin in the directory.
///
/// Separate from `plan_shim` on purpose. The file lists all plugins at once, so
/// generating it per plugin would leave the last writer's plugin the only one
/// registered.
pub fn marketplace_manifest_op(marketplace_dir: &Path, shim_plugins: &[String]) -> Result<FsOp> {
    let marketplace = json!({
        "name": MARKETPLACE_NAME,
        "plugins": shim_plugins
            .iter()
            .map(|name| json!({
                "name": name,
                // The generator writes each shim to
                // `{marketplace_dir}/{shim_plugin_name}/`, so the source is that
                // directory, relative to the marketplace root. A source that
                // points anywhere else is the same bug in a new costume.
                "source": format!("./{name}"),
                "description": format!(
                    "Generated by agentsync so this host can run hooks it cannot express \
                     natively (shim for {name})"
                ),
            }))
            .collect::<Vec<_>>(),
    });
    Ok(FsOp::WriteFile {
        path: marketplace_dir.join(".claude-plugin/marketplace.json"),
        contents: format!("{}\n", serde_json::to_string_pretty(&marketplace)?),
    })
}

/// Single-quote a path for `sh -c`. Generated commands run through a shell, and
/// a path with a space would otherwise split into two arguments.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{HookHandler, HookId};

    fn handler(index: usize, if_pattern: Option<&str>) -> HookHandler {
        let mut h = HookHandler::new(
            HookId {
                source: "security-guidance@claude-plugins-official:hooks/hooks.json".into(),
                event: "PostToolUse".into(),
                group: 1,
                index,
            },
            "PostToolUse",
            "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/review.sh\"",
        );
        h.matcher = Some("Bash".into());
        h.if_pattern = if_pattern.map(str::to_string);
        h.plugin_root = Some("/cache/claude-plugins-official/security-guidance/2.0.6".into());
        h.async_rewake = true;
        h.rewake_message = Some("security findings follow".into());
        h.rewake_summary = Some("Commit security review found issues".into());
        h
    }

    fn input(handlers: Vec<HookHandler>) -> ShimInput {
        ShimInput {
            marketplace_dir: "/home/u/.agentsync/shims/codex".into(),
            plugin: "security-guidance".into(),
            marketplace: "claude-plugins-official".into(),
            handlers,
            allowed_output: vec!["systemMessage".into()],
            fold_into_system_message: vec!["rewakeMessage".into()],
            output_strategy: HookOutputStrategy::Legacy,
            agentsync_bin: "/usr/local/bin/agentsync".into(),
            vendor: vec![],
            target_caps: vec![crate::core::model::HookCap::AsyncRewake],
        }
    }

    fn written(ops: &[crate::core::plan::FsOp], ends_with: &str) -> String {
        ops.iter()
            .find_map(|op| match op {
                crate::core::plan::FsOp::WriteFile { path, contents }
                    if path.to_string_lossy().ends_with(ends_with) =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no write op ending in {ends_with}"))
    }

    #[test]
    fn each_handler_gets_its_own_sidecar_carrying_its_own_filter() {
        // Five handlers with byte-identical commands must not collapse. This is
        // the bug the whole domain exists to report.
        let g = plan_shim(&input(vec![
            handler(0, Some("Bash(git commit:*)")),
            handler(1, Some("Bash(git push:*)")),
        ]))
        .unwrap();

        let a: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-0.json")).unwrap();
        let b: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-1.json")).unwrap();
        assert_eq!(a.if_pattern.as_deref(), Some("Bash(git commit:*)"));
        assert_eq!(b.if_pattern.as_deref(), Some("Bash(git push:*)"));
        assert_eq!(a.command, b.command, "the fixture's commands are identical");
    }

    #[test]
    fn the_sidecar_records_the_originals_plugin_root_not_the_shims() {
        let g = plan_shim(&input(vec![handler(0, None)])).unwrap();
        let spec: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-0.json")).unwrap();
        assert_eq!(
            spec.plugin_root.as_deref(),
            Some(std::path::Path::new(
                "/cache/claude-plugins-official/security-guidance/2.0.6"
            ))
        );
    }

    #[test]
    fn the_generated_manifest_keeps_the_event_and_matcher_and_calls_the_shim() {
        let g = plan_shim(&input(vec![handler(0, Some("Bash(git commit:*)"))])).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&written(&g.ops, "hooks/hooks.json")).unwrap();
        let group = &manifest["hooks"]["PostToolUse"][0];
        assert_eq!(group["matcher"], "Bash");
        let command = group["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains("/usr/local/bin/agentsync"), "{command}");
        assert!(command.contains("hook-shim --spec"), "{command}");
        // The filter is emulated by the runtime, so it must NOT be re-emitted
        // into a manifest the target cannot honour anyway.
        assert!(group["hooks"][0].get("if").is_none());
    }

    #[test]
    fn a_handler_carrying_async_rewake_gets_the_full_key_set_the_target_supports() {
        // Regression for the bug where `asyncRewake` was silently dropped even
        // though the target declares support for it. Asserting the complete
        // key set, not just that one key is present, so a future dropped
        // field fails this test instead of sliding past it.
        let g = plan_shim(&input(vec![handler(0, Some("Bash(git commit:*)"))])).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&written(&g.ops, "hooks/hooks.json")).unwrap();
        let entry = &manifest["hooks"]["PostToolUse"][0]["hooks"][0];
        let mut keys: Vec<&str> = entry
            .as_object()
            .expect("hook entry must be a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["asyncRewake", "command", "type"],
            "the full generated key set must be exactly this, so a dropped or \
             newly-added field fails the test rather than passing silently: {entry}"
        );
        assert_eq!(entry["asyncRewake"], true);
    }

    #[test]
    fn async_rewake_is_not_reemitted_when_the_target_does_not_declare_the_cap() {
        let mut i = input(vec![handler(0, None)]);
        i.target_caps = vec![];
        let g = plan_shim(&i).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&written(&g.ops, "hooks/hooks.json")).unwrap();
        let entry = &manifest["hooks"]["PostToolUse"][0]["hooks"][0];
        assert!(
            entry.get("asyncRewake").is_none(),
            "must not claim a capability the target never declared: {entry}"
        );
    }

    #[test]
    fn rewake_text_is_carried_into_the_sidecar_only_when_the_target_lacks_the_cap() {
        // When the target does not declare rewake_message/rewake_summary as
        // manifest capabilities (true of every current target), the sidecar
        // must still carry the configured text so the runtime can fold it
        // into systemMessage. See src/shim/output.rs.
        let g = plan_shim(&input(vec![handler(0, None)])).unwrap();
        let spec: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-0.json")).unwrap();
        assert_eq!(
            spec.rewake_message.as_deref(),
            Some("security findings follow")
        );
        assert_eq!(
            spec.rewake_summary.as_deref(),
            Some("Commit security review found issues")
        );
    }

    #[test]
    fn the_marketplace_manifest_lists_the_generated_plugin() {
        let g = plan_shim(&input(vec![handler(0, None)])).unwrap();
        let op = marketplace_manifest_op(
            std::path::Path::new("/home/u/.agentsync/shims/codex"),
            std::slice::from_ref(&g.shim_plugin),
        )
        .unwrap();
        let contents = match op {
            crate::core::plan::FsOp::WriteFile { contents, .. } => contents,
            other => panic!("expected a WriteFile op, got {other:?}"),
        };
        let mkt: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(mkt["name"], g.marketplace_name);
        let names: Vec<&str> = mkt["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert!(names.contains(&g.shim_plugin.as_str()), "got {names:?}");
    }

    #[test]
    fn the_marketplace_manifest_lists_every_shim_plugin_not_just_the_last() {
        // plan_shim itself no longer writes this file: writing it per plugin
        // would let the second shim's write erase the first from the manifest.
        let op = marketplace_manifest_op(
            std::path::Path::new("/home/u/.agentsync/shims/codex"),
            &[
                "agentsync-shim-claude-plugins-official-security-guidance".to_string(),
                "agentsync-shim-claude-plugins-official-other-plugin".to_string(),
            ],
        )
        .unwrap();
        let contents = match op {
            crate::core::plan::FsOp::WriteFile { contents, .. } => contents,
            other => panic!("expected a WriteFile op, got {other:?}"),
        };
        let mkt: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let names: Vec<&str> = mkt["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "agentsync-shim-claude-plugins-official-security-guidance",
                "agentsync-shim-claude-plugins-official-other-plugin",
            ]
        );
    }

    #[test]
    fn each_marketplace_entry_carries_a_source_matching_where_the_generator_writes() {
        // Codex rejects a plugin entry that carries only `name`: it requires
        // `source` too. The source must point at the directory `plan_shim`
        // actually writes to, or a host that trusts it lands on nothing.
        let marketplace_dir = std::path::Path::new("/home/u/.agentsync/shims/codex");
        let g = plan_shim(&input(vec![handler(0, None)])).unwrap();
        let op =
            marketplace_manifest_op(marketplace_dir, std::slice::from_ref(&g.shim_plugin)).unwrap();
        let contents = match op {
            crate::core::plan::FsOp::WriteFile { contents, .. } => contents,
            other => panic!("expected a WriteFile op, got {other:?}"),
        };
        let mkt: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let plugins = mkt["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        let entry = &plugins[0];
        assert_eq!(entry["name"], g.shim_plugin);
        let source = entry["source"].as_str().expect("source must be present");

        // The directory the generator's own ops write into, taken from the
        // sidecar write op, is the ground truth for where `source` must point.
        let written_dir = g
            .ops
            .iter()
            .find_map(|op| match op {
                crate::core::plan::FsOp::WriteFile { path, .. } => path
                    .strip_prefix(marketplace_dir)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                _ => None,
            })
            .expect("plan_shim must write at least one file under the marketplace dir");
        assert_eq!(source, format!("./{written_dir}"));
    }

    #[test]
    fn the_shim_gets_its_own_plugin_json_with_a_real_semver_version() {
        // Codex's install path reads plugin.json before anything else. With no
        // manifest there, `codex plugin add` fails on "missing plugin.json"
        // before it ever reaches hooks.json. The version must parse as three
        // dot-separated numbers, not a bare integer like "1".
        let g = plan_shim(&input(vec![handler(0, None)])).unwrap();
        let contents = written(&g.ops, ".claude-plugin/plugin.json");
        let manifest: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(manifest["name"], g.shim_plugin);
        let version = manifest["version"]
            .as_str()
            .expect("version must be a string");
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "version {version:?} is not three-part semver"
        );
        for part in parts {
            part.parse::<u64>()
                .unwrap_or_else(|_| panic!("version component {part:?} is not a number"));
        }

        // The manifest must live under the shim's own plugin directory, the
        // same one the install actually points `source` at.
        let path_written = g
            .ops
            .iter()
            .find_map(|op| match op {
                crate::core::plan::FsOp::WriteFile { path, .. }
                    if path
                        .to_string_lossy()
                        .ends_with(".claude-plugin/plugin.json") =>
                {
                    Some(path.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(
            path_written.ends_with(format!("{}/.claude-plugin/plugin.json", g.shim_plugin)),
            "plugin.json must live under the shim's own directory: {path_written:?}"
        );
    }

    #[test]
    fn same_named_plugins_from_different_marketplaces_get_different_directories() {
        let a = shim_plugin_name("claude-plugins-official", "security-guidance");
        let b = shim_plugin_name("some-other-marketplace", "security-guidance");
        assert_ne!(
            a, b,
            "same plugin name from two marketplaces must not collide"
        );
    }

    #[test]
    fn generation_is_byte_stable_so_a_rerun_reports_no_drift() {
        let a = plan_shim(&input(vec![handler(0, Some("Bash(git commit:*)"))])).unwrap();
        let b = plan_shim(&input(vec![handler(0, Some("Bash(git commit:*)"))])).unwrap();
        assert_eq!(format!("{:?}", a.ops), format!("{:?}", b.ops));
    }

    #[test]
    fn the_originals_other_content_is_carried_across_by_symlink() {
        // The shim REPLACES the original, so its skills and commands must come
        // with it. A link rather than a copy means they cannot drift apart.
        let mut i = input(vec![handler(0, None)]);
        i.vendor = vec![
            "/cache/claude-plugins-official/security-guidance/2.0.6/skills".into(),
            "/cache/claude-plugins-official/security-guidance/2.0.6/commands".into(),
        ];
        let g = plan_shim(&i).unwrap();
        let links: Vec<_> = g
            .ops
            .iter()
            .filter_map(|op| match op {
                crate::core::plan::FsOp::Link { target, link } => {
                    Some((target.clone(), link.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 2, "got {links:?}");
        assert!(
            links.iter().any(|(_, link)| link.ends_with("skills")),
            "skills must land under the shim plugin: {links:?}"
        );
    }

    #[test]
    fn a_handler_with_no_plugin_root_still_generates() {
        // A user-level settings hook has no plugin root. It must not be dropped.
        let mut h = handler(0, None);
        h.plugin_root = None;
        let g = plan_shim(&input(vec![h])).unwrap();
        let spec: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-0.json")).unwrap();
        assert!(spec.plugin_root.is_none());
    }

    #[test]
    fn sidecars_record_the_handler_event_and_the_target_output_strategy() {
        // Losing either value falls back to legacy, event-blind output at
        // runtime, where Codex accepts malformed data without a clear error.
        let mut i = input(vec![handler(0, None)]);
        i.output_strategy = crate::core::model::HookOutputStrategy::CodexV1;
        let g = plan_shim(&i).unwrap();
        let spec: crate::shim::ShimSpec =
            serde_json::from_str(&written(&g.ops, "post_tool_use-1-0.json")).unwrap();
        assert_eq!(spec.event.as_deref(), Some("PostToolUse"));
        assert_eq!(
            spec.output_strategy,
            crate::core::model::HookOutputStrategy::CodexV1
        );
    }
}
