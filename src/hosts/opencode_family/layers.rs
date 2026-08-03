//! Config layer discovery and origin tracking for the OpenCode family.
//!
//! # Verified precedence
//!
//! Every ordering below was proved against the pinned runtimes by writing a
//! distinguishing key into each candidate layer and reading back
//! `<host> debug config`. None of it is taken from documentation.
//!
//! Highest precedence first:
//!
//! | Rank | Layer | Writable | Evidence |
//! |---|---|---|---|
//! | 1 | `<PREFIX>_CONFIG_CONTENT` (inline) | no | inline value beat a project file |
//! | 2 | `<PREFIX>_CONFIG_DIR` profile | yes | profile `model` beat project `model` |
//! | 3 | project dir (`.opencode` / `.kilo`, `.kilocode`) | yes | project beat default global |
//! | 4 | `<PREFIX>_CONFIG` explicit file | yes | project `model` beat explicit file |
//! | 5 | default XDG global config | yes | lowest observed |
//!
//! Within a single directory, `<id>.jsonc` outranks `<id>.json`; both are read
//! and deep-merged, so a partial higher layer does not erase lower fields.
//!
//! Two further verified facts shape this module:
//!
//! * `<PREFIX>_CONFIG_DIR` **adds** a layer. It does not replace the default
//!   global config, whose values still resolve underneath it.
//! * `<host> debug paths` reports the XDG config directory even when
//!   `<PREFIX>_CONFIG_DIR` is set. The active profile therefore cannot be read
//!   back from `debug paths` and must be resolved from the environment here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::transaction::ConfigScope;

/// Which host in the OpenCode family a lookup is for.
///
/// The two hosts share an engine but never share values. Every path, prefix,
/// and directory name is selected from this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    OpenCode,
    Kilo,
}

impl Family {
    /// Directory and config-file base name (`~/.config/<id>/<id>.jsonc`).
    pub fn id(self) -> &'static str {
        match self {
            Family::OpenCode => "opencode",
            Family::Kilo => "kilo",
        }
    }

    /// Map a descriptor/host name back to its family. `None` for every host
    /// outside the OpenCode family.
    pub fn from_host_name(name: &str) -> Option<Self> {
        match name {
            "opencode" => Some(Family::OpenCode),
            "kilo" => Some(Family::Kilo),
            _ => None,
        }
    }

    /// Environment variable prefix. Verified present in both binaries.
    pub fn env_prefix(self) -> &'static str {
        match self {
            Family::OpenCode => "OPENCODE",
            Family::Kilo => "KILO",
        }
    }

    /// Project config directory names, highest precedence first.
    ///
    /// Kilo reads its current `.kilo` name and the documented legacy
    /// `.kilocode` name; both were verified to resolve. Kilo must never read
    /// `.opencode`, and OpenCode must never read either Kilo name.
    pub fn project_dir_names(self) -> &'static [&'static str] {
        match self {
            Family::OpenCode => &[".opencode"],
            Family::Kilo => &[".kilo", ".kilocode"],
        }
    }

    fn env(self, suffix: &str) -> String {
        format!("{}_{}", self.env_prefix(), suffix)
    }
}

/// Where a layer's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    /// A file on disk.
    File(PathBuf),
    /// Config text supplied inline through `<PREFIX>_CONFIG_CONTENT`.
    ///
    /// Observable but never writable: there is no file to edit.
    Inline,
}

/// Why a layer cannot be written by agentsync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalControl {
    /// Supplied inline through the environment.
    Inline,
    /// Delivered by a remote or cloud control plane.
    Remote,
    /// Installed and owned by a managed deployment.
    Managed,
}

impl ExternalControl {
    pub fn reason(&self) -> &'static str {
        match self {
            ExternalControl::Inline => "supplied inline through the environment",
            ExternalControl::Remote => "delivered by a remote control plane",
            ExternalControl::Managed => "owned by a managed deployment",
        }
    }
}

/// One discovered configuration layer.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLayer {
    pub source: LayerSource,
    pub scope: ConfigScope,
    /// Lower wins, matching [`crate::transaction::ConfigOrigin`].
    pub precedence: u32,
    /// Whether the layer is present. An absent file is still reported so the
    /// caller can create it, but it never contributes a value and never gets
    /// an invented source.
    pub exists: bool,
    pub writable: bool,
    pub external_control: Option<ExternalControl>,
}

impl ConfigLayer {
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            LayerSource::File(path) => Some(path),
            LayerSource::Inline => None,
        }
    }
}

/// Injectable environment, so tests never depend on the real process
/// environment and never race each other through `std::env::set_var`.
#[derive(Debug, Clone, Default)]
pub struct Env {
    home: PathBuf,
    vars: BTreeMap<String, String>,
}

