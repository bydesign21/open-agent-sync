//! Npm and local plugin targets for the OpenCode family.
//!
//! Measured against the pinned runtimes (see `docs/open-work.md`, "Verified
//! runtime contracts"):
//!
//! * the host's `plugin` config key is a JSON **array**, never an object;
//! * **both** `<config>/plugin/` and `<config>/plugins/` directories are
//!   scanned;
//! * any exported async function in a module file is treated as a plugin.
//!
//! Neither host resolves a bare marketplace plugin name, and neither has a
//! marketplace for OpenCode/Kilo plugins at all, so an npm or local mapping
//! must come from an explicit manifest target — this module never guesses
//! one from what it finds on disk.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::model::{PluginConfigSource, PluginOccurrence, PluginTargetState, ScopeKind};
use crate::transaction::{ConfigOrigin, ConfigScope, compute_sha256};

use super::layers::{self, Env, Family};

/// One entry read from a host's `plugin` array: the identity it names, plus
/// the exact JSON it round-trips through the JSONC editor as. A plain string
/// entry's identity is itself; a `[spec, options]` tuple's identity is the
/// spec, and `options` (which may contain JSON `null`) is preserved as exact
/// JSON text via the entry's own `raw` value rather than re-derived through
/// TOML.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginArrayEntry {
    pub identity: String,
    pub raw: Value,
}

/// Both directory names the runtime scans, in a fixed order.
pub const PLUGIN_DIR_NAMES: [&str; 2] = ["plugin", "plugins"];

/// Module file extensions the runtime can load as a plugin.
const MODULE_EXTENSIONS: [&str; 4] = ["ts", "js", "mjs", "cjs"];

/// Read the `plugin` array out of a parsed JSONC document value. Absent is
/// empty, not invented. A `plugin` key that is not an array is reported,
/// never silently coerced into one — measured fact: it is always an array.
pub fn read_plugin_array(root: &Value) -> Result<Vec<PluginArrayEntry>, String> {
    match root.get("plugin") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                entry_identity(item)
                    .map(|identity| PluginArrayEntry {
                        identity,
                        raw: item.clone(),
                    })
                    .ok_or_else(|| format!("plugin entry is not a recognized shape: {item}"))
            })
            .collect(),
        Some(other) => Err(format!(
            "`plugin` must be a JSON array, not {}; measured runtime behavior never accepts an object here",
            kind_name(other)
        )),
    }
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "an object",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::Null => "null",
        Value::Array(_) => "an array",
    }
}

fn entry_identity(item: &Value) -> Option<String> {
    match item {
        Value::String(s) => Some(s.clone()),
        Value::Array(tuple) => tuple.first()?.as_str().map(str::to_string),
        _ => None,
    }
}

/// Build the exact `plugin` array JSON text with `identity` set to `raw`,
/// replacing any existing entry with the same identity and otherwise
/// appending. Every other entry's exact JSON text — including a `null` inside
/// a tuple's options — survives untouched.
pub fn upsert_plugin_array_json(
    current: &[PluginArrayEntry],
    identity: &str,
    raw: Value,
) -> String {
    let mut items: Vec<Value> = current
        .iter()
        .filter(|e| e.identity != identity)
        .map(|e| e.raw.clone())
        .collect();
    items.push(raw);
    Value::Array(items).to_string()
}

/// Build the exact `plugin` array JSON text with `identity` removed.
pub fn remove_from_plugin_array_json(current: &[PluginArrayEntry], identity: &str) -> String {
    let items: Vec<Value> = current
        .iter()
        .filter(|e| e.identity != identity)
        .map(|e| e.raw.clone())
        .collect();
    Value::Array(items).to_string()
}

/// The measured heuristic: any exported async function makes a module file a
/// plugin. Covers `export async function`, `export default async function`,
/// and `export const NAME = async (...)`.
pub fn looks_like_a_plugin_module(source: &str) -> bool {
    if source.contains("export async function") || source.contains("export default async function")
    {
        return true;
    }
    source.split("export const").skip(1).any(|rest| {
        rest.split_once('=')
            .is_some_and(|(_, value)| value.trim_start().starts_with("async"))
    })
}

