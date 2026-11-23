//! AST syntax highlighting and enclosing symbol extraction powered by `tree-sitter`.

use crate::traits::{
    DiffMergeError, DiffMergeResult, MAX_SYNTAX_HIGHLIGHT_BYTES, SyntaxHighlighter,
};
use branvio_core::domain::{
    DiffLineOrigin, EnclosingSymbol, FileDiffPatch, SymbolKind, SyntaxTokenKind, SyntaxTokenSpan,
};
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use smallvec::SmallVec;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

const MAX_SIGNATURE_LEN: usize = 96;

/// Base JS/TS highlights combined with `tree_sitter_typescript::HIGHLIGHTS_QUERY`.
const JS_TS_BASE_HIGHLIGHTS: &str = r#"
(comment) @comment
(string) @string
(template_string) @string
(number) @number
(true) @constant
(false) @constant
(null) @constant
(undefined) @constant
(property_identifier) @property
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function)
(call_expression function: (identifier) @function)
(call_expression function: (member_expression property: (property_identifier) @function))
(class_declaration name: (type_identifier) @type)
(interface_declaration name: (type_identifier) @type)
[
  "const"
  "let"
  "var"
  "function"
  "return"
  "if"
  "else"
  "for"
  "while"
  "do"
  "switch"
  "case"
  "default"
  "break"
  "continue"
  "try"
  "catch"
  "finally"
  "throw"
  "async"
  "await"
  "import"
  "from"
  "class"
  "extends"
  "new"
  "in"
  "of"
  "typeof"
  "instanceof"
  "void"
  "delete"
  "yield"
] @keyword
"#;

const JSX_EXTRA_HIGHLIGHTS: &str = r#"
(jsx_opening_element name: (identifier) @tag)
(jsx_self_closing_element name: (identifier) @tag)
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrammarKind {
    Rust,
    TypeScript,
    Tsx,
    Python,
    Json,
}

struct CompiledGrammar {
    language: Language,
    query: Query,
}

static RUST_GRAMMAR: OnceLock<Result<CompiledGrammar, String>> = OnceLock::new();
static TS_GRAMMAR: OnceLock<Result<CompiledGrammar, String>> = OnceLock::new();
static TSX_GRAMMAR: OnceLock<Result<CompiledGrammar, String>> = OnceLock::new();
static PY_GRAMMAR: OnceLock<Result<CompiledGrammar, String>> = OnceLock::new();
static JSON_GRAMMAR: OnceLock<Result<CompiledGrammar, String>> = OnceLock::new();

fn compile_grammar(language: Language, query_src: &str) -> Result<CompiledGrammar, String> {
    let query = Query::new(&language, query_src).map_err(|err| err.to_string())?;
    Ok(CompiledGrammar { language, query })
}

fn grammar_for_path(path: &RepoRelPath) -> Option<GrammarKind> {
    let (_, ext) = path.file_name().rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Some(GrammarKind::Rust),
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(GrammarKind::TypeScript),
        "tsx" | "jsx" => Some(GrammarKind::Tsx),
        "py" | "pyi" => Some(GrammarKind::Python),
        "json" | "jsonc" => Some(GrammarKind::Json),
        _ => None,
    }
}

