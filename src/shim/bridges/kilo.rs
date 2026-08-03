//! Generated Kilo hook bridge. Pinned to Kilo `7.4.17`.
//!
//! Module shape — deliberately NOT interchangeable with OpenCode's:
//!
//! ```ts
//! const server = async (ctx) => ({ /* callbacks */ })
//! export default { id: "agentsync-hooks", server }
//! ```
//!
//! Paths resolve from the active Kilo profile (`hosts::opencode_family::layers`)
//! and `AGENTSYNC_STATE_HOME`, never a hardcoded `~`:
//!
//! - bridge: `<active-profile>/plugin/agentsync-hooks.generated.ts`
//! - index: `<agentsync-state>/shims/kilo/index.json`
//! - sidecars: `<agentsync-state>/shims/kilo/specs/*.json`
//!
//! The bridge, the index, every sidecar, the event mapping, the output
//! strategy, the target path, the current agentsync binary, and every hash
//! form ONE validity contract — the same shape as
//! `crate::shim::generate::Generated::verify_on_disk`, reused rather than
//! reinvented: [`GeneratedBridge::verify_on_disk`] re-derives every byte the
//! generator would produce and refuses to trust anything that has drifted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::hosts::opencode_family::layers::FamilyLayers;
use crate::shim::ShimSpec;
use crate::transaction::{FilePrecondition, FileTransaction, compute_sha256};

/// The only Kilo version hook actions are supported against. Other versions
/// remain usable for every other domain; only hook actions are blocked, and
/// the block message always names the observed version.
pub const PINNED_VERSION: &str = "7.4.17";

/// The nine measured Kilo runtime callbacks, in a fixed order so generation
/// is byte-stable across runs. Nothing outside this list may ever be treated
/// as a real callback — an unmeasured name is a bug in the caller, not a new
/// capability.
pub const CALLBACKS: [&str; 9] = [
    "config",
    "auth",
    "event",
    "chat.message",
    "chat.params",
    "tool.execute.before",
    "tool.execute.after",
    "session.idle",
    "session.error",
];

/// Callbacks measured to fire with no output channel a bridged action could
/// travel through. A handler can never be bridged onto one of these: see
/// `crate::shim::bridge_output::translate`.
pub const NO_OUTPUT_CALLBACKS: [&str; 3] = ["config", "auth", "event"];

/// One portable handler, already mapped by the caller onto the exact Kilo
/// callback name it targets. This module trusts that mapping rather than
/// re-deriving it from a source-host event name — the portable-event ->
/// Kilo-callback mapping is a separate, not-yet-measured concern (see
/// `docs/open-work.md`); what IS measured, and what this module enforces, is
/// that only the nine names in [`CALLBACKS`] are ever accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgedHandler {
    pub callback: String,
    pub spec: ShimSpec,
}

/// Everything needed to generate one Kilo hook bridge.
pub struct KiloBridgeInput {
    /// `<active-profile>` — the directory `hosts::opencode_family::layers`
    /// resolves as the writable global profile for Kilo. The bridge lives at
    /// `<active_profile_dir>/plugin/agentsync-hooks.generated.ts`.
    pub active_profile_dir: PathBuf,
    /// `paths::state_dir()` (or its `AGENTSYNC_STATE_HOME` override). The
    /// index and sidecars live under `<state_home>/shims/kilo/`.
    pub state_home: PathBuf,
    /// Absolute path of the agentsync binary the bridge invokes.
    pub agentsync_bin: PathBuf,
    /// Handlers to bridge, each already targeting a measured callback.
    pub handlers: Vec<BridgedHandler>,
}

/// One generated bridge: every file it writes, plus the exact bytes so a
/// later call can prove nothing has drifted.
#[derive(Debug)]
pub struct GeneratedBridge {
    pub plugin_dir: PathBuf,
    pub bridge_path: PathBuf,
    pub bridge_contents: String,
    /// `<state_home>/shims/kilo` — the shared root the index and every
    /// sidecar live under. Claimed as one tree, rather than per-file, so a
    /// fresh state directory only needs one ownership marker.
    pub state_dir: PathBuf,
    pub index_path: PathBuf,
    pub index_contents: String,
    pub sidecars: Vec<(PathBuf, String)>,
}

