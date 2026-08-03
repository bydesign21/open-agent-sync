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
//!
//! Dispatch reuses the SAME `agentsync hook-shim --spec <sidecar>` contract
//! the Codex shim and the OpenCode bridge (`src/shim/bridges/opencode.rs`)
//! both invoke. There is no separate `bridge-shim` subcommand — the generated
//! JS resolves which sidecar(s) answer a callback from the index at call
//! time, then hands each sidecar's path straight to `hook-shim --spec`,
//! exactly like `src/shim/run.rs` already does for Codex.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::core::model::HookOutputStrategy;
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

/// Build a bare-bones [`ShimSpec`] for `callback`, pinned to the Kilo
/// bridge's output strategy.
///
/// `event` is set to `callback`, not a source-host portable event name
/// (`PreToolUse` etc.) — `src/shim/output.rs` reads `spec.event` as the
/// callback name for `KiloV1`/`OpenCodeV1` strategies and hands it straight
/// to `bridge_output::translate`. A caller that put a portable event name
/// there instead would build a sidecar whose real dispatch silently
/// mistranslates. Mirrors `crate::shim::bridges::opencode::spec_for`.
pub fn spec_for(callback: &str, source_id: &str, command: &str) -> ShimSpec {
    ShimSpec {
        source_id: source_id.to_string(),
        command: command.to_string(),
        plugin_root: None,
        if_pattern: None,
        event: Some(callback.to_string()),
        output_strategy: HookOutputStrategy::KiloV1,
        allowed_output: vec![],
        fold_into_system_message: vec![],
        rewake_message: None,
        rewake_summary: None,
        timeout_seconds: None,
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
                // The full path, not just the file name: the bridge resolves
                // which sidecar(s) answer a callback entirely from this index
                // at call time (see `render_bridge`), so it needs the whole
                // path to hand to `hook-shim --spec`, not a bare name it would
                // have to re-root itself.
                "path": path,
                "sha256": compute_sha256(contents.as_bytes()),
                "source_id": handler.spec.source_id,
            }));
            sidecars.push((path, contents));
        }
        events_json.insert(callback.to_string(), Value::Array(entries));
    }

    let bridge_contents = render_bridge(&input.agentsync_bin, &index_path);
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

/// Render the bridge module.
///
/// Every one of the nine measured callbacks is always registered — which
/// ones actually have sidecars to run is entirely the index's business, read
/// at call time (`loadIndex()`), so regenerating the index alone (no handler
/// shape change) never requires rewriting this file, and a callback with no
/// configured handler simply dispatches nothing (`index.events[callback] ??
/// []` is empty) rather than being absent from the module.
///
/// Dispatch reuses the EXISTING `agentsync hook-shim --spec <sidecar>`
/// contract — the same one the Codex shim and the OpenCode bridge invoke —
/// rather than a second, Kilo-only mechanism. There is no `bridge-shim`
/// subcommand; inventing one here would be exactly the second scheme this
/// project's shim work has repeatedly rejected.
fn render_bridge(agentsync_bin: &Path, index_path: &Path) -> String {
    let bin = ts_string(&agentsync_bin.to_string_lossy());
    let index = ts_string(&index_path.to_string_lossy());
    format!(
        r#"// Generated by agentsync. Do not edit by hand — regenerate with `agentsync apply`.
//
// Bridges the Kilo plugin hook surface (measured: config, auth, event,
// chat.message, chat.params, tool.execute.before, tool.execute.after,
// session.idle, session.error) to handlers agentsync tracks, by invoking
// `agentsync hook-shim --spec <sidecar>` and interpreting its one typed
// bridge action object (see src/shim/bridge_output.rs). This is the SAME
// dispatcher the Codex shim and the OpenCode bridge use.
//
// This module shape is deliberately not interchangeable with OpenCode's
// named async-function export shape: Kilo loads a default export carrying
// an `id` and a `server` factory, and must never be mistaken for the other
// host's plugin.
//
// tool.execute.before/after are AWAITED: a failed sidecar run throws, which
// stops the intercepted tool call rather than letting it proceed as if
// nothing had gone wrong. Every other callback is fire-and-forget: a failure
// is caught and logged, never left to crash the host or vanish silently.

import {{ spawnSync }} from "node:child_process";
import {{ readFileSync }} from "node:fs";

const AGENTSYNC_BIN = {bin};
const INDEX_PATH = {index};

function loadIndex() {{
  return JSON.parse(readFileSync(INDEX_PATH, "utf8"));
}}

function runSidecar(specPath, stdin) {{
  const result = spawnSync(AGENTSYNC_BIN, ["hook-shim", "--spec", specPath], {{
    input: stdin,
    encoding: "utf8",
  }});
  if (result.error) {{
    console.error(`agentsync: could not run the hook shim for ${{specPath}}: ${{result.error}}`);
    return null;
  }}
  if (result.status !== 0) {{
    if (result.stderr) {{
      console.error(`agentsync: ${{result.stderr}}`);
    }}
    return null;
  }}
  if (!result.stdout) {{
    return null;
  }}
  try {{
    return JSON.parse(result.stdout);
  }} catch (e) {{
    console.error(`agentsync: malformed bridge output from ${{specPath}}: ${{e}}`);
    return null;
  }}
}}

async function dispatch(callback, ctx, awaited) {{
  const index = loadIndex();
  const specs = (index.events?.[callback] ?? []).map((s) => s.path);
  const stdin = JSON.stringify(ctx ?? {{}});
  let last = null;
  for (const spec of specs) {{
    const action = runSidecar(spec, stdin);
    if (action) {{
      last = action;
      if (action.block) return action;
    }} else if (awaited) {{
      throw new Error(`agentsync: the hook shim for ${{callback}} (${{spec}}) failed`);
    }} else {{
      console.error(`agentsync: the hook shim for ${{callback}} (${{spec}}) failed; continuing`);
    }}
  }}
  return last;
}}

async function fireAndForget(callback, ctx) {{
  try {{
    return await dispatch(callback, ctx, false);
  }} catch (e) {{
    console.error(`agentsync: ${{callback}} failed: ${{e}}`);
    return null;
  }}
}}

const server = async (ctx) => ({{
  config: async (input) => {{
    await fireAndForget("config", input);
  }},
  auth: async (input) => {{
    await fireAndForget("auth", input);
  }},
  event: async (input) => {{
    await fireAndForget("event", input);
  }},
  "chat.message": async (input) => {{
    return await fireAndForget("chat.message", input);
  }},
  "chat.params": async (input) => {{
    return await fireAndForget("chat.params", input);
  }},
  "tool.execute.before": async (input) => {{
    const action = await dispatch("tool.execute.before", input, true);
    if (action?.block) {{
      throw new Error(action.message ?? "blocked by agentsync");
    }}
    return action;
  }},
  "tool.execute.after": async (input) => {{
    return await dispatch("tool.execute.after", input, true);
  }},
  "session.idle": async (input) => {{
    await fireAndForget("session.idle", input);
  }},
  "session.error": async (input) => {{
    await fireAndForget("session.error", input);
  }},
}});

export default {{ id: "agentsync-hooks", server }};
"#,
    )
}

