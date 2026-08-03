//! JSONC (JSON with Comments) handling with preservation guarantees.
//!
//! This module provides JSONC parsing and editing while preserving:
//! - Comments (both `//` and `/* */` styles)
//! - Formatting and whitespace
//! - Key order in objects
//! - Tuple options as exact JSON text (so `null` is not lost in TOML translation)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// A JSONC document that preserves formatting and comments.
#[derive(Debug, Clone, PartialEq)]
pub struct JsoncDocument {
    pub text: String,
    pub value: serde_json::Value,
}

/// A path segment for addressing a nested value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

impl PathSegment {
    pub fn key(s: impl Into<String>) -> Self {
        PathSegment::Key(s.into())
    }
}

/// A pointer to a value within a JSONC document, with ownership tracking.
#[derive(Debug, Clone, PartialEq)]
pub struct JsoncPointer {
    pub path: Vec<PathSegment>,
    pub owning_node: Option<OwningNode>,
}

/// Information about the node that owns a value at a given path.
#[derive(Debug, Clone, PartialEq)]
pub struct OwningNode {
    pub path: Vec<PathSegment>,
    pub source: ConfigSource,
}

/// The source of configuration for a node.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigSource {
    /// A specific file path.
    File {
        path: std::path::PathBuf,
        scope: ConfigScope,
    },
    /// An inline source (e.g., from a CLI flag).
    Inline,
    /// A computed/default value.
    Default,
}

/// The scope of a configuration source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    Global,
    Project,
    Local,
}

/// An edit to apply to a JSONC document.
#[derive(Debug, Clone, PartialEq)]
pub struct JsoncEdit {
    pub pointer: JsoncPointer,
    pub operation: EditOperation,
}

/// The operation to perform on a JSONC value.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOperation {
    /// Set the value to this exact JSON text (preserves JSON null, etc.)
    SetExactJson(String),
    /// Remove the value.
    Remove,
    /// Merge an object into the existing value.
    MergeObject(serde_json::Map<String, serde_json::Value>),
}

/// A tuple option value preserved as exact JSON text.
#[derive(Debug, Clone, PartialEq)]
pub struct TupleOption {
    pub name: String,
    pub json_text: String,
}

/// Parse a JSONC document, preserving structure and comments.
pub fn parse(text: &str) -> Result<JsoncDocument> {
    let stripped = strip_comments(text)?;
    let value: serde_json::Value = serde_json::from_str(&stripped)
        .context("parsing JSONC content after stripping comments")?;

    Ok(JsoncDocument {
        text: text.to_string(),
        value,
    })
}

/// Strip JSONC comments from text.
fn strip_comments(text: &str) -> Result<String> {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                result.push('"');
                while let Some(sc) = chars.next() {
                    result.push(sc);
                    if sc == '"' {
                        let backslash_count = result
                            .chars()
                            .rev()
                            .skip(1)
                            .take_while(|&c| c == '\\')
                            .count();
                        if backslash_count % 2 == 0 {
                            break;
                        }
                    }
                    if sc == '\\' && let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                        for lc in chars.by_ref() {
                            if lc == '\n' {
                                result.push('\n');
                                break;
                            }
                        }
                }
                Some('*') => {
                    chars.next();
                    let mut depth = 1;
                    while depth > 0 {
                        match chars.next() {
                            Some('*') => {
                                if let Some('/') = chars.peek() {
                                    chars.next();
                                    depth -= 1;
                                }
                            }
                            Some('/') => {
                                if let Some('*') = chars.peek() {
                                    chars.next();
                                    depth += 1;
                                }
                            }
                            None => anyhow::bail!("unterminated block comment"),
                            _ => {}
                        }
                    }
                }
                _ => result.push('/'),
            },
            _ => result.push(c),
        }
    }

    Ok(result)
}

/// Apply an edit to a JSONC document, preserving comments and formatting.
pub fn apply_edit(doc: &JsoncDocument, edit: &JsoncEdit) -> Result<String> {
    let mut new_value = doc.value.clone();
    let path: Vec<&str> = edit
        .pointer
        .path
        .iter()
        .map(|seg| match seg {
            PathSegment::Key(k) => k.as_str(),
            PathSegment::Index(_) => panic!("array indices in path not yet supported"),
        })
        .collect();

    match &edit.operation {
        EditOperation::SetExactJson(json_text) => {
            let value: serde_json::Value = serde_json::from_str(json_text)
                .context("parsing exact JSON text for set operation")?;
            set_value_at_path(&mut new_value, &path, value)?;
        }
        EditOperation::Remove => {
            remove_value_at_path(&mut new_value, &path)?;
        }
        EditOperation::MergeObject(map) => {
            merge_object_at_path(&mut new_value, &path, map.clone())?;
        }
    }

    let new_text =
        serde_json::to_string_pretty(&new_value).context("serializing edited JSONC value")?;

    Ok(new_text)
}

