//! Plan execution.
//!
//! Two rules here are load-bearing:
//!
//! 1. **A failed step does not abort the run.** Stopping midway leaves you
//!    unable to tell which half landed. Every step reports its own outcome and
//!    the summary distinguishes done / failed / skipped.
//! 2. **The manifest is written once, at the end, and only if every manifest op
//!    succeeded.** A half-written manifest is worse than an unwritten one,
//!    because the next run diffs against a lie.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::plan::{FsOp, ManifestOp, Plan, Step};
use crate::hosts::{Host, runner};
use crate::manifest::Manifest;
use crate::paths;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed,
    /// Not attempted: the host is not installed, or the step is something the
    /// user must do by hand.
    Skipped,
}

#[derive(Clone, Debug)]
pub struct StepResult {
    pub label: String,
    pub outcome: Outcome,
    /// Command line for host steps, so the report is reproducible.
    pub command: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub results: Vec<StepResult>,
    pub manifest_written: bool,
    /// Populated when the manifest could not be written; the reason matters
    /// because the host side may already have changed.
    pub manifest_error: Option<String>,
}

impl Report {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.results.iter().filter(|r| r.outcome == outcome).count()
    }
    pub fn any_failed(&self) -> bool {
        self.count(Outcome::Failed) > 0 || self.manifest_error.is_some()
    }
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} done", self.count(Outcome::Done))];
        if self.count(Outcome::Failed) > 0 {
            parts.push(format!("{} failed", self.count(Outcome::Failed)));
        }
        if self.count(Outcome::Skipped) > 0 {
            parts.push(format!("{} skipped", self.count(Outcome::Skipped)));
        }
        parts.join(", ")
    }
}

/// A progress notification.
///
/// `Started` exists because some steps are slow — a plugin install clones a
/// repository — and a caller that only hears about *finished* steps has nothing
/// to display while one is in flight, which reads as a hang.
#[derive(Clone, Copy, Debug)]
pub enum Progress<'a> {
    Started {
        /// Zero-based index into `plan.steps`.
        index: usize,
        label: &'a str,
    },
    Finished(&'a StepResult),
}