fn compiled_grammar(kind: GrammarKind) -> DiffMergeResult<&'static CompiledGrammar> {
    let slot = match kind {
        GrammarKind::Rust => RUST_GRAMMAR.get_or_init(|| {
            compile_grammar(
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )
        }),
        GrammarKind::TypeScript => TS_GRAMMAR.get_or_init(|| {
            let query_src = format!(
                "{JS_TS_BASE_HIGHLIGHTS}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            );
            compile_grammar(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                &query_src,
            )
        }),
        GrammarKind::Tsx => TSX_GRAMMAR.get_or_init(|| {
            let query_src = format!(
                "{JS_TS_BASE_HIGHLIGHTS}\n{JSX_EXTRA_HIGHLIGHTS}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            );
            compile_grammar(tree_sitter_typescript::LANGUAGE_TSX.into(), &query_src)
        }),
        GrammarKind::Python => PY_GRAMMAR.get_or_init(|| {
            compile_grammar(
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            )
        }),
        GrammarKind::Json => JSON_GRAMMAR.get_or_init(|| {
            compile_grammar(
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )
        }),
    };

    slot.as_ref()
        .map_err(|msg| DiffMergeError::SyntaxParser(msg.clone()))
}

/// AST syntax highlighter & enclosing symbol extractor backed by `tree-sitter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TreeSitterHighlighter;

impl TreeSitterHighlighter {
    /// Creates a new [`TreeSitterHighlighter`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

impl SyntaxHighlighter for TreeSitterHighlighter {
    fn highlight_patch(
        &self,
        patch: &mut FileDiffPatch,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> DiffMergeResult<()> {
        if patch.delta.is_binary || patch.hunks.is_empty() {
            return Ok(());
        }

        let Some(kind) = grammar_for_path(&patch.delta.new_path)
            .or_else(|| patch.delta.old_path.as_ref().and_then(grammar_for_path))
        else {
            return Ok(());
        };

        let compiled = compiled_grammar(kind)?;
        let path_str = patch.delta.new_path.as_str();
        let old_parsed = parse_source_for_highlight(path_str, &compiled.language, old_source)?;
        let new_parsed = parse_source_for_highlight(path_str, &compiled.language, new_source)?;

        let mut cursor = QueryCursor::new();

        for hunk in &mut patch.hunks {
            let mut old_needed_lines = BTreeSet::new();
            let mut new_needed_lines = BTreeSet::new();

            for line in &hunk.lines {
                match line.origin {
                    DiffLineOrigin::Deletion => {
                        if let Some(ln) = line.old_lineno {
                            old_needed_lines.insert(ln);
                        }
                    }
                    DiffLineOrigin::Addition => {
                        if let Some(ln) = line.new_lineno {
                            new_needed_lines.insert(ln);
                        }
                    }
                    DiffLineOrigin::Context => {
                        if new_parsed.is_some() {
                            if let Some(ln) = line.new_lineno {
                                new_needed_lines.insert(ln);
                            }
                        } else if let Some(ln) = line.old_lineno {
                            old_needed_lines.insert(ln);
                        }
                    }
                    DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew => {}
                }
            }

            let old_tokens = match old_parsed.as_ref() {
                Some(parsed) if !old_needed_lines.is_empty() => collect_hunk_line_tokens(
                    &compiled.query,
                    &mut cursor,
                    parsed,
                    &old_needed_lines,
                ),
                _ => HashMap::new(),
            };

            let new_tokens = match new_parsed.as_ref() {
                Some(parsed) if !new_needed_lines.is_empty() => collect_hunk_line_tokens(
                    &compiled.query,
                    &mut cursor,
                    parsed,
                    &new_needed_lines,
                ),
                _ => HashMap::new(),
            };

            if hunk.enclosing_symbol.is_none() {
                hunk.enclosing_symbol = pick_hunk_enclosing_symbol(
                    kind,
                    hunk,
                    new_parsed.as_ref(),
                    old_parsed.as_ref(),
                );
            }

            let backing_len = patch.backing_text.len();
            for line in &mut hunk.lines {
                let raw_text_len = {
                    let s = (line.content_range.start as usize).min(backing_len);
                    let e = (line.content_range.end as usize).min(backing_len);
                    if s <= e {
                        patch.backing_text[s..e]
                            .strip_suffix('\r')
                            .map_or(e - s, str::len)
                    } else {
                        0
                    }
                };
                let max_u32 = u32::try_from(raw_text_len).unwrap_or(u32::MAX);

                let spans = match line.origin {
                    DiffLineOrigin::Deletion => line
                        .old_lineno
                        .and_then(|ln| old_tokens.get(&ln))
                        .cloned()
                        .unwrap_or_default(),
                    DiffLineOrigin::Addition => line
                        .new_lineno
                        .and_then(|ln| new_tokens.get(&ln))
                        .cloned()
                        .unwrap_or_default(),
                    DiffLineOrigin::Context => {
                        if new_parsed.is_some() {
                            line.new_lineno
                                .and_then(|ln| new_tokens.get(&ln))
                                .cloned()
                                .unwrap_or_default()
                        } else {
                            line.old_lineno
                                .and_then(|ln| old_tokens.get(&ln))
                                .cloned()
                                .unwrap_or_default()
                        }
                    }
                    DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew => Vec::new(),
                };

                line.syntax_tokens = clamp_and_dedup_spans(spans, max_u32);
            }
        }

        Ok(())
    }

    fn extract_enclosing_symbol(
        &self,
        path: &RepoRelPath,
        source: &[u8],
        target_line_1_based: u32,
    ) -> Option<EnclosingSymbol> {
        let kind = grammar_for_path(path)?;
        if source.is_empty() || target_line_1_based == 0 {
            return None;
        }
        let compiled = compiled_grammar(kind).ok()?;
        let parsed =
            parse_source_for_highlight(path.as_str(), &compiled.language, Some(source)).ok()??;
        find_enclosing_symbol_in_parsed(kind, &parsed, target_line_1_based)
    }
}

struct ParsedSource<'a> {
    bytes: &'a [u8],
    tree: Tree,
    /// Per-line `(start_byte, content_end_byte_excluding_crlf)` indexed by 0-based row.
    line_ranges: Vec<(usize, usize)>,
}

fn parse_source_for_highlight<'a>(
    path: &str,
    language: &Language,
    source: Option<&'a [u8]>,
) -> DiffMergeResult<Option<ParsedSource<'a>>> {
    let Some(bytes) = source else {
        return Ok(None);
    };
    if bytes.is_empty() || bytes.len() > MAX_SYNTAX_HIGHLIGHT_BYTES {
        return Ok(None);
    }
    if std::str::from_utf8(bytes).is_err() {
        return Err(DiffMergeError::UnsupportedEncoding {
            path: path.to_string(),
        });
    }

    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|err| DiffMergeError::SyntaxParser(err.to_string()))?;
    let tree = parser
        .parse(bytes, None)
        .ok_or_else(|| DiffMergeError::SyntaxParser("tree-sitter parse returned None".into()))?;
    let line_ranges = compute_line_ranges(bytes);
    Ok(Some(ParsedSource {
        bytes,
        tree,
        line_ranges,
    }))
}

