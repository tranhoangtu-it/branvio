//! High-speed Myers and Histogram diff engine backed by `imara-diff` with word-level
//! intra-line diffing, compact shared-buffer storage, and EOL normalization.

use crate::traits::{
    DiffAlgorithmKind, DiffEngine, DiffMergeError, DiffMergeResult, DiffOptions,
    MAX_TEXT_DIFF_BYTES,
};
use branvio_core::domain::{
    ByteRange, DiffHunk, DiffLine, DiffLineOrigin, EolChange, FileDelta, FileDiffPatch,
    InlineByteSpan,
};
use compact_str::{CompactString, format_compact};
use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::sync::Arc;

const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";
const MAX_PAIRED_WORD_DIFF_LINES: usize = 32;
const MAX_WORD_DIFF_LINE_BYTES: usize = 1024;

/// Production [`DiffEngine`] implementation powered by `imara-diff`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImaraDiffEngine;

impl ImaraDiffEngine {
    /// Creates a new [`ImaraDiffEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug, Clone, Copy)]
struct ParsedLine<'a> {
    /// Line text excluding `\n` (retaining `\r` if CRLF so byte-exact patches round-trip cleanly).
    raw_no_lf: &'a str,
    /// Line text excluding both `\n` and trailing `\r`.
    clean_no_eol: &'a str,
    /// Whether this line was terminated by `\n`.
    has_lf: bool,
    /// Whether this line ended in `\r\n`.
    is_crlf: bool,
}

struct SliceTokenSource<'a, T>(&'a [T]);

struct SliceTokenIter<'a, T>(std::slice::Iter<'a, T>);

impl<'a, T: AsRef<[u8]>> Iterator for SliceTokenIter<'a, T> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(AsRef::as_ref)
    }
}

impl<'a, T: AsRef<[u8]>> TokenSource for SliceTokenSource<'a, T> {
    type Token = &'a [u8];
    type Tokenizer = SliceTokenIter<'a, T>;

    #[inline]
    fn tokenize(&self) -> Self::Tokenizer {
        SliceTokenIter(self.0.iter())
    }

    #[inline]
    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

struct PendingLine<'a> {
    origin: DiffLineOrigin,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
    text: &'a str,
    old_text: &'a str,
    has_trailing_newline: bool,
    word_highlights: SmallVec<[InlineByteSpan; 4]>,
}