/// Whether the runtime will actually load external plugins right now.
/// `KILO_PURE` disables them; this must always report
/// [`BridgeHealth::DisabledByPureMode`], never [`BridgeHealth::Healthy`] — a
/// plausible-looking "healthy" here would be exactly the manufactured-value
/// failure this project exists to prevent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeHealth {
    Healthy,
    /// `KILO_PURE` is set. The bridge file may be generated and correct, but
    /// Kilo will not load it.
    DisabledByPureMode,
}

pub fn health(layers: &FamilyLayers) -> BridgeHealth {
    if layers.pure {
        BridgeHealth::DisabledByPureMode
    } else {
        BridgeHealth::Healthy
    }
}

/// Block hook actions unless the observed Kilo version is exactly
/// [`PINNED_VERSION`]. Other domains (MCP, plugins, skills, instructions)
/// remain usable regardless — this gate is reachable only from hook
/// generation. `None` (version could not be determined) blocks exactly like
/// a wrong version: an unknown version is never treated as the pinned one.
pub fn check_version(observed: Option<&str>) -> Result<()> {
    match observed {
        Some(v) if v == PINNED_VERSION => Ok(()),
        Some(v) => bail!(
            "Kilo hook actions are blocked: agentsync only supports Kilo {PINNED_VERSION}, \
             but the observed version is {v}. Other domains remain usable."
        ),
        None => bail!(
            "Kilo hook actions are blocked: the installed Kilo version could not be determined, \
             so it cannot be confirmed to be the supported {PINNED_VERSION}."
        ),
    }
}

fn sidecar_name(callback: &str, index: usize) -> String {
    format!("{}-{index}.json", callback.replace('.', "-"))
}

/// Build the bridge TS, the index, and every sidecar's exact bytes.
///
/// Every handler's callback must be one of the nine measured names, and must
/// have a measured output channel (see [`NO_OUTPUT_CALLBACKS`]) — a handler
/// targeting `config`, `auth`, or `event` is refused outright rather than
/// silently generating a callback wire-up that can never deliver anything.
pub fn generate(input: &KiloBridgeInput) -> Result<GeneratedBridge> {
    for handler in &input.handlers {
        if !CALLBACKS.contains(&handler.callback.as_str()) {
            bail!(
                "{} targets {:?}, which is not one of the nine measured Kilo callbacks",
                handler.spec.source_id,
                handler.callback
            );
        }
        if NO_OUTPUT_CALLBACKS.contains(&handler.callback.as_str()) {
            bail!(
                "{} targets {:?}, which was measured to fire with no output channel a bridged \
                 action could travel through; it cannot be bridged",
                handler.spec.source_id,
                handler.callback
            );
        }
    }

    let plugin_dir = input.active_profile_dir.join("plugin");
    let bridge_path = plugin_dir.join("agentsync-hooks.generated.ts");
    let state_dir = input.state_home.join("shims/kilo");
    let specs_dir = state_dir.join("specs");
    let index_path = state_dir.join("index.json");

    // Group by callback, preserving handler order within a callback, so
    // generation is deterministic regardless of input order upstream.
    let mut by_callback: std::collections::BTreeMap<&str, Vec<&BridgedHandler>> =
        std::collections::BTreeMap::new();
    for handler in &input.handlers {
        by_callback
            .entry(handler.callback.as_str())
            .or_default()
            .push(handler);
    }

    let mut sidecars = Vec::new();
    let mut events_json = serde_json::Map::new();
    for (callback, handlers) in &by_callback {
        let mut entries = Vec::new();
        for (index, handler) in handlers.iter().enumerate() {
            let name = sidecar_name(callback, index);
            let path = specs_dir.join(&name);
            let contents = format!("{}\n", handler.spec.to_json()?);
            entries.push(json!({
                "sidecar": name,
                "sha256": compute_sha256(contents.as_bytes()),
                "source_id": handler.spec.source_id,
            }));
            sidecars.push((path, contents));
        }
        events_json.insert(callback.to_string(), Value::Array(entries));
    }

    let bridge_contents = render_bridge(&input.agentsync_bin, &index_path, &by_callback);
    let bin_hash = compute_sha256(
        std::fs::read(&input.agentsync_bin)
            .unwrap_or_default()
            .as_slice(),
    );

    let index_value = json!({
        "schema": "agentsync/kilo-hook-bridge/v1",
        "kilo_version_required": PINNED_VERSION,
        "output_strategy": "kilo_v1",
        "agentsync_bin": {
            "path": input.agentsync_bin,
            "sha256": bin_hash,
        },
        "bridge": {
            "path": bridge_path,
            "sha256": compute_sha256(bridge_contents.as_bytes()),
        },
        "events": Value::Object(events_json),
    });
    let index_contents = format!("{}\n", serde_json::to_string_pretty(&index_value)?);

    Ok(GeneratedBridge {
        plugin_dir,
        bridge_path,
        bridge_contents,
        state_dir,
        index_path,
        index_contents,
        sidecars,
    })
}