impl Env {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            vars: BTreeMap::new(),
        }
    }

    /// Snapshot the real process environment.
    pub fn from_process() -> Self {
        Self {
            home: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            vars: std::env::vars().collect(),
        }
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// A `1`/`true` style flag. Anything else, including absent, is false.
    fn flag(&self, key: &str) -> bool {
        matches!(
            self.var(key).map(str::to_ascii_lowercase).as_deref(),
            Some("1") | Some("true") | Some("yes")
        )
    }

    /// `$XDG_CONFIG_HOME`, defaulting to `~/.config`.
    pub fn xdg_config_home(&self) -> PathBuf {
        match self.var("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => self.home.join(".config"),
        }
    }
}

/// Precedence bands. Lower wins. Gaps leave room for the `.jsonc` / `.json`
/// split and for extra project directory names.
const PRECEDENCE_INLINE: u32 = 0;
const PRECEDENCE_PROFILE_DIR: u32 = 10;
const PRECEDENCE_PROJECT: u32 = 20;
const PRECEDENCE_EXPLICIT_FILE: u32 = 60;
const PRECEDENCE_GLOBAL: u32 = 70;

/// Config file base names within a directory, highest precedence first.
/// `.jsonc` outranking `.json` was verified directly.
const FORMATS: [&str; 2] = ["jsonc", "json"];

/// The result of discovering every layer for one host.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyLayers {
    pub family: Family,
    /// The directory agentsync writes global changes into. This is the
    /// `<PREFIX>_CONFIG_DIR` profile when set, otherwise the XDG config dir.
    pub active_profile_dir: PathBuf,
    /// `<PREFIX>_PURE` is set. External plugins and hooks do not load, so they
    /// must be reported disabled and never healthy.
    pub pure: bool,
    /// `<PREFIX>_DISABLE_PROJECT_CONFIG` is set, so no project layer is read.
    pub project_config_disabled: bool,
    /// Every candidate layer, highest precedence first, including absent ones.
    pub layers: Vec<ConfigLayer>,
}

impl FamilyLayers {
    /// Layers that actually exist, highest precedence first. Absent files are
    /// excluded: they contribute no value and must not invent a source.
    pub fn existing(&self) -> impl Iterator<Item = &ConfigLayer> {
        self.layers.iter().filter(|layer| layer.exists)
    }

    /// The layer a global write should target: the highest-precedence writable
    /// file inside the active profile directory.
    pub fn global_write_target(&self) -> Option<&ConfigLayer> {
        self.layers.iter().find(|layer| {
            layer.writable
                && layer.scope == ConfigScope::Global
                && layer
                    .path()
                    .and_then(Path::parent)
                    .is_some_and(|parent| parent == self.active_profile_dir)
        })
    }

    /// Layers that are observable but cannot be written.
    pub fn external(&self) -> impl Iterator<Item = &ConfigLayer> {
        self.layers
            .iter()
            .filter(|layer| layer.exists && !layer.writable)
    }
}

/// Discover every configuration layer for `family`.
///
/// `repo` is the project root, when one applies. Passing `None` discovers only
/// global layers.
pub fn discover(family: Family, env: &Env, repo: Option<&Path>) -> FamilyLayers {
    let mut layers = Vec::new();

    // 1. Inline config from the environment. Observable, never writable.
    if env.var(&family.env("CONFIG_CONTENT")).is_some() {
        layers.push(ConfigLayer {
            source: LayerSource::Inline,
            scope: ConfigScope::Global,
            precedence: PRECEDENCE_INLINE,
            exists: true,
            writable: false,
            external_control: Some(ExternalControl::Inline),
        });
    }

    // 2. The `<PREFIX>_CONFIG_DIR` profile, when set. Verified to outrank the
    //    project layer, and to add a layer rather than replace the global one.
    let profile_dir = env.var(&family.env("CONFIG_DIR")).map(PathBuf::from);
    if let Some(dir) = &profile_dir {
        push_dir_layers(
            &mut layers,
            family,
            dir,
            ConfigScope::Global,
            PRECEDENCE_PROFILE_DIR,
        );
    }

    // 3. Project layers, unless disabled.
    let project_config_disabled = env.flag(&family.env("DISABLE_PROJECT_CONFIG"));
    if let (Some(repo), false) = (repo, project_config_disabled) {
        for (index, dir_name) in family.project_dir_names().iter().enumerate() {
            push_dir_layers(
                &mut layers,
                family,
                &repo.join(dir_name),
                ConfigScope::Project,
                PRECEDENCE_PROJECT + (index as u32 * FORMATS.len() as u32),
            );
        }
    }

    // 4. An explicit `<PREFIX>_CONFIG` file. Verified to rank below project.
    if let Some(file) = env.var(&family.env("CONFIG")) {
        let path = PathBuf::from(file);
        layers.push(ConfigLayer {
            exists: path.is_file(),
            source: LayerSource::File(path),
            scope: ConfigScope::Global,
            precedence: PRECEDENCE_EXPLICIT_FILE,
            writable: true,
            external_control: None,
        });
    }

    // 5. The default XDG global config, always present as a candidate.
    let xdg_dir = env.xdg_config_home().join(family.id());
    push_dir_layers(
        &mut layers,
        family,
        &xdg_dir,
        ConfigScope::Global,
        PRECEDENCE_GLOBAL,
    );

    layers.sort_by_key(|layer| layer.precedence);

    FamilyLayers {
        family,
        active_profile_dir: profile_dir.unwrap_or(xdg_dir),
        pure: env.flag(&family.env("PURE")),
        project_config_disabled,
        layers,
    }
}

