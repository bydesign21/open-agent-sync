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
    /// Populated when the tool could not write the manifest. The reason
    /// matters, because host-side changes can already exist.
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
/// `Started` exists because some steps are slow. A plugin install, for
/// example, clones a repository. A caller that hears about only *finished*
/// steps has nothing to show while a step runs, and that looks like a hang.
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
    // Maps a guard key to the label of the step that failed while carrying it,
    // so a later skip can name the actual cause instead of an opaque key.
    let mut failed_guards: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for (index, planned) in plan.steps.iter().enumerate() {
        progress(Progress::Started {
            index,
            label: &planned.label,
        });

        if let Some(guard) = &planned.guard
            && let Some(failed_label) = failed_guards.get(guard)
        {
            let command = match &planned.step {
                Step::Host { host, argv, .. } => Some(format!("{host} {}", argv.join(" "))),
                _ => None,
            };
            let result = StepResult {
                label: planned.label.clone(),
                outcome: Outcome::Skipped,
                command,
                message: format!(
                    "skipped: \"{failed_label}\" failed (guard \"{guard}\"), so this step \
                     did not run"
                ),
            };
            progress(Progress::Finished(&result));
            report.results.push(result);
            continue;
        }

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

            Step::ConfigTransaction(transaction) => {
                let mut transaction = transaction.clone();
                match transaction.execute() {
                    Ok(result) => StepResult {
                        label: planned.label.clone(),
                        outcome: Outcome::Done,
                        command: None,
                        message: format!("updated {} config source(s)", result.written_files.len()),
                    },
                    Err(e) => StepResult {
                        label: planned.label.clone(),
                        outcome: Outcome::Failed,
                        command: None,
                        message: e.to_string(),
                    },
                }
            }

            Step::FileTransaction(transaction) => {
                let mut transaction = transaction.clone();
                match transaction.execute() {
                    Ok(()) => StepResult {
                        label: planned.label.clone(),
                        outcome: Outcome::Done,
                        command: None,
                        message: "updated guarded artifacts".into(),
                    },
                    Err(e) => StepResult {
                        label: planned.label.clone(),
                        outcome: Outcome::Failed,
                        command: None,
                        message: e.to_string(),
                    },
                }
            }

            Step::Manual(text) => StepResult {
                label: planned.label.clone(),
                outcome: Outcome::Skipped,
                command: None,
                message: text.clone(),
            },
        };

        if result.outcome == Outcome::Failed
            && let Some(guard) = &planned.guard
        {
            failed_guards.insert(guard.clone(), planned.label.clone());
        }

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
                "manifest not written: at least one manifest edit failed. A \
                 partially-applied manifest makes the next run diff against \
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
        ManifestOp::UpsertInstruction {
            name,
            source,
            scope,
            repos,
        } => {
            manifest.instructions.insert(
                name.clone(),
                crate::manifest::InstructionEntry {
                    source: source.clone(),
                    scope: *scope,
                    repos: repos.clone(),
                    hosts: None,
                },
            );
        }
        ManifestOp::RemoveInstruction(name) => {
            manifest.instructions.remove(name);
        }
        ManifestOp::SetInstructionHosts { name, hosts } => {
            manifest
                .instructions
                .get_mut(name)
                .with_context(|| format!("instructions.{name} is not in the manifest"))?
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
            // This replaces content that agentsync did not create, so it counts
            // as a real mutation. Back it up first, and mention that in the
            // result message.
            let mut backed_up = None;
            if link.symlink_metadata().is_ok() {
                if link.is_dir() && std::fs::symlink_metadata(link)?.file_type().is_dir() {
                    backed_up = paths::backup(link)?;
                    std::fs::remove_dir_all(link)?;
                } else {
                    std::fs::remove_file(link)?;
                }
            }
            crate::platform::symlink(target, link)?;
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
                        // "unlink" label. Purging is a separate, explicit
                        // operation.
                        anyhow::bail!(
                            "{} is real content, not a link. Use a removal action to delete it.",
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
                    "canonical path {} already exists. This tool refuses to overwrite it.",
                    to.display()
                );
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            paths::backup(from)?;
            move_path(from, to)?;
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
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(format!("removed {} (backed up)", paths::contract(path)))
        }
        FsOp::WriteFile { path, contents } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, contents)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(format!("wrote {}", paths::contract(path)))
        }
    }
}