/// Render the bridge module. Callbacks with no handlers are omitted entirely
/// — Kilo simply never calls a key that is not there.
fn render_bridge(
    agentsync_bin: &Path,
    index_path: &Path,
    by_callback: &std::collections::BTreeMap<&str, Vec<&BridgedHandler>>,
) -> String {
    let mut callbacks = String::new();
    for callback in by_callback.keys() {
        callbacks.push_str(&format!(
            "  {:?}: async (input) => invoke({:?}, input),\n",
            callback, callback
        ));
    }
    format!(
        "// Generated by agentsync. Do not edit by hand.\n\
         // Kilo hook bridge. Pinned to Kilo {PINNED_VERSION}.\n\
         //\n\
         // This module shape is deliberately not interchangeable with OpenCode's\n\
         // named async-function export shape: Kilo loads a default export\n\
         // carrying an `id` and a `server` factory, and must never be\n\
         // mistaken for the other host's plugin.\n\
         const AGENTSYNC_BIN = {agentsync_bin:?};\n\
         const AGENTSYNC_INDEX = {index_path:?};\n\
         \n\
         async function invoke(callback, input) {{\n\
         \x20\x20const {{ execFileSync }} = await import(\"node:child_process\");\n\
         \x20\x20const out = execFileSync(\n\
         \x20\x20\x20\x20AGENTSYNC_BIN,\n\
         \x20\x20\x20\x20[\"bridge-shim\", \"--index\", AGENTSYNC_INDEX, \"--callback\", callback],\n\
         \x20\x20\x20\x20{{ input: JSON.stringify(input ?? {{}}), encoding: \"utf8\" }},\n\
         \x20\x20);\n\
         \x20\x20return JSON.parse(out);\n\
         }}\n\
         \n\
         const server = async (ctx) => ({{\n\
         {callbacks}}});\n\
         \n\
         export default {{ id: \"agentsync-hooks\", server }};\n",
        agentsync_bin = agentsync_bin.display(),
        index_path = index_path.display(),
    )
}

impl GeneratedBridge {
    /// Build the guarded `FileTransaction` for this generation, or `None`
    /// when everything on disk already matches — the actual proof of
    /// two-pass convergence, since a caller that gets `None` has nothing left
    /// to plan.
    ///
    /// `existing_index` is the previously-written index bytes, when any; used
    /// only to decide preconditions (`Absent` for a first write, `Sha256` of
    /// the prior bytes for a verified regeneration). Nothing here reads the
    /// filesystem itself — every fact comes from the caller, so this stays
    /// testable without a real directory.
    pub fn transaction(
        &self,
        plugin_dir_exists: bool,
        state_dir_exists: bool,
        existing_bridge: Option<&[u8]>,
        existing_index: Option<&[u8]>,
        existing_sidecars: &[Option<Vec<u8>>],
    ) -> Result<Option<FileTransaction>> {
        let bridge_unchanged = existing_bridge == Some(self.bridge_contents.as_bytes());
        let index_unchanged = existing_index == Some(self.index_contents.as_bytes());
        let sidecars_unchanged = existing_sidecars.len() == self.sidecars.len()
            && self
                .sidecars
                .iter()
                .zip(existing_sidecars)
                .all(|((_, contents), existing)| existing.as_deref() == Some(contents.as_bytes()));
        if plugin_dir_exists
            && state_dir_exists
            && bridge_unchanged
            && index_unchanged
            && sidecars_unchanged
        {
            return Ok(None);
        }

        let mut tx = FileTransaction::new();
        if !plugin_dir_exists {
            tx = tx.claim_fresh_directory(&self.plugin_dir);
        }
        if !state_dir_exists {
            tx = tx.claim_fresh_directory(&self.state_dir);
        }

        let bridge_precondition = match existing_bridge {
            None => FilePrecondition::Absent,
            Some(bytes) => FilePrecondition::Sha256(compute_sha256(bytes)),
        };
        tx = tx.write_generated(
            &self.bridge_path,
            self.bridge_contents.clone(),
            bridge_precondition,
        );

        let index_precondition = match existing_index {
            None => FilePrecondition::Absent,
            Some(bytes) => FilePrecondition::Sha256(compute_sha256(bytes)),
        };
        tx = tx.write_generated(
            &self.index_path,
            self.index_contents.clone(),
            index_precondition,
        );

        for (i, (path, contents)) in self.sidecars.iter().enumerate() {
            let precondition = match existing_sidecars.get(i).and_then(|s| s.as_ref()) {
                None => FilePrecondition::Absent,
                Some(bytes) => FilePrecondition::Sha256(compute_sha256(bytes)),
            };
            tx = tx.write_generated(path, contents.clone(), precondition);
        }

        Ok(Some(tx))
    }

