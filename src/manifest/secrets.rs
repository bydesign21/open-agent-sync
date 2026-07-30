//! The secret gate.
//!
//! The manifest is meant to be committed. It must therefore hold *names* of
//! environment variables, never their values. This is enforced as a hard gate on
//! save rather than a lint, because a lint that can be ignored will be, and the
//! failure mode is a credential in git history.

/// A value that looks like a live credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretFinding {
    /// Dotted path into the manifest, e.g. `mcp.upskillai-knowledge.headers.Authorization`.
    pub location: String,
    pub reason: &'static str,
}

/// True when the value is nothing but environment-variable references and
/// punctuation, which is the shape we *want* people to write.
fn is_only_references(value: &str) -> bool {
    let mut rest = value;
    let mut saw_reference = false;
    while let Some(start) = rest.find("${") {
        let before = &rest[..start];
        if before.chars().any(|c| c.is_ascii_alphanumeric()) {
            // Literal alphanumeric text outside a reference: keep scanning the
            // whole value with the heuristics instead.
            return false;
        }
        let Some(end) = rest[start..].find('}') else {
            return false;
        };
        saw_reference = true;
        rest = &rest[start + end + 1..];
    }
    saw_reference && !rest.chars().any(|c| c.is_ascii_alphanumeric())
}

fn looks_like_hex(token: &str) -> bool {
    token.len() >= 32 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn looks_like_base64ish(token: &str) -> bool {
    if token.len() < 40 {
        return false;
    }
    let ok = token.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == '-' || c == '_'
    });
    if !ok {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    has_upper && has_lower && has_digit
}

const PREFIXES: &[(&str, &str)] = &[
    ("sk-", "OpenAI-style secret key prefix"),
    ("sk_live_", "live secret key prefix"),
    ("sk_test_", "test secret key prefix"),
    ("ghp_", "GitHub personal access token prefix"),
    ("gho_", "GitHub OAuth token prefix"),
    ("github_pat_", "GitHub fine-grained token prefix"),
    ("xoxb-", "Slack bot token prefix"),
    ("xoxp-", "Slack user token prefix"),
    ("AKIA", "AWS access key id prefix"),
    ("AIza", "Google API key prefix"),
    ("npm_", "npm token prefix"),
    ("dop_v1_", "DigitalOcean token prefix"),
    ("glpat-", "GitLab token prefix"),
];

/// Inspect one value. Returns the reason it looks like a credential, if it does.
pub fn inspect(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || is_only_references(trimmed) {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("bearer ") && !trimmed.contains("${") {
        return Some("literal Bearer token");
    }

    for (prefix, reason) in PREFIXES {
        // Case-sensitive: `AKIA` and `akia` are not the same signal.
        if trimmed.starts_with(prefix) && trimmed.len() > prefix.len() + 8 {
            return Some(reason);
        }
    }

    // Only judge opaque single tokens. A sentence or a path with a long word in
    // it is not a credential, and flagging it would train people to bypass this.
    let token = trimmed.split_whitespace().last().unwrap_or(trimmed);
    if trimmed.split_whitespace().count() > 1 && !lower.starts_with("bearer ") {
        return None;
    }
    if token.contains('/') || token.contains('\\') {
        return None;
    }
    if looks_like_hex(token) {
        return Some("64-char-class hex string, likely a token");
    }
    if looks_like_base64ish(token) {
        return Some("long mixed-case token, likely a credential");
    }
    None
}

/// Check a labelled set of values, accumulating findings.
pub fn check(location: &str, value: &str, out: &mut Vec<SecretFinding>) {
    if let Some(reason) = inspect(value) {
        out.push(SecretFinding {
            location: location.to_string(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_references_are_the_blessed_form() {
        assert_eq!(inspect("${UPSKILLAI_KNOWLEDGE_TOKEN}"), None);
        assert_eq!(inspect("Bearer ${TOKEN}"), None);
    }

    #[test]
    fn the_real_token_from_claude_json_is_caught() {
        // The shape actually found in ~/.claude.json: a 64-char hex bearer.
        let v = "Bearer f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae";
        assert!(inspect(v).is_some());
        let bare = "f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae";
        assert!(inspect(bare).is_some());
    }

    #[test]
    fn known_prefixes_are_caught() {
        assert!(inspect("ghp_abcdefghijklmnopqrstuvwxyz01").is_some());
        assert!(inspect("sk-abcdefghijklmnopqrstuvwxyz").is_some());
        assert!(inspect("xoxb-1234567890-abcdefghij").is_some());
    }

    #[test]
    fn ordinary_config_values_pass() {
        for ok in [
            "info",
            "debug",
            "https://api.example.com/platform/knowledge/mcp",
            "/Applications/KiCad/KiCad.app/Contents/Frameworks/Python.framework/Versions/Current/bin/python3",
            "node",
            "application/json",
            "--experimental-vm-modules",
        ] {
            assert_eq!(inspect(ok), None, "false positive on {ok:?}");
        }
    }

    #[test]
    fn long_prose_is_not_a_credential() {
        assert_eq!(
            inspect("this is a fairly long human sentence with many words in it"),
            None
        );
    }
}
