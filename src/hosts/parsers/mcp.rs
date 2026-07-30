//! MCP config parsers.
//!
//! All of them normalize two idioms so the same server read from different hosts
//! compares equal:
//!
//! * `env = { FOO = "${FOO}" }` becomes `env_from = ["FOO"]`. Self-referential
//!   passthrough is the same intent expressed differently, and treating it as a
//!   literal value would make every such server look divergent forever.
//! * `Authorization: Bearer ${VAR}` becomes `bearer_token_env = "VAR"`. This
//!   matters because it is the difference between a server Codex can accept and
//!   one it must refuse — Codex has `--bearer-token-env-var` but no `--header`.
//!
//! A *literal* `Authorization: Bearer <token>` is deliberately **not**
//! normalized. It is kept as a header and reported, so it surfaces as an unsafe
//! row rather than being quietly laundered into the manifest.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{McpRead, ParseCtx};
use crate::core::model::{AuthStatus, HttpServer, McpServer, Scope, StdioServer, Transport};
use crate::manifest::secrets;

/// `~/.claude.json`: user-scope `mcpServers` plus per-repo `projects.*.mcpServers`.
pub fn claude_json_v1(text: &str, ctx: &ParseCtx) -> Result<McpRead> {
    let root: Value =
        serde_json::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = McpRead::default();

    if let Some(map) = root.get("mcpServers").and_then(Value::as_object) {
        collect(map, Scope::User, &mut out);
    }

    if let Some(projects) = root.get("projects").and_then(Value::as_object) {
        for (path, project) in projects {
            if let Some(map) = project.get("mcpServers").and_then(Value::as_object)
                && !map.is_empty()
            {
                collect(map, Scope::Local(path.clone()), &mut out);
            }
        }
    }
    Ok(out)
}

/// `<repo>/.mcp.json`: a bare `mcpServers` map, committed and shared. Read by
/// both Claude Code and (per its loader) Codex.
pub fn mcp_json_v1(text: &str, ctx: &ParseCtx) -> Result<McpRead> {
    let root: Value =
        serde_json::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = McpRead::default();
    let repo = ctx.repo.clone().unwrap_or_default();
    if let Some(map) = root.get("mcpServers").and_then(Value::as_object) {
        collect(map, Scope::Project(repo), &mut out);
    }
    Ok(out)
}

fn collect(map: &serde_json::Map<String, Value>, scope: Scope, out: &mut McpRead) {
    for (name, def) in map {
        match parse_json_server(name, def) {
            Ok((server, warnings)) => {
                out.warnings.extend(warnings);
                out.servers.push((scope.clone(), server));
            }
            Err(e) => out
                .warnings
                .push(format!("skipped {scope} server {name:?}: {e:#}")),
        }
    }
}

fn parse_json_server(name: &str, def: &Value) -> Result<(McpServer, Vec<String>)> {
    let mut warnings = Vec::new();
    let declared = def.get("type").and_then(Value::as_str);
    let has_url = def.get("url").and_then(Value::as_str).is_some();
    let has_command = def.get("command").and_then(Value::as_str).is_some();

    let is_http = match declared {
        Some("http") | Some("sse") => true,
        Some("stdio") => false,
        // No `type` key: infer, which is what the hosts themselves do.
        _ => has_url && !has_command,
    };

    let transport = if is_http {
        let url = def
            .get("url")
            .and_then(Value::as_str)
            .context("http server has no `url`")?
            .to_string();
        let mut headers = BTreeMap::new();
        if let Some(map) = def.get("headers").and_then(Value::as_object) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    headers.insert(k.clone(), s.to_string());
                }
            }
        }
        let (headers, bearer_token_env) = split_bearer(name, headers, &mut warnings);
        Transport::Http(HttpServer {
            url,
            headers,
            bearer_token_env,
        })
    } else {
        let command = def
            .get("command")
            .and_then(Value::as_str)
            .context("stdio server has no `command`")?
            .to_string();
        let args = def
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut env = BTreeMap::new();
        if let Some(map) = def.get("env").and_then(Value::as_object) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    env.insert(k.clone(), s.to_string());
                }
            }
        }
        let (env, env_from) = split_passthrough(env);
        Transport::Stdio(StdioServer {
            command,
            args,
            env,
            env_from,
        })
    };

    Ok((
        McpServer {
            name: name.to_string(),
            transport,
        },
        warnings,
    ))
}

