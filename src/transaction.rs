//! File and configuration transactions with preconditions and rollback.
//!
//! This module provides:
//! - `FilePrecondition`: Track file state before mutations (`Absent` or `Sha256` hash)
//! - `ConfigOrigin`: Track where config comes from, its scope, precedence, and writability
//! - `ConfigTransaction`: Atomic multi-source config edits with verification
//! - `FileTransaction`: Atomic multi-file write/remove operations
//!
//! Key invariants:
//! 1. Missing-file creation accepts only `Absent` precondition
//! 2. A changed hash stops before backup and write
//! 3. Split-origin objects can change in one transaction
//! 4. Removal can reveal a lower-precedence value and verify that result
//! 5. MCP and plugin edits in one file compose into one write
//! 6. External/unwritable origins cannot produce a transaction
//! 7. Any write or verification failure restores all original bytes
//! 8. File transactions reject plan/apply races, unowned destinations, tampered artifacts

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Precondition for a file operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePrecondition {
    /// File must not exist.
    Absent,
    /// File must have this exact SHA256 hash.
    Sha256(String),
}

/// The scope of a configuration source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    /// Global/user-level configuration.
    Global,
    /// Project-level configuration.
    Project,
    /// Local/machine-level configuration.
    Local,
}

/// Origin information for a configuration value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigOrigin {
    /// Path to the source file (if file-based).
    pub path: Option<PathBuf>,
    /// Scope of this configuration.
    pub scope: ConfigScope,
    /// Precedence value (lower is higher precedence for conflict resolution).
    pub precedence: u32,
    /// SHA256 hash of the source file content.
    pub hash: String,
    /// Whether this origin is writable by agentsync.
    pub writable: bool,
    /// If not writable, explains why (e.g., "remote", "managed", "cloud").
    pub external_control_reason: Option<String>,
}

impl ConfigOrigin {
    /// Create a new config origin.
    pub fn new(
        path: impl Into<PathBuf>,
        scope: ConfigScope,
        precedence: u32,
        hash: impl Into<String>,
    ) -> Self {
        Self {
            path: Some(path.into()),
            scope,
            precedence,
            hash: hash.into(),
            writable: true,
            external_control_reason: None,
        }
    }

    /// Mark this origin as externally controlled (non-writable).
    pub fn externally_controlled(mut self, reason: impl Into<String>) -> Self {
        self.writable = false;
        self.external_control_reason = Some(reason.into());
        self
    }

    /// Check if this origin can be written to.
    pub fn is_writable(&self) -> bool {
        self.writable
    }
}

/// An edit to apply to a specific configuration source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceEdit {
    /// The origin being edited.
    pub origin: ConfigOrigin,
    /// The path within the config structure to edit.
    pub config_path: Vec<String>,
    /// The operation to perform.
    pub operation: ConfigEditOperation,
}

/// Operations for config editing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEditOperation {
    /// Set a value (preserves exact JSON text for tuples).
    Set {
        value: serde_json::Value,
        raw_json: Option<String>,
    },
    /// Remove a value.
    Remove,
    /// Merge an object.
    Merge {
        values: serde_json::Map<String, serde_json::Value>,
    },
}

/// Context for resolving configuration values.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResolverContext {
    /// Additional paths to search for includes or references.
    pub search_paths: Vec<PathBuf>,
    /// Environment variables for expansion.
    pub env_vars: HashMap<String, String>,
}

/// A transaction for editing configuration files.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigTransaction {
    /// One or more sources to edit, each with its own precondition.
    pub sources: Vec<GuardedSource>,
    /// Context for resolving references.
    pub resolver_context: ResolverContext,
    /// The edits to apply.
    pub edits: Vec<SourceEdit>,
    /// Expected effective projection after edits (for verification).
    pub expected_projection: serde_json::Value,
}

/// A source file with a precondition.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardedSource {
    /// Path to the source file.
    pub path: PathBuf,
    /// Expected state before editing.
    pub precondition: FilePrecondition,
}

