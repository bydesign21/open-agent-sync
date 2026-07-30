//! The few places where the operating systems genuinely differ.
//!
//! Only symlinks. Everything else — config paths, CLI invocation, the manifest —
//! is the same everywhere, because `dirs::home_dir` already resolves `~` per
//! platform and both host CLIs take the same arguments.

use std::path::Path;

use anyhow::{Context, Result};

/// Create a symlink at `link` pointing to `target`.
///
/// Unix has one call for both files and directories. Windows needs to know which
/// it is up front, and creating one at all requires either Developer Mode or an
/// elevated process — so the error is worth reporting clearly rather than as a
/// bare OS code, since it is a machine-configuration problem, not a bug.
pub fn symlink(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("linking {} -> {}", link.display(), target.display()))
    }

    #[cfg(windows)]
    {
        let result = if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        result.with_context(|| {
            format!(
                "linking {} -> {}. On Windows this needs Developer Mode enabled \
                 (Settings > Privacy & security > For developers) or an elevated \
                 terminal.",
                link.display(),
                target.display()
            )
        })
    }
}

/// Read a symlink and recreate it at `to`, preserving the link rather than
/// following it.
pub fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    let target =
        std::fs::read_link(from).with_context(|| format!("reading link {}", from.display()))?;
    symlink(&target, to)
}