/// `FOO = "${FOO}"` is passthrough, not a literal value.
fn split_passthrough(env: BTreeMap<String, String>) -> (BTreeMap<String, String>, Vec<String>) {
    let mut literal = BTreeMap::new();
    let mut from = Vec::new();
    for (k, v) in env {
        if v == format!("${{{k}}}") {
            from.push(k);
        } else {
            literal.insert(k, v);
        }
    }
    from.sort();
    (literal, from)
}

/// Pull `Authorization: Bearer ${VAR}` out into `bearer_token_env`. A literal
/// token stays put and earns a warning.
fn split_bearer(
    name: &str,
    mut headers: BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> (BTreeMap<String, String>, Option<String>) {
    let key = headers
        .keys()
        .find(|k| k.eq_ignore_ascii_case("authorization"))
        .cloned();
    let Some(key) = key else {
        return (headers, None);
    };
    let value = headers.get(&key).cloned().unwrap_or_default();

    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or(&value)
        .trim();

    if let Some(inner) = rest.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        headers.remove(&key);
        return (headers, Some(inner.to_string()));
    }

    if let Some(reason) = secrets::inspect(&value) {
        warnings.push(format!(
            "{name}: Authorization header holds a literal credential ({reason})"
        ));
    }
    (headers, None)
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// `~/.codex/config.toml` `[mcp_servers.NAME]`.
pub fn codex_toml_v1(text: &str, ctx: &ParseCtx) -> Result<McpRead> {
    let root: toml::Value =
        toml::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = McpRead::default();

    let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(out);
    };

    for (name, def) in servers {
        match parse_codex_server(name, def) {
            Ok(server) => out.servers.push((Scope::User, server)),
            Err(e) => out
                .warnings
                .push(format!("skipped codex server {name:?}: {e:#}")),
        }
    }
    Ok(out)
}

fn parse_codex_server(name: &str, def: &toml::Value) -> Result<McpServer> {
    let table = def.as_table().context("server entry is not a table")?;

    let transport = if let Some(url) = table.get("url").and_then(toml::Value::as_str) {
        let mut headers = BTreeMap::new();
        for key in ["http_headers", "env_http_headers"] {
            if let Some(map) = table.get(key).and_then(toml::Value::as_table) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        // env_http_headers maps a header to an env var *name*;
                        // render it as a reference so it compares equal to the
                        // same intent expressed on another host.
                        let value = if key == "env_http_headers" {
                            format!("${{{s}}}")
                        } else {
                            s.to_string()
                        };
                        headers.insert(k.clone(), value);
                    }
                }
            }
        }
        let bearer_token_env = table
            .get("bearer_token_env_var")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        Transport::Http(HttpServer {
            url: url.to_string(),
            headers,
            bearer_token_env,
        })
    } else {
        let command = table
            .get("command")
            .and_then(toml::Value::as_str)
            .context("stdio server has no `command`")?
            .to_string();
        let args = table
            .get("args")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut env = BTreeMap::new();
        if let Some(map) = table.get("env").and_then(toml::Value::as_table) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    env.insert(k.clone(), s.to_string());
                }
            }
        }
        let mut env_from: Vec<String> = table
            .get("env_vars")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        env_from.sort();
        Transport::Stdio(StdioServer {
            command,
            args,
            env,
            env_from,
        })
    };

    Ok(McpServer {
        name: name.to_string(),
        transport,
    })
}

// ---------------------------------------------------------------------------
// Auth status
// ---------------------------------------------------------------------------

