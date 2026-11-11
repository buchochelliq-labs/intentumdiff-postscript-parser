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
fn line_columns(raw_line: &str) -> (u32, u32) {
    let start = raw_line.len() - raw_line.trim_start().len();
    let end = raw_line.trim_end().len();
    (start as u32, end as u32)
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
// Tokenizer
// ---------------------------------------------------------------------------
//
// PostScript is token based: whitespace between tokens and `%` comments carry
// no meaning, while the token sequence (numbers, names, operators, strings,
// procedure braces) is the program. Semantic labels are therefore built from
// the token sequence joined by single spaces (#7), and every token — including
// those sharing a row with a procedure's `{` or `}` — lands in some node (#6).
// String, hex-string and ASCII85 tokens are kept verbatim, so whitespace inside
// them stays significant. Data read via `currentfile` is preserved verbatim.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TokKind {
    /// Numbers, executable names and operators.
    Regular,
    /// `/name` or `//name`.
    Literal,
    /// `(...)`, `<...>` or `<~...~>`.
    StringLit,
    /// `{`
    Open,
    /// `}`
    Close,
    /// `[`, `]`, `<<`, `>>` and stray delimiters.
    Delim,
}

#[derive(Clone, Debug)]
struct Token<'a> {
    kind: TokKind,
    text: &'a str,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

struct Lexed<'a> {
    tokens: Vec<Token<'a>>,
    /// Byte offset where verbatim `currentfile` data starts, if any.
    data_start: Option<usize>,
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Maps byte offsets to 0-based (line, UTF-8 byte column).
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }
    fn pos(&self, offset: usize) -> (u32, u32) {
        let line = match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        (line as u32, (offset - self.starts[line]) as u32)
    }
}

fn lex<'a>(source: &'a str, index: &LineIndex) -> Lexed<'a> {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut data_pending = false;
    let mut data_start = None;

    let push = |kind: TokKind, start: usize, end: usize, tokens: &mut Vec<Token<'a>>| {
        let (start_line, start_col) = index.pos(start);
        let (end_line, end_col) = index.pos(end);
        tokens.push(Token {
            kind,
            text: &source[start..end],
            start_line,
            start_col,
            end_line,
            end_col,
        });
    };

    while i < n {
        let b = bytes[i];
        if is_ws(b) {
            if b == b'\n' && data_pending {
                data_start = Some(i + 1);
                break;
            }
            i += 1;
            continue;
        }
        let start = i;
        match b {
            b'%' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'(' => {
                let mut depth = 0i32;
                while i < n {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                let end = i.min(n);
                i = end;
                push(TokKind::StringLit, start, end, &mut tokens);
            }
            b'<' => {
                if i + 1 < n && bytes[i + 1] == b'<' {
                    i += 2;
                    push(TokKind::Delim, start, i, &mut tokens);
                } else if i + 1 < n && bytes[i + 1] == b'~' {
                    i += 2;
                    while i < n && !(bytes[i] == b'~' && i + 1 < n && bytes[i + 1] == b'>') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                    push(TokKind::StringLit, start, i, &mut tokens);
                } else {
                    i += 1;
                    while i < n && bytes[i] != b'>' {
                        i += 1;
                    }
                    i = (i + 1).min(n);
                    push(TokKind::StringLit, start, i, &mut tokens);
                }
            }
            b'>' => {
                i += if i + 1 < n && bytes[i + 1] == b'>' {
                    2
                } else {
                    1
                };
                push(TokKind::Delim, start, i, &mut tokens);
            }
            b'{' | b'}' | b'[' | b']' | b')' => {
                i += 1;
                let kind = match b {
                    b'{' => TokKind::Open,
                    b'}' => TokKind::Close,
                    _ => TokKind::Delim,
                };
                push(kind, start, i, &mut tokens);
            }
            b'/' => {
                i += 1;
                if i < n && bytes[i] == b'/' {
                    i += 1;
                }
                while i < n && !is_ws(bytes[i]) && !is_delim(bytes[i]) {
                    i += 1;
                }
                push(TokKind::Literal, start, i, &mut tokens);
            }
            _ => {
                while i < n && !is_ws(bytes[i]) && !is_delim(bytes[i]) {
                    i += 1;
                }
                push(TokKind::Regular, start, i, &mut tokens);
                if &source[start..i] == "currentfile" {
                    data_pending = true;
                }
            }
        }
    }
    Lexed { tokens, data_start }
}

