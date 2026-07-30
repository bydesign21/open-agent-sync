//! argv templating and subprocess execution.
//!
//! Template vocabulary, deliberately tiny:
//!
//! * `{key}` anywhere inside an element is replaced by a scalar. An unknown key
//!   is an error, never an empty string — a silently-empty argument would
//!   produce a command that runs and does the wrong thing.
//! * An element that is exactly `{key...}` splices a list in place, contributing
//!   zero or more arguments. This is the only construct allowed to vanish.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

pub type Scalars = BTreeMap<String, String>;
pub type Lists = BTreeMap<String, Vec<String>>;

pub fn render(argv: &[String], scalars: &Scalars, lists: &Lists) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(argv.len());
    for element in argv {
        if let Some(key) = element
            .strip_prefix('{')
            .and_then(|e| e.strip_suffix("...}"))
        {
            out.extend(lists.get(key).cloned().unwrap_or_default());
            continue;
        }
        out.push(substitute(element, scalars)?);
    }
    Ok(out)
}

fn substitute(element: &str, scalars: &Scalars) -> Result<String> {
    let mut out = String::with_capacity(element.len());
    let mut rest = element;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            bail!("unterminated `{{` in argv element {element:?}");
        };
        let key = &after[..end];
        match scalars.get(key) {
            Some(value) => out.push_str(value),
            None => bail!(
                "argv element {element:?} references unknown template key {key:?}; \
                 known keys: {}",
                scalars.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
    /// Whichever stream carries the useful message.
    pub fn message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        self.stdout.trim().to_string()
    }
}

pub fn run(bin: &Path, argv: &[String], cwd: Option<&Path>) -> Result<Output> {
    let mut cmd = Command::new(bin);
    cmd.args(argv);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output()?;
    Ok(Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// Render one command as a copy-pasteable shell line.
pub fn shell_line(bin: &str, argv: &[String]) -> String {
    let mut parts = vec![shell_escape::escape(bin.into()).to_string()];
    parts.extend(
        argv.iter()
            .map(|a| shell_escape::escape(a.clone().into()).to_string()),
    );
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn s(pairs: &[(&str, &str)]) -> Scalars {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn substitutes_scalars_including_embedded_ones() {
        let out = render(
            &argv(&["mcp", "add", "{name}", "--scope={scope}"]),
            &s(&[("name", "kicad"), ("scope", "user")]),
            &Lists::new(),
        )
        .unwrap();
        assert_eq!(out, vec!["mcp", "add", "kicad", "--scope=user"]);
    }

    #[test]
    fn splices_lists_and_lets_them_be_empty() {
        let lists = Lists::from([("args".to_string(), vec!["a".to_string(), "b".to_string()])]);
        let out = render(&argv(&["run", "{args...}", "{extra...}"]), &s(&[]), &lists).unwrap();
        assert_eq!(out, vec!["run", "a", "b"]);
    }

    #[test]
    fn an_unknown_scalar_is_an_error_not_an_empty_argument() {
        let err = render(&argv(&["mcp", "add", "{name}"]), &s(&[]), &Lists::new()).unwrap_err();
        assert!(err.to_string().contains("unknown template key"), "{err}");
    }

    #[test]
    fn unterminated_brace_is_rejected() {
        let err = render(&argv(&["{oops"]), &s(&[]), &Lists::new()).unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn shell_line_quotes_json_arguments() {
        let line = shell_line(
            "claude",
            &argv(&["mcp", "add-json", "k", r#"{"command":"node"}"#]),
        );
        assert!(line.starts_with("claude mcp add-json k "));
        // The JSON must survive a round trip through a shell.
        assert!(line.contains("command"));
        assert!(line.contains('\'') || line.contains('\\'));
    }
}