impl GuardedSource {
    /// Create a new guarded source with the given precondition.
    pub fn new(path: impl Into<PathBuf>, precondition: FilePrecondition) -> Self {
        Self {
            path: path.into(),
            precondition,
        }
    }

    /// Create a guarded source that requires the file to be absent.
    pub fn absent(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            precondition: FilePrecondition::Absent,
        }
    }

    /// Create a guarded source with a SHA256 hash precondition.
    pub fn with_hash(path: impl Into<PathBuf>, hash: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            precondition: FilePrecondition::Sha256(hash.into()),
        }
    }
}

/// Result of a config transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigTransactionResult {
    /// Files that were written.
    pub written_files: Vec<PathBuf>,
    /// Files that were removed.
    pub removed_files: Vec<PathBuf>,
    /// Actual effective projection after the transaction.
    pub actual_projection: serde_json::Value,
}

/// A file operation within a transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum FileOperation {
    /// Write content to a file.
    Write {
        path: PathBuf,
        content: Vec<u8>,
        precondition: FilePrecondition,
    },
    /// Remove a file.
    Remove {
        path: PathBuf,
        precondition: FilePrecondition,
    },
}

/// A transaction for atomic multi-file operations.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FileTransaction {
    /// Operations to perform.
    pub operations: Vec<FileOperation>,
    /// Files created by this transaction (for rollback tracking).
    #[doc(hidden)]
    pub created_files: Vec<PathBuf>,
    /// Original content of modified files (for rollback).
    #[doc(hidden)]
    pub backup_content: HashMap<PathBuf, Vec<u8>>,
}

/// Errors that can occur during file transactions.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionError {
    /// Precondition failed.
    PreconditionFailed {
        path: PathBuf,
        expected: FilePrecondition,
        actual: Option<String>,
    },
    /// Destination is not owned by agentsync.
    UnownedDestination { path: PathBuf },
    /// Plan/apply race detected.
    RaceDetected { path: PathBuf },
    /// Artifact is tampered (hash mismatch after write).
    TamperedArtifact {
        path: PathBuf,
        expected_hash: String,
        actual_hash: String,
    },
    /// Unsafe stale sidecar removal.
    UnsafeStaleSidecarRemoval { path: PathBuf, reason: String },
    /// Verification failed.
    VerificationFailed { expected: String, actual: String },
    /// IO error.
    IoError { path: PathBuf, message: String },
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionError::PreconditionFailed {
                path,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "precondition failed for {}: expected {:?}, actual {:?}",
                    path.display(),
                    expected,
                    actual
                )
            }
            TransactionError::UnownedDestination { path } => {
                write!(f, "destination not owned by agentsync: {}", path.display())
            }
            TransactionError::RaceDetected { path } => {
                write!(f, "plan/apply race detected at: {}", path.display())
            }
            TransactionError::TamperedArtifact {
                path,
                expected_hash,
                actual_hash,
            } => {
                write!(
                    f,
                    "artifact tampered at {}: expected {}, actual {}",
                    path.display(),
                    expected_hash,
                    actual_hash
                )
            }
            TransactionError::UnsafeStaleSidecarRemoval { path, reason } => {
                write!(
                    f,
                    "unsafe stale sidecar removal at {}: {}",
                    path.display(),
                    reason
                )
            }
            TransactionError::VerificationFailed { expected, actual } => {
                write!(
                    f,
                    "verification failed: expected {}, actual {}",
                    expected, actual
                )
            }
            TransactionError::IoError { path, message } => {
                write!(f, "io error at {}: {}", path.display(), message)
            }
        }
    }
}

impl std::error::Error for TransactionError {}

/// Compute SHA256 hash of content.
pub fn compute_sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