fn compute_line_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        if let Some(rel_nl) = bytes[start..].iter().position(|&b| b == b'\n') {
            let raw_end = start + rel_nl;
            let content_end = if raw_end > start && bytes[raw_end - 1] == b'\r' {
                raw_end - 1
            } else {
                raw_end
            };
            ranges.push((start, content_end));
            start = raw_end + 1;
        } else {
            let content_end = if bytes.len() > start && bytes[bytes.len() - 1] == b'\r' {
                bytes.len() - 1
            } else {
                bytes.len()
            };
            ranges.push((start, content_end));
            break;
        }
    }
    ranges
}

fn collect_hunk_line_tokens(
    query: &Query,
    cursor: &mut QueryCursor,
    parsed: &ParsedSource<'_>,
    needed_1_based: &BTreeSet<u32>,
) -> HashMap<u32, Vec<(u32, u32, SyntaxTokenKind, u8)>> {
    let mut result: HashMap<u32, Vec<(u32, u32, SyntaxTokenKind, u8)>> = HashMap::new();
    let Some(&first_ln) = needed_1_based.first() else {
        return result;
    };
    let Some(&last_ln) = needed_1_based.last() else {
        return result;
    };

    let first_row = usize::try_from(first_ln.saturating_sub(1)).unwrap_or(0);
    let last_row = usize::try_from(last_ln.saturating_sub(1)).unwrap_or(0);
    if first_row >= parsed.line_ranges.len() {
        return result;
    }
    let clamped_last_row = last_row.min(parsed.line_ranges.len() - 1);
    let range_start = parsed.line_ranges[first_row].0;
    let range_end = parsed.line_ranges[clamped_last_row].1;

    if range_start >= range_end {
        return result;
    }

    cursor.set_byte_range(range_start..range_end);
    let capture_names = query.capture_names();
    let mut captures = cursor.captures(query, parsed.tree.root_node(), parsed.bytes);

    while let Some((mat, capture_idx)) = captures.next() {
        let cap = mat.captures()[*capture_idx];
        let cap_name = capture_names.get(cap.index as usize).copied().unwrap_or("");
        let Some((token_kind, priority)) = map_capture_name(cap_name) else {
            continue;
        };

        let start_pt = cap.node.start_position();
        let end_pt = cap.node.end_position();
        let row_lo = start_pt.row.max(first_row);
        let row_hi = end_pt.row.min(clamped_last_row);

        for row in row_lo..=row_hi {
            let lineno = (row as u32).saturating_add(1);
            if !needed_1_based.contains(&lineno) {
                continue;
            }
            let (line_start, line_end) = parsed.line_ranges[row];
            let line_len = line_end.saturating_sub(line_start);

            let col_start = if row == start_pt.row {
                start_pt.column.min(line_len)
            } else {
                0
            };
            let col_end = if row == end_pt.row {
                end_pt.column.min(line_len)
            } else {
                line_len
            };

            if col_start < col_end {
                result.entry(lineno).or_default().push((
                    col_start as u32,
                    col_end as u32,
                    token_kind,
                    priority,
                ));
            }
        }
    }

    result
}

