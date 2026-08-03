//! Generated OpenCode hook bridge. Pinned to OpenCode `1.18.11`.
//!
//! Module shape (measured: any exported async function is loaded as a plugin):
//!
//! ```ts
//! export const AgentsyncHooks = async (ctx) => ({ /* callbacks */ })
//! ```
//!
//! Unlike the Codex shim (`src/shim/generate.rs`), which stands in for a
//! whole Claude-plugin marketplace entry (`hooks.json` + `plugin.json` +
//! sidecars, installed with `codex plugin add`), OpenCode has no marketplace
//! or CLI plugin command at all — it just scans `<config>/plugin(s)/` for any
//! file exporting an async function (measured; see `docs/open-work.md`).
//! So the generated artifact here is three things instead:
//!
//! 1. one fixed bridge script, `agentsync-hooks.ts`, written into OpenCode's
//!    own plugin scan directory;
//! 2. an index naming which sidecar(s) answer which of the nine measured
//!    callbacks, plus the exact bytes/paths everything must still match;
//! 3. one sidecar per handler — the same [`crate::shim::ShimSpec`] the Codex
//!    shim uses, so `agentsync hook-shim --spec <sidecar>` and its output
//!    translation (`src/shim/output.rs`, `src/shim/bridge_output.rs`) are
//!    reused verbatim rather than inventing a second runtime.
//!
//! Bridge, index, sidecars, event mapping, output strategy, target path,
//! current binary, and hashes are checked together by [`verify`] as ONE
//! validity contract: any single mismatch invalidates the whole thing, the
//! same principle `crate::shim::generate::Generated::verify_on_disk` and
//! `crate::domains::hooks::verify_shim_artifact` already apply to the Codex
//! shim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::core::model::HookOutputStrategy;
use crate::shim::ShimSpec;
use crate::transaction::{FilePrecondition, FileTransaction, compute_sha256, is_agentsync_owned};

/// The only OpenCode version hook actions are trusted against. Measured, not
/// documented (`docs/open-work.md`, "Verified runtime contracts").
pub const PINNED_VERSION: &str = "1.18.11";

/// The nine measured callbacks this bridge answers for. See
/// `crate::core::model::opencode_family_hook_fidelity`, which this list must
/// stay in lockstep with: a name here that fidelity does not recognise, or a
/// fidelity name missing here, is exactly the kind of drift this contract
/// exists to catch.
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

/// Callbacks whose failure must stop the intercepted operation rather than be
/// caught and logged. Both wrap a tool call and can act on it; every other
/// callback has no operation of its own to stop (see
/// `crate::core::model::opencode_family_hook_fidelity`), so a failure there
/// is caught, logged, and never allowed to propagate.
pub const AWAITED_CALLBACKS: [&str; 2] = ["tool.execute.before", "tool.execute.after"];

/// Whether `observed` is the one OpenCode version hook actions may run
/// against. Every other version still works for every other agentsync
/// domain (MCP, plugins, skills, instructions) — only hook actions are
/// blocked, and the block names the version actually observed rather than
/// a bare "unsupported version" message.
pub fn check_version(observed: &str) -> Result<(), String> {
    if observed == PINNED_VERSION {
        Ok(())
    } else {
        Err(format!(
            "agentsync only runs OpenCode hook actions against the pinned version \
             {PINNED_VERSION}; the installed opencode reports {observed}. Other domains \
             are unaffected, but hook actions are blocked until {PINNED_VERSION} is installed."
        ))
    }
}

/// `<OpenCode XDG config>/plugins/agentsync-hooks.ts`.
pub fn bridge_path(xdg_config_home: &Path) -> PathBuf {
    xdg_config_home.join("opencode/plugins/agentsync-hooks.ts")
}

/// `<agentsync-state>/shims/opencode/index.json`.
pub fn index_path(state_home: &Path) -> PathBuf {
    state_home.join("shims/opencode/index.json")
}

/// `<agentsync-state>/shims/opencode/specs/`.
pub fn specs_dir(state_home: &Path) -> PathBuf {
    state_home.join("shims/opencode/specs")
}

/// A stable, unique sidecar file name for the `n`th handler answering
/// `callback`. The callback name carries dots (`tool.execute.before`), which
/// are replaced so the file name stays a single path component everywhere.
pub fn sidecar_file_name(callback: &str, n: usize) -> String {
    format!("{}-{n}.json", callback.replace('.', "_"))
}

