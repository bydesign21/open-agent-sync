//! Path helpers. This file does all tilde expansion, in one place, so
//! descriptors and manifests can both use `~/...` without each call site
//! reimplementing it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Expand a leading `~` or `$HOME`. Anything else is returned unchanged.
pub fn expand(input: &str) -> PathBuf {
    let home = dirs::home_dir();
    let Some(home) = home else {
        return PathBuf::from(input);
    };
    if input == "~" {
        return home;
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = input.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    PathBuf::from(input)
}

/// Render a path with the home directory collapsed to `~`. Use this for
/// display, and for paths written into the manifest, which must stay
/// machine-portable.
pub fn contract(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// `~/.config/agentsync`, overridable with `AGENTSYNC_HOME` (used by tests).
pub fn config_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("AGENTSYNC_HOME") {
        return PathBuf::from(explicit);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agentsync")
}

pub fn manifest_path() -> PathBuf {
    config_dir().join("manifest.toml")
}

/// Canonical skill content lives here. Hosts get symlinks that point to it.
pub fn skills_dir() -> PathBuf {
    config_dir().join("skills")
}

/// Canonical instruction files live here. Hosts get symlinks that point to it.
pub fn prompts_dir() -> PathBuf {
    config_dir().join("prompts")
}

pub fn hosts_dir() -> PathBuf {
    config_dir().join("hosts")
}

pub fn backups_dir() -> PathBuf {
    config_dir().join("backups")
}

/// Per-repo manifest, committed alongside the code it configures.
pub fn project_manifest_path(repo: &Path) -> PathBuf {
    repo.join(".agentsync.toml")
}

/// Copy `path` into a timestamped backup directory before this tool changes it.
///
/// The timestamp is a monotonically increasing counter directory, not a
/// formatted date. This needs no time crate, and stays deterministic under
/// test.
pub fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let root = backups_dir();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating backup dir {}", root.display()))?;

    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let mut n = 0usize;
    let dest = loop {
        let candidate = root.join(format!("{stem}.{n}.bak"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
        if n > 10_000 {
            anyhow::bail!("refusing to create more than 10000 backups of {stem}");
        }
    };

    if path.is_dir() {
        copy_dir(path, &dest)?;
    } else {
        std::fs::copy(path, &dest)
            .with_context(|| format!("backing up {} to {}", path.display(), dest.display()))?;
    }
    Ok(Some(dest))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if ty.is_symlink() {
            // Preserve the link. Do not follow it: a backup that dereferences
            // links would duplicate canonical content without warning.
            crate::platform::copy_symlink(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_and_contract_round_trip() {
        let home = dirs::home_dir().unwrap();
        let expanded = expand("~/foo/bar");
        assert_eq!(expanded, home.join("foo/bar"));
        assert_eq!(contract(&expanded), "~/foo/bar");
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        assert_eq!(expand("/usr/bin/node"), PathBuf::from("/usr/bin/node"));
        assert_eq!(contract(Path::new("/usr/bin/node")), "/usr/bin/node");
    }
}