/// A double-quoted, single-line JS string literal. Every path this renders is
/// a filesystem path this process resolved itself (never user text), but a
/// quote or backslash in it must still not break out of the string literal.
fn ts_string(raw: &str) -> String {
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
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
        // Deliberately literal, not `for callback in NO_OUTPUT_CALLBACKS`.
        // Iterating the constant under test makes the test vacuous the moment
        // an entry is removed from it: the loop body simply stops running and
        // the test still passes.
        let measured_without_output_channel = ["config", "auth", "event"];
        assert_eq!(
            NO_OUTPUT_CALLBACKS.as_slice(),
            measured_without_output_channel.as_slice(),
            "the measured no-output callbacks changed; re-measure against the runtime \
             before editing this list"
        );
        for callback in measured_without_output_channel {
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
    fn every_measured_callback_is_always_registered_even_with_no_handlers() {
        // The bridge always registers all nine measured callbacks: which ones
        // have anything to run is entirely the index's business, resolved at
        // call time, so regenerating the index alone (no handler shape
        // change) never requires rewriting this file. A callback nothing
        // targets simply dispatches an empty sidecar list at runtime rather
        // than being absent from the module.
        let tmp = tempfile::tempdir().unwrap();
        let g = generate(&input(tmp.path(), vec![])).unwrap();
        for callback in CALLBACKS {
            assert!(
                g.bridge_contents.contains(callback),
                "every measured callback must always be registered: missing {callback:?} in {}",
                g.bridge_contents
            );
        }
        let index: serde_json::Value = serde_json::from_str(&g.index_contents).unwrap();
        assert_eq!(
            index["events"],
            serde_json::json!({}),
            "with no handlers the index must record no sidecars for any callback"
        );
    }

    // ---- dispatcher contract: `hook-shim`, never a second scheme ----

    #[test]
    fn the_bridge_invokes_the_existing_hook_shim_contract_never_a_second_scheme() {
        // This is the regression test for the defect a lead review found:
        // the bridge once invoked a nonexistent `bridge-shim` subcommand.
        // `hook-shim --spec <sidecar>` is the ONE dispatcher every generated
        // shim (Codex, OpenCode, Kilo) must invoke.
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
                .contains(r#"["hook-shim", "--spec", specPath]"#),
            "the bridge must invoke `hook-shim --spec <sidecar>`: {}",
            g.bridge_contents
        );
        assert!(
            !g.bridge_contents.contains("bridge-shim"),
            "`bridge-shim` is not a real subcommand and must never appear anywhere in the \
             generated bridge: {}",
            g.bridge_contents
        );
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
    #[ignore]
    fn write_execution_proof_fixture() {
        // Not part of the gate suite. Materializes a bridge pointed at the
        // REAL release binary (env `AGENTSYNC_EXEC_PROOF_BIN`), with a
        // handler whose command echoes a sentinel, so a driver script can
        // bundle it with `bun build`, actually run the bundled bridge under
        // `bun`, invoke `tool.execute.before`, and prove the sentinel comes
        // back through the real `hook-shim` dispatch end to end — not just
        // that the file bundles.
        //   AGENTSYNC_EXEC_PROOF_BIN=/path/to/release/agentsync \
        //     cargo test shim::bridges::kilo::tests::write_execution_proof_fixture \
        //     -- --ignored --nocapture
        let bin = std::env::var("AGENTSYNC_EXEC_PROOF_BIN")
            .expect("set AGENTSYNC_EXEC_PROOF_BIN to the real release binary path");
        let dir = std::path::PathBuf::from("/tmp/agentsync-kilo-exec-proof");
        let _ = std::fs::remove_dir_all(&dir);
        let g = generate(&KiloBridgeInput {
            active_profile_dir: dir.join("profile"),
            state_home: dir.join("state"),
            agentsync_bin: PathBuf::from(bin),
            handlers: vec![BridgedHandler {
                callback: "tool.execute.before".into(),
                spec: spec_for(
                    "tool.execute.before",
                    "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
                    r#"echo '{"systemMessage":"KILO_EXEC_PROOF_SENTINEL_9f3a7c"}'"#,
                ),
            }],
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
