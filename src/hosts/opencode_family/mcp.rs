//! MCP reconciliation behaviour for the OpenCode family.
//!
//! The parsers themselves live in [`crate::hosts::parsers::mcp`] alongside the
//! other host parsers. The contract tests live here, with the rest of the
//! family engine, because what they pin down is family behaviour rather than
//! parser plumbing.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::core::model::{OAuthState, Scope, Transport};
    use crate::hosts::parsers::{
        ParseCtx,
        mcp::{kilo_mcp_project_jsonc_v1, opencode_mcp_jsonc_v1},
    };

    fn ctx() -> ParseCtx {
        ParseCtx {
            repo: Some("/repo".into()),
            origin: PathBuf::from("/cfg/opencode/opencode.jsonc"),
        }
    }

    fn read(text: &str) -> crate::hosts::parsers::McpRead {
        opencode_mcp_jsonc_v1(text, &ctx()).expect("parse")
    }

    fn only(text: &str) -> crate::core::model::McpServer {
        let mut found = read(text);
        assert_eq!(found.servers.len(), 1, "expected exactly one server");
        found.servers.remove(0).1
    }

    #[test]
    fn jsonc_comments_do_not_prevent_reading_servers() {
        let server = only(
            r#"{
              // a comment that must not break parsing
              "mcp": { "s": { "type": "local", "command": ["srv"] } }
            }"#,
        );
        assert_eq!(server.name, "s");
    }

    #[test]
    fn a_command_array_round_trips_without_shell_splitting() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local",
               "command":["my-server","--flag","an argument with spaces"]}}}"#,
        );
        let Transport::Stdio(stdio) = server.transport else {
            panic!("expected stdio")
        };
        assert_eq!(stdio.command, "my-server");
        assert_eq!(
            stdio.args,
            vec!["--flag".to_string(), "an argument with spaces".to_string()],
            "an argument containing spaces must survive as one argument"
        );
    }

    #[test]
    fn literal_environment_values_and_env_references_stay_distinct() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"LITERAL":"plain-value","TOKEN":"{env:TOKEN}"}}}}"#,
        );
        let Transport::Stdio(stdio) = server.transport else {
            panic!("expected stdio")
        };
        assert_eq!(stdio.env.get("LITERAL").unwrap(), "plain-value");
        assert!(
            !stdio.env.contains_key("TOKEN"),
            "a reference must not be recorded as a literal value"
        );
        assert_eq!(stdio.env_from, vec!["TOKEN".to_string()]);
    }

    #[test]
    fn an_environment_key_reading_a_differently_named_variable_is_blocked() {
        let found = read(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"A":"{env:SOMETHING_ELSE}"}}}}"#,
        );
        let Transport::Stdio(stdio) = &found.servers[0].1.transport else {
            panic!("expected stdio")
        };
        assert!(stdio.env_from.is_empty());
        assert!(
            !stdio.env.contains_key("A"),
            "the placeholder text must not be flattened into a literal value, \
             or it would be pushed verbatim into other hosts"
        );
        assert!(
            found.warnings.iter().any(|w| w.contains("blocked")),
            "expected a blocker warning, got {:?}",
            found.warnings
        );
    }

    #[test]
    fn a_bearer_env_reference_is_distinct_from_a_plain_header() {
        let server = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "headers":{"Authorization":"Bearer {env:TOK}","X-Trace":"on"}}}}"#,
        );
        let Transport::Http(http) = server.transport else {
            panic!("expected http")
        };
        assert_eq!(http.bearer_token_env.as_deref(), Some("TOK"));
        assert_eq!(http.headers.get("X-Trace").unwrap(), "on");
        assert!(
            !http.headers.contains_key("Authorization"),
            "a bearer env reference must not also remain a raw header"
        );
    }

    #[test]
    fn a_literal_authorization_header_is_not_laundered_into_a_bearer_reference() {
        let server = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "headers":{"Authorization":"Bearer sk-literal-secret"}}}}"#,
        );
        let Transport::Http(http) = server.transport else {
            panic!("expected http")
        };
        assert_eq!(http.bearer_token_env, None);
        assert_eq!(
            http.headers.get("Authorization").unwrap(),
            "Bearer sk-literal-secret",
            "a literal credential must stay visible to the secret gate"
        );
    }

    #[test]
    fn enabled_cwd_and_timeout_are_represented() {
        let server = only(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "enabled":false,"cwd":"/work","timeout":5000}}}"#,
        );
        assert_eq!(server.enabled, Some(false));
        assert_eq!(server.cwd.as_deref(), Some("/work"));
        assert_eq!(
            server.timeout_json.as_deref(),
            Some("5000"),
            "the timeout is carried as exact JSON text because its unit is \
             unverified against the runtime"
        );
    }

    #[test]
    fn an_absent_field_stays_absent_rather_than_defaulting() {
        let server = only(r#"{"mcp":{"s":{"type":"local","command":["srv"]}}}"#);
        assert_eq!(
            server.enabled, None,
            "absent must not become Some(true); the host default is not ours to invent"
        );
        assert_eq!(server.timeout_json, None);
        assert_eq!(server.cwd, None);
        assert_eq!(server.oauth, OAuthState::Unspecified);
    }

    #[test]
    fn oauth_follow_up_is_only_emitted_for_explicit_oauth_state() {
        let plain = only(r#"{"mcp":{"s":{"type":"remote","url":"https://public.example/mcp"}}}"#);
        assert_eq!(plain.oauth, OAuthState::Unspecified);
        assert!(
            !plain.needs_oauth_login(),
            "a public HTTP server must not generate OAuth work nobody asked for"
        );

        let disabled =
            only(r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp","oauth":false}}}"#);
        assert_eq!(disabled.oauth, OAuthState::Disabled);
        assert!(!disabled.needs_oauth_login());

        let automatic =
            only(r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp","oauth":true}}}"#);
        assert_eq!(automatic.oauth, OAuthState::Automatic);
        assert!(automatic.needs_oauth_login());
    }

    #[test]
    fn an_oauth_client_secret_is_only_carried_as_an_environment_reference() {
        let referenced = only(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "oauth":{"client_id":"abc","client_secret":"{env:OAUTH_SECRET}"}}}}"#,
        );
        assert_eq!(
            referenced.oauth,
            OAuthState::Client {
                client_id: "abc".into(),
                client_secret_env: Some("OAUTH_SECRET".into()),
            }
        );

        let literal = read(
            r#"{"mcp":{"s":{"type":"remote","url":"https://e.example/mcp",
               "oauth":{"client_id":"abc","client_secret":"sk-literal"}}}}"#,
        );
        let OAuthState::Client {
            client_secret_env, ..
        } = &literal.servers[0].1.oauth
        else {
            panic!("expected an explicit client")
        };
        assert_eq!(
            *client_secret_env, None,
            "a literal client secret must never be carried into the manifest"
        );
        let joined = literal.warnings.join(" ");
        assert!(joined.contains("literal"), "expected a warning: {joined}");
        assert!(
            !joined.contains("sk-literal"),
            "the warning must not echo the secret itself: {joined}"
        );
    }

    #[test]
    fn kilo_project_scope_environment_references_are_blocked() {
        let found = kilo_mcp_project_jsonc_v1(
            r#"{"mcp":{"s":{"type":"local","command":["srv"],
               "environment":{"TOKEN":"{env:TOKEN}"}}}}"#,
            &ParseCtx {
                repo: Some("/repo".into()),
                origin: PathBuf::from("/repo/.kilo/kilo.jsonc"),
            },
        )
        .unwrap();

        assert!(matches!(found.servers[0].0, Scope::Project(_)));
        assert!(
            found
                .warnings
                .iter()
                .any(|w| w.contains("project-scope environment references are blocked")),
            "expected a Kilo project-scope blocker, got {:?}",
            found.warnings
        );
    }

    #[test]
    fn a_malformed_or_unknown_server_is_skipped_with_a_warning_not_invented() {
        for body in [
            r#"{"mcp":{"s":{"type":"local"}}}"#,
            r#"{"mcp":{"s":{"type":"remote"}}}"#,
            r#"{"mcp":{"s":{"type":"telepathy","command":["srv"]}}}"#,
            r#"{"mcp":{"s":{"type":"local","command":[]}}}"#,
            r#"{"mcp":{"s":"not-an-object"}}"#,
        ] {
            let found = read(body);
            assert!(
                found.servers.is_empty(),
                "{body} must not produce an invented server"
            );
            assert_eq!(found.warnings.len(), 1, "{body} must report why");
        }
    }

    #[test]
    fn a_config_without_an_mcp_section_yields_nothing() {
        let found = read(r#"{"model":"some/model"}"#);
        assert!(found.servers.is_empty());
        assert!(found.warnings.is_empty());
    }
}
