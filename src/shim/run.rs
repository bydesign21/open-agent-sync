//! The shim runtime.
//!
//! Every failure here fails loud. A hook that silently does not run is
//! indistinguishable from a hook that found nothing, and for a security review
//! that is the worst outcome available. The one silent path is a deliberate
//! filter no-match, which is the whole reason the shim exists.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::shim::matcher::{self, Match};
use crate::shim::{ShimSpec, output};

#[derive(Debug, Default)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Run one shimmed handler against the host's hook input.
pub fn execute(spec: &ShimSpec, stdin: &str) -> Outcome {
    let mut stderr = String::new();

    if let Some(pattern) = &spec.if_pattern {
        match serde_json::from_str::<serde_json::Value>(stdin) {
            Err(_) => {
                // The hook input itself is not JSON. Missing data must never
                // read as a deliberate no-match, so run the hook and say why.
                stderr.push_str(&format!(
                    "agentsync: the hook input for {} is not JSON, so the filter {pattern:?} \
                     did not run and the hook ran unfiltered\n",
                    spec.source_id
                ));
            }
            Ok(parsed) => {
                let tool = parsed
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let empty = serde_json::Value::Object(Default::default());
                let tool_input = parsed.get("tool_input").unwrap_or(&empty);

                match matcher::matches(pattern, tool, tool_input) {
                    Match::No => return Outcome::default(),
                    Match::Yes => {}
                    Match::Unparseable => {
                        // Fail open, and say so. Silence here would hide a
                        // filter we never understood.
                        stderr.push_str(&format!(
                            "agentsync: the filter {pattern:?} on {} could not be parsed, \
                             so the hook ran unfiltered\n",
                            spec.source_id
                        ));
                    }
                }
            }
        }
    }

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&spec.command);
    if let Some(root) = &spec.plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!(
                    "{stderr}agentsync: could not start the hook for {}: {e}\n",
                    spec.source_id
                ),
                code: 1,
            };
        }
    };

    // Write stdin on its own thread. If the payload is larger than the pipe
    // buffer and the child fills its own stdout buffer before reading, a
    // write on this thread and a read on the same thread would deadlock: each
    // side waits on a buffer the other side must drain first.
    let pipe = child.stdin.take();
    let payload = stdin.to_string();
    let writer = std::thread::spawn(move || {
        if let Some(mut pipe) = pipe {
            // A hook that ignores stdin closes the pipe early. That is not an
            // error. Anything else means the hook ran on truncated input.
            if let Err(e) = pipe.write_all(payload.as_bytes())
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                return Some(e.to_string());
            }
        }
        // The pipe drops here, which closes the child's stdin.
        None
    });

    let done = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!(
                    "{stderr}agentsync: the hook for {} failed: {e}\n",
                    spec.source_id
                ),
                code: 1,
            };
        }
    };
    let write_error = writer.join().ok().flatten();

    stderr.push_str(&String::from_utf8_lossy(&done.stderr));
    if let Some(e) = write_error {
        stderr.push_str(&format!(
            "agentsync: writing stdin to the hook for {} failed: {e}\n",
            spec.source_id
        ));
    }

    let code = match done.status.code() {
        Some(c) => c,
        None => {
            // No exit code means the process was killed by a signal. Name it,
            // so an operator can tell a signalled hook from a broken shim
            // instead of seeing the same generic failure code for both.
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = done.status.signal() {
                    stderr.push_str(&format!(
                        "agentsync: the hook for {} was killed by signal {sig}\n",
                        spec.source_id
                    ));
                }
            }
            1
        }
    };

    Outcome {
        stdout: output::normalize(&String::from_utf8_lossy(&done.stdout), spec),
        stderr,
        code,
    }
}

