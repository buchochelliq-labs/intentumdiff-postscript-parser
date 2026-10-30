//! PostScript parser plugin — full-parse mode.
//!
//! Handles `.ps` and `.eps` files.
//! PostScript is a stack-based language; this plugin extracts named procedure
//! definitions and variable bindings at brace depth 0.
//!
//! Semantic nodes produced:
//!   postscript_file    — root
//!   procedure          — /name { … } def (label = name)
//!   bound_procedure    — /name { … } bind def (label = name)
//!   variable_definition — /name value def  (no braces, label = name)
//!
//! Detection:
//!   .eps → "postscript" unconditionally.
//!   .ps  → only if the content begins with "%!" (PostScript magic comment).

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct PostScriptParser;

// ---------------------------------------------------------------------------
// Node helpers
// ---------------------------------------------------------------------------

// Columns are UTF-8 byte offsets into the original line, not character counts.
// Work on the untrimmed line so indentation and multi-byte characters retain
// their source coordinates.
fn line_columns(raw_line: &str) -> (u32, u32) {
    let start = raw_line.len() - raw_line.trim_start().len();
    let end = raw_line.trim_end().len();
    (start as u32, end as u32)
}

fn leaf(id: &str, node_type: &str, label: &str, line: u32, raw_line: &str) -> SemanticNode {
    let (start_col, end_col) = line_columns(raw_line);
    SemanticNodeBuilder::new(
        id, node_type, label, line, start_col, line, end_col, String::new(),
    )
    .build()
}

fn block_node(
    id: &str,
    node_type: &str,
    label: &str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id, node_type, label, start_line, start_col, end_line, end_col, String::new(),
    )
    .build()
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