/// Canonical token text: tokens joined by one space. Whitespace inside string
/// tokens is preserved because each token is copied verbatim.
fn token_label(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    for (i, t) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(t.text);
    }
    out
}

/// Splits a token run into source rows: a new row starts when a token begins
/// on a later line than the previous token ended.
fn rows<'t, 'a>(tokens: &'t [Token<'a>]) -> Vec<&'t [Token<'a>]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=tokens.len() {
        if i == tokens.len() || tokens[i].start_line > tokens[i - 1].end_line {
            if start < i {
                out.push(&tokens[start..i]);
            }
            start = i;
        }
    }
    out
}

fn span_node(
    id: &str,
    node_type: &str,
    label: &str,
    tokens: &[Token<'_>],
    children: Vec<SemanticNode>,
) -> SemanticNode {
    let first = &tokens[0];
    let last = &tokens[tokens.len() - 1];
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        first.start_line,
        first.start_col,
        last.end_line,
        last.end_col,
        String::new(),
    )
    .children(children)
    .build()
}

fn statement_node(id: &str, row: &[Token<'_>]) -> SemanticNode {
    span_node(id, "ps_statement", &token_label(row), row, Vec::new())
}

/// Classifies a top-level row: `/name value… def` is a variable definition with
/// a `ps_value` child; anything else is a `ps_statement`.
fn top_level_row(id: &str, row: &[Token<'_>]) -> SemanticNode {
    let n = row.len();
    if n >= 3
        && row[0].kind == TokKind::Literal
        && row[0].text.len() > 1
        && !row[0].text.starts_with("//")
        && row[n - 1].kind == TokKind::Regular
        && row[n - 1].text == "def"
        && row[1..n - 1]
            .iter()
            .all(|t| !matches!(t.kind, TokKind::Open | TokKind::Close))
    {
        let value = &row[1..n - 1];
        let name = &row[0].text[1..];
        let child = span_node(
            &format!("{id}.0"),
            "ps_value",
            &token_label(value),
            value,
            Vec::new(),
        );
        return span_node(id, "variable_definition", name, row, vec![child]);
    }
    statement_node(id, row)
}

fn next_id(counter: &mut usize) -> String {
    let id = format!("0.{}", *counter);
    *counter += 1;
    id
}

const PROC_MODIFIERS: [&str; 4] = ["bind", "readonly", "executeonly", "noaccess"];

pub(crate) fn parse_postscript(source: &str) -> String {
    let index = LineIndex::new(source);
    let lexed = lex(source, &index);
    let tokens = &lexed.tokens;
    let mut children: Vec<SemanticNode> = Vec::new();
    let mut counter: usize = 0;

    // Pending top-level tokens not yet assigned to a node.
    let mut run_start = 0usize;
    let mut i = 0usize;
    let flush = |from: usize, to: usize, children: &mut Vec<SemanticNode>, counter: &mut usize| {
        for row in rows(&tokens[from..to]) {
            let id = next_id(counter);
            children.push(top_level_row(&id, row));
        }
    };

    while i < tokens.len() {
        let is_proc_start = tokens[i].kind == TokKind::Literal
            && tokens[i].text.len() > 1
            && !tokens[i].text.starts_with("//")
            && tokens.get(i + 1).is_some_and(|t| t.kind == TokKind::Open);
        if !is_proc_start {
            i += 1;
            continue;
        }
        flush(run_start, i, &mut children, &mut counter);

        // Find the matching close brace (or EOF for incomplete input).
        let body_start = i + 2;
        let mut depth = 1i32;
        let mut j = body_start;
        while j < tokens.len() {
            match tokens[j].kind {
                TokKind::Open => depth += 1,
                TokKind::Close => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let body_end = j.min(tokens.len());
        let mut end = if j < tokens.len() {
            j + 1
        } else {
            tokens.len()
        };

        // Optional `bind`/access modifiers followed by `def` close the definition.
        let mut k = end;
        let mut bound = false;
        while k < tokens.len()
            && tokens[k].kind == TokKind::Regular
            && PROC_MODIFIERS.contains(&tokens[k].text)
        {
            bound |= tokens[k].text == "bind";
            k += 1;
        }
        if j < tokens.len()
            && k < tokens.len()
            && tokens[k].kind == TokKind::Regular
            && tokens[k].text == "def"
        {
            end = k + 1;
        } else {
            bound = false;
        }

        let id = next_id(&mut counter);
        let body: Vec<SemanticNode> = rows(&tokens[body_start..body_end])
            .into_iter()
            .enumerate()
            .map(|(n, row)| statement_node(&format!("{id}.{n}"), row))
            .collect();
        let node_type = if bound {
            "bound_procedure"
        } else {
            "procedure"
        };
        let name = &tokens[i].text[1..];
        children.push(span_node(&id, node_type, name, &tokens[i..end], body));

        i = end;
        run_start = end;
    }
    flush(run_start, tokens.len(), &mut children, &mut counter);

    if let Some(start) = lexed.data_start {
        let data = &source[start..];
        if !data.trim().is_empty() {
            let (start_line, start_col) = index.pos(start);
            let end = start + data.trim_end().len();
            let (end_line, end_col) = index.pos(end);
            let id = next_id(&mut counter);
            children.push(
                SemanticNodeBuilder::new(
                    id,
                    "ps_data",
                    data,
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                    String::new(),
                )
                .build(),
            );
        }
    }

    let total_lines = source.lines().count().saturating_sub(1) as u32;
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
        let source = format!("%!PS\n{line}\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let statement = root["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_type"] == "ps_statement")
            .unwrap();
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
        let source = format!("%!PS\n{line}\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let definition = root["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_type"] == "variable_definition")
            .unwrap();
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
        let source = format!("%!PS\n  /draw {{\n{statement}\n{close}\n");
        let root: serde_json::Value = serde_json::from_str(&parse_postscript(&source)).unwrap();
        let procedure = root["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_type"] == "bound_procedure")
            .unwrap();
        assert_eq!(procedure["position"]["start_line"].as_u64(), Some(1));
        assert_eq!(procedure["position"]["start_col"].as_u64(), Some(2));
        assert_eq!(procedure["position"]["end_line"].as_u64(), Some(3));
        assert_eq!(
            procedure["position"]["end_col"].as_u64(),
            Some(close.trim_end().len() as u64)
        );
        let body = procedure["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_type"] == "ps_statement" && node["label"] == statement.trim())
            .unwrap();
        assert_eq!(body["node_type"], "ps_statement");
        assert_eq!(body["position"]["start_line"].as_u64(), Some(2));
        assert_eq!(body["position"]["start_col"].as_u64(), Some(4));
        assert_eq!(
            body["position"]["end_col"].as_u64(),
            Some(statement.len() as u64)
        );
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

    fn tree(source: &str) -> serde_json::Value {
        serde_json::from_str(&parse_postscript(source)).unwrap()
    }

    fn top(source: &str) -> Vec<serde_json::Value> {
        tree(source)["children"].as_array().unwrap().clone()
    }

    // #6: body content on the procedure's opening row was dropped.
    #[test]
    fn inline_procedure_body_is_retained() {
        let old = "%!PS\n/f { 1 } def\nf ==\n";
        let new = "%!PS\n/f { 2 } def\nf ==\n";
        let old_top = top(old);
        let new_top = top(new);
        let proc_old = &old_top[0];
        assert_eq!(proc_old["node_type"], "procedure");
        assert_eq!(proc_old["label"], "f");
        let body = &proc_old["children"][0];
        assert_eq!(body["node_type"], "ps_statement");
        assert_eq!(body["label"], "1");
        assert_eq!(body["position"]["start_line"], 1);
        assert_eq!(body["position"]["start_col"], 5);
        assert_eq!(body["position"]["end_col"], 6);
        assert_eq!(new_top[0]["children"][0]["label"], "2");
        assert_ne!(proc_old["structural_hash"], new_top[0]["structural_hash"]);
        assert_eq!(old_top[1]["label"], "f ==");
        assert_eq!(proc_old["position"]["end_col"], 12);
    }

    // #6: body content on the closing row was dropped.
    #[test]
    fn closing_row_body_is_retained() {
        let old = "%!PS\n/f {\n  1 } def\nf ==\n";
        let new = "%!PS\n/f {\n  2 } def\nf ==\n";
        let o = top(old);
        let n = top(new);
        let body = &o[0]["children"][0];
        assert_eq!(body["label"], "1");
        assert_eq!(body["position"]["start_line"], 2);
        assert_eq!(body["position"]["start_col"], 2);
        assert_eq!(body["position"]["end_col"], 3);
        assert_eq!(n[0]["children"][0]["label"], "2");
        assert_ne!(o[0]["structural_hash"], n[0]["structural_hash"]);
        assert_eq!(o[0]["position"]["end_line"], 2);
        assert_eq!(o[0]["position"]["end_col"], 9);
    }

    #[test]
    fn braces_in_strings_and_comments_do_not_close_procedures() {
        let src = "%!PS\n/f { (}) show % } not a brace\n  (a{b) 2 } bind def\nf\n";
        let t = top(src);
        assert_eq!(t[0]["node_type"], "bound_procedure");
        let labels: Vec<_> = t[0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(labels, vec!["(}) show", "(a{b) 2"]);
        assert_eq!(t[1]["label"], "f");
    }

    #[test]
    fn trailing_content_after_def_is_retained() {
        let t = top("%!PS\n/f { 1 } def f ==\n");
        assert_eq!(t[0]["node_type"], "procedure");
        assert_eq!(t[1]["node_type"], "ps_statement");
        assert_eq!(t[1]["label"], "f ==");
        assert_eq!(t[1]["position"]["start_col"], 13);
    }

    #[test]
    fn incomplete_procedure_keeps_body() {
        let t = top("%!PS\n/f { 1");
        assert_eq!(t[0]["node_type"], "procedure");
        assert_eq!(t[0]["children"][0]["label"], "1");
    }

    // #7: spacing between tokens is not semantic.
    #[test]
    fn inter_token_spacing_is_preserving() {
        let old = tree("%!PS\n2 3 add ==\n");
        let new = tree("%!PS\n2  3   add  ==\n");
        assert_eq!(old["children"][0]["label"], "2 3 add ==");
        assert_eq!(new["children"][0]["label"], "2 3 add ==");
        assert_eq!(old["structural_hash"], new["structural_hash"]);
        assert_eq!(new["children"][0]["position"]["end_col"], 14);
        // Whitespace-only and comment-only edits are also preserving.
        let commented = tree("%!PS\n2 3 add == % sum\n");
        assert_eq!(old["structural_hash"], commented["structural_hash"]);
    }

    #[test]
    fn operator_and_string_edits_stay_meaningful() {
        let base = tree("%!PS\n2 3 add ==\n");
        assert_ne!(
            base["structural_hash"],
            tree("%!PS\n2 3 mul ==\n")["structural_hash"]
        );
        // Whitespace inside a string literal is content.
        let a = tree("%!PS\n(a b) show\n");
        let b = tree("%!PS\n(a  b) show\n");
        assert_ne!(a["structural_hash"], b["structural_hash"]);
        assert_eq!(b["children"][0]["label"], "(a  b) show");
    }

    #[test]
    fn currentfile_data_is_preserved_verbatim() {
        let a = tree("%!PS\ncurrentfile 2 string readhexstring pop\nab cd\n");
        let b = tree("%!PS\ncurrentfile 2 string readhexstring pop\nab  cd\n");
        let data = &a["children"][1];
        assert_eq!(data["node_type"], "ps_data");
        assert_eq!(data["position"]["start_line"], 2);
        assert_ne!(a["structural_hash"], b["structural_hash"]);
    }

    #[test]
    fn variable_definition_spacing_is_preserving() {
        let a = tree("%!PS\n/x 1 2 add def\n");
        let b = tree("%!PS\n/x  1 2  add   def\n");
        assert_eq!(a["children"][0]["node_type"], "variable_definition");
        assert_eq!(a["children"][0]["children"][0]["label"], "1 2 add");
        assert_eq!(a["structural_hash"], b["structural_hash"]);
    }
}