/// The `agentsync hook-shim` entry point. Returns the exit code to use.
pub fn main(spec_path: &Path) -> Result<i32> {
    let spec = ShimSpec::load(spec_path).with_context(|| {
        format!(
            "the shim at {} is broken, so its hook did not run",
            spec_path.display()
        )
    })?;
    let mut stdin = String::new();
    // A host may invoke a hook with no input at all. That is not a failure.
    // A read that fails partway through is worth naming, though.
    let read_error = std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).err();

    let mut out = execute(&spec, &stdin);
    if let Some(e) = read_error {
        out.stderr.push_str(&format!(
            "agentsync: reading stdin for {} failed: {e}\n",
            spec.source_id
        ));
    }
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    Ok(out.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::ShimSpec;

    fn spec(command: &str, if_pattern: Option<&str>) -> ShimSpec {
        ShimSpec {
            source_id: "demo@mkt:hooks/hooks.json:post_tool_use:0:0".into(),
            command: command.into(),
            plugin_root: None,
            if_pattern: if_pattern.map(str::to_string),
            allowed_output: vec!["systemMessage".into()],
            fold_into_system_message: vec![],
        }
    }

    fn input(tool: &str, command: &str) -> String {
        serde_json::json!({
            "tool_name": tool,
            "tool_input": { "command": command }
        })
        .to_string()
    }

    #[test]
    fn a_matching_filter_runs_the_command() {
        let out = execute(
            &spec(
                "echo '{\"systemMessage\":\"ran\"}'",
                Some("Bash(git commit:*)"),
            ),
            &input("Bash", "git commit -m x"),
        );
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("ran"), "got {out:?}");
    }

    #[test]
    fn a_filter_that_does_not_match_exits_quietly_without_running_anything() {
        let out = execute(
            &spec("echo SHOULD_NOT_RUN", Some("Bash(git commit:*)")),
            &input("Bash", "git commit-tree x"),
        );
        assert_eq!(out.code, 0);
        assert_eq!(out.stdout, "", "the command must not have run: {out:?}");
    }

    #[test]
    fn an_unparseable_filter_fails_open_and_runs_the_command() {
        // Running a redundant review is safe. Skipping one is not.
        let out = execute(
            &spec("echo ran", Some("Bash(git commit:*|nonsense")),
            &input("Bash", "git commit"),
        );
        assert!(out.stdout.contains("ran"), "must fail open: {out:?}");
        assert!(
            out.stderr.contains("could not be parsed"),
            "and must say why: {out:?}"
        );
    }

    #[test]
    fn the_original_exit_code_is_propagated() {
        let out = execute(&spec("exit 3", None), &input("Bash", "x"));
        assert_eq!(out.code, 3);
    }

    #[test]
    fn the_hook_input_reaches_the_command_on_stdin() {
        // The command must print something that is not a JSON object. `cat`
        // alone replays the hook input, and normalisation would then treat that
        // as the hook's own structured output and filter it.
        let out = execute(
            &spec("printf 'saw: %s' \"$(cat)\"", None),
            &input("Bash", "git push"),
        );
        assert!(out.stdout.contains("git push"), "stdin not piped: {out:?}");
    }

    #[test]
    fn plugin_root_is_exported_so_the_original_paths_resolve() {
        let mut s = spec("echo \"root=$CLAUDE_PLUGIN_ROOT\"", None);
        s.plugin_root = Some("/original/plugin/2.0.6".into());
        let out = execute(&s, &input("Bash", "x"));
        assert!(
            out.stdout.contains("root=/original/plugin/2.0.6"),
            "the shim's own root would break every path in the command: {out:?}"
        );
    }

    #[test]
    fn output_is_normalised_to_the_targets_accepted_fields() {
        let out = execute(
            &spec(
                "echo '{\"systemMessage\":\"keep\",\"rewakeSummary\":\"SENTINEL_VALUE\"}'",
                None,
            ),
            &input("Bash", "x"),
        );
        assert!(out.stdout.contains("keep"), "kept field lost: {out:?}");
        assert!(
            !out.stdout.contains("SENTINEL_VALUE"),
            "the rejected field's VALUE must not survive: {out:?}"
        );
        assert!(
            out.stdout.contains("rewakeSummary"),
            "but the drop must be NAMED, never silent: {out:?}"
        );
    }

    #[test]
    fn a_command_that_cannot_start_fails_loudly_rather_than_reporting_success() {
        let out = execute(&spec("/nonexistent/binary/xyz", None), &input("Bash", "x"));
        assert_ne!(out.code, 0, "a hook that did not run must not look clean");
    }

    #[test]
    fn a_large_stdin_payload_does_not_deadlock_a_command_that_ignores_it() {
        // If the payload is bigger than the pipe buffer and the child never
        // reads it, writing on this thread while also waiting on this thread
        // would deadlock: each side blocks on a buffer the other must drain.
        let payload = "x".repeat(200_000);
        let out = execute(&spec("true", None), &payload);
        assert_eq!(out.code, 0, "must not hang: {out:?}");
    }

    #[test]
    fn malformed_stdin_fails_open_and_runs_the_command() {
        // Missing or broken input must never read as a deliberate no-match.
        let out = execute(
            &spec("echo ran", Some("Bash(git commit:*)")),
            "not-json-stdin",
        );
        assert!(out.stdout.contains("ran"), "must fail open: {out:?}");
        assert!(
            out.stderr.contains("is not JSON"),
            "and must say why: {out:?}"
        );
    }

    #[test]
    fn a_signal_killed_hook_names_the_signal_rather_than_a_generic_failure() {
        let out = execute(&spec("kill -9 $$", None), &input("Bash", "x"));
        assert_ne!(out.code, 0, "got {out:?}");
        assert!(out.stderr.contains("signal"), "got {out:?}");
    }
}