pub(crate) fn detect_language_impl(filename: &str, content: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".eps") {
        return "postscript".to_string();
    }
    if lower.ends_with(".ps") && content.trim_start().starts_with("%!") {
        return "postscript".to_string();
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub(crate) fn parse_postscript(source: &str) -> String {
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut counter: usize = 0;
    let mut brace_depth: i32 = 0;
    let mut total_lines = 0u32;

    // Track an open procedure: (name, start_line, start_byte_col, statements).
    let mut pending: Option<(String, u32, u32, Vec<SemanticNode>)> = None;

    for (idx, raw_line) in source.lines().enumerate() {
        let lineno = idx as u32;
        total_lines = lineno;
        let trimmed = raw_line.trim();

        // PostScript line comments start with %
        if trimmed.starts_with('%') {
            continue;
        }

        let depth_before = brace_depth;
        let open_count = trimmed.chars().filter(|&c| c == '{').count() as i32;
        let close_count = trimmed.chars().filter(|&c| c == '}').count() as i32;
        brace_depth += open_count - close_count;

        // At brace depth 0, look for definitions
        if depth_before == 0 && open_count == 0 {
            // Possible: /name value def  (no braces → variable_definition leaf)
            // Heuristic: line matches "/name <non-brace tokens> def"
            let upper = trimmed.to_uppercase();
            if trimmed.starts_with('/') && upper.ends_with("DEF") && pending.is_none() {
                let name_end = trimmed[1..]
                    .find(char::is_whitespace)
                    .unwrap_or(trimmed.len() - 1)
                    + 1;
                let name = &trimmed[1..name_end];
                if !name.is_empty() {
                    let id = format!("0.{}", counter);
                    counter += 1;
                    let mut var = leaf(&id, "variable_definition", name, lineno, raw_line);
                    // #46: the VALUE tokens between the name and `def` are review content.
                    // Anchor the value within the original UTF-8 line, not at column zero.
                    let value_region = &trimmed[name_end..trimmed.len() - 3];
                    let value = value_region.trim();
                    if !value.is_empty() {
                        let value_start = (raw_line.len() - raw_line.trim_start().len())
                            + name_end
                            + (value_region.len() - value_region.trim_start().len());
                        let value_end = value_start + value.len();
                        var.children = vec![SemanticNodeBuilder::new(
                            format!("{id}.0"),
                            "ps_value",
                            value,
                            lineno,
                            value_start as u32,
                            lineno,
                            value_end as u32,
                            String::new(),
                        )
                        .build()];
                    }
                    children.push(var);
                }
                continue;
            }
        }

        // At depth 0: opening of a named procedure /name {
        if depth_before == 0 && open_count > 0 && pending.is_none() {
            // Look for /name pattern before the first {
            if let Some(brace_pos) = trimmed.find('{') {
                let before = trimmed[..brace_pos].trim();
                if before.starts_with('/') {
                    let name_end = before[1..]
                        .find(char::is_whitespace)
                        .unwrap_or(before.len() - 1)
                        + 1;
                    let name = before[1..name_end].to_string();
                    if !name.is_empty() {
                        pending = Some((name, lineno, line_columns(raw_line).0, Vec::new()));
                    }
                }
            }
        }

        // When brace depth returns to 0, close the pending procedure
        if brace_depth == 0 && pending.is_some() {
            let (name, start, start_col, statements) = pending.take().unwrap();
            let upper = trimmed.to_uppercase();
            // "bind def" → bound_procedure; plain "def" → procedure
            let node_type = if upper.contains("BIND") && upper.ends_with("DEF") {
                "bound_procedure"
            } else {
                "procedure"
            };
            let id = format!("0.{}", counter);
            counter += 1;
            let mut block = block_node(
                &id,
                node_type,
                &name,
                start,
                start_col,
                lineno,
                line_columns(raw_line).1,
            );
            block.children = statements
                .into_iter()
                .enumerate()
                .map(|(i, mut stmt)| {
                    stmt.id = format!("{id}.{i}");
                    stmt
                })
                .collect();
            children.push(block);
            continue;
        }

        // #46: interior procedure lines and bare top-level operator lines are review
        // content — `/Helvetica findfont 14 scalefont setfont` was captured NOWHERE, so a
        // bump in it hashed style-only.
        if trimmed.is_empty() {
            continue;
        }
        if let Some((_, _, _, statements)) = pending.as_mut() {
            statements.push(leaf("tmp", "ps_statement", trimmed, lineno, raw_line));
        } else if depth_before == 0 && open_count == 0 {
            let id = format!("0.{}", counter);
            counter += 1;
            children.push(leaf(&id, "ps_statement", trimmed, lineno, raw_line));
        }
    }

    let root = SemanticNodeBuilder::new(
        "0",
        "postscript_file",
        "postscript_file",
        0,
        0,
        total_lines,
        source.lines().last().map_or(0, |line| line_columns(line).1),
        String::new(),
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

// ---------------------------------------------------------------------------
// WIT guest impl
// ---------------------------------------------------------------------------

impl Guest for PostScriptParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "postscript".to_string()
    }
    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "%!PS\n/Helvetica findfont 12 scalefont setfont\n72 720 moveto\n(Hello, World!) show\nshowpage\n".to_string(),
            new: "%!PS\n/Helvetica findfont 14 scalefont setfont\n/greet {\n    moveto\n    show\n} def\n72 720 (Hello, World!) greet\n72 700 (Goodbye, World!) greet\nshowpage\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        parse_postscript(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["postscript".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(PostScriptParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    intentumdiff_plugin_sdk::plugin_compliance_tests! {
        process: parse_postscript,
        detect_fn: detect_language_impl,
        detect_cases: [
            ("figure.eps", "",      "postscript"),
            ("doc.ps",     "%!PS", "postscript"),
            ("other.ps",   "plain text", ""),
            ("code.py",    "",      ""),
        ],
        grammar_id: "postscript",
        language_ids: ["postscript"],
    }

    const SAMPLE: &str = "%!PS-Adobe-3.0\n\
        /pi 3.14159 def\n\
        /square {\n\
            dup mul\n\
        } def\n\
        /draw-circle {\n\
            newpath\n\
            0 0 50 0 360 arc\n\
            closepath fill\n\
        } bind def\n";

    #[test]
    fn test_valid_json_no_error() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, "SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, "SAMPLE");
    }

    #[test]
    fn test_root_is_postscript_file() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&out, "postscript_file", "SAMPLE");
    }

    #[test]
    fn test_procedure_found() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "procedure", "procedure");
    }

    #[test]
    fn test_bound_procedure_found() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "bound_procedure",
            "bound_procedure",
        );
    }

    #[test]
    fn test_variable_definition_found() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "variable_definition",
            "variable_definition",
        );
    }

    #[test]
    fn test_eps_detected_without_magic() {
        // .eps files are always PostScript regardless of content
        assert_eq!(detect_language_impl("logo.eps", ""), "postscript");
    }

    #[test]
    fn test_ps_requires_magic_comment() {
        assert_eq!(
            detect_language_impl("doc.ps", "%!PS-Adobe-3.0\n% content"),
            "postscript"
        );
        assert_eq!(detect_language_impl("doc.ps", "plain text"), "");
    }

    #[test]
    fn statement_spans_cover_edited_numeric_bytes() {
        let line = "  /Helvetica findfont 14 scalefont setfont  ";
        let source = format!("%!PS\\n{line}\\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let statement = root["children"].as_array().unwrap().iter()
            .find(|node| node["node_type"] == "ps_statement").unwrap();
        let span = &statement["position"];
        let numeric = line.find("14").unwrap() as u64;
        assert_eq!(span["start_line"].as_u64(), Some(1));
        assert_eq!(span["end_line"].as_u64(), Some(1));
        assert_eq!(span["start_col"].as_u64(), Some(2));
        assert_eq!(span["end_col"].as_u64(), Some(line.trim_end().len() as u64));
        assert!(span["start_col"].as_u64().unwrap() <= numeric);
        assert!(span["end_col"].as_u64().unwrap() >= numeric + 2);
    }

    #[test]
    fn definition_value_span_is_exact_utf8_byte_interval() {
        let line = "  /größe 14 def ";
        let source = format!("%!PS\\n{line}\\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let definition = root["children"].as_array().unwrap().iter()
            .find(|node| node["node_type"] == "variable_definition").unwrap();
        let value = &definition["children"][0];
        assert_eq!(value["node_type"], "ps_value");
        assert_eq!(value["label"], "14");
        assert_eq!(value["position"]["start_line"].as_u64(), Some(1));
        let numeric = line.find("14").unwrap() as u64;
        assert_eq!(value["position"]["start_col"].as_u64(), Some(numeric));
        assert_eq!(value["position"]["end_col"].as_u64(), Some(numeric + 2));
    }

    #[test]
    fn procedure_and_indented_statement_spans_are_source_anchored() {
        let statement = "    (café) 14 show";
        let close = "  } bind def ";
        let source = format!("%!PS\\n  /draw {{\\n{statement}\\n{close}\\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let procedure = root["children"].as_array().unwrap().iter()
            .find(|node| node["node_type"] == "bound_procedure").unwrap();
        assert_eq!(procedure["position"]["start_line"].as_u64(), Some(1));
        assert_eq!(procedure["position"]["start_col"].as_u64(), Some(2));
        assert_eq!(procedure["position"]["end_line"].as_u64(), Some(3));
        assert_eq!(procedure["position"]["end_col"].as_u64(), Some(close.trim_end().len() as u64));
        let body = &procedure["children"][0];
        assert_eq!(body["node_type"], "ps_statement");
        assert_eq!(body["position"]["start_line"].as_u64(), Some(2));
        assert_eq!(body["position"]["start_col"].as_u64(), Some(4));
        assert_eq!(body["position"]["end_col"].as_u64(), Some(statement.len() as u64));
        let numeric = statement.find("14").unwrap() as u64;
        assert!(body["position"]["start_col"].as_u64().unwrap() <= numeric);
        assert!(body["position"]["end_col"].as_u64().unwrap() >= numeric + 2);
    }

    #[test]
    fn test_labels_nonempty() {
        let out = parse_postscript(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "procedure", "labels");
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "bound_procedure", "labels");
    }
}