fn map_capture_name(name: &str) -> Option<(SyntaxTokenKind, u8)> {
    if name.starts_with("string.special.key") {
        return Some((SyntaxTokenKind::Property, 5));
    }
    if name.starts_with("function.macro") || name.starts_with("macro") {
        return Some((SyntaxTokenKind::Macro, 5));
    }
    if name.starts_with("comment") {
        return Some((SyntaxTokenKind::Comment, 6));
    }
    if name.starts_with("attribute") || name.starts_with("tag") || name.starts_with("label") {
        return Some((SyntaxTokenKind::Attribute, 5));
    }
    if name.starts_with("keyword") {
        return Some((SyntaxTokenKind::Keyword, 4));
    }
    if name.starts_with("string") || name.starts_with("escape") || name.starts_with("embedded") {
        return Some((SyntaxTokenKind::String, 4));
    }
    if name.starts_with("number") || name.starts_with("float") {
        return Some((SyntaxTokenKind::Number, 4));
    }
    if name.starts_with("function") {
        return Some((SyntaxTokenKind::Function, 3));
    }
    if name.starts_with("type") || name.starts_with("constructor") {
        return Some((SyntaxTokenKind::Type, 3));
    }
    if name.starts_with("constant") {
        return Some((SyntaxTokenKind::Constant, 3));
    }
    if name.starts_with("property") {
        return Some((SyntaxTokenKind::Property, 3));
    }
    if name.starts_with("operator") {
        return Some((SyntaxTokenKind::Operator, 2));
    }
    if name.starts_with("punctuation") {
        return Some((SyntaxTokenKind::Punctuation, 2));
    }
    if name.starts_with("variable") {
        return Some((SyntaxTokenKind::Variable, 1));
    }
    None
}