    /// Check that every artifact on disk still matches this generation
    /// contract exactly, the same shape as
    /// `crate::shim::generate::Generated::verify_on_disk`. Any drift —
    /// tampering, a stale binary path, a hand-edited sidecar — fails closed.
    pub fn verify_on_disk(&self) -> Result<()> {
        let actual_bridge = std::fs::read_to_string(&self.bridge_path)
            .with_context(|| format!("reading generated bridge {}", self.bridge_path.display()))?;
        if actual_bridge != self.bridge_contents {
            bail!(
                "generated bridge {} does not match the current generation contract",
                self.bridge_path.display()
            );
        }
        let actual_index = std::fs::read_to_string(&self.index_path)
            .with_context(|| format!("reading generated index {}", self.index_path.display()))?;
        if actual_index != self.index_contents {
            bail!(
                "generated index {} does not match the current generation contract",
                self.index_path.display()
            );
        }
        for (path, contents) in &self.sidecars {
            let actual = std::fs::read_to_string(path)
                .with_context(|| format!("reading generated sidecar {}", path.display()))?;
            if actual != *contents {
                bail!(
                    "generated sidecar {} does not match the current generation contract",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::HookOutputStrategy;
    use crate::hosts::opencode_family::layers::{Env, Family, discover};

    fn spec(source_id: &str, event: &str) -> ShimSpec {
        ShimSpec {
            source_id: source_id.to_string(),
            command: "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/review.sh\"".into(),
            plugin_root: Some("/cache/mkt/security-guidance/2.0.6".into()),
            if_pattern: None,
            event: Some(event.to_string()),
            output_strategy: HookOutputStrategy::KiloV1,
            allowed_output: vec!["systemMessage".into()],
            fold_into_system_message: vec!["rewakeMessage".into()],
            rewake_message: None,
            rewake_summary: None,
            timeout_seconds: None,
        }
    }

    fn handler(callback: &str, source_id: &str, event: &str) -> BridgedHandler {
        BridgedHandler {
            callback: callback.to_string(),
            spec: spec(source_id, event),
        }
    }

    fn input(tmp: &Path, handlers: Vec<BridgedHandler>) -> KiloBridgeInput {
        KiloBridgeInput {
            active_profile_dir: tmp.join("profile"),
            state_home: tmp.join("state"),
            agentsync_bin: tmp.join("bin/agentsync"),
            handlers,
        }
    }

    // ---- version gate ----

    #[test]
    fn the_exact_pinned_version_is_allowed() {
        assert!(check_version(Some(PINNED_VERSION)).is_ok());
    }

    #[test]
    fn a_different_observed_version_blocks_and_names_it() {
        let err = check_version(Some("7.4.16")).unwrap_err().to_string();
        assert!(err.contains("7.4.16"), "got {err}");
        assert!(err.contains(PINNED_VERSION), "got {err}");
    }

    #[test]
    fn an_undetermined_version_blocks_rather_than_assuming_the_pin() {
        let err = check_version(None).unwrap_err().to_string();
        assert!(err.contains("could not be determined"), "got {err}");
    }

    // ---- KILO_PURE health ----

    #[test]
    fn kilo_pure_reports_disabled_never_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env::new(tmp.path().join("home")).set("KILO_PURE", "1");
        let layers = discover(Family::Kilo, &env, None);
        assert_eq!(health(&layers), BridgeHealth::DisabledByPureMode);
    }

    #[test]
    fn without_kilo_pure_the_bridge_reports_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env::new(tmp.path().join("home"));
        let layers = discover(Family::Kilo, &env, None);
        assert_eq!(health(&layers), BridgeHealth::Healthy);
    }

    // ---- module shape distinguishability ----

    #[test]
    fn the_kilo_bridge_shape_is_not_interchangeable_with_opencodes() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                "PreToolUse",
            )],
        ))
        .unwrap();
        assert!(
            g.bridge_contents
                .contains("export default { id: \"agentsync-hooks\", server };"),
            "must use Kilo's default-export shape: {}",
            g.bridge_contents
        );
        assert!(
            !g.bridge_contents.contains("export const AgentsyncHooks"),
            "must NOT use OpenCode's named-export shape, or an OpenCode loader could \
             mistake this for its own plugin: {}",
            g.bridge_contents
        );
    }

    // ---- the nine measured callbacks: golden generation ----

    #[test]
    fn tool_execute_before_and_after_are_wired_with_their_own_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![
                handler(
                    "tool.execute.before",
                    "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                    "PreToolUse",
                ),
                handler(
                    "tool.execute.after",
                    "demo@mkt:hooks/hooks.json:post_tool_use:0:0",
                    "PostToolUse",
                ),
            ],
        ))
        .unwrap();
        assert!(g.bridge_contents.contains("\"tool.execute.before\""));
        assert!(g.bridge_contents.contains("\"tool.execute.after\""));
        assert_eq!(g.sidecars.len(), 2);
        let index: serde_json::Value = serde_json::from_str(&g.index_contents).unwrap();
        assert!(index["events"]["tool.execute.before"].is_array());
        assert!(index["events"]["tool.execute.after"].is_array());
    }

    #[test]
    fn session_idle_and_session_error_are_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![
                handler("session.idle", "demo@mkt:hooks/hooks.json:stop:0:0", "Stop"),
                handler(
                    "session.error",
                    "demo@mkt:hooks/hooks.json:notif:0:0",
                    "Notification",
                ),
            ],
        ))
        .unwrap();
        assert!(g.bridge_contents.contains("\"session.idle\""));
        assert!(g.bridge_contents.contains("\"session.error\""));
    }

    #[test]
    fn chat_message_and_chat_params_are_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![
                handler(
                    "chat.message",
                    "demo@mkt:hooks/hooks.json:prompt:0:0",
                    "UserPromptSubmit",
                ),
                handler(
                    "chat.params",
                    "demo@mkt:hooks/hooks.json:prompt2:0:0",
                    "UserPromptSubmit",
                ),
            ],
        ))
        .unwrap();
        assert!(g.bridge_contents.contains("\"chat.message\""));
        assert!(g.bridge_contents.contains("\"chat.params\""));
    }

    #[test]
    fn config_auth_and_event_are_refused_never_silently_wired() {
        let tmp = tempfile::tempdir().unwrap();
        for callback in NO_OUTPUT_CALLBACKS {
            let err = generate(&input(
                tmp.path(),
                vec![handler(
                    callback,
                    "demo@mkt:hooks/hooks.json:x:0:0",
                    "SessionStart",
                )],
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains(callback), "got {err}");
            assert!(err.contains("no output channel"), "got {err}");
        }
    }

    #[test]
    fn an_unmeasured_callback_name_is_refused_not_guessed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let err = generate(&input(
            tmp.path(),
            vec![handler(
                "tool.execute.retry",
                "demo@mkt:hooks/hooks.json:x:0:0",
                "PostToolUse",
            )],
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("tool.execute.retry"), "got {err}");
    }

    #[test]
    fn a_callback_with_no_handlers_is_omitted_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(tmp.path(), vec![])).unwrap();
        for callback in CALLBACKS {
            assert!(
                !g.bridge_contents.contains(&format!("{callback:?}:")),
                "an unused callback must not appear at all: {}",
                g.bridge_contents
            );
        }
    }

    // ---- path resolution ----

    #[test]
    fn paths_resolve_from_the_active_profile_and_state_home_never_a_literal_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(tmp.path(), vec![])).unwrap();
        assert_eq!(
            g.bridge_path,
            tmp.path()
                .join("profile/plugin/agentsync-hooks.generated.ts")
        );
        assert_eq!(g.index_path, tmp.path().join("state/shims/kilo/index.json"));
        assert!(!g.bridge_path.to_string_lossy().contains('~'));
    }

    #[test]
    fn the_active_profile_dir_honours_kilo_config_dir_via_the_shared_layer_engine() {
        // This is the exact fact measured in docs/open-work.md: KILO_CONFIG_DIR
        // outranks the default XDG global config as the active writable profile,
        // and `kilo debug paths` cannot be trusted to reflect it — so this must
        // come from the environment via the shared layer engine, never a
        // hardcoded `~/.config/kilo`.
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("custom-profile");
        let env =
            Env::new(tmp.path().join("home")).set("KILO_CONFIG_DIR", profile.display().to_string());
        let layers = discover(Family::Kilo, &env, None);
        assert_eq!(layers.active_profile_dir, profile);
    }

    // ---- validity contract: tampering ----

    fn write_generated_fixture(g: &GeneratedBridge) {
        std::fs::create_dir_all(g.bridge_path.parent().unwrap()).unwrap();
        std::fs::write(&g.bridge_path, &g.bridge_contents).unwrap();
        std::fs::create_dir_all(g.index_path.parent().unwrap()).unwrap();
        std::fs::write(&g.index_path, &g.index_contents).unwrap();
        for (path, contents) in &g.sidecars {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn a_tampered_bridge_file_fails_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                "PreToolUse",
            )],
        ))
        .unwrap();
        write_generated_fixture(&g);
        assert!(g.verify_on_disk().is_ok());

        std::fs::write(&g.bridge_path, "// tampered\n").unwrap();
        let err = g.verify_on_disk().unwrap_err().to_string();
        assert!(err.contains("does not match"), "got {err}");
    }

    #[test]
    fn a_tampered_sidecar_fails_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                "PreToolUse",
            )],
        ))
        .unwrap();
        write_generated_fixture(&g);
        let (sidecar_path, _) = &g.sidecars[0];
        std::fs::write(sidecar_path, "{ not the generated json").unwrap();
        let err = g.verify_on_disk().unwrap_err().to_string();
        assert!(err.contains("does not match"), "got {err}");
    }

    // ---- apply-time races and ownership ----

    #[test]
    fn a_fresh_plugin_directory_is_claimed_before_the_bridge_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let profile = state.join("kilo-profile");
        let g = generate(&KiloBridgeInput {
            active_profile_dir: profile.clone(),
            state_home: state.clone(),
            agentsync_bin: tmp.path().join("bin/agentsync"),
            handlers: vec![],
        })
        .unwrap();
        let mut tx = g
            .transaction(false, false, None, None, &[])
            .unwrap()
            .expect("a fresh generation must produce a transaction");
        tx.execute().unwrap();
        assert!(g.bridge_path.is_file());
        assert!(g.plugin_dir.join(".agentsync-owned").exists());
    }

    #[test]
    fn an_existing_unowned_plugin_directory_is_never_claimed_or_written_into() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let plugin_dir = profile.join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("users-own-plugin.ts"), b"user content").unwrap();

        let g = generate(&input(tmp.path(), vec![])).unwrap();
        assert_eq!(g.plugin_dir, plugin_dir);
        let mut tx = g
            .transaction(true, false, None, None, &[])
            .unwrap()
            .expect("directory exists but bridge does not, so a write is still planned");
        let result = tx.execute();
        assert!(
            result.is_err(),
            "an unowned directory must never be silently written into"
        );
        assert!(!g.bridge_path.exists());
        assert!(
            !plugin_dir.join(".agentsync-owned").exists(),
            "no ownership marker may appear in a directory agentsync did not create"
        );
        assert_eq!(
            std::fs::read(plugin_dir.join("users-own-plugin.ts")).unwrap(),
            b"user content"
        );
    }

    #[test]
    fn a_plan_apply_race_on_the_bridge_is_rejected_without_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let profile = state.join("kilo-profile");
        let g = generate(&KiloBridgeInput {
            active_profile_dir: profile.clone(),
            state_home: state.clone(),
            agentsync_bin: tmp.path().join("bin/agentsync"),
            handlers: vec![],
        })
        .unwrap();
        std::fs::create_dir_all(&g.plugin_dir).unwrap();
        std::fs::write(g.plugin_dir.join(".agentsync-owned"), b"").unwrap();
        std::fs::write(&g.bridge_path, "// a previous generation\n").unwrap();
        let stale_hash_of_something_else = compute_sha256(b"not what is actually on disk");

        let mut tx = FileTransaction::new().write_generated(
            &g.bridge_path,
            g.bridge_contents.clone(),
            FilePrecondition::Sha256(stale_hash_of_something_else),
        );
        assert!(tx.execute().is_err(), "a race must be rejected");
        let on_disk = std::fs::read_to_string(&g.bridge_path).unwrap();
        assert_eq!(
            on_disk, "// a previous generation\n",
            "a rejected race must never overwrite the file"
        );
    }

    // ---- two-pass convergence at the generation-decision level ----

    #[test]
    fn a_second_generation_with_matching_bytes_produces_no_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                "PreToolUse",
            )],
        ))
        .unwrap();
        let none = g
            .transaction(
                true,
                true,
                Some(g.bridge_contents.as_bytes()),
                Some(g.index_contents.as_bytes()),
                &[Some(g.sidecars[0].1.as_bytes().to_vec())],
            )
            .unwrap();
        assert!(
            none.is_none(),
            "byte-identical generation must plan nothing, proving convergence"
        );
    }

    #[test]
    fn a_stale_binary_path_forces_a_verified_regeneration() {
        let tmp = tempfile::tempdir().unwrap();
        let g_old = generate(&KiloBridgeInput {
            active_profile_dir: tmp.path().join("profile"),
            state_home: tmp.path().join("state"),
            agentsync_bin: tmp.path().join("bin/agentsync-old"),
            handlers: vec![],
        })
        .unwrap();
        let g_new = generate(&KiloBridgeInput {
            active_profile_dir: tmp.path().join("profile"),
            state_home: tmp.path().join("state"),
            agentsync_bin: tmp.path().join("bin/agentsync-new"),
            handlers: vec![],
        })
        .unwrap();
        assert_ne!(g_old.bridge_contents, g_new.bridge_contents);
        let some = g_new
            .transaction(
                true,
                true,
                Some(g_old.bridge_contents.as_bytes()),
                None,
                &[],
            )
            .unwrap();
        assert!(
            some.is_some(),
            "a changed generation contract must still be planned"
        );
    }

    // ---- generation is deterministic ----

    #[test]
    #[ignore]
    fn write_bun_build_fixture() {
        // Not part of the gate suite. Run explicitly to materialize a real
        // generated bridge for `bun build` to compile against:
        //   cargo test shim::bridges::kilo::tests::write_bun_build_fixture \
        //       -- --ignored --nocapture
        let dir = std::path::PathBuf::from("/tmp/agentsync-kilo-bridge-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        let g = generate(&KiloBridgeInput {
            active_profile_dir: dir.join("profile"),
            state_home: dir.join("state"),
            agentsync_bin: PathBuf::from("/usr/local/bin/agentsync"),
            handlers: vec![
                handler(
                    "tool.execute.before",
                    "security-guidance@claude-plugins-official:hooks/hooks.json:pre_tool_use:0:0",
                    "PreToolUse",
                ),
                handler(
                    "tool.execute.after",
                    "security-guidance@claude-plugins-official:hooks/hooks.json:post_tool_use:0:0",
                    "PostToolUse",
                ),
                handler(
                    "chat.message",
                    "security-guidance@claude-plugins-official:hooks/hooks.json:prompt:0:0",
                    "UserPromptSubmit",
                ),
                handler(
                    "session.idle",
                    "security-guidance@claude-plugins-official:hooks/hooks.json:stop:0:0",
                    "Stop",
                ),
            ],
        })
        .unwrap();
        write_generated_fixture(&g);
        println!("{}", g.bridge_path.display());
    }

    #[test]
    fn generation_is_byte_stable_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let make = || {
            input(
                tmp.path(),
                vec![
                    handler(
                        "tool.execute.before",
                        "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                        "PreToolUse",
                    ),
                    handler("session.idle", "demo@mkt:hooks/hooks.json:stop:0:0", "Stop"),
                ],
            )
        };
        let a = generate(&make()).unwrap();
        let b = generate(&make()).unwrap();
        assert_eq!(a.bridge_contents, b.bridge_contents);
        assert_eq!(a.index_contents, b.index_contents);
    }
}