/// One handler to bridge, plus which measured callback it answers.
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeHandler {
    pub callback: String,
    pub spec: ShimSpec,
}

/// Everything needed to (re)generate the bridge for one OpenCode profile.
pub struct BridgeInput {
    pub xdg_config_home: PathBuf,
    pub state_home: PathBuf,
    pub agentsync_bin: PathBuf,
    /// In stable, deterministic order — this becomes the order sidecars run
    /// in for a callback with more than one handler.
    pub handlers: Vec<BridgeHandler>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRef {
    pub path: PathBuf,
    pub sha256: String,
}

/// The one validity contract tying the bridge, its index, and every sidecar
/// together. See the module docs and [`verify`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeIndex {
    pub opencode_pinned_version: String,
    pub agentsync_bin: PathBuf,
    pub agentsync_bin_sha256: String,
    pub bridge_path: PathBuf,
    pub bridge_sha256: String,
    /// Callback name -> its sidecars, in run order.
    pub events: BTreeMap<String, Vec<SidecarRef>>,
}

impl BridgeIndex {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serialising bridge index")
    }

    pub fn from_json(text: &str) -> Result<BridgeIndex> {
        serde_json::from_str(text).context("parsing bridge index")
    }
}

/// A transaction plus everything needed to verify it landed, or to decide
/// that nothing needs to change at all.
#[derive(Debug)]
pub struct Generated {
    pub transaction: FileTransaction,
    pub index: BridgeIndex,
    pub bridge_path: PathBuf,
    pub index_path: PathBuf,
}

/// Read the current on-disk index, if any. Absence is not an error: it is
/// the ordinary state before the bridge is ever generated.
pub fn read_existing_index(state_home: &Path) -> Option<BridgeIndex> {
    let path = index_path(state_home);
    let text = std::fs::read_to_string(&path).ok()?;
    BridgeIndex::from_json(&text).ok()
}

/// Build the fixed bridge script. Byte-stable for the same
/// `agentsync_bin`/`index_path` pair, so a rerun with no handler change
/// produces identical bytes and a plan/apply pass sees no drift.
///
/// Every callback in [`CALLBACKS`] is present, so a host that loads this
/// module always registers all nine. Which ones actually have sidecars to run
/// is entirely the index's business, read at call time rather than baked in,
/// so regenerating the index alone (no handler shape change) never requires
/// rewriting this file.
pub fn render_bridge_ts(agentsync_bin: &Path, index_path: &Path) -> String {
    let bin = ts_string(&agentsync_bin.to_string_lossy());
    let index = ts_string(&index_path.to_string_lossy());
    format!(
        r#"// Generated by agentsync. Do not edit by hand — regenerate with `agentsync apply`.
//
// Bridges the OpenCode plugin hook surface (measured: config, auth, event,
// chat.message, chat.params, tool.execute.before, tool.execute.after,
// session.idle, session.error) to handlers agentsync tracks, by invoking
// `agentsync hook-shim --spec <sidecar>` and interpreting its one typed
// bridge action object (see src/shim/bridge_output.rs).
//
// tool.execute.before/after are AWAITED: a failed sidecar run throws, which
// stops the intercepted tool call rather than letting it proceed as if
// nothing had gone wrong. Every other callback is fire-and-forget: a failure
// is caught and logged, never left to crash the host or vanish silently.

import {{ spawnSync }} from "node:child_process";
import {{ readFileSync }} from "node:fs";

const AGENTSYNC_BIN = {bin};
const INDEX_PATH = {index};

type BridgeAction = {{
  callback: string;
  fidelity: string;
  message?: string;
  block: boolean;
}};

function loadIndex(): any {{
  return JSON.parse(readFileSync(INDEX_PATH, "utf8"));
}}

function runSidecar(specPath: string, stdin: string): BridgeAction | null {{
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
    return JSON.parse(result.stdout) as BridgeAction;
  }} catch (e) {{
    console.error(`agentsync: malformed bridge output from ${{specPath}}: ${{e}}`);
    return null;
  }}
}}