impl DiffEngine for ImaraDiffEngine {
    fn diff_blobs(
        &self,
        mut delta: FileDelta,
        old_bytes: &[u8],
        new_bytes: &[u8],
        options: &DiffOptions,
    ) -> DiffMergeResult<FileDiffPatch> {
        let path_str = delta.new_path.as_str().to_string();
        if old_bytes.len() > MAX_TEXT_DIFF_BYTES {
            return Err(DiffMergeError::InputTooLarge {
                path: path_str,
                byte_len: old_bytes.len(),
                max_bytes: MAX_TEXT_DIFF_BYTES,
            });
        }
        if new_bytes.len() > MAX_TEXT_DIFF_BYTES {
            return Err(DiffMergeError::InputTooLarge {
                path: path_str,
                byte_len: new_bytes.len(),
                max_bytes: MAX_TEXT_DIFF_BYTES,
            });
        }

        if delta.is_binary || looks_binary(old_bytes) || looks_binary(new_bytes) {
            return Err(DiffMergeError::BinaryFileUnsupported(path_str));
        }

        let old_str =
            std::str::from_utf8(old_bytes).map_err(|_| DiffMergeError::UnsupportedEncoding {
                path: path_str.clone(),
            })?;
        let new_str = std::str::from_utf8(new_bytes)
            .map_err(|_| DiffMergeError::UnsupportedEncoding { path: path_str })?;
        let old_lines = parse_lines(old_str);
        let new_lines = parse_lines(new_str);

        let eol_change = classify_eol_change(&old_lines, &new_lines);
        delta.eol_change = eol_change;

        let normalize_crlf = options.respect_autocrlf && eol_change != EolChange::Unchanged;
        let old_keys = build_comparison_keys(&old_lines, options.ignore_whitespace, normalize_crlf);
        let new_keys = build_comparison_keys(&new_lines, options.ignore_whitespace, normalize_crlf);

        let algo = match options.algorithm {
            DiffAlgorithmKind::Histogram => Algorithm::Histogram,
            DiffAlgorithmKind::Myers => Algorithm::Myers,
        };

        let input = InternedInput::new(SliceTokenSource(&old_keys), SliceTokenSource(&new_keys));
        let mut diff = Diff::compute(algo, &input);
        diff.postprocess_lines(&input);

        let total_additions = diff.count_additions();
        let total_deletions = diff.count_removals();
        delta.additions = total_additions;
        delta.deletions = total_deletions;

        let raw_hunks: Vec<imara_diff::Hunk> = diff.hunks().collect();
        if raw_hunks.is_empty() {
            return Ok(FileDiffPatch {
                delta,
                backing_text: Arc::from(""),
                hunks: Vec::new(),
            });
        }

        let ctx = options.context_lines;
        let groups = group_raw_hunks(&raw_hunks, ctx);

        let mut backing =
            String::with_capacity((old_bytes.len() + new_bytes.len()).min(256 * 1024));
        let mut no_nl_marker_range: Option<ByteRange> = None;
        let mut built_hunks = Vec::with_capacity(groups.len());

        let total_old = u32::try_from(old_lines.len()).unwrap_or(u32::MAX);
        let total_new = u32::try_from(new_lines.len()).unwrap_or(u32::MAX);

        for (hunk_idx, group) in groups.into_iter().enumerate() {
            let old_ctx_start = group[0].before.start.saturating_sub(ctx);
            let old_ctx_end =
                (group[group.len() - 1].before.end.saturating_add(ctx)).min(total_old);
            let new_ctx_start = group[0].after.start.saturating_sub(ctx);
            let new_ctx_end = (group[group.len() - 1].after.end.saturating_add(ctx)).min(total_new);

            let mut pending: Vec<PendingLine<'_>> = Vec::with_capacity(
                ((old_ctx_end - old_ctx_start) + (new_ctx_end - new_ctx_start)) as usize + 2,
            );

            let mut cur_old = old_ctx_start;
            let mut cur_new = new_ctx_start;

            for sub in group {
                while cur_old < sub.before.start && cur_new < sub.after.start {
                    let old_line = &old_lines[cur_old as usize];
                    let new_line = &new_lines[cur_new as usize];
                    pending.push(PendingLine {
                        origin: DiffLineOrigin::Context,
                        old_lineno: Some(cur_old + 1),
                        new_lineno: Some(cur_new + 1),
                        text: new_line.raw_no_lf,
                        old_text: old_line.raw_no_lf,
                        has_trailing_newline: new_line.has_lf,
                        word_highlights: SmallVec::new(),
                    });
                    if !old_line.has_lf && !new_line.has_lf {
                        pending.push(eof_marker_pending(DiffLineOrigin::EofNoNewlineNew));
                    }
                    cur_old += 1;
                    cur_new += 1;
                }

                let del_start_idx = pending.len();
                for old_pos in sub.before.start..sub.before.end {
                    let line = &old_lines[old_pos as usize];
                    pending.push(PendingLine {
                        origin: DiffLineOrigin::Deletion,
                        old_lineno: Some(old_pos + 1),
                        new_lineno: None,
                        text: line.raw_no_lf,
                        old_text: line.raw_no_lf,
                        has_trailing_newline: line.has_lf,
                        word_highlights: SmallVec::new(),
                    });
                    if !line.has_lf {
                        pending.push(eof_marker_pending(DiffLineOrigin::EofNoNewlineOld));
                    }
                }
                let del_end_idx = pending.len();

                let add_start_idx = pending.len();
                for new_pos in sub.after.start..sub.after.end {
                    let line = &new_lines[new_pos as usize];
                    pending.push(PendingLine {
                        origin: DiffLineOrigin::Addition,
                        old_lineno: None,
                        new_lineno: Some(new_pos + 1),
                        text: line.raw_no_lf,
                        old_text: line.raw_no_lf,
                        has_trailing_newline: line.has_lf,
                        word_highlights: SmallVec::new(),
                    });
                    if !line.has_lf {
                        pending.push(eof_marker_pending(DiffLineOrigin::EofNoNewlineNew));
                    }
                }
                let add_end_idx = pending.len();

                annotate_word_diffs(
                    &mut pending,
                    &old_lines,
                    &new_lines,
                    del_start_idx..del_end_idx,
                    add_start_idx..add_end_idx,
                );

                cur_old = sub.before.end;
                cur_new = sub.after.end;
            }

            while cur_old < old_ctx_end && cur_new < new_ctx_end {
                let old_line = &old_lines[cur_old as usize];
                let new_line = &new_lines[cur_new as usize];
                pending.push(PendingLine {
                    origin: DiffLineOrigin::Context,
                    old_lineno: Some(cur_old + 1),
                    new_lineno: Some(cur_new + 1),
                    text: new_line.raw_no_lf,
                    old_text: old_line.raw_no_lf,
                    has_trailing_newline: new_line.has_lf,
                    word_highlights: SmallVec::new(),
                });
                if !old_line.has_lf && !new_line.has_lf {
                    pending.push(eof_marker_pending(DiffLineOrigin::EofNoNewlineNew));
                }
                cur_old += 1;
                cur_new += 1;
            }

            let old_count = old_ctx_end - old_ctx_start;
            let new_count = new_ctx_end - new_ctx_start;
            let old_start_1 = if old_count == 0 {
                old_ctx_start
            } else {
                old_ctx_start + 1
            };
            let new_start_1 = if new_count == 0 {
                new_ctx_start
            } else {
                new_ctx_start + 1
            };

            let header_raw = format_hunk_header(old_start_1, old_count, new_start_1, new_count);
            let fingerprint = compute_hunk_fingerprint(&pending);

            let mut diff_lines = Vec::with_capacity(pending.len());
            for item in pending {
                let (content_range, old_content_range) = if matches!(
                    item.origin,
                    DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew
                ) {
                    let r = if let Some(existing) = no_nl_marker_range {
                        existing
                    } else {
                        let r = push_backing_slice(&mut backing, NO_NEWLINE_MARKER);
                        no_nl_marker_range = Some(r);
                        r
                    };
                    (r, r)
                } else {
                    let content_range = push_backing_slice(&mut backing, item.text);
                    let old_content_range = if item.old_text == item.text {
                        content_range
                    } else {
                        push_backing_slice(&mut backing, item.old_text)
                    };
                    (content_range, old_content_range)
                };

                diff_lines.push(DiffLine {
                    origin: item.origin,
                    old_lineno: item.old_lineno,
                    new_lineno: item.new_lineno,
                    content_range,
                    old_content_range,
                    has_trailing_newline: item.has_trailing_newline,
                    word_highlights: item.word_highlights,
                    syntax_tokens: SmallVec::new(),
                });
            }

            built_hunks.push(DiffHunk {
                hunk_index: u32::try_from(hunk_idx).unwrap_or(u32::MAX),
                old_start: old_start_1,
                old_lines: old_count,
                new_start: new_start_1,
                new_lines: new_count,
                header_raw,
                enclosing_symbol: None,
                lines: diff_lines,
                content_fingerprint: fingerprint,
            });
        }