fn clamp_and_dedup_spans(
    raw: Vec<(u32, u32, SyntaxTokenKind, u8)>,
    max_len: u32,
) -> SmallVec<[SyntaxTokenSpan; 8]> {
    if raw.is_empty() || max_len == 0 {
        return SmallVec::new();
    }

    // Deduplicate identical byte intervals by keeping highest capture specificity priority.
    let mut by_range: BTreeMap<(u32, u32), (SyntaxTokenKind, u8)> = BTreeMap::new();
    for (s, e, kind, prio) in raw {
        let cs = s.min(max_len);
        let ce = e.min(max_len);
        if cs >= ce {
            continue;
        }
        by_range
            .entry((cs, ce))
            .and_modify(|existing| {
                if prio >= existing.1 {
                    *existing = (kind, prio);
                }
            })
            .or_insert((kind, prio));
    }

    // Sort by start ascending, higher priority first, longer span first, then keep non-overlapping.
    let mut ordered: Vec<(u32, u32, SyntaxTokenKind, u8)> = by_range
        .into_iter()
        .map(|((s, e), (kind, prio))| (s, e, kind, prio))
        .collect();
    ordered.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| a.1.cmp(&b.1))
    });

    let mut out: SmallVec<[SyntaxTokenSpan; 8]> = SmallVec::new();
    let mut last_end = 0u32;
    let mut last_prio = 0u8;
    for (s, e, kind, prio) in ordered {
        if s >= last_end {
            out.push(SyntaxTokenSpan {
                start: s,
                end: e,
                kind,
            });
            last_end = e;
            last_prio = prio;
        } else if prio > last_prio && s >= out.last().map_or(0, |prev| prev.start) {
            if let Some(prev) = out.last_mut() {
                if s > prev.start {
                    prev.end = s;
                    out.push(SyntaxTokenSpan {
                        start: s,
                        end: e,
                        kind,
                    });
                } else {
                    *prev = SyntaxTokenSpan {
                        start: s,
                        end: e,
                        kind,
                    };
                }
            }
            last_end = e;
            last_prio = prio;
        }
    }
    out
}

fn pick_hunk_enclosing_symbol(
    kind: GrammarKind,
    hunk: &branvio_core::domain::DiffHunk,
    new_parsed: Option<&ParsedSource<'_>>,
    old_parsed: Option<&ParsedSource<'_>>,
) -> Option<EnclosingSymbol> {
    if let Some(parsed) = new_parsed {
        let target = hunk
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Addition)
            .and_then(|l| l.new_lineno)
            .or((hunk.new_start > 0).then_some(hunk.new_start));
        if let Some(line_1) = target
            && let Some(sym) = find_enclosing_symbol_in_parsed(kind, parsed, line_1)
        {
            return Some(sym);
        }
    }

    if let Some(parsed) = old_parsed {
        let target = hunk
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Deletion)
            .and_then(|l| l.old_lineno)
            .or((hunk.old_start > 0).then_some(hunk.old_start));
        if let Some(line_1) = target {
            return find_enclosing_symbol_in_parsed(kind, parsed, line_1);
        }
    }
    None
}

fn find_enclosing_symbol_in_parsed(
    grammar: GrammarKind,
    parsed: &ParsedSource<'_>,
    target_line_1_based: u32,
) -> Option<EnclosingSymbol> {
    let row = usize::try_from(target_line_1_based.saturating_sub(1)).ok()?;
    let &(line_start, line_end) = parsed.line_ranges.get(row)?;
    let line_bytes = &parsed.bytes[line_start..line_end];
    let first_non_ws = line_bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0);

    let pt = Point::new(row, first_non_ws);
    let mut node = parsed.tree.root_node().descendant_for_point_range(pt, pt)?;

    loop {
        if let Some(sym_kind) = classify_symbol_node(grammar, node) {
            let signature = extract_node_signature(node, parsed.bytes);
            if !signature.is_empty() {
                return Some(EnclosingSymbol {
                    kind: sym_kind,
                    signature,
                    start_line: (node.start_position().row as u32).saturating_add(1),
                });
            }
        }
        node = node.parent()?;
    }
}

fn classify_symbol_node(grammar: GrammarKind, node: Node<'_>) -> Option<SymbolKind> {
    match grammar {
        GrammarKind::Rust => match node.kind() {
            "function_item" => {
                if has_ancestor_kind(node, &["impl_item", "trait_item"]) {
                    Some(SymbolKind::Method)
                } else {
                    Some(SymbolKind::Function)
                }
            }
            "struct_item" => Some(SymbolKind::Struct),
            "enum_item" => Some(SymbolKind::Enum),
            "trait_item" => Some(SymbolKind::Trait),
            "impl_item" => Some(SymbolKind::Impl),
            "mod_item" => Some(SymbolKind::Module),
            "const_item" | "static_item" => Some(SymbolKind::Constant),
            _ => None,
        },
        GrammarKind::TypeScript | GrammarKind::Tsx => match node.kind() {
            "function_declaration" | "generator_function_declaration" => Some(SymbolKind::Function),
            "method_definition" => Some(SymbolKind::Method),
            "class_declaration" => Some(SymbolKind::Class),
            "interface_declaration" => Some(SymbolKind::Interface),
            "enum_declaration" => Some(SymbolKind::Enum),
            _ => None,
        },
        GrammarKind::Python => match node.kind() {
            "function_definition" => {
                if has_ancestor_kind(node, &["class_definition"]) {
                    Some(SymbolKind::Method)
                } else {
                    Some(SymbolKind::Function)
                }
            }
            "class_definition" => Some(SymbolKind::Class),
            _ => None,
        },
        GrammarKind::Json => None,
    }
}

