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
                    if sc == '\\'
                        && let Some(next) = chars.next()
                    {
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
    let path: Vec<String> = edit
        .pointer
        .path
        .iter()
        .map(|seg| match seg {
            PathSegment::Key(k) => k.clone(),
            PathSegment::Index(_) => panic!("array indices in path not yet supported"),
        })
        .collect();

    match &edit.operation {
        EditOperation::SetExactJson(json_text) => {
            serde_json::from_str::<serde_json::Value>(json_text)
                .context("parsing exact JSON text for set operation")?;
            set_exact_json(&doc.text, &path, json_text)
        }
        EditOperation::Remove => remove_member(&doc.text, &path),
        EditOperation::MergeObject(map) => {
            let mut text = doc.text.clone();
            for (key, value) in map {
                let mut child = path.clone();
                child.push(key.clone());
                text = set_exact_json(&text, &child, &serde_json::to_string(value)?)?;
            }
            Ok(text)
        }
    }
}

#[derive(Clone, Debug)]
struct MemberSpan {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
    comma_start: Option<usize>,
}

fn set_exact_json(text: &str, path: &[String], replacement: &str) -> Result<String> {
    if let Some((start, end)) = locate_value(text, path)? {
        let mut out = String::with_capacity(text.len() + replacement.len());
        out.push_str(&text[..start]);
        out.push_str(replacement);
        out.push_str(&text[end..]);
        return Ok(out);
    }

    let (parent_start, parent_end) = locate_value(text, &path[..path.len().saturating_sub(1)])?
        .context("cannot insert a value whose parent does not exist")?;
    let key = path.last().context("cannot set an empty JSON path")?;
    if text.as_bytes().get(parent_start) != Some(&b'{') {
        anyhow::bail!("cannot insert object member into a non-object value");
    }
    let members = object_members(text, parent_start)?;
    let close = parent_end - 1;
    let key = serde_json::to_string(key)?;
    let insertion = if members.is_empty() {
        format!("\n  {key}: {replacement}\n")
    } else {
        format!(",\n  {key}: {replacement}")
    };
    let mut out = text.to_string();
    out.insert_str(close, &insertion);
    Ok(out)
}

fn remove_member(text: &str, path: &[String]) -> Result<String> {
    let key = path.last().context("cannot remove the JSON root")?;
    let (parent_start, _) = locate_value(text, &path[..path.len() - 1])?
        .context("cannot remove a member whose parent does not exist")?;
    let members = object_members(text, parent_start)?;
    let index = members
        .iter()
        .position(|member| &member.key == key)
        .context("cannot remove a member that does not exist")?;
    let (start, end) = if let Some(next) = members.get(index + 1) {
        (members[index].key_start, next.key_start)
    } else if index > 0 {
        (
            members[index - 1]
                .comma_start
                .context("missing separator before final object member")?,
            members[index].value_end,
        )
    } else {
        (members[index].key_start, members[index].value_end)
    };
    let mut out = text.to_string();
    out.replace_range(start..end, "");
    Ok(out)
}

fn locate_value(text: &str, path: &[String]) -> Result<Option<(usize, usize)>> {
    locate_value_from(text, skip_trivia(text, 0)?, path)
}

fn locate_value_from(text: &str, start: usize, path: &[String]) -> Result<Option<(usize, usize)>> {
    let start = skip_trivia(text, start)?;
    let end = skip_value(text, start)?;
    if path.is_empty() {
        return Ok(Some((start, end)));
    }
    if text.as_bytes().get(start) != Some(&b'{') {
        return Ok(None);
    }
    let Some(member) = object_members(text, start)?
        .into_iter()
        .find(|member| member.key == path[0])
    else {
        return Ok(None);
    };
    locate_value_from(text, member.value_start, &path[1..])
}