        Ok(FileDiffPatch {
            delta,
            backing_text: Arc::from(backing),
            hunks: built_hunks,
        })
    }
}

#[inline]
fn eof_marker_pending(origin: DiffLineOrigin) -> PendingLine<'static> {
    PendingLine {
        origin,
        old_lineno: None,
        new_lineno: None,
        text: NO_NEWLINE_MARKER,
        old_text: NO_NEWLINE_MARKER,
        has_trailing_newline: true,
        word_highlights: SmallVec::new(),
    }
}

#[inline]
fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

fn parse_lines(text: &str) -> Vec<ParsedLine<'_>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        if let Some(rel_nl) = text[start..].find('\n') {
            let raw_no_lf = &text[start..start + rel_nl];
            let clean_no_eol = raw_no_lf.strip_suffix('\r').unwrap_or(raw_no_lf);
            let is_crlf = raw_no_lf.ends_with('\r');
            lines.push(ParsedLine {
                raw_no_lf,
                clean_no_eol,
                has_lf: true,
                is_crlf,
            });
            start = start + rel_nl + 1;
        } else {
            let raw_no_lf = &text[start..];
            let clean_no_eol = raw_no_lf.strip_suffix('\r').unwrap_or(raw_no_lf);
            lines.push(ParsedLine {
                raw_no_lf,
                clean_no_eol,
                has_lf: false,
                is_crlf: false,
            });
            break;
        }
    }
    lines
}