fn has_ancestor_kind(mut node: Node<'_>, kinds: &[&str]) -> bool {
    while let Some(parent) = node.parent() {
        if kinds.contains(&parent.kind()) {
            return true;
        }
        node = parent;
    }
    false
}

fn extract_node_signature(node: Node<'_>, source: &[u8]) -> CompactString {
    let start = node.start_byte();
    let end = node
        .child_by_field_name("body")
        .map_or_else(|| node.end_byte(), |body| body.start_byte())
        .min(source.len());
    if start >= end {
        return CompactString::default();
    }
    let Ok(header) = std::str::from_utf8(&source[start..end]) else {
        return CompactString::default();
    };
    let trimmed = header.trim().trim_end_matches(':').trim();

    let mut collapsed = String::with_capacity(trimmed.len().min(MAX_SIGNATURE_LEN + 4));
    let mut prev_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !collapsed.is_empty() {
                prev_ws = true;
            }
        } else {
            if prev_ws {
                collapsed.push(' ');
                prev_ws = false;
            }
            collapsed.push(ch);
            if collapsed.len() >= MAX_SIGNATURE_LEN {
                let trunc_at = collapsed
                    .char_indices()
                    .take_while(|(idx, c)| {
                        idx + c.len_utf8() <= MAX_SIGNATURE_LEN.saturating_sub(3)
                    })
                    .last()
                    .map_or(0, |(idx, c)| idx + c.len_utf8());
                collapsed.truncate(trunc_at);
                collapsed.push_str("...");
                break;
            }
        }
    }
    CompactString::from(collapsed.trim())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::diff_engine::ImaraDiffEngine;
    use crate::traits::{DiffEngine, DiffOptions};
    use branvio_core::domain::{DeltaArea, DeltaStatus, EolChange, FileDelta, FileMode};

    fn dummy_delta(path: &str) -> FileDelta {
        FileDelta {
            old_path: None,
            new_path: RepoRelPath::normalize(path).unwrap(),
            status: DeltaStatus::Modified,
            area: DeltaArea::CommitDiff,
            old_mode: FileMode::Regular644,
            new_mode: FileMode::Regular644,
            old_oid: None,
            new_oid: None,
            is_binary: false,
            lfs: None,
            eol_change: EolChange::Unchanged,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn all_supported_language_queries_compile_and_highlight_cleanly() {
        let hl = TreeSitterHighlighter::new();
        let diff_engine = ImaraDiffEngine::new();

        // 1. Rust: enclosing method inside impl + keyword/type/string highlighting.
        let rs_old = b"pub struct Engine {\n    name: String,\n}\n\nimpl Engine {\n    pub fn run(&self) -> usize {\n        let msg = \"ok\";\n        msg.len()\n    }\n}\n";
        let rs_new = b"pub struct Engine {\n    name: String,\n}\n\nimpl Engine {\n    pub fn run(&self) -> usize {\n        // comment\n        let msg = \"done\";\n        msg.len()\n    }\n}\n";
        let mut rs_patch = diff_engine
            .diff_blobs(
                dummy_delta("src/engine.rs"),
                rs_old,
                rs_new,
                &DiffOptions::default(),
            )
            .unwrap();
        hl.highlight_patch(&mut rs_patch, Some(rs_old), Some(rs_new))
            .unwrap();

        let sym = rs_patch.hunks[0].enclosing_symbol.as_ref().unwrap();
        assert_eq!(sym.kind, SymbolKind::Method);
        assert!(sym.signature.contains("pub fn run(&self) -> usize"));
        assert_eq!(sym.start_line, 6);

        let comment_line = rs_patch.hunks[0]
            .lines
            .iter()
            .find(|l| rs_patch.line_text(l).contains("// comment"))
            .unwrap();
        assert!(
            comment_line
                .syntax_tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Comment)
        );
        // Verify all spans on a line are strictly sorted & non-overlapping.
        for line in &rs_patch.hunks[0].lines {
            for pair in line.syntax_tokens.windows(2) {
                assert!(pair[0].end <= pair[1].start);
            }
        }

        // 2. TypeScript
        let ts_src = b"export interface Config { port: number; }\nexport function boot(cfg: Config): string {\n  const label = 'ready';\n  return label;\n}\n";
        let ts_sym = hl
            .extract_enclosing_symbol(&RepoRelPath::normalize("src/app.ts").unwrap(), ts_src, 3)
            .unwrap();
        assert_eq!(ts_sym.kind, SymbolKind::Function);
        assert!(ts_sym.signature.contains("function boot"));

        // 3. TSX
        let tsx_old = b"export function App() { return <div className=\"a\" />; }\n";
        let tsx_new = b"export function App() { return <div className=\"b\" />; }\n";
        let mut tsx_patch = diff_engine
            .diff_blobs(
                dummy_delta("web/App.tsx"),
                tsx_old,
                tsx_new,
                &DiffOptions::default(),
            )
            .unwrap();
        hl.highlight_patch(&mut tsx_patch, Some(tsx_old), Some(tsx_new))
            .unwrap();
        assert!(
            tsx_patch.hunks[0].lines[1]
                .syntax_tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Keyword || t.kind == SyntaxTokenKind::String)
        );

        // 4. Python
        let py_src = b"class Worker:\n    def execute(self, n: int) -> int:\n        total = n + 1\n        return total\n";
        let py_sym = hl
            .extract_enclosing_symbol(&RepoRelPath::normalize("worker.py").unwrap(), py_src, 3)
            .unwrap();
        assert_eq!(py_sym.kind, SymbolKind::Method);
        assert!(py_sym.signature.contains("def execute"));

        // 5. JSON
        let json_old = b"{\n  \"port\": 8080\n}\n";
        let json_new = b"{\n  \"port\": 9090\n}\n";
        let mut json_patch = diff_engine
            .diff_blobs(
                dummy_delta("config.json"),
                json_old,
                json_new,
                &DiffOptions::default(),
            )
            .unwrap();
        hl.highlight_patch(&mut json_patch, Some(json_old), Some(json_new))
            .unwrap();
        let json_add = json_patch.hunks[0]
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Addition)
            .unwrap();
        assert!(
            json_add
                .syntax_tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Property)
        );
        assert!(
            json_add
                .syntax_tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Number)
        );

        // 6. Unknown extension returns Ok(()) gracefully with empty tokens.
        let mut txt_patch = diff_engine
            .diff_blobs(
                dummy_delta("notes.txt"),
                b"a\n",
                b"b\n",
                &DiffOptions::default(),
            )
            .unwrap();
        hl.highlight_patch(&mut txt_patch, Some(b"a\n"), Some(b"b\n"))
            .unwrap();
        assert!(txt_patch.hunks[0].enclosing_symbol.is_none());

        // 7. Oversized buffers (> MAX_SYNTAX_HIGHLIGHT_BYTES) gracefully skip AST parsing without failing.
        let big = vec![b'a'; MAX_SYNTAX_HIGHLIGHT_BYTES + 16];
        hl.highlight_patch(&mut rs_patch, None, Some(&big)).unwrap();

        // 8. Non-UTF-8 source bytes passed directly to highlight_patch are rejected cleanly.
        let err = hl
            .highlight_patch(&mut rs_patch, None, Some(b"fn x() { /* \xff */ }\n"))
            .unwrap_err();
        assert!(matches!(err, DiffMergeError::UnsupportedEncoding { .. }));
    }
}