/// `codex mcp list --json`: an array of `{ name, auth_status, ... }`.
pub fn codex_auth_v1(text: &str, ctx: &ParseCtx) -> Result<super::AuthRead> {
    let root: Value = serde_json::from_str(text)
        .with_context(|| format!("parsing auth status from {}", ctx.origin.display()))?;
    let mut out = super::AuthRead::default();

    let Some(entries) = root.as_array() else {
        out.warnings
            .push("expected a JSON array of servers".to_string());
        return Ok(out);
    };

    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let raw = entry
            .get("auth_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        out.statuses
            .insert(name.to_string(), AuthStatus::parse(raw));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

/// Produce the JSON document `claude mcp add-json` expects, undoing the
/// normalizations done on read.
pub fn claude_json_v1_serialize(server: &McpServer) -> Result<String> {
    let value = match &server.transport {
        Transport::Stdio(s) => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String("stdio".into()));
            obj.insert("command".into(), Value::String(s.command.clone()));
            if !s.args.is_empty() {
                obj.insert(
                    "args".into(),
                    Value::Array(s.args.iter().cloned().map(Value::String).collect()),
                );
            }
            let mut env = serde_json::Map::new();
            for (k, v) in &s.env {
                env.insert(k.clone(), Value::String(v.clone()));
            }
            for k in &s.env_from {
                env.insert(k.clone(), Value::String(format!("${{{k}}}")));
            }
            if !env.is_empty() {
                obj.insert("env".into(), Value::Object(env));
            }
            Value::Object(obj)
        }
        Transport::Http(h) => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String("http".into()));
            obj.insert("url".into(), Value::String(h.url.clone()));
            let mut headers = serde_json::Map::new();
            for (k, v) in &h.headers {
                headers.insert(k.clone(), Value::String(v.clone()));
            }
            if let Some(var) = &h.bearer_token_env {
                headers.insert(
                    "Authorization".into(),
                    Value::String(format!("Bearer ${{{var}}}")),
                );
            }
            if !headers.is_empty() {
                obj.insert("headers".into(), Value::Object(headers));
            }
            Value::Object(obj)
        }
    };
    Ok(serde_json::to_string(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ctx() -> ParseCtx {
        ParseCtx::default()
    }

    #[test]
    fn reads_user_and_local_scopes_from_claude_json() {
        let text = r#"{
          "mcpServers": {
            "kicad": { "command": "node", "args": ["/x/index.js"], "env": { "LOG_LEVEL": "info" } }
          },
          "projects": {
            "/repo/one": { "mcpServers": { "pulumi": { "command": "pulumi", "args": ["mcp"] } } },
            "/repo/two": { "mcpServers": {} }
          }
        }"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        let scopes: Vec<&Scope> = read.servers.iter().map(|(s, _)| s).collect();
        assert!(scopes.contains(&&Scope::User));
        assert!(scopes.contains(&&Scope::Local("/repo/one".into())));
        // An empty per-repo map must not create a phantom scope.
        assert!(!scopes.contains(&&Scope::Local("/repo/two".into())));
        assert_eq!(read.servers.len(), 2);
    }

    #[test]
    fn self_referential_env_becomes_passthrough() {
        let text =
            r#"{"mcpServers":{"x":{"command":"n","env":{"TOKEN":"${TOKEN}","LEVEL":"info"}}}}"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        let Transport::Stdio(s) = &read.servers[0].1.transport else {
            panic!("expected stdio");
        };
        assert_eq!(s.env_from, vec!["TOKEN"]);
        assert_eq!(s.env.get("LEVEL").map(String::as_str), Some("info"));
    }

    #[test]
    fn bearer_reference_is_lifted_out_of_headers() {
        let text = r#"{"mcpServers":{"k":{"type":"http","url":"https://a.test/mcp","headers":{"Authorization":"Bearer ${TOK}"}}}}"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        let Transport::Http(h) = &read.servers[0].1.transport else {
            panic!("expected http");
        };
        assert_eq!(h.bearer_token_env.as_deref(), Some("TOK"));
        assert!(h.headers.is_empty());
    }

    #[test]
    fn literal_bearer_is_kept_and_reported_not_laundered() {
        let text = r#"{"mcpServers":{"k":{"type":"http","url":"https://a.test/mcp",
          "headers":{"Authorization":"Bearer f0a6ab6e38d15c1541fe6ed1e67ba64c78ccbe02505d358d58c30020aa5340ae"}}}}"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        let Transport::Http(h) = &read.servers[0].1.transport else {
            panic!("expected http");
        };
        assert!(h.bearer_token_env.is_none());
        assert!(h.headers.contains_key("Authorization"));
        assert_eq!(read.warnings.len(), 1, "{:?}", read.warnings);
        assert!(read.warnings[0].contains("literal credential"));
    }

    #[test]
    fn infers_transport_when_type_is_absent() {
        let text = r#"{"mcpServers":{
            "a":{"url":"https://a.test/mcp"},
            "b":{"command":"node"}}}"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        let mut kinds: Vec<&str> = read
            .servers
            .iter()
            .map(|(_, s)| match s.transport {
                Transport::Http(_) => "http",
                Transport::Stdio(_) => "stdio",
            })
            .collect();
        kinds.sort();
        assert_eq!(kinds, vec!["http", "stdio"]);
    }

    #[test]
    fn reads_codex_config_toml() {
        let text = r#"
model = "gpt-5"
[projects."/some/repo"]
trust_level = "trusted"

[mcp_servers.atlassian_rovo]
url = "https://mcp.atlassian.com/v1/mcp"

[mcp_servers.mcp-unity]
command = "node"
args = ["/x/index.js"]

[mcp_servers.mcp-unity.tools.recompile_scripts]
approval_mode = "approve"
"#;
        let read = codex_toml_v1(text, &ctx()).unwrap();
        assert_eq!(read.servers.len(), 2);
        assert!(read.servers.iter().all(|(s, _)| *s == Scope::User));
    }

    #[test]
    fn codex_env_http_headers_render_as_references() {
        let text = r#"
[mcp_servers.k]
url = "https://a.test/mcp"
[mcp_servers.k.env_http_headers]
"X-Key" = "MY_KEY"
"#;
        let read = codex_toml_v1(text, &ctx()).unwrap();
        let Transport::Http(h) = &read.servers[0].1.transport else {
            panic!("expected http");
        };
        assert_eq!(
            h.headers.get("X-Key").map(String::as_str),
            Some("${MY_KEY}")
        );
    }

    #[test]
    fn a_server_read_from_either_host_compares_equal() {
        let claude = claude_json_v1(
            r#"{"mcpServers":{"k":{"type":"http","url":"https://a.test/mcp",
                 "headers":{"Authorization":"Bearer ${TOK}"}}}}"#,
            &ctx(),
        )
        .unwrap();
        let codex = codex_toml_v1(
            "[mcp_servers.k]\nurl = \"https://a.test/mcp\"\nbearer_token_env_var = \"TOK\"\n",
            &ctx(),
        )
        .unwrap();
        assert_eq!(claude.servers[0].1, codex.servers[0].1);
    }

    #[test]
    fn serializer_round_trips_through_the_parser() {
        let original = claude_json_v1(
            r#"{"mcpServers":{"k":{"command":"node","args":["a"],
                 "env":{"LEVEL":"info","TOKEN":"${TOKEN}"}}}}"#,
            &ctx(),
        )
        .unwrap()
        .servers
        .remove(0)
        .1;
        let json = claude_json_v1_serialize(&original).unwrap();
        let wrapped = format!(r#"{{"mcpServers":{{"k":{json}}}}}"#);
        let again = claude_json_v1(&wrapped, &ctx())
            .unwrap()
            .servers
            .remove(0)
            .1;
        assert_eq!(original, again);
    }

    #[test]
    fn reads_codex_auth_status() {
        // Trimmed from real `codex mcp list --json` output.
        let text = r#"[
          {"name":"sentry","auth_status":"not_logged_in","transport":{"type":"streamable_http"}},
          {"name":"vanta","auth_status":"o_auth","transport":{"type":"streamable_http"}},
          {"name":"tradingview","auth_status":"unsupported","transport":{"type":"stdio"}},
          {"name":"noStatus","transport":{"type":"stdio"}}
        ]"#;
        let read = codex_auth_v1(text, &ctx()).unwrap();
        assert!(read.statuses["sentry"].needs_login());
        assert!(!read.statuses["vanta"].needs_login());
        assert!(!read.statuses["tradingview"].needs_login());
        // A missing field must not be read as logged out.
        assert!(!read.statuses["noStatus"].needs_login());
    }

    #[test]
    fn malformed_entries_are_skipped_with_a_warning_not_dropped_silently() {
        let text = r#"{"mcpServers":{"broken":{"args":["x"]},"ok":{"command":"node"}}}"#;
        let read = claude_json_v1(text, &ctx()).unwrap();
        assert_eq!(read.servers.len(), 1);
        assert_eq!(read.warnings.len(), 1);
        assert!(read.warnings[0].contains("broken"));
    }
}