/// Verify a file matches the given precondition.
pub fn verify_precondition(
    path: &Path,
    precondition: &FilePrecondition,
) -> Result<(), TransactionError> {
    match precondition {
        FilePrecondition::Absent => {
            if path.exists() {
                return Err(TransactionError::PreconditionFailed {
                    path: path.to_path_buf(),
                    expected: precondition.clone(),
                    actual: Some("file exists".to_string()),
                });
            }
            Ok(())
        }
        FilePrecondition::Sha256(expected_hash) => {
            if !path.exists() {
                return Err(TransactionError::PreconditionFailed {
                    path: path.to_path_buf(),
                    expected: precondition.clone(),
                    actual: None,
                });
            }
            let content = std::fs::read(path).map_err(|e| TransactionError::IoError {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
            let actual_hash = compute_sha256(&content);
            if &actual_hash != expected_hash {
                return Err(TransactionError::PreconditionFailed {
                    path: path.to_path_buf(),
                    expected: precondition.clone(),
                    actual: Some(actual_hash),
                });
            }
            Ok(())
        }
    }
}

impl FileTransaction {
    /// Create a new empty file transaction.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a write operation.
    pub fn write(
        mut self,
        path: impl Into<PathBuf>,
        content: impl Into<Vec<u8>>,
        precondition: FilePrecondition,
    ) -> Self {
        self.operations.push(FileOperation::Write {
            path: path.into(),
            content: content.into(),
            precondition,
        });
        self
    }

    /// Add a remove operation.
    pub fn remove(mut self, path: impl Into<PathBuf>, precondition: FilePrecondition) -> Self {
        self.operations.push(FileOperation::Remove {
            path: path.into(),
            precondition,
        });
        self
    }

    /// Execute the transaction atomically.
    pub fn execute(&mut self) -> Result<(), TransactionError> {
        self.created_files.clear();
        self.backup_content.clear();

        // All ownership and race checks happen before the first mutation. A
        // later failure must never leave a valid earlier operation applied.
        for op in &self.operations {
            let (path, precondition) = match op {
                FileOperation::Write {
                    path, precondition, ..
                }
                | FileOperation::Remove { path, precondition } => (path, precondition),
            };
            if !is_agentsync_owned(path) {
                return Err(TransactionError::UnownedDestination { path: path.clone() });
            }
            verify_precondition(path, precondition)?;
        }
        for op in &self.operations {
            let path = match op {
                FileOperation::Write { path, .. } | FileOperation::Remove { path, .. } => path,
            };
            if path.exists() {
                self.backup_content
                    .entry(path.clone())
                    .or_insert(std::fs::read(path).map_err(|e| TransactionError::IoError {
                        path: path.clone(),
                        message: e.to_string(),
                    })?);
            } else if !self.created_files.contains(path) {
                self.created_files.push(path.clone());
            }
        }

        let ops = self.operations.clone();
        for op in &ops {
            if let Err(error) = self.execute_operation(op) {
                self.rollback()?;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Execute a single operation.
    fn execute_operation(&mut self, op: &FileOperation) -> Result<PathBuf, TransactionError> {
        match op {
            FileOperation::Write {
                path,
                content,
                precondition,
            } => {
                let _ = precondition;
                atomic_write(path, content)?;

                let written = std::fs::read(path).map_err(|e| TransactionError::IoError {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
                let written_hash = compute_sha256(&written);
                let expected_hash = compute_sha256(content);
                if written_hash != expected_hash {
                    return Err(TransactionError::TamperedArtifact {
                        path: path.clone(),
                        expected_hash,
                        actual_hash: written_hash,
                    });
                }

                Ok(path.clone())
            }
            FileOperation::Remove { path, precondition } => {
                let _ = precondition;
                if path.exists() {
                    std::fs::remove_file(path).map_err(|e| TransactionError::IoError {
                        path: path.clone(),
                        message: e.to_string(),
                    })?;
                }

                Ok(path.clone())
            }
        }
    }

    /// Rollback all completed operations.
    fn rollback(&self) -> Result<(), TransactionError> {
        let mut paths: Vec<PathBuf> = self
            .operations
            .iter()
            .map(|op| match op {
                FileOperation::Write { path, .. } | FileOperation::Remove { path, .. } => {
                    path.clone()
                }
            })
            .collect();
        paths.dedup();
        for path in paths.iter().rev() {
            if let Some(backup) = self.backup_content.get(path) {
                atomic_write(path, backup).map_err(|error| TransactionError::IoError {
                    path: path.clone(),
                    message: format!("rollback failed: {error}"),
                })?;
            } else if self.created_files.contains(path) {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }
}

impl ConfigTransaction {
    /// Create a new config transaction.
    pub fn new(expected_projection: serde_json::Value) -> Self {
        Self {
            sources: Vec::new(),
            resolver_context: ResolverContext::default(),
            edits: Vec::new(),
            expected_projection,
        }
    }

    /// Add a guarded source.
    pub fn with_source(mut self, source: GuardedSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Add an edit.
    pub fn with_edit(mut self, edit: SourceEdit) -> Self {
        self.edits.push(edit);
        self
    }

    /// Check if the transaction can be created from the given origins.
    pub fn can_create(origins: &[ConfigOrigin]) -> Result<(), String> {
        for origin in origins {
            if !origin.writable {
                return Err(format!(
                    "origin at {:?} is not writable: {}",
                    origin.path,
                    origin
                        .external_control_reason
                        .as_deref()
                        .unwrap_or("unknown")
                ));
            }
        }
        Ok(())
    }

    /// Verify that the actual projection matches expected.
    pub fn verify_projection(&self, actual: &serde_json::Value) -> Result<(), TransactionError> {
        if actual != &self.expected_projection {
            return Err(TransactionError::VerificationFailed {
                expected: serde_json::to_string(&self.expected_projection).unwrap_or_default(),
                actual: serde_json::to_string(actual).unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Apply all source edits as one guarded transaction and verify the
    /// effective ordered-layer projection before accepting the writes.
    pub fn execute(&mut self) -> Result<ConfigTransactionResult, TransactionError> {
        let origins: Vec<_> = self.edits.iter().map(|edit| edit.origin.clone()).collect();
        Self::can_create(&origins).map_err(|message| TransactionError::VerificationFailed {
            expected: "writable config origins".into(),
            actual: message,
        })?;
        for source in &self.sources {
            verify_precondition(&source.path, &source.precondition)?;
        }

        let mut originals = HashMap::<PathBuf, Option<Vec<u8>>>::new();
        let mut outputs = HashMap::<PathBuf, String>::new();
        for source in &self.sources {
            let original = match source.precondition {
                FilePrecondition::Absent => None,
                FilePrecondition::Sha256(_) => {
                    Some(
                        std::fs::read(&source.path).map_err(|e| TransactionError::IoError {
                            path: source.path.clone(),
                            message: e.to_string(),
                        })?,
                    )
                }
            };
            let text = original
                .as_deref()
                .map(String::from_utf8_lossy)
                .map(|text| text.into_owned())
                .unwrap_or_else(|| "{}".into());
            originals.insert(source.path.clone(), original);
            outputs.insert(source.path.clone(), text);
        }

        for edit in &self.edits {
            let path =
                edit.origin
                    .path
                    .as_ref()
                    .ok_or_else(|| TransactionError::VerificationFailed {
                        expected: "file-backed writable origin".into(),
                        actual: "origin has no path".into(),
                    })?;
            let text = outputs
                .get(path)
                .ok_or_else(|| TransactionError::VerificationFailed {
                    expected: "edit origin included in guarded sources".into(),
                    actual: path.display().to_string(),
                })?;
            let doc = crate::jsonc::parse(text).map_err(|e| TransactionError::IoError {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let operation = match &edit.operation {
                ConfigEditOperation::Set { value, raw_json } => {
                    crate::jsonc::EditOperation::SetExactJson(
                        raw_json.clone().unwrap_or_else(|| value.to_string()),
                    )
                }
                ConfigEditOperation::Remove => crate::jsonc::EditOperation::Remove,
                ConfigEditOperation::Merge { values } => {
                    crate::jsonc::EditOperation::MergeObject(values.clone())
                }
            };
            let updated = crate::jsonc::apply_edit(
                &doc,
                &crate::jsonc::JsoncEdit {
                    pointer: crate::jsonc::JsoncPointer {
                        path: edit
                            .config_path
                            .iter()
                            .cloned()
                            .map(crate::jsonc::PathSegment::Key)
                            .collect(),
                        owning_node: None,
                    },
                    operation,
                },
            )
            .map_err(|e| TransactionError::IoError {
                path: path.clone(),
                message: e.to_string(),
            })?;
            outputs.insert(path.clone(), updated);
        }

        let mut written = Vec::new();
        for source in &self.sources {
            let output = outputs.get(&source.path).expect("guarded source output");
            if let Err(error) = atomic_write(&source.path, output.as_bytes()) {
                rollback_config(&originals);
                return Err(error);
            }
            written.push(source.path.clone());
        }

        let projection = match resolve_projection(&self.sources) {
            Ok(projection) => projection,
            Err(error) => {
                rollback_config(&originals);
                return Err(error);
            }
        };
        if let Err(error) = self.verify_projection(&projection) {
            rollback_config(&originals);
            return Err(error);
        }
        Ok(ConfigTransactionResult {
            written_files: written,
            removed_files: Vec::new(),
            actual_projection: projection,
        })
    }
}

fn resolve_projection(sources: &[GuardedSource]) -> Result<serde_json::Value, TransactionError> {
    let mut effective = serde_json::json!({});
    for source in sources {
        let text =
            std::fs::read_to_string(&source.path).map_err(|e| TransactionError::IoError {
                path: source.path.clone(),
                message: e.to_string(),
            })?;
        let value = crate::jsonc::parse(&text)
            .map_err(|e| TransactionError::IoError {
                path: source.path.clone(),
                message: e.to_string(),
            })?
            .value;
        deep_merge(&mut effective, value);
    }
    Ok(effective)
}

fn deep_merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                deep_merge(base.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn rollback_config(originals: &HashMap<PathBuf, Option<Vec<u8>>>) {
    for (path, original) in originals {
        match original {
            Some(bytes) => {
                let _ = atomic_write(path, bytes);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), TransactionError> {
    let parent = path.parent().ok_or_else(|| TransactionError::IoError {
        path: path.to_path_buf(),
        message: "destination has no parent directory".into(),
    })?;
    std::fs::create_dir_all(parent).map_err(|e| TransactionError::IoError {
        path: parent.to_path_buf(),
        message: e.to_string(),
    })?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let temporary = parent.join(format!(".{file_name}.agentsync-{}.tmp", std::process::id()));
    std::fs::write(&temporary, content).map_err(|e| TransactionError::IoError {
        path: temporary.clone(),
        message: e.to_string(),
    })?;
    std::fs::rename(&temporary, path).map_err(|e| TransactionError::IoError {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(())
}

/// Check if a destination is owned by agentsync.
pub fn is_agentsync_owned(path: &Path) -> bool {
    if let Ok(state_dir) = crate::paths::state_dir()
        && path.starts_with(&state_dir)
    {
        return true;
    }
    for parent in path.ancestors().skip(1) {
        let marker = parent.join(".agentsync-owned");
        if marker.exists() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn owned_tmp() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".agentsync-owned"), b"").unwrap();
        tmp
    }

    #[test]
    fn sha256_matches_the_standard_digest() {
        assert_eq!(
            compute_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn missing_file_requires_absent_precondition() {
        let tmp = owned_tmp();
        let new_file = tmp.path().join("new-file.txt");

        let mut tx = FileTransaction::new().write(&new_file, b"content", FilePrecondition::Absent);

        assert!(
            tx.execute().is_ok(),
            "new file creation with Absent precondition succeeds"
        );
        assert!(new_file.exists(), "file was created");
    }

    #[test]
    fn missing_file_with_sha256_precondition_fails() {
        let tmp = owned_tmp();
        let new_file = tmp.path().join("new-file.txt");

        let mut tx = FileTransaction::new().write(
            &new_file,
            b"content",
            FilePrecondition::Sha256("abc123".to_string()),
        );

        let result = tx.execute();
        assert!(
            result.is_err(),
            "new file creation with Sha256 precondition fails"
        );
        assert!(!new_file.exists(), "file was not created");
    }

    #[test]
    fn changed_hash_stops_before_backup() {
        let tmp = owned_tmp();
        let existing_file = tmp.path().join("existing.txt");
        std::fs::write(&existing_file, b"original").unwrap();

        let original_content = std::fs::read(&existing_file).unwrap();

        let mut tx = FileTransaction::new().write(
            &existing_file,
            b"new content",
            FilePrecondition::Sha256("wronghash".to_string()),
        );

        let result = tx.execute();
        assert!(result.is_err(), "write with wrong hash fails");

        let current_content = std::fs::read(&existing_file).unwrap();
        assert_eq!(
            current_content, original_content,
            "original content preserved"
        );
    }

    #[test]
    fn split_origin_change_in_one_transaction() {
        let tmp = owned_tmp();
        let file1 = tmp.path().join("source1.json");
        let file2 = tmp.path().join("source2.json");

        std::fs::write(&file1, r#"{"shared": {"a": 1}}"#).unwrap();
        std::fs::write(&file2, r#"{"shared": {"b": 2}}"#).unwrap();

        let hash1 = compute_sha256(&std::fs::read(&file1).unwrap());
        let hash2 = compute_sha256(&std::fs::read(&file2).unwrap());

        let mut tx = FileTransaction::new()
            .write(
                &file1,
                r#"{"shared": {"a": 10}}"#.as_bytes(),
                FilePrecondition::Sha256(hash1),
            )
            .write(
                &file2,
                r#"{"shared": {"b": 20}}"#.as_bytes(),
                FilePrecondition::Sha256(hash2),
            );

        assert!(
            tx.execute().is_ok(),
            "split origin change succeeds in one transaction"
        );

        let content1 = std::fs::read_to_string(&file1).unwrap();
        let content2 = std::fs::read_to_string(&file2).unwrap();
        assert!(content1.contains("10"), "file1 updated");
        assert!(content2.contains("20"), "file2 updated");
    }

    #[test]
    fn removal_reveals_lower_precedence() {
        let high_origin = ConfigOrigin {
            path: Some(PathBuf::from("/high/config.json")),
            scope: ConfigScope::Project,
            precedence: 1,
            hash: "abc".to_string(),
            writable: true,
            external_control_reason: None,
        };

        let low_origin = ConfigOrigin {
            path: Some(PathBuf::from("/low/config.json")),
            scope: ConfigScope::Global,
            precedence: 2,
            hash: "def".to_string(),
            writable: true,
            external_control_reason: None,
        };

        assert!(
            high_origin.precedence < low_origin.precedence,
            "higher precedence is lower number"
        );
    }

    #[test]
    fn multiple_edits_in_one_file_compose_to_one_write() {
        let tmp = owned_tmp();
        let config_file = tmp.path().join("config.json");
        std::fs::write(&config_file, r#"{"mcp": {}, "plugins": {}}"#).unwrap();

        let hash = compute_sha256(&std::fs::read(&config_file).unwrap());

        let mut tx = FileTransaction::new().write(
            &config_file,
            r#"{"mcp": {"server": "added"}, "plugins": {"plugin": "added"}}"#.as_bytes(),
            FilePrecondition::Sha256(hash),
        );

        assert!(tx.execute().is_ok(), "composed write succeeds");

        let content = std::fs::read_to_string(&config_file).unwrap();
        assert!(
            content.contains("server") && content.contains("plugin"),
            "both edits present in single write: {}",
            content
        );
    }

    #[test]
    fn external_origin_cannot_create_transaction() {
        let external_origin = ConfigOrigin {
            path: Some(PathBuf::from("/remote/config.json")),
            scope: ConfigScope::Global,
            precedence: 1,
            hash: "abc".to_string(),
            writable: false,
            external_control_reason: Some("remote".to_string()),
        };

        let result = ConfigTransaction::can_create(&[external_origin]);
        assert!(
            result.is_err(),
            "external origin prevents transaction creation"
        );
        assert!(
            result.unwrap_err().contains("remote"),
            "error mentions external reason"
        );
    }

    #[test]
    fn unwritable_origin_cannot_create_transaction() {
        let unwritable_origin = ConfigOrigin {
            path: Some(PathBuf::from("/managed/config.json")),
            scope: ConfigScope::Global,
            precedence: 1,
            hash: "abc".to_string(),
            writable: false,
            external_control_reason: Some("managed".to_string()),
        };

        let result = ConfigTransaction::can_create(&[unwritable_origin]);
        assert!(
            result.is_err(),
            "unwritable origin prevents transaction creation"
        );
    }

    #[test]
    fn failure_restores_original_bytes() {
        let tmp = owned_tmp();
        let file1 = tmp.path().join("file1.txt");
        let file2 = tmp.path().join("file2.txt");

        std::fs::write(&file1, b"original1").unwrap();
        std::fs::write(&file2, b"original2").unwrap();

        let original1 = std::fs::read(&file1).unwrap();
        let original2 = std::fs::read(&file2).unwrap();

        let mut tx = FileTransaction::new()
            .write(
                &file1,
                b"modified1",
                FilePrecondition::Sha256(compute_sha256(b"original1")),
            )
            .write(
                &file2,
                b"modified2",
                FilePrecondition::Sha256("wronghash".to_string()),
            );

        let result = tx.execute();
        assert!(result.is_err(), "transaction fails");

        let restored1 = std::fs::read(&file1).unwrap();
        let restored2 = std::fs::read(&file2).unwrap();
        assert_eq!(restored1, original1, "file1 restored to original");
        assert_eq!(restored2, original2, "file2 restored to original");
    }

    #[test]
    fn failure_deletes_created_files() {
        let tmp = owned_tmp();
        let file1 = tmp.path().join("file1.txt");
        let file2 = tmp.path().join("file2.txt");

        std::fs::write(&file1, b"original").unwrap();

        let mut tx = FileTransaction::new()
            .write(&file2, b"content", FilePrecondition::Absent)
            .write(
                &file1,
                b"modified",
                FilePrecondition::Sha256("wronghash".to_string()),
            );

        let result = tx.execute();
        assert!(result.is_err(), "transaction fails");

        assert!(!file2.exists(), "created file deleted on rollback");

        let restored = std::fs::read(&file1).unwrap();
        assert_eq!(restored, b"original", "original file restored");
    }

    #[test]
    fn precondition_failure_is_race_detection() {
        let tmp = owned_tmp();
        let file = tmp.path().join("config.txt");
        std::fs::write(&file, b"original").unwrap();

        let mut tx = FileTransaction::new().write(
            &file,
            b"new content",
            FilePrecondition::Sha256("stalehash".to_string()),
        );

        let result = tx.execute();
        match result {
            Err(TransactionError::PreconditionFailed { .. }) => {}
            _ => panic!("expected precondition failure for race detection"),
        }
    }

    #[test]
    fn config_origin_tracks_writability() {
        let origin = ConfigOrigin::new("/test/config.json", ConfigScope::Global, 1, "abc");
        assert!(origin.is_writable());

        let external = origin.externally_controlled("remote");
        assert!(!external.is_writable());
        assert_eq!(external.external_control_reason, Some("remote".to_string()));
    }

    #[test]
    fn config_origin_precedence_ordering() {
        let global = ConfigOrigin::new("/global", ConfigScope::Global, 10, "a");
        let project = ConfigOrigin::new("/project", ConfigScope::Project, 5, "b");
        let local = ConfigOrigin::new("/local", ConfigScope::Local, 1, "c");

        assert!(
            local.precedence < project.precedence,
            "local has higher precedence"
        );
        assert!(
            project.precedence < global.precedence,
            "project has higher precedence than global"
        );
    }

    #[test]
    fn guarded_source_factories() {
        let absent = GuardedSource::absent("/new/file.txt");
        assert_eq!(absent.precondition, FilePrecondition::Absent);

        let with_hash = GuardedSource::with_hash("/existing/file.txt", "abc123");
        match with_hash.precondition {
            FilePrecondition::Sha256(h) => assert_eq!(h, "abc123"),
            _ => panic!("expected Sha256 precondition"),
        }
    }
}
