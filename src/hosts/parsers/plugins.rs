//! Plugin and marketplace parsers.

use anyhow::{Context, Result};
use serde_json::Value;

use super::{ParseCtx, PluginRead};
use crate::core::model::{InstalledPlugin, MarketplaceSource};

/// Split `name@marketplace`. A key without `@` has no marketplace, which is
/// legal for locally-scaffolded plugins.
fn split_id(id: &str) -> (String, Option<String>) {
    match id.rsplit_once('@') {
        Some((name, market)) if !name.is_empty() && !market.is_empty() => {
            (name.to_string(), Some(market.to_string()))
        }
        _ => (id.to_string(), None),
    }
}

/// `~/.claude/plugins/installed_plugins.json` (`version: 2`).
///
/// Only user-scoped installs are collected. A project-scoped install belongs to
/// one repo and would otherwise look like global drift.
pub fn claude_plugins_v1(text: &str, ctx: &ParseCtx) -> Result<PluginRead> {
    let root: Value =
        serde_json::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = PluginRead::default();

    let Some(plugins) = root.get("plugins").and_then(Value::as_object) else {
        return Ok(out);
    };

    for (id, installs) in plugins {
        let (name, marketplace) = split_id(id);
        let user_scoped = installs
            .as_array()
            .map(|a| {
                a.iter()
                    .any(|i| i.get("scope").and_then(Value::as_str).unwrap_or("user") == "user")
            })
            .unwrap_or(false);
        if !user_scoped {
            continue;
        }
        out.plugins.insert(
            name.clone(),
            InstalledPlugin {
                name,
                marketplace: marketplace.unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

/// `~/.claude/plugins/known_marketplaces.json`.
pub fn claude_marketplaces_v1(text: &str, ctx: &ParseCtx) -> Result<PluginRead> {
    let root: Value =
        serde_json::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = PluginRead::default();

    let Some(map) = root.as_object() else {
        return Ok(out);
    };

    for (name, entry) in map {
        let Some(source) = entry.get("source") else {
            continue;
        };
        let kind = source.get("source").and_then(Value::as_str).unwrap_or("");
        let parsed = match kind {
            "github" => source
                .get("repo")
                .and_then(Value::as_str)
                .map(|r| MarketplaceSource::GitHub(r.to_string())),
            "directory" => source
                .get("path")
                .and_then(Value::as_str)
                .map(|p| MarketplaceSource::Directory(p.to_string())),
            _ => source
                .get("url")
                .and_then(Value::as_str)
                .map(|u| MarketplaceSource::Url(u.to_string())),
        };
        match parsed {
            Some(source) => {
                out.marketplaces.insert(name.clone(), source);
            }
            None => out
                .warnings
                .push(format!("marketplace {name:?}: unrecognized source shape")),
        }
    }
    Ok(out)
}

/// `~/.codex/config.toml` `[plugins."name@marketplace"]`.
///
/// Codex records no marketplace *sources* in config.toml — its curated
/// marketplaces are implicit — so this returns plugins only. The descriptor's
/// `implicit_marketplaces` covers the rest, which keeps us from reporting a
/// built-in marketplace as missing.
pub fn codex_plugins_toml_v1(text: &str, ctx: &ParseCtx) -> Result<PluginRead> {
    let root: toml::Value =
        toml::from_str(text).with_context(|| format!("parsing {}", ctx.origin.display()))?;
    let mut out = PluginRead::default();

    let Some(plugins) = root.get("plugins").and_then(toml::Value::as_table) else {
        return Ok(out);
    };

    for (id, entry) in plugins {
        let enabled = entry
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            continue;
        }
        let (name, marketplace) = split_id(id);
        out.plugins.insert(
            name.clone(),
            InstalledPlugin {
                name,
                marketplace: marketplace.unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ctx() -> ParseCtx {
        ParseCtx::default()
    }

    #[test]
    fn reads_user_scoped_claude_plugins_and_skips_project_ones() {
        let text = r#"{
          "version": 2,
          "plugins": {
            "superpowers@claude-plugins-official": [{ "scope": "user", "version": "6.2.0" }],
            "code-review@claude-plugins-official": [
              { "scope": "project", "projectPath": "/repos/x", "version": "unknown" }
            ]
          }
        }"#;
        let read = claude_plugins_v1(text, &ctx()).unwrap();
        assert_eq!(read.plugins.len(), 1);
        assert_eq!(
            read.plugins["superpowers"].marketplace,
            "claude-plugins-official"
        );
    }

    #[test]
    fn reads_github_and_directory_marketplaces() {
        let text = r#"{
          "claude-plugins-official": { "source": { "source": "github", "repo": "anthropics/claude-plugins-official" } },
          "i-have-adhd": { "source": { "source": "directory", "path": "/Users/x/Downloads/i-have-adhd" } }
        }"#;
        let read = claude_marketplaces_v1(text, &ctx()).unwrap();
        assert_eq!(
            read.marketplaces["claude-plugins-official"],
            MarketplaceSource::GitHub("anthropics/claude-plugins-official".into())
        );
        assert_eq!(
            read.marketplaces["i-have-adhd"],
            MarketplaceSource::Directory("/Users/x/Downloads/i-have-adhd".into())
        );
    }

    #[test]
    fn reads_codex_plugin_table_and_honours_enabled_false() {
        let text = r#"
[plugins."atlassian-rovo@openai-curated"]
enabled = true

[plugins."hubspot@openai-curated"]
enabled = false
"#;
        let read = codex_plugins_toml_v1(text, &ctx()).unwrap();
        assert_eq!(read.plugins.len(), 1);
        assert!(read.plugins.contains_key("atlassian-rovo"));
    }

    #[test]
    fn splits_ids_with_hyphens_in_both_halves() {
        assert_eq!(
            split_id("everything-evenhub@everything-evenhub"),
            (
                "everything-evenhub".into(),
                Some("everything-evenhub".into())
            )
        );
        assert_eq!(split_id("local-only"), ("local-only".into(), None));
    }

    #[test]
    fn missing_sections_are_empty_not_an_error() {
        assert!(claude_plugins_v1("{}", &ctx()).unwrap().plugins.is_empty());
        assert!(
            codex_plugins_toml_v1("model = \"x\"\n", &ctx())
                .unwrap()
                .plugins
                .is_empty()
        );
    }
}