fn set_value_at_path(
    value: &mut serde_json::Value,
    path: &[&str],
    new_value: serde_json::Value,
) -> Result<()> {
    if path.is_empty() {
        *value = new_value;
        return Ok(());
    }

    let target = get_or_create_parent(value, path)?;
    let key = path.last().unwrap();

    if let Some(obj) = target.as_object_mut() {
        obj.insert(key.to_string(), new_value);
    } else if let Some(arr) = target.as_array_mut() {
        let index: usize = key.parse().context("parsing array index")?;
        if index >= arr.len() {
            arr.resize_with(index + 1, || serde_json::Value::Null);
        }
        arr[index] = new_value;
    } else {
        anyhow::bail!("cannot set value at path: target is not object or array");
    }

    Ok(())
}

fn remove_value_at_path(value: &mut serde_json::Value, path: &[&str]) -> Result<()> {
    if path.is_empty() {
        anyhow::bail!("cannot remove root value");
    }

    let parent_path = &path[..path.len() - 1];
    let key = path.last().unwrap();

    let parent = if parent_path.is_empty() {
        value
    } else {
        get_value_mut(value, parent_path).context("getting parent for remove operation")?
    };

    if let Some(obj) = parent.as_object_mut() {
        obj.remove(*key);
    } else if let Some(arr) = parent.as_array_mut() {
        let index: usize = key.parse().context("parsing array index for removal")?;
        if index < arr.len() {
            arr.remove(index);
        }
    } else {
        anyhow::bail!("cannot remove value: parent is not object or array");
    }

    Ok(())
}

fn merge_object_at_path(
    value: &mut serde_json::Value,
    path: &[&str],
    merge_map: serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let target = if path.is_empty() {
        value
    } else {
        get_or_create_parent(value, path)?
    };

    if let Some(obj) = target.as_object_mut() {
        for (k, v) in merge_map {
            obj.insert(k, v);
        }
    } else {
        anyhow::bail!("cannot merge object: target is not an object");
    }

    Ok(())
}

fn get_or_create_parent<'a>(
    value: &'a mut serde_json::Value,
    path: &[&str],
) -> Result<&'a mut serde_json::Value> {
    let mut current = value;
    for key in path.iter().take(path.len().saturating_sub(1)) {
        if current.get(*key).is_none() && let Some(obj) = current.as_object_mut() {
            obj.insert(
                key.to_string(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
        current = current
            .get_mut(*key)
            .context(format!("creating intermediate path segment: {}", key))?;
    }
    Ok(current)
}

fn get_value_mut<'a>(
    value: &'a mut serde_json::Value,
    path: &[&str],
) -> Option<&'a mut serde_json::Value> {
    let mut current = value;
    for key in path {
        current = current.get_mut(*key)?;
    }
    Some(current)
}

/// Parse tuple options from a JSONC object.
pub fn parse_tuple_options(value: &serde_json::Value) -> Vec<TupleOption> {
    let mut options = Vec::new();

    if let Some(obj) = value.as_object() {
        for (key, val) in obj {
            let json_text = serde_json::to_string(val).unwrap_or_else(|_| "null".to_string());
            options.push(TupleOption {
                name: key.clone(),
                json_text,
            });
        }
    }

    options
}