/// Push the `<id>.jsonc` then `<id>.json` candidates for one directory.
fn push_dir_layers(
    layers: &mut Vec<ConfigLayer>,
    family: Family,
    dir: &Path,
    scope: ConfigScope,
    base_precedence: u32,
) {
    for (offset, format) in FORMATS.iter().enumerate() {
        let path = dir.join(format!("{}.{format}", family.id()));
        layers.push(ConfigLayer {
            exists: path.is_file(),
            source: LayerSource::File(path),
            scope: scope.clone(),
            precedence: base_precedence + offset as u32,
            writable: true,
            external_control: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn env_at(root: &Path) -> Env {
        Env::new(root.join("home")).set("XDG_CONFIG_HOME", root.join("cfg").display().to_string())
    }

    #[test]
    fn each_host_uses_its_own_xdg_root_and_never_the_other() {
        let dir = tmp();
        let env = env_at(dir.path());
        let opencode = discover(Family::OpenCode, &env, None);
        let kilo = discover(Family::Kilo, &env, None);

        assert!(
            opencode
                .active_profile_dir
                .ends_with(Path::new("cfg/opencode"))
        );
        assert!(kilo.active_profile_dir.ends_with(Path::new("cfg/kilo")));
        for layer in &opencode.layers {
            let shown = layer.path().unwrap().display().to_string();
            assert!(
                !shown.contains("kilo"),
                "an OpenCode layer must never reference Kilo: {shown}"
            );
        }
        for layer in &kilo.layers {
            let shown = layer.path().unwrap().display().to_string();
            assert!(
                !shown.contains("opencode"),
                "a Kilo layer must never reference OpenCode: {shown}"
            );
        }
    }

    #[test]
    fn jsonc_outranks_json_in_the_same_directory() {
        let dir = tmp();
        let env = env_at(dir.path());
        write(&dir.path().join("cfg/opencode/opencode.jsonc"), "{}");
        write(&dir.path().join("cfg/opencode/opencode.json"), "{}");

        let found = discover(Family::OpenCode, &env, None);
        let existing: Vec<_> = found.existing().collect();
        assert_eq!(existing.len(), 2, "both formats are read and merged");
        assert!(
            existing[0].path().unwrap().ends_with("opencode.jsonc"),
            "jsonc must outrank json, got {:?}",
            existing[0].path()
        );
        assert!(existing[0].precedence < existing[1].precedence);
    }

    #[test]
    fn project_layers_outrank_the_default_global_layer() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        let env = env_at(dir.path());
        write(&dir.path().join("cfg/opencode/opencode.json"), "{}");
        write(&repo.join(".opencode/opencode.json"), "{}");

        let found = discover(Family::OpenCode, &env, Some(&repo));
        let existing: Vec<_> = found.existing().collect();
        assert_eq!(existing[0].scope, ConfigScope::Project);
        assert!(existing[0].precedence < existing[1].precedence);
    }

    #[test]
    fn kilo_reads_its_legacy_directory_and_ignores_opencode() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        let env = env_at(dir.path());
        write(&repo.join(".kilo/kilo.json"), "{}");
        write(&repo.join(".kilocode/kilo.json"), "{}");
        write(&repo.join(".opencode/opencode.json"), "{}");

        let found = discover(Family::Kilo, &env, Some(&repo));
        let paths: Vec<String> = found
            .existing()
            .map(|layer| layer.path().unwrap().display().to_string())
            .collect();

        assert!(paths.iter().any(|p| p.contains(".kilo/kilo.json")));
        assert!(paths.iter().any(|p| p.contains(".kilocode/kilo.json")));
        assert!(
            !paths.iter().any(|p| p.contains(".opencode")),
            "Kilo must ignore .opencode, got {paths:?}"
        );

        let current = found
            .existing()
            .find(|l| l.path().unwrap().to_string_lossy().contains(".kilo/"))
            .unwrap();
        let legacy = found
            .existing()
            .find(|l| l.path().unwrap().to_string_lossy().contains(".kilocode/"))
            .unwrap();
        assert!(
            current.precedence < legacy.precedence,
            "the current name must outrank the legacy name"
        );
    }

    #[test]
    fn config_dir_profile_outranks_project_and_keeps_the_global_layer() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        let profile = dir.path().join("profile");
        let env = env_at(dir.path()).set("KILO_CONFIG_DIR", profile.display().to_string());
        write(&profile.join("kilo.json"), "{}");
        write(&repo.join(".kilo/kilo.json"), "{}");
        write(&dir.path().join("cfg/kilo/kilo.json"), "{}");

        let found = discover(Family::Kilo, &env, Some(&repo));
        let existing: Vec<_> = found.existing().collect();

        assert_eq!(
            existing[0].path().unwrap(),
            profile.join("kilo.json"),
            "the active profile must outrank the project layer"
        );
        assert_eq!(found.active_profile_dir, profile);
        assert!(
            existing
                .iter()
                .any(|l| l.path().unwrap().starts_with(dir.path().join("cfg/kilo"))),
            "the default global layer must survive alongside the profile"
        );
        assert_eq!(
            found.global_write_target().unwrap().path().unwrap(),
            profile.join("kilo.jsonc"),
            "global writes target the active profile, preferring jsonc"
        );
    }

    #[test]
    fn disabling_project_config_removes_every_project_layer() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        let env = env_at(dir.path()).set("OPENCODE_DISABLE_PROJECT_CONFIG", "1");
        write(&repo.join(".opencode/opencode.json"), "{}");

        let found = discover(Family::OpenCode, &env, Some(&repo));
        assert!(found.project_config_disabled);
        assert!(
            !found.layers.iter().any(|l| l.scope == ConfigScope::Project),
            "no project layer may be reported when project config is disabled"
        );
    }

    #[test]
    fn inline_config_is_observable_and_never_writable() {
        let dir = tmp();
        let env = env_at(dir.path()).set("OPENCODE_CONFIG_CONTENT", "{\"model\":\"x\"}");

        let found = discover(Family::OpenCode, &env, None);
        let inline = found.existing().next().unwrap();

        assert_eq!(inline.source, LayerSource::Inline);
        assert_eq!(inline.precedence, PRECEDENCE_INLINE);
        assert!(!inline.writable, "inline config has no file to write");
        assert_eq!(inline.external_control, Some(ExternalControl::Inline));
        assert!(inline.path().is_none(), "inline config must invent no path");
        assert_eq!(found.external().count(), 1);
    }

    #[test]
    fn an_explicit_config_file_ranks_below_the_project_layer() {
        let dir = tmp();
        let repo = dir.path().join("repo");
        let explicit = dir.path().join("explicit.json");
        let env = env_at(dir.path()).set("OPENCODE_CONFIG", explicit.display().to_string());
        write(&explicit, "{}");
        write(&repo.join(".opencode/opencode.json"), "{}");

        let found = discover(Family::OpenCode, &env, Some(&repo));
        let existing: Vec<_> = found.existing().collect();
        assert_eq!(existing[0].scope, ConfigScope::Project);
        assert_eq!(existing[1].path().unwrap(), explicit);
    }

    #[test]
    fn absent_files_are_reported_absent_and_contribute_nothing() {
        let dir = tmp();
        let env = env_at(dir.path());

        let found = discover(Family::OpenCode, &env, None);
        assert!(
            !found.layers.is_empty(),
            "candidates are still enumerated so a file can be created"
        );
        assert_eq!(
            found.existing().count(),
            0,
            "an absent file must never become an existing source"
        );
        assert!(found.layers.iter().all(|layer| !layer.exists));
    }

    #[test]
    fn pure_mode_is_reported_for_each_host_independently() {
        let dir = tmp();
        let env = env_at(dir.path()).set("OPENCODE_PURE", "1");

        assert!(discover(Family::OpenCode, &env, None).pure);
        assert!(
            !discover(Family::Kilo, &env, None).pure,
            "OPENCODE_PURE must not put Kilo into pure mode"
        );

        let kilo_env = env_at(dir.path()).set("KILO_PURE", "true");
        assert!(discover(Family::Kilo, &kilo_env, None).pure);
        assert!(!discover(Family::OpenCode, &kilo_env, None).pure);
    }

    #[test]
    fn an_empty_environment_variable_is_treated_as_unset() {
        let dir = tmp();
        let env = env_at(dir.path())
            .set("OPENCODE_CONFIG_DIR", "")
            .set("OPENCODE_CONFIG_CONTENT", "")
            .set("OPENCODE_PURE", "");

        let found = discover(Family::OpenCode, &env, None);
        assert!(!found.pure);
        assert!(
            found
                .active_profile_dir
                .ends_with(Path::new("cfg/opencode"))
        );
        assert!(
            !found.layers.iter().any(|l| l.source == LayerSource::Inline),
            "an empty inline variable must not create a layer"
        );
    }
}