/// Rename the path. If rename does not work across filesystems, copy the
/// content, then delete the original.
///
/// Handles a file and a directory: skills are directories, instruction files
/// are files, and this function adopts both the same way.
fn move_path(from: &Path, to: &PathBuf) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_tree(from, to)?;
        std::fs::remove_dir_all(from)?;
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
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
    use crate::core::plan::Plan;

    use crate::testutil::TmpHome;

    fn tmp_home() -> TmpHome {
        TmpHome::new()
    }

    #[test]
    fn a_failed_step_does_not_stop_the_run() {
        let home = tmp_home();
        let mut plan = Plan::default();
        // First step fails: it sets hosts on an entry that does not exist.
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
    fn a_failed_guarded_step_skips_the_later_step_sharing_its_guard() {
        // A failed shim install must not be followed by removing the plugin it
        // was replacing. Modelled here with a manifest op that fails, guarding
        // a manual step that must never run once the guard has failed.
        let home = tmp_home();
        let mut plan = Plan::default();
        plan.push_guarded(
            "install the shim",
            Step::Manifest(ManifestOp::SetMcpHosts {
                name: "nope".into(),
                hosts: Some(vec!["codex".into()]),
            }),
            0,
            "shim:codex:example",
        );
        plan.push_guarded(
            "remove the original",
            Step::Manual("remove original plugin".into()),
            0,
            "shim:codex:example",
        );

        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results[0].outcome, Outcome::Failed);
        assert_eq!(report.results[1].outcome, Outcome::Skipped);
        assert!(
            report.results[1].message.contains("shim:codex:example"),
            "the skip message must name the guard that caused it: {}",
            report.results[1].message
        );
    }

    #[test]
    fn a_succeeding_guarded_step_lets_the_later_step_run() {
        let home = tmp_home();
        let mut plan = Plan::default();
        plan.push_guarded(
            "install the shim",
            Step::Manifest(ManifestOp::UpsertSkill {
                name: "a".into(),
                source: "skills/a".into(),
            }),
            0,
            "shim:codex:example",
        );
        plan.push_guarded(
            "remove the original",
            Step::Manual("remove original plugin".into()),
            0,
            "shim:codex:example",
        );

        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results[0].outcome, Outcome::Done);
        assert_eq!(
            report.results[1].outcome,
            Outcome::Skipped,
            "Manual steps are always reported Skipped, but this must be the \
             'do this by hand' skip, not the guard skip"
        );
        assert!(
            !report.results[1].message.contains("shim:codex:example"),
            "a successful guard must not produce a guard-skip message: {}",
            report.results[1].message
        );
    }

    #[test]
    fn an_unguarded_failure_does_not_stop_unrelated_steps() {
        // Existing behaviour that other domains rely on: a failure with no
        // guard key must not affect any other step, guarded or not.
        let home = tmp_home();
        let mut plan = Plan::default();
        plan.push(
            "unrelated failing step",
            Step::Manifest(ManifestOp::SetMcpHosts {
                name: "nope".into(),
                hosts: Some(vec!["codex".into()]),
            }),
        );
        plan.push_guarded(
            "install the shim",
            Step::Manifest(ManifestOp::UpsertSkill {
                name: "a".into(),
                source: "skills/a".into(),
            }),
            0,
            "shim:codex:example",
        );
        plan.push_guarded(
            "remove the original",
            Step::Manual("remove original plugin".into()),
            0,
            "shim:codex:example",
        );

        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results[0].outcome, Outcome::Failed);
        assert_eq!(
            report.results[1].outcome,
            Outcome::Done,
            "the unrelated failure must not guard-skip a step with a different key"
        );
        assert_eq!(report.results[2].outcome, Outcome::Skipped);
        assert!(
            !report.results[2].message.contains("shim:codex:example"),
            "the guard here never failed, so this must be the plain manual skip: {}",
            report.results[2].message
        );
    }

    #[test]
    fn one_shims_failed_guard_does_not_skip_a_different_shims_steps() {
        // Two shims in one plan: `a`'s install fails, `b`'s install and
        // removal are unrelated and must both still run. Guard keys are
        // per-shim, so a failure under one key must never leak into another.
        let home = tmp_home();
        let mut plan = Plan::default();
        plan.push_guarded(
            "install a",
            Step::Manifest(ManifestOp::SetMcpHosts {
                name: "nope".into(),
                hosts: Some(vec!["codex".into()]),
            }),
            0,
            "shim:codex:a",
        );
        plan.push_guarded(
            "remove original a",
            Step::Manual("remove original a".into()),
            0,
            "shim:codex:a",
        );
        plan.push_guarded(
            "install b",
            Step::Manifest(ManifestOp::UpsertSkill {
                name: "b".into(),
                source: "skills/b".into(),
            }),
            0,
            "shim:codex:b",
        );
        plan.push_guarded(
            "remove original b",
            Step::Manual("remove original b".into()),
            0,
            "shim:codex:b",
        );

        let mut manifest = Manifest::default();
        let report = run(
            &plan,
            &mut manifest,
            &home.path().join("manifest.toml"),
            &[],
            |_| {},
        );
        assert_eq!(report.results[0].outcome, Outcome::Failed, "a's install");
        assert_eq!(
            report.results[1].outcome,
            Outcome::Skipped,
            "a's removal must be skipped by a's own failed guard"
        );
        assert!(report.results[1].message.contains("shim:codex:a"));
        assert_eq!(
            report.results[2].outcome,
            Outcome::Done,
            "b's install must not be affected by a's failure"
        );
        assert_eq!(
            report.results[3].outcome,
            Outcome::Skipped,
            "b's removal is still the ordinary manual skip"
        );
        assert!(
            !report.results[3].message.contains("shim:codex:"),
            "b's guard never failed, so this must not read like a guard skip: {}",
            report.results[3].message
        );
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
    fn unlink_refuses_to_delete_real_content() {
        let home = tmp_home();
        // A directory (a skill) and a file (an instruction file) are both real
        // content: "unlink" must never be the thing that deletes them.
        let dir = home.path().join("real-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = apply_fs_op(&FsOp::Unlink(dir)).unwrap_err();
        assert!(err.to_string().contains("real content"), "{err}");

        let file = home.path().join("CLAUDE.md");
        std::fs::write(&file, "# instructions").unwrap();
        // A plain file is removable by unlink, because agentsync must clear a
        // host-owned instruction file before it replaces the file with a link.
        let message = apply_fs_op(&FsOp::Unlink(file.clone())).unwrap();
        assert!(message.contains("unlinked"), "{message}");
        assert!(!file.exists());
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