fn classify_eol_change(old_lines: &[ParsedLine<'_>], new_lines: &[ParsedLine<'_>]) -> EolChange {
    let mut old_lf = 0usize;
    let mut old_crlf = 0usize;
    for l in old_lines.iter().filter(|l| l.has_lf) {
        if l.is_crlf {
            old_crlf += 1;
        } else {
            old_lf += 1;
        }
    }

    let mut new_lf = 0usize;
    let mut new_crlf = 0usize;
    for l in new_lines.iter().filter(|l| l.has_lf) {
        if l.is_crlf {
            new_crlf += 1;
        } else {
            new_lf += 1;
        }
    }

    if (new_lf > 0 && new_crlf > 0) || (new_lines.is_empty() && old_lf > 0 && old_crlf > 0) {
        EolChange::MixedLineEndings
    } else if old_lf > 0 && old_crlf == 0 && new_crlf > 0 && new_lf == 0 {
        EolChange::LfToCrlf
    } else if old_crlf > 0 && old_lf == 0 && new_lf > 0 && new_crlf == 0 {
        EolChange::CrlfToLf
    } else {
        EolChange::Unchanged
    }
}

fn build_comparison_keys<'a>(
    lines: &'a [ParsedLine<'a>],
    ignore_whitespace: bool,
    normalize_crlf: bool,
) -> Vec<Cow<'a, [u8]>> {
    let mut keys = Vec::with_capacity(lines.len());
    for line in lines {
        if ignore_whitespace {
            let collapsed = collapse_horizontal_whitespace(line.clean_no_eol);
            let mut bytes = collapsed.into_bytes();
            if line.has_lf {
                bytes.push(b'\n');
            }
            keys.push(Cow::Owned(bytes));
        } else if normalize_crlf {
            let mut bytes = line.clean_no_eol.as_bytes().to_vec();
            if line.has_lf {
                bytes.push(b'\n');
            }
            keys.push(Cow::Owned(bytes));
        } else if line.has_lf {
            let mut bytes = Vec::with_capacity(line.raw_no_lf.len() + 1);
            bytes.extend_from_slice(line.raw_no_lf.as_bytes());
            bytes.push(b'\n');
            keys.push(Cow::Owned(bytes));
        } else {
            keys.push(Cow::Borrowed(line.raw_no_lf.as_bytes()));
        }
    }
    keys
}

