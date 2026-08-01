//! Evaluating an `if` filter the target host cannot honour.
//!
//! Only two shapes are supported: a bare tool name, and `Tool(prefix:*)`.
//! Anything else returns [`Match::Unparseable`], and the caller fails open.
//! A redundant security review is safe. A skipped one is not.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    Yes,
    No,
    /// The pattern is not one of the two supported shapes.
    Unparseable,
}

/// Does `pattern` select this invocation?
pub fn matches(pattern: &str, tool_name: &str, tool_input: &Value) -> Match {
    let pattern = pattern.trim();

    // Bare tool name, for example `Bash`.
    let Some(open) = pattern.find('(') else {
        if pattern.is_empty() || pattern.contains(')') || pattern.contains('|') {
            return Match::Unparseable;
        }
        return yes_if(pattern == tool_name);
    };

    // `Tool(prefix:*)`. The closing parenthesis must be the last character,
    // otherwise this is a shape we do not model.
    if !pattern.ends_with(')') || pattern[open..].contains('|') {
        return Match::Unparseable;
    }
    let tool = &pattern[..open];
    if tool.is_empty() || tool.contains('|') {
        return Match::Unparseable;
    }
    let inner = &pattern[open + 1..pattern.len() - 1];
    if inner.contains('(') || inner.contains(')') {
        return Match::Unparseable;
    }
    let Some(prefix) = inner.strip_suffix(":*") else {
        return Match::Unparseable;
    };
    if prefix.is_empty() {
        return Match::Unparseable;
    }
    if tool != tool_name {
        return Match::No;
    }

    let Some(command) = tool_input.get("command").and_then(Value::as_str) else {
        return Match::No;
    };
    yes_if(command_starts_with(command.trim_start(), prefix))
}

/// A prefix match that stops at a word boundary, so `git commit` does not
/// select `git commit-tree`.
fn command_starts_with(command: &str, prefix: &str) -> bool {
    let Some(rest) = command.strip_prefix(prefix) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_whitespace(),
    }
}

fn yes_if(b: bool) -> Match {
    if b { Match::Yes } else { Match::No }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> serde_json::Value {
        json!({ "command": command })
    }

    #[test]
    fn a_prefix_pattern_matches_a_command_that_starts_with_it() {
        assert_eq!(
            matches("Bash(git commit:*)", "Bash", &bash("git commit -m hello")),
            Match::Yes
        );
    }

    #[test]
    fn a_prefix_pattern_stops_at_a_word_boundary() {
        // The whole point. `git commit-tree` is a different command, and a
        // naive starts_with would fire the commit review on it.
        assert_eq!(
            matches("Bash(git commit:*)", "Bash", &bash("git commit-tree x")),
            Match::No
        );
    }

    #[test]
    fn an_exact_prefix_with_nothing_after_it_matches() {
        assert_eq!(
            matches("Bash(git commit:*)", "Bash", &bash("git commit")),
            Match::Yes
        );
    }

    #[test]
    fn a_different_tool_never_matches() {
        assert_eq!(
            matches("Bash(git commit:*)", "Edit", &bash("git commit -m x")),
            Match::No
        );
    }

    #[test]
    fn a_bare_tool_name_matches_any_invocation_of_that_tool() {
        assert_eq!(
            matches("Bash", "Bash", &bash("anything at all")),
            Match::Yes
        );
        assert_eq!(matches("Bash", "Write", &bash("anything")), Match::No);
    }

    #[test]
    fn leading_whitespace_in_the_command_does_not_defeat_the_prefix() {
        assert_eq!(
            matches("Bash(git push:*)", "Bash", &bash("   git push origin main")),
            Match::Yes
        );
    }

    #[test]
    fn a_pattern_we_cannot_parse_is_reported_not_guessed() {
        // Never silently answer No: the caller fails open, because running a
        // redundant security review is safe and skipping one is not.
        assert_eq!(
            matches("Bash(git commit:*|Edit(*)", "Bash", &bash("git commit")),
            Match::Unparseable
        );
    }

    #[test]
    fn a_missing_command_field_cannot_match_a_prefix_pattern() {
        assert_eq!(
            matches("Bash(git commit:*)", "Bash", &serde_json::json!({})),
            Match::No
        );
    }

    #[test]
    fn a_piped_tool_list_before_the_paren_is_reported_not_guessed() {
        // `Edit|Write` style tool lists are real hook config shapes. We do not
        // model them. Reporting `No` here would silently skip the hook.
        assert_eq!(
            matches("Bash|Edit(git commit:*)", "Edit", &bash("git commit -m x")),
            Match::Unparseable
        );
    }

    #[test]
    fn a_piped_tool_list_with_no_paren_is_reported_not_guessed() {
        assert_eq!(
            matches("Bash|Edit", "Edit", &bash("anything")),
            Match::Unparseable
        );
    }

    #[test]
    fn an_empty_tool_name_is_reported_not_guessed() {
        assert_eq!(matches("(x:*)", "Bash", &bash("x foo")), Match::Unparseable);
    }

    #[test]
    fn a_trailing_paren_group_is_a_shape_we_do_not_model() {
        // `Bash(a:*)(b:*)` must not be read as the literal prefix `a:*)(b`.
        assert_eq!(
            matches("Bash(a:*)(b:*)", "Bash", &bash("a foo")),
            Match::Unparseable
        );
    }

    #[test]
    fn an_empty_prefix_is_reported_not_treated_as_match_everything() {
        // `Bash(:*)` reads like "match everything" but a literal empty
        // prefix only matches an empty command. Report it instead of
        // guessing either way.
        assert_eq!(
            matches("Bash(:*)", "Bash", &bash("anything")),
            Match::Unparseable
        );
    }
}
