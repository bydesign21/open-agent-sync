//! The few places where the operating systems genuinely differ.
//!
//! Only symlinks differ. Everything else — config paths, CLI invocation, the
//! manifest — stays the same everywhere. `dirs::home_dir` already resolves `~`
//! per platform, and both host CLIs take the same arguments.

use std::path::Path;

use anyhow::{Context, Result};

/// Create a symlink at `link` that points to `target`.
///
/// Unix has one call for both files and directories. Windows needs to know
/// which one it is, in advance. Creating a symlink at all needs Developer Mode
/// or an elevated process. Report the error clearly, not as a bare OS code,
/// because this is a machine-configuration problem, not a bug.
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
                "linking {} -> {}. On Windows, this needs Developer Mode \
                 (Settings > Privacy & security > For developers), or an elevated \
                 terminal.",
                link.display(),
                target.display()
            )
        })
    }
}

/// Read a symlink and recreate it at `to`. Preserve the link. Do not follow
/// it.
pub fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    let target =
        std::fs::read_link(from).with_context(|| format!("reading link {}", from.display()))?;
    symlink(&target, to)
}