/// Every plugin-shaped file directly inside `<dir>/plugin/` and
/// `<dir>/plugins/`. Not recursive: this mirrors what the runtime scans.
/// Purely a filesystem read — it does not consult the manifest, so it finds
/// exactly what is really there, including files no target names.
pub fn scan_plugin_dirs(dir: &Path, scope: ScopeKind) -> Vec<(String, PluginOccurrence)> {
    let mut out = Vec::new();
    for dir_name in PLUGIN_DIR_NAMES {
        let candidate = dir.join(dir_name);
        let Ok(entries) = std::fs::read_dir(&candidate) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !MODULE_EXTENSIONS.contains(&ext) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if !looks_like_a_plugin_module(&String::from_utf8_lossy(&bytes)) {
                continue;
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push((
                name,
                PluginOccurrence::File {
                    path: path.clone(),
                    sha256: compute_sha256(&bytes),
                    scope,
                },
            ));
        }
    }
    out
}

/// Where agentsync copies an explicit local target to, per family — a
/// host-owned name so an unowned destination is never silently replaced.
///
/// * OpenCode: `<profile>/plugins/agentsync-<name>.<ext>`
/// * Kilo: `<profile>/plugin/agentsync-<name>.<ext>`
pub fn host_owned_local_path(
    family: Family,
    profile_dir: &Path,
    name: &str,
    source: &Path,
) -> PathBuf {
    let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("ts");
    let file_name = format!("agentsync-{name}.{ext}");
    match family {
        Family::OpenCode => profile_dir.join("plugins").join(file_name),
        Family::Kilo => profile_dir.join("plugin").join(file_name),
    }
}

fn to_config_scope(scope: ScopeKind) -> ConfigScope {
    match scope {
        ScopeKind::User => ConfigScope::Global,
        ScopeKind::Project => ConfigScope::Project,
        ScopeKind::Local => ConfigScope::Local,
    }
}