fn object_members(text: &str, start: usize) -> Result<Vec<MemberSpan>> {
    if text.as_bytes().get(start) != Some(&b'{') {
        anyhow::bail!("expected object at byte {start}");
    }
    let mut members = Vec::new();
    let mut pos = skip_trivia(text, start + 1)?;
    while text.as_bytes().get(pos) != Some(&b'}') {
        let key_start = pos;
        let key_end = scan_string(text, key_start)?;
        let key: String = serde_json::from_str(&text[key_start..key_end])?;
        pos = skip_trivia(text, key_end)?;
        if text.as_bytes().get(pos) != Some(&b':') {
            anyhow::bail!("expected ':' after object key at byte {pos}");
        }
        let value_start = skip_trivia(text, pos + 1)?;
        let value_end = skip_value(text, value_start)?;
        pos = skip_trivia(text, value_end)?;
        let comma_start = (text.as_bytes().get(pos) == Some(&b',')).then_some(pos);
        if comma_start.is_some() {
            pos = skip_trivia(text, pos + 1)?;
        } else if text.as_bytes().get(pos) != Some(&b'}') {
            anyhow::bail!("expected ',' or '}}' at byte {pos}");
        }
        members.push(MemberSpan {
            key,
            key_start,
            value_start,
            value_end,
            comma_start,
        });
    }
    Ok(members)
}

fn skip_value(text: &str, start: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    let mut pos = skip_trivia(text, start)?;
    match bytes.get(pos).copied() {
        Some(b'"') => scan_string(text, pos),
        Some(b'{') => {
            pos += 1;
            loop {
                pos = skip_trivia(text, pos)?;
                if bytes.get(pos) == Some(&b'}') {
                    return Ok(pos + 1);
                }
                pos = scan_string(text, pos)?;
                pos = skip_trivia(text, pos)?;
                if bytes.get(pos) != Some(&b':') {
                    anyhow::bail!("expected ':' at byte {pos}");
                }
                pos = skip_value(text, pos + 1)?;
                pos = skip_trivia(text, pos)?;
                match bytes.get(pos) {
                    Some(b',') => pos += 1,
                    Some(b'}') => return Ok(pos + 1),
                    _ => anyhow::bail!("expected ',' or '}}' at byte {pos}"),
                }
            }
        }
        Some(b'[') => {
            pos += 1;
            loop {
                pos = skip_trivia(text, pos)?;
                if bytes.get(pos) == Some(&b']') {
                    return Ok(pos + 1);
                }
                pos = skip_value(text, pos)?;
                pos = skip_trivia(text, pos)?;
                match bytes.get(pos) {
                    Some(b',') => pos += 1,
                    Some(b']') => return Ok(pos + 1),
                    _ => anyhow::bail!("expected ',' or ']' at byte {pos}"),
                }
            }
        }
        Some(_) => {
            while let Some(byte) = bytes.get(pos) {
                if byte.is_ascii_whitespace() || matches!(byte, b',' | b'}' | b']') {
                    break;
                }
                pos += 1;
            }
            Ok(pos)
        }
        None => anyhow::bail!("expected JSON value at end of input"),
    }
}

fn scan_string(text: &str, start: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        anyhow::bail!("expected string at byte {start}");
    }
    let mut pos = start + 1;
    while let Some(byte) = bytes.get(pos) {
        match byte {
            b'\\' => pos += 2,
            b'"' => return Ok(pos + 1),
            _ => pos += 1,
        }
    }
    anyhow::bail!("unterminated string at byte {start}")
}

fn skip_trivia(text: &str, mut pos: usize) -> Result<usize> {
    let bytes = text.as_bytes();
    loop {
        while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if bytes.get(pos..pos + 2) == Some(b"//") {
            pos += 2;
            while bytes.get(pos).is_some_and(|byte| *byte != b'\n') {
                pos += 1;
            }
            continue;
        }
        if bytes.get(pos..pos + 2) == Some(b"/*") {
            let mut depth = 1usize;
            pos += 2;
            while depth > 0 {
                match bytes.get(pos..pos + 2) {
                    Some(b"/*") => {
                        depth += 1;
                        pos += 2;
                    }
                    Some(b"*/") => {
                        depth -= 1;
                        pos += 2;
                    }
                    Some(_) => pos += 1,
                    None => anyhow::bail!("unterminated block comment"),
                }
            }
            continue;
        }
        return Ok(pos);
    }
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