async function dispatch(
  callback: string,
  ctx: unknown,
  awaited: boolean,
): Promise<BridgeAction | null> {{
  const index = loadIndex();
  const specs: string[] = (index.events?.[callback] ?? []).map((s: any) => s.path);
  const stdin = JSON.stringify(ctx ?? {{}});
  let last: BridgeAction | null = null;
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

async function fireAndForget(callback: string, ctx: unknown): Promise<BridgeAction | null> {{
  try {{
    return await dispatch(callback, ctx, false);
  }} catch (e) {{
    console.error(`agentsync: ${{callback}} failed: ${{e}}`);
    return null;
  }}
}}

export const AgentsyncHooks = async (_ctx: unknown) => ({{
  config: async (input: unknown) => {{
    await fireAndForget("config", input);
  }},
  auth: async (input: unknown) => {{
    await fireAndForget("auth", input);
  }},
  event: async (input: unknown) => {{
    await fireAndForget("event", input);
  }},
  "chat.message": async (input: unknown) => {{
    return await fireAndForget("chat.message", input);
  }},
  "chat.params": async (input: unknown) => {{
    return await fireAndForget("chat.params", input);
  }},
  "tool.execute.before": async (input: unknown) => {{
    const action = await dispatch("tool.execute.before", input, true);
    if (action?.block) {{
      throw new Error(action.message ?? "blocked by agentsync");
    }}
    return action;
  }},
  "tool.execute.after": async (input: unknown) => {{
    return await dispatch("tool.execute.after", input, true);
  }},
  "session.idle": async (input: unknown) => {{
    await fireAndForget("session.idle", input);
  }},
  "session.error": async (input: unknown) => {{
    await fireAndForget("session.error", input);
  }},
}});
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

/// Build the guarded write for one profile, given the previously generated
/// index (if any). Handlers no longer present have their sidecars removed;
/// everything else is a plain guarded write, `Absent` for a new file and
/// `Sha256` of whatever is currently on disk otherwise, so a plan/apply race
/// or manual edit is caught rather than overwritten blind.
///
/// Returns an error, rather than a transaction that would fail at apply time,
/// when the callback name is not one of [`CALLBACKS`], or when the bridge's
/// destination directory already exists and was not created by agentsync —
/// claiming it now would plant an ownership marker into a tree agentsync
/// never made, which defeats the guard the marker exists to provide.
pub fn plan_bridge(input: &BridgeInput, existing: Option<&BridgeIndex>) -> Result<Generated> {
    for handler in &input.handlers {
        if !CALLBACKS.contains(&handler.callback.as_str()) {
            bail!(
                "{:?} is not one of the nine measured OpenCode callbacks",
                handler.callback
            );
        }
        if handler.spec.rewake_message.is_some() || handler.spec.rewake_summary.is_some() {
            bail!(
                "{} configures rewakeMessage/rewakeSummary, which the OpenCode bridge has no \
                 channel to deliver (see src/shim/bridge_output.rs); it is not generated",
                handler.spec.source_id
            );
        }
    }

    let bridge_path = bridge_path(&input.xdg_config_home);
    let index_path = index_path(&input.state_home);
    let specs_dir = specs_dir(&input.state_home);

    let bin_bytes = std::fs::read(&input.agentsync_bin).with_context(|| {
        format!(
            "reading the agentsync binary at {} to record its hash",
            input.agentsync_bin.display()
        )
    })?;

    let bridge_contents = render_bridge_ts(&input.agentsync_bin, &index_path);
    let bridge_sha256 = compute_sha256(bridge_contents.as_bytes());

    let mut events: BTreeMap<String, Vec<SidecarRef>> = BTreeMap::new();
    let mut sidecar_writes: Vec<(PathBuf, String)> = Vec::new();
    let mut counters: BTreeMap<String, usize> = BTreeMap::new();
    for handler in &input.handlers {
        let n = counters.entry(handler.callback.clone()).or_insert(0);
        let name = sidecar_file_name(&handler.callback, *n);
        *n += 1;
        let path = specs_dir.join(name);
        let contents = format!("{}\n", handler.spec.to_json()?);
        let sha256 = compute_sha256(contents.as_bytes());
        events
            .entry(handler.callback.clone())
            .or_default()
            .push(SidecarRef {
                path: path.clone(),
                sha256,
            });
        sidecar_writes.push((path, contents));
    }

    let index = BridgeIndex {
        opencode_pinned_version: PINNED_VERSION.to_string(),
        agentsync_bin: input.agentsync_bin.clone(),
        agentsync_bin_sha256: compute_sha256(&bin_bytes),
        bridge_path: bridge_path.clone(),
        bridge_sha256,
        events,
    };
    let index_contents = format!("{}\n", index.to_json()?);

    let mut tx = FileTransaction::new();

    // The bridge script lives outside `paths::state_dir()` (it must be where
    // OpenCode's own plugin scan looks), so it needs its own ownership proof.
    // `claim_fresh_directory` is legal only when the directory is genuinely
    // absent; if it already exists, the ONLY way this write is legitimate is
    // that a previous run already claimed it (an ancestor `.agentsync-owned`
    // marker). Anything else is another tool's directory, and this is
    // refused rather than planted into.
    if let Some(plugins_dir) = bridge_path.parent() {
        if !plugins_dir.exists() {
            tx = tx.claim_fresh_directory(plugins_dir);
        } else if !is_agentsync_owned(&bridge_path) {
            bail!(
                "{} already exists and was not created by agentsync; refusing to claim it",
                plugins_dir.display()
            );
        }
    }
    tx = tx.write_generated(
        &bridge_path,
        bridge_contents,
        precondition_for(&bridge_path),
    );

    for (path, contents) in sidecar_writes {
        let precondition = precondition_for(&path);
        tx = tx.write_generated(path, contents, precondition);
    }

    // Sidecars from the previous generation that no longer answer any
    // configured handler are removed, guarded by the hash the previous
    // generation actually wrote — never a blind removal, and never one that
    // fires if the file changed after it was selected for removal.
    if let Some(existing) = existing {
        let desired: std::collections::BTreeSet<&PathBuf> =
            index.events.values().flatten().map(|s| &s.path).collect();
        for sidecar in existing.events.values().flatten() {
            if !desired.contains(&sidecar.path) {
                tx = tx.remove_stale_sidecar(
                    sidecar.path.clone(),
                    FilePrecondition::Sha256(sidecar.sha256.clone()),
                );
            }
        }
    }

    tx = tx.write_generated(&index_path, index_contents, precondition_for(&index_path));

    Ok(Generated {
        transaction: tx,
        index,
        bridge_path,
        index_path,
    })
}

fn precondition_for(path: &Path) -> FilePrecondition {
    match std::fs::read(path) {
        Ok(bytes) => FilePrecondition::Sha256(compute_sha256(&bytes)),
        Err(_) => FilePrecondition::Absent,
    }
}

/// Check that every part of the validity contract — bridge, index, every
/// sidecar, the event mapping, the pinned version, and the current binary —
/// still agrees. A single mismatch anywhere invalidates the whole thing: this
/// never reports "partially installed" as healthy.
pub fn verify(index: &BridgeIndex, observed_opencode_version: &str) -> Result<()> {
    check_version(observed_opencode_version).map_err(anyhow::Error::msg)?;
    if index.opencode_pinned_version != PINNED_VERSION {
        bail!(
            "the generated index was built for OpenCode {}, not the pinned {PINNED_VERSION}",
            index.opencode_pinned_version
        );
    }

    let bridge_bytes = std::fs::read(&index.bridge_path)
        .with_context(|| format!("reading generated bridge {}", index.bridge_path.display()))?;
    if compute_sha256(&bridge_bytes) != index.bridge_sha256 {
        bail!(
            "the generated bridge at {} does not match the recorded contract",
            index.bridge_path.display()
        );
    }

    let bin_bytes = std::fs::read(&index.agentsync_bin).with_context(|| {
        format!(
            "reading the agentsync binary at {} recorded in the index",
            index.agentsync_bin.display()
        )
    })?;
    if compute_sha256(&bin_bytes) != index.agentsync_bin_sha256 {
        bail!(
            "the agentsync binary at {} has changed since the bridge was generated \
             (a package manager may have swapped it on upgrade); regenerate the bridge",
            index.agentsync_bin.display()
        );
    }

    for (callback, sidecars) in &index.events {
        if !CALLBACKS.contains(&callback.as_str()) {
            bail!("{callback:?} is not one of the nine measured OpenCode callbacks");
        }
        for sidecar in sidecars {
            let bytes = std::fs::read(&sidecar.path)
                .with_context(|| format!("reading generated sidecar {}", sidecar.path.display()))?;
            if compute_sha256(&bytes) != sidecar.sha256 {
                bail!(
                    "the generated sidecar {} does not match the recorded contract",
                    sidecar.path.display()
                );
            }
        }
    }
    Ok(())
}

/// Whether the bridge for `input` is already installed and matches the
/// current generation contract exactly, so nothing needs to change.
///
/// This deliberately regenerates the contract from `input` and asks whether
/// the same bytes are already on disk, rather than trusting a stale index —
/// the same reasoning `crate::shim::generate::Generated::verify_on_disk`
/// applies to the Codex shim.
pub fn already_converged(input: &BridgeInput, observed_opencode_version: &str) -> bool {
    let existing = read_existing_index(&input.state_home);
    let Ok(generated) = plan_bridge(input, existing.as_ref()) else {
        return false;
    };
    if !generated.transaction.operations.is_empty()
        && generated.transaction.operations.iter().any(|op| {
            !matches!(
                op,
                crate::transaction::FileOperation::Write { precondition, .. }
                | crate::transaction::FileOperation::WriteGenerated { precondition, .. }
                    if matches!(precondition, FilePrecondition::Sha256(_))
            )
        })
    {
        // Any Absent precondition, removal, or directory claim means real
        // work remains: either something is missing, or something must go.
        return false;
    }
    verify(&generated.index, observed_opencode_version).is_ok()
}

/// Build a bare-bones [`ShimSpec`] for `callback`, pinned to the OpenCode
/// bridge's output strategy. Building blocks for whatever caller (currently:
/// this module's own tests) constructs a [`BridgeHandler`] from a detected
/// hook — kept here so every OpenCode sidecar is built exactly the same way.
pub fn spec_for(callback: &str, source_id: &str, command: &str) -> ShimSpec {
    ShimSpec {
        source_id: source_id.to_string(),
        command: command.to_string(),
        plugin_root: None,
        if_pattern: None,
        event: Some(callback.to_string()),
        output_strategy: HookOutputStrategy::OpenCodeV1,
        allowed_output: vec![],
        fold_into_system_message: vec![],
        rewake_message: None,
        rewake_summary: None,
        timeout_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::{FileOperation, TransactionError};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct StateHomeGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl StateHomeGuard {
        fn set(path: &Path) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var("AGENTSYNC_STATE_HOME").ok();
            unsafe { std::env::set_var("AGENTSYNC_STATE_HOME", path) };
            StateHomeGuard {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for StateHomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var("AGENTSYNC_STATE_HOME", v) },
                None => unsafe { std::env::remove_var("AGENTSYNC_STATE_HOME") },
            }
        }
    }

    fn handler(callback: &str, id: &str) -> BridgeHandler {
        BridgeHandler {
            callback: callback.to_string(),
            spec: spec_for(callback, id, "true"),
        }
    }

    fn fake_binary(dir: &Path) -> PathBuf {
        let path = dir.join("agentsync-bin");
        std::fs::write(&path, b"fake agentsync binary bytes").unwrap();
        path
    }

    fn input(tmp: &Path, handlers: Vec<BridgeHandler>) -> BridgeInput {
        BridgeInput {
            xdg_config_home: tmp.join("cfg"),
            state_home: tmp.join("state"),
            agentsync_bin: fake_binary(tmp),
            handlers,
        }
    }

    // -----------------------------------------------------------------
    // Version pin
    // -----------------------------------------------------------------

    #[test]
    fn the_pinned_version_is_accepted() {
        assert!(check_version(PINNED_VERSION).is_ok());
    }

    #[test]
    fn any_other_observed_version_blocks_hook_actions_and_names_it() {
        let err = check_version("1.19.0").unwrap_err();
        assert!(
            err.contains("1.19.0"),
            "must name the observed version: {err}"
        );
        assert!(err.contains(PINNED_VERSION), "must name the pin: {err}");
    }

    // -----------------------------------------------------------------
    // Paths
    // -----------------------------------------------------------------

    #[test]
    fn paths_resolve_under_xdg_config_and_agentsync_state_never_a_hardcoded_home() {
        let xdg = Path::new("/tmp/probe-xdg-config");
        let state = Path::new("/tmp/probe-state");
        assert_eq!(
            bridge_path(xdg),
            Path::new("/tmp/probe-xdg-config/opencode/plugins/agentsync-hooks.ts")
        );
        assert_eq!(
            index_path(state),
            Path::new("/tmp/probe-state/shims/opencode/index.json")
        );
        assert_eq!(
            specs_dir(state),
            Path::new("/tmp/probe-state/shims/opencode/specs")
        );
    }

    #[test]
    fn sidecar_file_names_are_unique_per_position_even_for_identical_callbacks() {
        let a = sidecar_file_name("tool.execute.before", 0);
        let b = sidecar_file_name("tool.execute.before", 1);
        assert_ne!(a, b);
        assert!(!a.contains('.') || a.ends_with(".json"));
    }

    // -----------------------------------------------------------------
    // Bridge rendering: golden coverage of all nine portable events
    // -----------------------------------------------------------------

    #[test]
    fn the_rendered_bridge_exports_the_measured_module_shape() {
        let ts = render_bridge_ts(
            Path::new("/usr/local/bin/agentsync"),
            Path::new("/state/index.json"),
        );
        assert!(
            ts.contains("export const AgentsyncHooks = async (_ctx: unknown) => ("),
            "must match the measured plugin module shape: {ts}"
        );
    }

    #[test]
    fn every_one_of_the_nine_measured_callbacks_is_present_in_the_rendered_bridge() {
        let ts = render_bridge_ts(Path::new("/bin/agentsync"), Path::new("/state/index.json"));
        for callback in CALLBACKS {
            assert!(
                ts.contains(callback),
                "callback {callback:?} missing from generated bridge: {ts}"
            );
        }
    }

    #[test]
    fn awaited_callbacks_can_throw_to_stop_the_operation() {
        let ts = render_bridge_ts(Path::new("/bin/agentsync"), Path::new("/state/index.json"));
        for callback in AWAITED_CALLBACKS {
            let marker = format!("dispatch(\"{callback}\", input, true)");
            assert!(
                ts.contains(&marker),
                "{callback} must be awaited (dispatch(..., true)) so a failed sidecar stops \
                 the operation: {ts}"
            );
        }
        assert!(
            ts.contains("throw new Error(action.message"),
            "tool.execute.before must be able to throw when blocked: {ts}"
        );
    }

    #[test]
    fn fire_and_forget_callbacks_are_caught_and_logged_never_thrown() {
        let ts = render_bridge_ts(Path::new("/bin/agentsync"), Path::new("/state/index.json"));
        for callback in CALLBACKS {
            if AWAITED_CALLBACKS.contains(&callback) {
                continue;
            }
            let marker = format!("fireAndForget(\"{callback}\", input)");
            assert!(
                ts.contains(&marker),
                "{callback} must go through the caught fire-and-forget path: {ts}"
            );
        }
        assert!(
            ts.contains("console.error"),
            "a caught failure must reach structured logging, not vanish: {ts}"
        );
    }

    #[test]
    fn rendering_is_byte_stable_for_the_same_inputs() {
        let a = render_bridge_ts(Path::new("/bin/agentsync"), Path::new("/state/index.json"));
        let b = render_bridge_ts(Path::new("/bin/agentsync"), Path::new("/state/index.json"));
        assert_eq!(a, b, "a rerun with no change must report no drift");
    }

    #[test]
    fn a_path_containing_a_quote_cannot_break_out_of_the_generated_string_literal() {
        let ts = render_bridge_ts(
            Path::new("/weird/\"bin\"/agentsync"),
            Path::new("/state/index.json"),
        );
        // If this were not escaped, the generated file would not even be
        // syntactically valid JS/TS.
        assert!(ts.contains(r#"\"bin\""#), "{ts}");
    }

    // -----------------------------------------------------------------
    // Generation + validity contract
    // -----------------------------------------------------------------

    #[test]
    fn a_fresh_install_claims_the_plugin_directory_and_writes_bridge_index_and_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
            )],
        );
        let mut generated = plan_bridge(&inp, None).unwrap();
        generated.transaction.execute().unwrap();

        assert!(generated.bridge_path.is_file());
        assert!(generated.index_path.is_file());
        let index =
            BridgeIndex::from_json(&std::fs::read_to_string(&generated.index_path).unwrap())
                .unwrap();
        assert_eq!(index.events["tool.execute.before"].len(), 1);
        let sidecar_path = &index.events["tool.execute.before"][0].path;
        assert!(sidecar_path.is_file(), "{sidecar_path:?}");

        verify(&index, PINNED_VERSION).expect("a freshly generated bridge must verify clean");
    }

    #[test]
    fn generation_refuses_a_callback_that_is_not_one_of_the_nine_measured_names() {
        let tmp = tempfile::tempdir().unwrap();
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.retry",
                "demo@mkt:hooks/hooks.json:x:0:0",
            )],
        );
        let err = plan_bridge(&inp, None).unwrap_err().to_string();
        assert!(err.contains("tool.execute.retry"), "{err}");
    }

    #[test]
    fn generation_refuses_a_handler_carrying_rewake_text_the_bridge_cannot_deliver() {
        let tmp = tempfile::tempdir().unwrap();
        let mut h = handler("session.idle", "demo@mkt:hooks/hooks.json:x:0:0");
        h.spec.rewake_message = Some("would rewake here".into());
        let err = plan_bridge(&input(tmp.path(), vec![h]), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("rewakeMessage"), "{err}");
    }

    #[test]
    fn a_second_generation_pass_with_no_handler_change_produces_byte_identical_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:pre_tool_use:0:0",
            )],
        );
        let mut first = plan_bridge(&inp, None).unwrap();
        first.transaction.execute().unwrap();
        let bridge_bytes_1 = std::fs::read(&first.bridge_path).unwrap();

        let existing = read_existing_index(&inp.state_home).unwrap();
        let mut second = plan_bridge(&inp, Some(&existing)).unwrap();
        second.transaction.execute().unwrap();
        let bridge_bytes_2 = std::fs::read(&first.bridge_path).unwrap();

        assert_eq!(bridge_bytes_1, bridge_bytes_2);
        assert!(already_converged(&inp, PINNED_VERSION));
    }

    #[test]
    fn a_handler_removed_from_the_desired_set_has_its_stale_sidecar_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp_with_two = input(
            tmp.path(),
            vec![
                handler("tool.execute.before", "demo@mkt:hooks/hooks.json:a:0:0"),
                handler("tool.execute.after", "demo@mkt:hooks/hooks.json:b:0:0"),
            ],
        );
        let mut first = plan_bridge(&inp_with_two, None).unwrap();
        first.transaction.execute().unwrap();
        let stale_sidecar = read_existing_index(&inp_with_two.state_home)
            .unwrap()
            .events["tool.execute.after"][0]
            .path
            .clone();
        assert!(stale_sidecar.is_file());

        let inp_with_one = BridgeInput {
            xdg_config_home: inp_with_two.xdg_config_home.clone(),
            state_home: inp_with_two.state_home.clone(),
            agentsync_bin: inp_with_two.agentsync_bin.clone(),
            handlers: vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:a:0:0",
            )],
        };
        let existing = read_existing_index(&inp_with_one.state_home).unwrap();
        let mut second = plan_bridge(&inp_with_one, Some(&existing)).unwrap();
        second.transaction.execute().unwrap();

        assert!(
            !stale_sidecar.exists(),
            "the sidecar for the removed handler must be gone: {stale_sidecar:?}"
        );
        let index2 =
            BridgeIndex::from_json(&std::fs::read_to_string(&second.index_path).unwrap()).unwrap();
        assert!(!index2.events.contains_key("tool.execute.after"));
    }

    #[test]
    fn a_preexisting_unowned_plugins_directory_is_never_claimed_or_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:a:0:0",
            )],
        );
        // Simulate a real, pre-existing OpenCode plugins directory that
        // agentsync never created — a real user plugin lives there.
        let plugins_dir = bridge_path(&inp.xdg_config_home)
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(plugins_dir.join("someone-elses-plugin.ts"), "// not ours").unwrap();

        let err = plan_bridge(&inp, None).unwrap_err().to_string();
        assert!(err.contains("not created by agentsync"), "{err}");
        assert!(
            !bridge_path(&inp.xdg_config_home).exists(),
            "the bridge must not have been written into an unowned directory"
        );
        assert!(
            !plugins_dir.join(".agentsync-owned").exists(),
            "an ownership marker must never be planted into a pre-existing directory"
        );
    }

    #[test]
    fn a_tampered_sidecar_invalidates_the_whole_contract_not_just_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:a:0:0",
            )],
        );
        let mut generated = plan_bridge(&inp, None).unwrap();
        generated.transaction.execute().unwrap();
        let index = read_existing_index(&inp.state_home).unwrap();
        verify(&index, PINNED_VERSION).unwrap();

        let sidecar_path = index.events["tool.execute.before"][0].path.clone();
        std::fs::write(&sidecar_path, "{ tampered }").unwrap();

        let err = verify(&index, PINNED_VERSION).unwrap_err().to_string();
        assert!(err.contains("sidecar"), "{err}");
    }

    #[test]
    fn a_tampered_bridge_file_invalidates_the_whole_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(tmp.path(), vec![handler("session.idle", "demo@mkt:x:0:0")]);
        let mut generated = plan_bridge(&inp, None).unwrap();
        generated.transaction.execute().unwrap();
        let index = read_existing_index(&inp.state_home).unwrap();

        std::fs::write(&generated.bridge_path, "// tampered").unwrap();
        let err = verify(&index, PINNED_VERSION).unwrap_err().to_string();
        assert!(err.contains("bridge"), "{err}");
    }

    #[test]
    fn a_binary_that_moved_since_generation_invalidates_the_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(tmp.path(), vec![handler("session.error", "demo@mkt:x:0:0")]);
        let mut generated = plan_bridge(&inp, None).unwrap();
        generated.transaction.execute().unwrap();
        let index = read_existing_index(&inp.state_home).unwrap();

        // The package manager swapped the binary at the same path.
        std::fs::write(&inp.agentsync_bin, b"a different binary now").unwrap();
        let err = verify(&index, PINNED_VERSION).unwrap_err().to_string();
        assert!(err.contains("binary"), "{err}");
    }

    #[test]
    fn a_wrong_observed_version_invalidates_the_contract_even_if_every_file_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(tmp.path(), vec![handler("chat.message", "demo@mkt:x:0:0")]);
        let mut generated = plan_bridge(&inp, None).unwrap();
        generated.transaction.execute().unwrap();
        let index = read_existing_index(&inp.state_home).unwrap();

        let err = verify(&index, "1.19.0").unwrap_err().to_string();
        assert!(err.contains("1.19.0"), "{err}");
    }

    #[test]
    fn a_plan_apply_race_on_a_sidecar_is_rejected_without_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = StateHomeGuard::set(&tmp.path().join("state"));
        let inp = input(
            tmp.path(),
            vec![handler(
                "tool.execute.before",
                "demo@mkt:hooks/hooks.json:a:0:0",
            )],
        );
        let mut first = plan_bridge(&inp, None).unwrap();
        first.transaction.execute().unwrap();
        let index = read_existing_index(&inp.state_home).unwrap();
        let sidecar_path = index.events["tool.execute.before"][0].path.clone();

        // Something else changed the sidecar between plan and apply.
        std::fs::write(&sidecar_path, "{}").unwrap();

        // Re-plan against the ORIGINAL (now stale) preconditions by reusing
        // the first transaction's ops directly, simulating a stale plan
        // executed after the race.
        let mut stale = plan_bridge(&inp, Some(&index)).unwrap();
        // Force the sidecar write back to the stale precondition captured
        // before the race, the way an already-built stale plan would carry it.
        for op in &mut stale.transaction.operations {
            if let FileOperation::WriteGenerated {
                path, precondition, ..
            } = op
                && path == &sidecar_path
            {
                *precondition = FilePrecondition::Sha256(compute_sha256(
                    std::fs::read(&sidecar_path).unwrap_or_default().as_slice(),
                ));
                // Corrupt the captured precondition to simulate staleness.
                *precondition = FilePrecondition::Sha256("stale-hash-does-not-match".into());
            }
        }
        let err = stale.transaction.execute().unwrap_err();
        assert!(
            matches!(
                err,
                TransactionError::PreconditionFailed { .. }
                    | TransactionError::TamperedArtifact { .. }
                    | TransactionError::RollbackFailed { .. }
            ),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar_path).unwrap(),
            "{}",
            "the race must not have been overwritten"
        );
    }

    #[test]
    fn every_measured_callback_name_has_a_recognised_hook_fidelity() {
        for callback in CALLBACKS {
            // This is the cross-check with OW-007: a callback name here that
            // fidelity does not recognise would silently fall back to
            // "unsupported" at runtime despite the bridge claiming to answer
            // it, which is exactly the drift this test exists to catch.
            let _ = crate::core::model::opencode_family_hook_fidelity(callback);
        }
        // config/auth/event are measured to have no output channel at all —
        // asserted explicitly, since a silent `None` there could otherwise
        // just as easily mean "not yet classified".
        for callback in ["config", "auth", "event"] {
            assert_eq!(
                crate::core::model::opencode_family_hook_fidelity(callback),
                None,
                "{callback} must have no claimed fidelity: there is nowhere for its output to go"
            );
        }
    }
}