fn collapse_horizontal_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.trim().chars() {
        if ch == ' ' || ch == '\t' {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

fn group_raw_hunks(raw: &[imara_diff::Hunk], context_lines: u32) -> Vec<&[imara_diff::Hunk]> {
    let mut groups = Vec::new();
    let mut group_start = 0usize;
    let merge_gap = context_lines.saturating_mul(2);

    for idx in 1..raw.len() {
        let prev = &raw[idx - 1];
        let curr = &raw[idx];
        let same_group = if context_lines == 0 {
            false
        } else {
            curr.before.start <= prev.before.end.saturating_add(merge_gap)
                || curr.after.start <= prev.after.end.saturating_add(merge_gap)
        };
        if !same_group {
            groups.push(&raw[group_start..idx]);
            group_start = idx;
        }
    }
    groups.push(&raw[group_start..]);
    groups
}

fn annotate_word_diffs(
    pending: &mut [PendingLine<'_>],
    old_lines: &[ParsedLine<'_>],
    new_lines: &[ParsedLine<'_>],
    del_range: std::ops::Range<usize>,
    add_range: std::ops::Range<usize>,
) {
    let del_indices: Vec<usize> = del_range
        .filter(|&idx| pending[idx].origin == DiffLineOrigin::Deletion)
        .collect();
    let add_indices: Vec<usize> = add_range
        .filter(|&idx| pending[idx].origin == DiffLineOrigin::Addition)
        .collect();

    if del_indices.is_empty()
        || add_indices.is_empty()
        || del_indices.len() > MAX_PAIRED_WORD_DIFF_LINES
        || add_indices.len() > MAX_PAIRED_WORD_DIFF_LINES
    {
        return;
    }

    let pair_count = del_indices.len().min(add_indices.len());
    for pair in 0..pair_count {
        let del_idx = del_indices[pair];
        let add_idx = add_indices[pair];

        let Some(old_lineno) = pending[del_idx].old_lineno else {
            continue;
        };
        let Some(new_lineno) = pending[add_idx].new_lineno else {
            continue;
        };

        let old_text = old_lines[(old_lineno - 1) as usize].clean_no_eol;
        let new_text = new_lines[(new_lineno - 1) as usize].clean_no_eol;
        if old_text.len() > MAX_WORD_DIFF_LINE_BYTES || new_text.len() > MAX_WORD_DIFF_LINE_BYTES {
            continue;
        }

        let (del_spans, add_spans) = compute_inline_word_spans(old_text, new_text);
        pending[del_idx].word_highlights = del_spans;
        pending[add_idx].word_highlights = add_spans;
    }
}

struct WordToken<'a> {
    start: u32,
    end: u32,
    bytes: &'a [u8],
}

fn tokenize_words(text: &str) -> Vec<WordToken<'_>> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut chars = text.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        let is_word = ch.is_alphanumeric() || ch == '_';
        let is_ws = ch == ' ' || ch == '\t';
        let mut end = start + ch.len_utf8();

        if is_word || is_ws {
            while let Some(&(next_idx, next_ch)) = chars.peek() {
                let next_matches = if is_word {
                    next_ch.is_alphanumeric() || next_ch == '_'
                } else {
                    next_ch == ' ' || next_ch == '\t'
                };
                if !next_matches {
                    break;
                }
                end = next_idx + next_ch.len_utf8();
                chars.next();
            }
        }

        tokens.push(WordToken {
            start: start as u32,
            end: end as u32,
            bytes: &bytes[start..end],
        });
    }
    tokens
}

fn compute_inline_word_spans(
    old_line: &str,
    new_line: &str,
) -> (SmallVec<[InlineByteSpan; 4]>, SmallVec<[InlineByteSpan; 4]>) {
    let old_tokens = tokenize_words(old_line);
    let new_tokens = tokenize_words(new_line);
    let old_slices: Vec<&[u8]> = old_tokens.iter().map(|t| t.bytes).collect();
    let new_slices: Vec<&[u8]> = new_tokens.iter().map(|t| t.bytes).collect();

    let input = InternedInput::new(SliceTokenSource(&old_slices), SliceTokenSource(&new_slices));
    let diff = Diff::compute(Algorithm::Myers, &input);

    let mut del_spans: SmallVec<[InlineByteSpan; 4]> = SmallVec::new();
    for (idx, tok) in old_tokens.iter().enumerate() {
        if diff.is_removed(idx as u32) {
            push_coalesced_span(&mut del_spans, tok.start, tok.end);
        }
    }

    let mut add_spans: SmallVec<[InlineByteSpan; 4]> = SmallVec::new();
    for (idx, tok) in new_tokens.iter().enumerate() {
        if diff.is_added(idx as u32) {
            push_coalesced_span(&mut add_spans, tok.start, tok.end);
        }
    }

    (del_spans, add_spans)
}

#[inline]
fn push_coalesced_span(target: &mut SmallVec<[InlineByteSpan; 4]>, start: u32, end: u32) {
    if let Some(last) = target.last_mut()
        && last.end == start
    {
        last.end = end;
        return;
    }
    target.push(InlineByteSpan { start, end });
}

fn push_backing_slice(backing: &mut String, text: &str) -> ByteRange {
    if text.is_empty() {
        return ByteRange { start: 0, end: 0 };
    }
    let start = u32::try_from(backing.len()).unwrap_or(u32::MAX);
    backing.push_str(text);
    let end = u32::try_from(backing.len()).unwrap_or(u32::MAX);
    ByteRange { start, end }
}

pub(crate) fn format_hunk_header(
    old_start: u32,
    old_lines: u32,
    new_start: u32,
    new_lines: u32,
) -> CompactString {
    match (old_lines, new_lines) {
        (1, 1) => format_compact!("@@ -{old_start} +{new_start} @@"),
        (1, _) => format_compact!("@@ -{old_start} +{new_start},{new_lines} @@"),
        (_, 1) => format_compact!("@@ -{old_start},{old_lines} +{new_start} @@"),
        _ => format_compact!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"),
    }
}

fn compute_hunk_fingerprint(lines: &[PendingLine<'_>]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for line in lines {
        let tag = match line.origin {
            DiffLineOrigin::Context => b' ',
            DiffLineOrigin::Addition => b'+',
            DiffLineOrigin::Deletion => b'-',
            DiffLineOrigin::EofNoNewlineOld => b'<',
            DiffLineOrigin::EofNoNewlineNew => b'>',
        };
        hash ^= u64::from(tag);
        hash = hash.wrapping_mul(PRIME);
        hash ^= u64::from(line.has_trailing_newline);
        hash = hash.wrapping_mul(PRIME);
        for &b in line.text.as_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(PRIME);
        }
        if line.old_text != line.text {
            hash ^= 0xff;
            hash = hash.wrapping_mul(PRIME);
            for &b in line.old_text.as_bytes() {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(PRIME);
            }
        }
        hash ^= b'\n' as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use branvio_core::domain::{DeltaArea, DeltaStatus, FileMode};
    use branvio_core::paths::RepoRelPath;

    fn sample_delta(path: &str) -> FileDelta {
        FileDelta {
            old_path: None,
            new_path: RepoRelPath::normalize(path).unwrap(),
            status: DeltaStatus::Modified,
            area: DeltaArea::Unstaged,
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
    fn computes_histogram_myers_and_word_level_inline_diffs() {
        let engine = ImaraDiffEngine::new();
        let old = b"fn alpha() {\n    let timeout_ms = 100;\n    return timeout_ms;\n}\n";
        let new = b"fn alpha() {\n    let timeout_ms = 250;\n    return timeout_ms + 1;\n}\n";

        let hist = engine
            .diff_blobs(
                sample_delta("src/lib.rs"),
                old,
                new,
                &DiffOptions::default(),
            )
            .unwrap();
        assert_eq!(hist.delta.additions, 2);
        assert_eq!(hist.delta.deletions, 2);
        assert_eq!(hist.hunks.len(), 1);
        assert_eq!(hist.hunks[0].header_raw, "@@ -1,4 +1,4 @@");

        let del_line = hist.hunks[0]
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Deletion && l.old_lineno == Some(2))
            .unwrap();
        let add_line = hist.hunks[0]
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Addition && l.new_lineno == Some(2))
            .unwrap();
        assert_eq!(hist.line_text(del_line), "    let timeout_ms = 100;");
        assert_eq!(
            &hist.line_text(del_line)[del_line.word_highlights[0].start as usize
                ..del_line.word_highlights[0].end as usize],
            "100"
        );
        assert_eq!(
            &hist.line_text(add_line)[add_line.word_highlights[0].start as usize
                ..add_line.word_highlights[0].end as usize],
            "250"
        );

        let myers = engine
            .diff_blobs(
                sample_delta("src/lib.rs"),
                old,
                new,
                &DiffOptions {
                    algorithm: DiffAlgorithmKind::Myers,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        assert_eq!(myers.hunks.len(), 1);
    }

    #[test]
    fn detects_eol_transitions_eof_no_newline_whitespace_ignore_and_binary() {
        let engine = ImaraDiffEngine::new();

        // LF -> CRLF with respect_autocrlf = true: tracks EolChange without noisy line churn
        let crlf_patch = engine
            .diff_blobs(
                sample_delta("crlf.txt"),
                b"line 1\nline 2\n",
                b"line 1\r\nline 2\r\n",
                &DiffOptions {
                    respect_autocrlf: true,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        assert_eq!(crlf_patch.delta.eol_change, EolChange::LfToCrlf);
        assert!(crlf_patch.hunks.is_empty());

        // Trailing newline removal at EOF must be detected as a real diff hunk!
        let eof_patch = engine
            .diff_blobs(
                sample_delta("eof.txt"),
                b"alpha\nbeta\n",
                b"alpha\nbeta",
                &DiffOptions::default(),
            )
            .unwrap();
        assert_eq!(eof_patch.hunks.len(), 1);
        assert!(
            eof_patch.hunks[0]
                .lines
                .iter()
                .any(|l| l.origin == DiffLineOrigin::EofNoNewlineNew)
        );

        // Ignore whitespace skips space-only formatting changes.
        let ws_patch = engine
            .diff_blobs(
                sample_delta("ws.rs"),
                b"let   x=1;\n",
                b"let x=1;\n",
                &DiffOptions {
                    ignore_whitespace: true,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        assert!(ws_patch.hunks.is_empty());

        // Mixed line endings reported explicitly.
        let mixed = engine
            .diff_blobs(
                sample_delta("mix.txt"),
                b"a\nb\n",
                b"a\r\nb\nc\n",
                &DiffOptions::default(),
            )
            .unwrap();
        assert_eq!(mixed.delta.eol_change, EolChange::MixedLineEndings);

        // Binary payload rejected cleanly.
        let bin_err = engine
            .diff_blobs(
                sample_delta("img.png"),
                b"\x89PNG\r\n\x1a\n\0\0\0",
                b"text",
                &DiffOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(bin_err, DiffMergeError::BinaryFileUnsupported(_)));
    }

    #[test]
    fn rejects_non_utf8_and_preserves_old_context_bytes_when_ignoring_whitespace_or_crlf() {
        let engine = ImaraDiffEngine::new();

        // Non-UTF-8 (Latin-1 / Shift-JIS) without NUL byte must return UnsupportedEncoding.
        let latin1_err = engine
            .diff_blobs(
                sample_delta("latin1.txt"),
                b"hello\n",
                b"caf\xe9\n",
                &DiffOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(
            latin1_err,
            DiffMergeError::UnsupportedEncoding { .. }
        ));

        let sjis_err = engine
            .diff_blobs(
                sample_delta("sjis.txt"),
                b"\x82\xa0\n",
                b"ok\n",
                &DiffOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(
            sjis_err,
            DiffMergeError::UnsupportedEncoding { .. }
        ));

        // Ignore whitespace: Context line keeps BOTH old and new exact representations!
        let ws_patch = engine
            .diff_blobs(
                sample_delta("ws_ctx.rs"),
                b"let   x = 1;\nlet old_only = 2;\n",
                b"let x = 1;\nlet new_only = 3;\n",
                &DiffOptions {
                    ignore_whitespace: true,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        let ctx_line = ws_patch.hunks[0]
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Context)
            .unwrap();
        assert_eq!(ws_patch.old_line_text(ctx_line), "let   x = 1;");
        assert_eq!(ws_patch.line_text(ctx_line), "let x = 1;");

        // respect_autocrlf: Context line keeps old LF vs new CRLF distinctly!
        let eol_patch = engine
            .diff_blobs(
                sample_delta("eol_ctx.txt"),
                b"keep\nold\n",
                b"keep\r\nnew\r\n",
                &DiffOptions {
                    respect_autocrlf: true,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        let eol_ctx = eol_patch.hunks[0]
            .lines
            .iter()
            .find(|l| l.origin == DiffLineOrigin::Context)
            .unwrap();
        assert_eq!(eol_patch.old_line_text(eol_ctx), "keep");
        assert_eq!(eol_patch.line_text(eol_ctx), "keep\r");
    }
}