/// Run every step, then persist the manifest if its ops all succeeded.
pub fn run(
    plan: &Plan,
    manifest: &mut Manifest,
    manifest_path: &Path,
    hosts: &[Host],
    mut progress: impl FnMut(Progress<'_>),
) -> Report {
    let mut report = Report::default();
    let mut manifest_dirty = false;
    let mut manifest_ops_ok = true;

    for (index, planned) in plan.steps.iter().enumerate() {
        progress(Progress::Started {
            index,
            label: &planned.label,
        });
        let result = match &planned.step {
            Step::Manifest(op) => {
                let r = apply_manifest_op(manifest, op);
                match r {
                    Ok(()) => {
                        manifest_dirty = true;
                        StepResult {
                            label: planned.label.clone(),
                            outcome: Outcome::Done,
                            command: None,
                            message: "manifest updated in memory".into(),
                        }
                    }
                    Err(e) => {
                        manifest_ops_ok = false;
                        StepResult {
                            label: planned.label.clone(),
                            outcome: Outcome::Failed,
                            command: None,
                            message: format!("{e:#}"),
                        }
                    }
                }
            }

            Step::Host { host, argv, cwd } => {
                let found = hosts.iter().find(|h| h.name() == host);
                match found.and_then(|h| h.bin.as_ref().map(|b| (h, b))) {
                    None => StepResult {
                        label: planned.label.clone(),
                        outcome: Outcome::Skipped,
                        command: Some(format!("{host} {}", argv.join(" "))),
                        message: format!("{host} is not installed"),
                    },
                    Some((h, bin)) => {
                        let command = runner::shell_line(&h.descriptor.detect.bin, argv);
                        match runner::run(bin, argv, cwd.as_deref()) {
                            Ok(out) if out.ok() => StepResult {
                                label: planned.label.clone(),
                                outcome: Outcome::Done,
                                command: Some(command),
                                message: out.message(),
                            },
                            Ok(out) => StepResult {
                                label: planned.label.clone(),
                                outcome: Outcome::Failed,
                                command: Some(command),
                                message: format!(
                                    "exit {}: {}",
                                    out.status
                                        .map(|c| c.to_string())
                                        .unwrap_or_else(|| "signal".into()),
                                    out.message()
                                ),
                            },
                            Err(e) => StepResult {
                                label: planned.label.clone(),
                                outcome: Outcome::Failed,
                                command: Some(command),
                                message: format!("{e:#}"),
                            },
                        }
                    }
                }
            }

            Step::Fs(op) => match apply_fs_op(op) {
                Ok(message) => StepResult {
                    label: planned.label.clone(),
                    outcome: Outcome::Done,
                    command: None,
                    message,
                },
                Err(e) => StepResult {
                    label: planned.label.clone(),
                    outcome: Outcome::Failed,
                    command: None,
                    message: format!("{e:#}"),
                },
            },

            Step::Manual(text) => StepResult {
                label: planned.label.clone(),
                outcome: Outcome::Skipped,
                command: None,
                message: text.clone(),
            },
        };

        progress(Progress::Finished(&result));
        report.results.push(result);
    }

    if manifest_dirty {
        if manifest_ops_ok {
            match manifest.save(manifest_path) {
                Ok(()) => report.manifest_written = true,
                Err(e) => report.manifest_error = Some(format!("{e:#}")),
            }
        } else {
            report.manifest_error = Some(
                "manifest not written: at least one manifest edit failed, and a \
                 partially-applied manifest would make the next run diff against \
                 a state that never existed"
                    .into(),
            );
        }
    }

    report
}

fn apply_manifest_op(manifest: &mut Manifest, op: &ManifestOp) -> Result<()> {
    match op {
        ManifestOp::UpsertMcp { name, entry } => {
            manifest.mcp.insert(name.clone(), (**entry).clone());
        }
        ManifestOp::RemoveMcp(name) => {
            manifest.mcp.remove(name);
        }
        ManifestOp::SetMcpHosts { name, hosts } => {
            manifest
                .mcp
                .get_mut(name)
                .with_context(|| format!("mcp.{name} is not in the manifest"))?
                .hosts = hosts.clone();
        }
        ManifestOp::SetMcpBearerEnv { name, var } => {
            let entry = manifest
                .mcp
                .get_mut(name)
                .with_context(|| format!("mcp.{name} is not in the manifest"))?;
            entry
                .headers
                .retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
            entry.bearer_token_env = Some(var.clone());
        }
        ManifestOp::UpsertSkill { name, source } => {
            manifest.skills.insert(
                name.clone(),
                crate::manifest::SkillEntry {
                    source: source.clone(),
                    hosts: None,
                },
            );
        }
        ManifestOp::RemoveSkill(name) => {
            manifest.skills.remove(name);
        }
        ManifestOp::SetSkillHosts { name, hosts } => {
            manifest
                .skills
                .get_mut(name)
                .with_context(|| format!("skills.{name} is not in the manifest"))?
                .hosts = hosts.clone();
        }
        ManifestOp::SetPluginHosts { name, hosts } => {
            manifest
                .plugins
                .get_mut(name)
                .with_context(|| format!("plugins.{name} is not in the manifest"))?
                .hosts = hosts.clone();
        }
        ManifestOp::SetMarketplaceHosts { name, hosts } => {
            manifest
                .marketplaces
                .get_mut(name)
                .with_context(|| format!("marketplaces.{name} is not in the manifest"))?
                .hosts = hosts.clone();
        }
        ManifestOp::UpsertPlugin { name, marketplace } => {
            manifest.plugins.insert(
                name.clone(),
                crate::manifest::PluginEntry {
                    marketplace: marketplace.clone(),
                    hosts: None,
                },
            );
        }
        ManifestOp::RemovePlugin(name) => {
            manifest.plugins.remove(name);
        }
        ManifestOp::UpsertMarketplace { name, entry } => {
            manifest
                .marketplaces
                .insert(name.clone(), (**entry).clone());
        }
        ManifestOp::RemoveMarketplace(name) => {
            manifest.marketplaces.remove(name);
        }
    }
    Ok(())
}

fn apply_fs_op(op: &FsOp) -> Result<String> {
    match op {
        FsOp::Link { target, link } => {
            if !target.exists() {
                anyhow::bail!("link target {} does not exist", target.display());
            }
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Replacing something we did not create is a real mutation, so back
            // it up first and say so.
            let mut backed_up = None;
            if link.symlink_metadata().is_ok() {
                if link.is_dir() && std::fs::symlink_metadata(link)?.file_type().is_dir() {
                    backed_up = paths::backup(link)?;
                    std::fs::remove_dir_all(link)?;
                } else {
                    std::fs::remove_file(link)?;
                }
            }
            std::os::unix::fs::symlink(target, link)
                .with_context(|| format!("linking {} -> {}", link.display(), target.display()))?;
            Ok(match backed_up {
                Some(b) => format!(
                    "linked {} (previous contents backed up to {})",
                    paths::contract(link),
                    paths::contract(&b)
                ),
                None => format!("linked {}", paths::contract(link)),
            })
        }
        FsOp::Unlink(path) => {
            match path.symlink_metadata() {
                Err(_) => Ok(format!("{} was already absent", paths::contract(path))),
                Ok(meta) => {
                    if meta.file_type().is_symlink() || meta.is_file() {
                        std::fs::remove_file(path)?;
                    } else {
                        // Refuse to silently delete real content behind an
                        // "unlink" label. Purging is a separate, explicit op.
                        anyhow::bail!(
                            "{} is a real directory, not a link; use a purge action to delete it",
                            path.display()
                        );
                    }
                    Ok(format!("unlinked {}", paths::contract(path)))
                }
            }
        }
        FsOp::MoveIntoCanonical { from, to } => {
            if to.exists() {
                anyhow::bail!(
                    "canonical path {} already exists; refusing to overwrite it",
                    to.display()
                );
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            paths::backup(from)?;
            move_dir(from, to)?;
            Ok(format!(
                "moved {} into {}",
                paths::contract(from),
                paths::contract(to)
            ))
        }
        FsOp::RemoveTree(path) => {
            if !path.exists() {
                return Ok(format!("{} was already absent", paths::contract(path)));
            }
            paths::backup(path)?;
            std::fs::remove_dir_all(path)?;
            Ok(format!("removed {} (backed up)", paths::contract(path)))
        }
    }
}

/// Rename, falling back to copy+delete across filesystems.
fn move_dir(from: &Path, to: &PathBuf) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from)?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if ty.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::plan::Plan;

    /// `AGENTSYNC_HOME` is process-global, so two tests setting it concurrently
    /// will point each other's backup directory at a `TempDir` that is about to
    /// be dropped. Every test that redirects it holds this lock for its whole
    /// body, which serializes them.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TmpHome {
        dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TmpHome {
        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }
    }

    fn tmp_home() -> TmpHome {
        // Poisoning only means an earlier test panicked; the lock is still ours.
        let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // Keep backups inside the temp tree.
        unsafe { std::env::set_var("AGENTSYNC_HOME", dir.path()) };
        TmpHome { dir, _guard: guard }
    }

    #[test]
    fn a_failed_step_does_not_stop_the_run() {
        let home = tmp_home();
        let mut plan = Plan::default();
        // First step fails: setting hosts on an entry that isn't there.
        plan.push(
            "set hosts on a missing entry",
            Step::Manifest(ManifestOp::SetMcpHosts {
                name: "nope".into(),
                hosts: Some(vec!["codex".into()]),
            }),
        );
        plan.push("manual thing", Step::Manual("export FOO=bar".into()));

        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results.len(), 2, "second step must still run");
        assert_eq!(report.results[0].outcome, Outcome::Failed);
        assert_eq!(report.results[1].outcome, Outcome::Skipped);
    }

    #[test]
    fn manifest_is_not_written_when_a_manifest_op_failed() {
        let home = tmp_home();
        let path = home.path().join("manifest.toml");
        let mut plan = Plan::default();
        plan.push(
            "upsert skill",
            Step::Manifest(ManifestOp::UpsertSkill {
                name: "a".into(),
                source: "skills/a".into(),
            }),
        );
        plan.push(
            "set hosts on a missing entry",
            Step::Manifest(ManifestOp::SetMcpHosts {
                name: "nope".into(),
                hosts: None,
            }),
        );

        let mut manifest = Manifest::default();
        let report = run(&plan, &mut manifest, &path, &[], |_| {});
        assert!(!report.manifest_written);
        assert!(report.manifest_error.is_some());
        assert!(!path.exists(), "a partial manifest must not be persisted");
    }

    #[test]
    fn manifest_is_written_when_all_ops_succeed() {
        let home = tmp_home();
        let path = home.path().join("manifest.toml");
        let mut plan = Plan::default();
        plan.push(
            "upsert skill",
            Step::Manifest(ManifestOp::UpsertSkill {
                name: "a".into(),
                source: "skills/a".into(),
            }),
        );
        let mut manifest = Manifest::default();
        let report = run(&plan, &mut manifest, &path, &[], |_| {});
        assert!(report.manifest_written, "{:?}", report.manifest_error);
        assert!(path.exists());
    }

    #[test]
    fn a_step_for_an_absent_host_is_skipped_not_failed() {
        let home = tmp_home();
        let mut plan = Plan::default();
        plan.push(
            "add to ghost",
            Step::Host {
                host: "ghost".into(),
                argv: vec!["mcp".into(), "add".into()],
                cwd: None,
            },
        );
        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results[0].outcome, Outcome::Skipped);
        assert!(report.results[0].message.contains("not installed"));
    }

    #[test]
    fn unlink_refuses_to_delete_a_real_directory() {
        let home = tmp_home();
        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let err = apply_fs_op(&FsOp::Unlink(real)).unwrap_err();
        assert!(err.to_string().contains("real directory"), "{err}");
    }

    #[test]
    fn link_backs_up_a_real_directory_before_replacing_it() {
        let home = tmp_home();
        let target = home.path().join("canonical/mine");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("SKILL.md"), "canonical").unwrap();

        let link = home.path().join("hostdir/mine");
        std::fs::create_dir_all(&link).unwrap();
        std::fs::write(link.join("SKILL.md"), "host copy").unwrap();

        let message = apply_fs_op(&FsOp::Link {
            target: target.clone(),
            link: link.clone(),
        })
        .unwrap();
        assert!(message.contains("backed up"), "{message}");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(link.join("SKILL.md")).unwrap(),
            "canonical"
        );
    }

    #[test]
    fn move_into_canonical_refuses_to_clobber() {
        let home = tmp_home();
        let from = home.path().join("from");
        let to = home.path().join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let err = apply_fs_op(&FsOp::MoveIntoCanonical { from, to }).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}