/// Read one scope's `plugin`-array-bearing config file. Always succeeds with
/// `exists: false` for a missing file — a missing file is a legitimate target
/// to create, not an error. Only a file that exists but cannot be parsed, or
/// whose `plugin` key is not an array, is an error.
fn read_config_source(
    path: &Path,
    scope: ScopeKind,
    precedence: u32,
) -> Result<PluginConfigSource, String> {
    if !path.is_file() {
        return Ok(PluginConfigSource {
            origin: ConfigOrigin::new(
                path,
                to_config_scope(scope),
                precedence,
                compute_sha256(b"{}"),
            ),
            entries: Vec::new(),
        });
    }
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let hash = compute_sha256(&bytes);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let doc = crate::jsonc::parse(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let entries = read_plugin_array(&doc.value)?;
    Ok(PluginConfigSource {
        origin: ConfigOrigin::new(path, to_config_scope(scope), precedence, hash),
        entries: entries.into_iter().map(|e| (e.identity, e.raw)).collect(),
    })
}

/// Whether `path` currently exists as a real file. `PluginConfigSource`
/// itself carries only what a `ConfigTransaction` needs (origin + entries);
/// this is checked separately so planning code can choose `Absent` vs a
/// `Sha256` precondition without re-deriving it from the hash of `"{}"`.
pub fn config_source_exists(path: &Path) -> bool {
    path.is_file()
}

/// Read every OpenCode-family npm/local plugin fact reachable for `family` —
/// user-scope and (when `repo` is given) project-scope config sources, their
/// profile directories, and every occurrence found in either the config array
/// or a host-owned plugin file on disk. Manifest-independent: this reports
/// exactly what exists, and mapping it to a manifest target is the caller's
/// job.
pub fn read_full_state(family: Family, env: &Env, repo: Option<&Path>) -> PluginTargetState {
    let mut state = PluginTargetState::default();
    let discovered = layers::discover(family, env, repo);

    // User (global) scope: the profile directory agentsync would write into.
    let user_profile = discovered.active_profile_dir.clone();
    if let Some(global) = discovered.global_write_target()
        && let Some(path) = global.path()
        && let Ok(source) = read_config_source(path, ScopeKind::User, global.precedence)
    {
        for (identity, _) in &source.entries {
            state
                .occurrences
                .entry(identity.clone())
                .or_default()
                .push(PluginOccurrence::Config(source.origin.clone()));
        }
        state.config.insert(ScopeKind::User, source);
    }
    state
        .profile_dir
        .insert(ScopeKind::User, user_profile.clone());
    for (name, occurrence) in scan_plugin_dirs(&user_profile, ScopeKind::User) {
        state.occurrences.entry(name).or_default().push(occurrence);
    }

    // Project scope: the highest-precedence project layer, when a repo was
    // given at all.
    if let Some(project_layer) = discovered
        .layers
        .iter()
        .find(|l| l.scope == crate::transaction::ConfigScope::Project)
        && let Some(path) = project_layer.path()
    {
        let project_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Ok(source) = read_config_source(path, ScopeKind::Project, project_layer.precedence) {
            for (identity, _) in &source.entries {
                state
                    .occurrences
                    .entry(identity.clone())
                    .or_default()
                    .push(PluginOccurrence::Config(source.origin.clone()));
            }
            state.config.insert(ScopeKind::Project, source);
        }
        state
            .profile_dir
            .insert(ScopeKind::Project, project_dir.clone());
        for (name, occurrence) in scan_plugin_dirs(&project_dir, ScopeKind::Project) {
            state.occurrences.entry(name).or_default().push(occurrence);
        }
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_key_absent_is_empty_not_invented() {
        let root = json!({"model": "some/model"});
        assert!(read_plugin_array(&root).unwrap().is_empty());
    }

    #[test]
    fn plugin_key_as_an_object_is_rejected_rather_than_coerced() {
        let root = json!({"plugin": {"not": "an-array"}});
        let err = read_plugin_array(&root).unwrap_err();
        assert!(err.contains("array"), "{err}");
    }

    #[test]
    fn a_plain_string_entry_is_its_own_identity() {
        let root = json!({"plugin": ["@company/pkg@1.4.2", "./local-plugin.ts"]});
        let entries = read_plugin_array(&root).unwrap();
        let identities: Vec<&str> = entries.iter().map(|e| e.identity.as_str()).collect();
        assert_eq!(identities, vec!["@company/pkg@1.4.2", "./local-plugin.ts"]);
    }

    #[test]
    fn a_tuple_entrys_identity_is_the_spec_and_options_survive_as_exact_json() {
        let root = json!({"plugin": [["pkg", {"apiKey": null}]]});
        let entries = read_plugin_array(&root).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].identity, "pkg");
        assert_eq!(
            entries[0].raw.to_string(),
            json!(["pkg", {"apiKey": null}]).to_string(),
            "the null inside the tuple's options must round-trip as exact JSON text"
        );
    }

    #[test]
    fn upsert_replaces_the_matching_identity_and_preserves_the_rest_verbatim() {
        let current = read_plugin_array(&json!({
            "plugin": ["kept-plugin", ["other", {"n": null}]]
        }))
        .unwrap();
        let text = upsert_plugin_array_json(&current, "other", json!(["other", {"n": 5}]));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!(["kept-plugin", ["other", {"n": 5}]]));
    }

    #[test]
    fn upsert_appends_a_new_identity() {
        let current = read_plugin_array(&json!({"plugin": ["existing"]})).unwrap();
        let text = upsert_plugin_array_json(&current, "new-one", json!("new-one"));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!(["existing", "new-one"]));
    }

    #[test]
    fn remove_drops_only_the_named_identity() {
        let current =
            read_plugin_array(&json!({"plugin": ["keep", "drop-me", ["drop-me-too", {}]]}))
                .unwrap();
        let text = remove_from_plugin_array_json(&current, "drop-me");
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!(["keep", ["drop-me-too", {}]]));
    }

    #[test]
    fn any_exported_async_function_shape_is_recognized() {
        assert!(looks_like_a_plugin_module(
            "export async function AgentsyncHooks(ctx) { return {}; }"
        ));
        assert!(looks_like_a_plugin_module(
            "export default async function (ctx) { return {}; }"
        ));
        assert!(looks_like_a_plugin_module(
            "export const AgentsyncHooks = async (ctx) => ({})"
        ));
    }

    #[test]
    fn a_module_with_no_exported_async_function_is_not_a_plugin() {
        assert!(!looks_like_a_plugin_module(
            "export function notAsync() { return 1; }"
        ));
        assert!(!looks_like_a_plugin_module("const x = 1;"));
    }

    #[test]
    fn scan_reads_both_plugin_and_plugins_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("plugin")).unwrap();
        std::fs::create_dir_all(tmp.path().join("plugins")).unwrap();
        std::fs::write(
            tmp.path().join("plugin/one.ts"),
            "export async function one() {}",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("plugins/two.ts"),
            "export async function two() {}",
        )
        .unwrap();
        std::fs::write(tmp.path().join("plugins/not-a-plugin.ts"), "const x = 1;").unwrap();

        let found = scan_plugin_dirs(tmp.path(), ScopeKind::User);
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"one"), "{names:?}");
        assert!(names.contains(&"two"), "{names:?}");
        assert!(
            !names.contains(&"not-a-plugin"),
            "a file with no exported async function must not be treated as a plugin: {names:?}"
        );
    }

    #[test]
    fn host_owned_paths_differ_by_family_directory_name() {
        let profile = PathBuf::from("/profile");
        let opencode = host_owned_local_path(
            Family::OpenCode,
            &profile,
            "local-policy",
            Path::new("plugins/local-policy.ts"),
        );
        let kilo = host_owned_local_path(
            Family::Kilo,
            &profile,
            "local-policy",
            Path::new("plugins/local-policy.ts"),
        );
        assert_eq!(
            opencode,
            PathBuf::from("/profile/plugins/agentsync-local-policy.ts")
        );
        assert_eq!(
            kilo,
            PathBuf::from("/profile/plugin/agentsync-local-policy.ts")
        );
        assert_ne!(
            opencode, kilo,
            "npm and local identities, and OpenCode vs Kilo destinations, must never collide"
        );
    }

    #[test]
    fn missing_config_file_reads_as_absent_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("opencode.jsonc");
        let source = read_config_source(&path, ScopeKind::User, 70).unwrap();
        assert!(source.entries.is_empty());
        assert!(!config_source_exists(&path));
    }

    #[test]
    fn read_full_state_finds_a_global_npm_entry_as_one_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_home = tmp.path().join("cfg");
        std::fs::create_dir_all(cfg_home.join("opencode")).unwrap();
        std::fs::write(
            cfg_home.join("opencode/opencode.jsonc"),
            r#"{"plugin": ["@company/opencode-security@1.4.2"]}"#,
        )
        .unwrap();
        let env = Env::new(tmp.path().join("home"))
            .set("XDG_CONFIG_HOME", cfg_home.display().to_string());

        let state = read_full_state(Family::OpenCode, &env, None);

        let occurrences = state
            .occurrences
            .get("@company/opencode-security@1.4.2")
            .expect("occurrence recorded");
        assert_eq!(occurrences.len(), 1);
        assert!(matches!(occurrences[0], PluginOccurrence::Config(_)));
    }

    #[test]
    fn read_full_state_keeps_global_and_project_occurrences_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_home = tmp.path().join("cfg");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(cfg_home.join("opencode")).unwrap();
        std::fs::create_dir_all(repo.join(".opencode")).unwrap();
        std::fs::write(
            cfg_home.join("opencode/opencode.jsonc"),
            r#"{"plugin": ["dup-plugin"]}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(".opencode/opencode.jsonc"),
            r#"{"plugin": ["dup-plugin"]}"#,
        )
        .unwrap();
        let env = Env::new(tmp.path().join("home"))
            .set("XDG_CONFIG_HOME", cfg_home.display().to_string());

        let state = read_full_state(Family::OpenCode, &env, Some(&repo));

        let occurrences = state
            .occurrences
            .get("dup-plugin")
            .expect("occurrence recorded");
        assert_eq!(
            occurrences.len(),
            2,
            "a global and a project definition are both kept, never collapsed"
        );
    }
}