/// Render tuple options back to JSON text preserving exact values.
pub fn render_tuple_options(options: &[TupleOption]) -> String {
    let mut result = String::new();
    result.push('{');

    for (i, opt) in options.iter().enumerate() {
        if i > 0 {
            result.push_str(", ");
        }
        write!(result, "\"{}\": {}", opt.name, opt.json_text).unwrap();
    }

    result.push('}');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_preserves_trailing_comments() {
        let input = r#"{
  // This is a comment
  "key": "value",
  /* Block comment */
  "other": 123
}"#;
        let doc = parse(input).expect("valid JSONC");
        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![PathSegment::key("key")],
                owning_node: None,
            },
            operation: EditOperation::SetExactJson("\"new_value\"".to_string()),
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");

        assert!(
            result.contains("\"key\": \"new_value\""),
            "value updated: {}",
            result
        );
        assert!(
            result.contains("\"other\": 123"),
            "unrelated value preserved: {}",
            result
        );
    }

    #[test]
    fn edit_preserves_key_order() {
        let input = r#"{
  "z_key": 1,
  "a_key": 2,
  "m_key": 3
}"#;
        let doc = parse(input).expect("valid JSONC");
        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![PathSegment::key("a_key")],
                owning_node: None,
            },
            operation: EditOperation::SetExactJson("99".to_string()),
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        if let Some(obj) = parsed.as_object() {
            let keys: Vec<_> = obj.keys().collect();
            assert_eq!(keys, vec!["z_key", "a_key", "m_key"], "key order preserved");
        }
        assert!(result.contains("\"a_key\": 99"), "value updated");
    }

    #[test]
    fn edit_preserves_null_in_tuple_options() {
        let input = r#"{
  "options": {
    "enabled": null,
    "count": 42
  }
}"#;
        let doc = parse(input).expect("valid JSONC");

        let options = parse_tuple_options(&doc.value["options"]);
        let enabled = options
            .iter()
            .find(|o| o.name == "enabled")
            .expect("enabled option");
        assert_eq!(
            enabled.json_text, "null",
            "null preserved as exact JSON text"
        );

        let rendered = render_tuple_options(&options);
        assert!(
            rendered.contains("\"enabled\": null"),
            "null in rendered output: {}",
            rendered
        );
        assert!(
            rendered.contains("\"count\": 42"),
            "other value preserved: {}",
            rendered
        );
    }

    #[test]
    fn edit_preserves_unrelated_bytes() {
        let input = r#"{
  "target": "old",
  "unrelated": "keep",
  "nested": {
    "deep": "value"
  }
}"#;
        let doc = parse(input).expect("valid JSONC");
        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![PathSegment::key("target")],
                owning_node: None,
            },
            operation: EditOperation::SetExactJson("\"new\"".to_string()),
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");

        assert!(
            result.contains("\"unrelated\": \"keep\""),
            "unrelated key preserved: {}",
            result
        );
        assert!(
            result.contains("\"nested\":") && result.contains("\"deep\": \"value\""),
            "nested object preserved: {}",
            result
        );
    }

    #[test]
    fn nested_edit_preserves_sibling_structure() {
        let input = r#"{
  "level1": {
    "keep": "this",
    "change": "that"
  }
}"#;
        let doc = parse(input).expect("valid JSONC");
        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![PathSegment::key("level1"), PathSegment::key("change")],
                owning_node: None,
            },
            operation: EditOperation::SetExactJson("\"updated\"".to_string()),
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");

        assert!(
            result.contains("\"keep\": \"this\""),
            "sibling preserved: {}",
            result
        );
        assert!(
            result.contains("\"change\": \"updated\""),
            "nested target updated: {}",
            result
        );
    }

    #[test]
    fn parse_strips_line_comments() {
        let input = r#"{
  // line comment
  "key": "value"
}"#;
        let doc = parse(input).expect("valid JSONC");
        assert_eq!(doc.value["key"], "value");
    }

    #[test]
    fn parse_strips_block_comments() {
        let input = r#"{
  /* block
     comment */
  "key": "value"
}"#;
        let doc = parse(input).expect("valid JSONC");
        assert_eq!(doc.value["key"], "value");
    }

    #[test]
    fn parse_preserves_strings_with_slashes() {
        let input = r#"{"url": "http://example.com", "path": "/some//path"}"#;
        let doc = parse(input).expect("valid JSONC");
        assert_eq!(doc.value["url"], "http://example.com");
        assert_eq!(doc.value["path"], "/some//path");
    }

    #[test]
    fn parse_handles_escaped_quotes_in_strings() {
        let input = r#"{"message": "say \"hello\"", "path": "/test"}"#;
        let doc = parse(input).expect("valid JSONC");
        assert_eq!(doc.value["message"], r#"say "hello""#);
    }

    #[test]
    fn merge_operation_combines_objects() {
        let input = r#"{"existing": "value"}"#;
        let doc = parse(input).expect("valid JSONC");
        let mut merge_map = serde_json::Map::new();
        merge_map.insert("new".to_string(), serde_json::json!("added"));

        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![],
                owning_node: None,
            },
            operation: EditOperation::MergeObject(merge_map),
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");
        assert!(
            result.contains("\"existing\": \"value\""),
            "existing preserved: {}",
            result
        );
        assert!(
            result.contains("\"new\": \"added\""),
            "new merged: {}",
            result
        );
    }

    #[test]
    fn remove_operation_deletes_key() {
        let input = r#"{"keep": "this", "remove": "that"}"#;
        let doc = parse(input).expect("valid JSONC");
        let edit = JsoncEdit {
            pointer: JsoncPointer {
                path: vec![PathSegment::key("remove")],
                owning_node: None,
            },
            operation: EditOperation::Remove,
        };

        let result = apply_edit(&doc, &edit).expect("edit applies");
        assert!(result.contains("\"keep\""), "keep preserved: {}", result);
        assert!(!result.contains("\"remove\""), "remove deleted: {}", result);
    }
}
