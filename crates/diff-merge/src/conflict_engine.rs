//! 4-Buffer `ropey::Rope` 3-Way Merge Conflict Engine (`Base`, `Ours`, `Theirs`, `Output`).

use crate::merge_buffers::ThreeWayMergeDocument;
use crate::traits::{
    ConflictedFileInputs, DiffMergeError, DiffMergeResult, MAX_TEXT_DIFF_BYTES, MergeConflictEngine,
};
use branvio_core::domain::{ConflictLinePick, ConflictRegion, ConflictResolution, ConflictSide};
use compact_str::CompactString;
use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};
use ropey::Rope;
use std::ops::Range;

/// Production [`MergeConflictEngine`] operating on four synchronized `ropey::Rope` buffers.
#[derive(Debug, Default, Clone, Copy)]
pub struct RopeConflictEngine;

impl RopeConflictEngine {
    /// Creates a new [`RopeConflictEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

struct ExactLineTokenSource<'a, T>(&'a [T]);

struct ExactLineTokenIter<'a, T>(std::slice::Iter<'a, T>);

impl<'a, T: AsRef<str>> Iterator for ExactLineTokenIter<'a, T> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|s| s.as_ref().as_bytes())
    }
}

impl<'a, T: AsRef<str>> TokenSource for ExactLineTokenSource<'a, T> {
    type Token = &'a [u8];
    type Tokenizer = ExactLineTokenIter<'a, T>;

    #[inline]
    fn tokenize(&self) -> Self::Tokenizer {
        ExactLineTokenIter(self.0.iter())
    }

    #[inline]
    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

struct TrimEolTokenSource<'a, T>(&'a [T]);

struct TrimEolTokenIter<'a, T>(std::slice::Iter<'a, T>);

impl<'a, T: AsRef<str>> Iterator for TrimEolTokenIter<'a, T> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|s| trim_line_eol(s.as_ref()).as_bytes())
    }
}

impl<'a, T: AsRef<str>> TokenSource for TrimEolTokenSource<'a, T> {
    type Token = &'a [u8];
    type Tokenizer = TrimEolTokenIter<'a, T>;

    #[inline]
    fn tokenize(&self) -> Self::Tokenizer {
        TrimEolTokenIter(self.0.iter())
    }

    #[inline]
    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone)]
struct ParsedMarkerBlock {
    output_start_line: usize,
    output_end_line_excl: usize,
    ours_content: Range<usize>,
    base_content: Option<Range<usize>>,
    theirs_content: Range<usize>,
    ours_label: CompactString,
    base_label: Option<CompactString>,
    theirs_label: CompactString,
}

struct StageMatchHints<'a> {
    base: Option<&'a [&'a str]>,
    ours: &'a [&'a str],
    theirs: &'a [&'a str],
}

#[derive(Debug, Clone, Copy, Default)]
struct EolStats {
    crlf: usize,
    lf: usize,
}

impl EolStats {
    #[inline]
    fn add(self, other: Self) -> Self {
        Self {
            crlf: self.crlf + other.crlf,
            lf: self.lf + other.lf,
        }
    }

    #[inline]
    fn total(self) -> usize {
        self.crlf + self.lf
    }

    #[inline]
    fn prefers_crlf(self) -> bool {
        self.crlf > self.lf
    }

    #[inline]
    fn is_pure_crlf(self) -> bool {
        self.crlf > 0 && self.lf == 0
    }

    #[inline]
    fn is_pure_lf(self) -> bool {
        self.lf > 0 && self.crlf == 0
    }
}

impl MergeConflictEngine for RopeConflictEngine {
    fn open_conflicted_file(
        &self,
        inputs: ConflictedFileInputs<'_>,
    ) -> DiffMergeResult<ThreeWayMergeDocument> {
        let path_str = inputs.path.as_str();
        check_input_size(path_str, inputs.ours_bytes)?;
        check_input_size(path_str, inputs.theirs_bytes)?;
        if let Some(base) = inputs.base_bytes {
            check_input_size(path_str, base)?;
        }
        if let Some(wt) = inputs.worktree_with_markers {
            check_input_size(path_str, wt)?;
        }

        if looks_binary(inputs.ours_bytes)
            || looks_binary(inputs.theirs_bytes)
            || inputs.base_bytes.is_some_and(looks_binary)
            || inputs.worktree_with_markers.is_some_and(looks_binary)
        {
            return Err(DiffMergeError::BinaryFileUnsupported(path_str.to_string()));
        }

        let ours_str = decode_utf8(path_str, inputs.ours_bytes)?;
        let theirs_str = decode_utf8(path_str, inputs.theirs_bytes)?;
        let base_str = inputs
            .base_bytes
            .map(|b| decode_utf8(path_str, b))
            .transpose()?;
        let wt_str = inputs
            .worktree_with_markers
            .map(|b| decode_utf8(path_str, b))
            .transpose()?;

        let stage_stats = count_eol_stats(ours_str)
            .add(count_eol_stats(theirs_str))
            .add(base_str.map_or(EolStats::default(), count_eol_stats));

        if let Some(wt) = wt_str {
            let wt_stats = count_eol_stats(wt);
            let use_crlf = if wt_stats.total() > 0 {
                wt_stats.prefers_crlf()
            } else {
                stage_stats.prefers_crlf()
            };
            let adapt_lf_to_crlf = wt_stats.is_pure_crlf() && stage_stats.is_pure_lf();
            return open_from_worktree_markers(
                inputs.path,
                inputs.ours_label,
                inputs.theirs_label,
                base_str,
                ours_str,
                theirs_str,
                wt,
                use_crlf,
                adapt_lf_to_crlf,
            );
        }

        let use_crlf = stage_stats.prefers_crlf();
        Ok(synthesize_from_three_stages(
            inputs.path,
            inputs.ours_label,
            inputs.theirs_label,
            base_str,
            ours_str,
            theirs_str,
            use_crlf,
        ))
    }

    fn resolve_region(
        &self,
        doc: &mut ThreeWayMergeDocument,
        region_id: u32,
        resolution: ConflictResolution,
    ) -> DiffMergeResult<()> {
        let Some(region_idx) = doc.regions.iter().position(|r| r.region_id == region_id) else {
            return Err(DiffMergeError::UnknownConflictRegion(region_id));
        };

        let region_snapshot = doc.regions[region_idx].clone();
        let out_total_lines = logical_line_count(&doc.output);
        if region_snapshot.output_line_range.start > region_snapshot.output_line_range.end
            || (region_snapshot.output_line_range.end as usize) > out_total_lines
        {
            return Err(DiffMergeError::InvalidSelection {
                hunk_index: region_id,
                reason: "conflict region output range out of bounds",
            });
        }

        let (mut replacement_text, is_resolved) =
            build_resolution_text(doc, &region_snapshot, &resolution)?;

        let old_range = region_snapshot.output_line_range;
        let start_line = old_range.start as usize;
        let end_line = old_range.end as usize;
        let start_char = doc.output.line_to_char(start_line);
        let end_char = doc.output.line_to_char(end_line);

        // If replacing inside the middle of the document (before remaining lines), ensure the
        // spliced replacement terminates with a line break so it does not concatenate onto the next line.
        let ends_before_eof = end_char < doc.output.len_chars();
        if ends_before_eof && !replacement_text.is_empty() && !replacement_text.ends_with('\n') {
            replacement_text.push_str(doc.line_ending);
        }

        doc.output.remove(start_char..end_char);
        doc.output.insert(start_char, &replacement_text);

        let new_line_count = count_spanned_lines(&replacement_text, ends_before_eof) as u32;
        let old_line_count = old_range.end - old_range.start;
        let line_delta = (new_line_count as i64) - (old_line_count as i64);

        let region = &mut doc.regions[region_idx];
        region.output_line_range = old_range.start..(old_range.start + new_line_count);
        region.resolution = resolution;
        region.is_resolved =
            is_resolved && !slice_has_marker_lines(&doc.output, region.output_line_range.clone());

        shift_downstream_regions(&mut doc.regions[region_idx + 1..], line_delta);
        Ok(())
    }

    fn edit_output_buffer(
        &mut self,
        doc: &mut ThreeWayMergeDocument,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) -> DiffMergeResult<()> {
        if byte_range.start > byte_range.end || byte_range.end > doc.output.len_bytes() {
            return Err(DiffMergeError::InvalidSelection {
                hunk_index: 0,
                reason: "edit byte range out of bounds for output buffer",
            });
        }

        let start_char = doc.output.try_byte_to_char(byte_range.start).map_err(|_| {
            DiffMergeError::InvalidSelection {
                hunk_index: 0,
                reason: "start byte offset out of bounds",
            }
        })?;
        let end_char = doc.output.try_byte_to_char(byte_range.end).map_err(|_| {
            DiffMergeError::InvalidSelection {
                hunk_index: 0,
                reason: "end byte offset out of bounds",
            }
        })?;
        if doc.output.char_to_byte(start_char) != byte_range.start
            || doc.output.char_to_byte(end_char) != byte_range.end
        {
            return Err(DiffMergeError::InvalidSelection {
                hunk_index: 0,
                reason: "edit byte range must align to UTF-8 character boundaries",
            });
        }

        let total_lines_before = logical_line_count(&doc.output);
        let orig_char_ranges: Vec<(usize, usize)> = doc
            .regions
            .iter()
            .map(|r| {
                let s_ln = (r.output_line_range.start as usize).min(total_lines_before);
                let e_ln = (r.output_line_range.end as usize).min(total_lines_before);
                (doc.output.line_to_char(s_ln), doc.output.line_to_char(e_ln))
            })
            .collect();

        let rem_chars = end_char - start_char;
        let ins_chars = replacement_utf8.chars().count();

        doc.output.remove(start_char..end_char);
        doc.output.insert(start_char, replacement_utf8);

        for (region, (orig_start, orig_end)) in doc.regions.iter_mut().zip(orig_char_ranges) {
            if rem_chars == 0 {
                // Pure insertion at `start_char`.
                if start_char <= orig_start {
                    let ns_char = orig_start + ins_chars;
                    let ne_char = orig_end + ins_chars;
                    region.output_line_range =
                        char_span_to_line_range(&doc.output, ns_char, ne_char);
                } else if start_char >= orig_end {
                    // Strictly after the region: unchanged.
                } else {
                    // Strictly inside the non-empty conflict region.
                    let ns_char = orig_start;
                    let ne_char = orig_end + ins_chars;
                    let new_range = char_span_to_line_range(&doc.output, ns_char, ne_char);
                    region.output_line_range = new_range.clone();
                    region.resolution = ConflictResolution::ManualEdit;
                    region.is_resolved = !slice_has_marker_lines(&doc.output, new_range);
                }
            } else if end_char <= orig_start {
                // Strictly above the region: shift without altering resolution status.
                let ns_char = orig_start + ins_chars - rem_chars;
                let ne_char = orig_end + ins_chars - rem_chars;
                region.output_line_range = char_span_to_line_range(&doc.output, ns_char, ne_char);
            } else if start_char >= orig_end {
                // Strictly below the region: unchanged.
            } else {
                // Overlaps this conflict region. Clamp cross-boundary replacements so user text
                // written above or below the conflict is never absorbed into `output_line_range`.
                let ns_char = if start_char >= orig_start {
                    orig_start
                } else {
                    start_char + ins_chars
                };
                let ne_char = if end_char <= orig_end {
                    orig_end + ins_chars - rem_chars
                } else {
                    start_char
                }
                .max(ns_char);

                let new_range = char_span_to_line_range(&doc.output, ns_char, ne_char);
                region.output_line_range = new_range.clone();
                region.resolution = ConflictResolution::ManualEdit;
                region.is_resolved = !slice_has_marker_lines(&doc.output, new_range);
            }
        }

        Ok(())
    }

    fn export_merged_bytes(
        &self,
        doc: &ThreeWayMergeDocument,
        require_all_resolved: bool,
    ) -> DiffMergeResult<Vec<u8>> {
        if require_all_resolved {
            let unresolved_regions = doc.unresolved_count();
            let marker_regions = self.scan_remaining_markers(doc).len();
            let remaining = unresolved_regions.max(marker_regions);
            if remaining > 0 {
                return Err(DiffMergeError::UnresolvedConflictsRemaining { remaining });
            }
        }

        let mut out = Vec::with_capacity(doc.output.len_bytes());
        for chunk in doc.output.chunks() {
            out.extend_from_slice(chunk.as_bytes());
        }
        Ok(out)
    }

    fn scan_remaining_markers(&self, doc: &ThreeWayMergeDocument) -> Vec<ConflictRegion> {
        let mut raw = String::with_capacity(doc.output.len_bytes());
        for chunk in doc.output.chunks() {
            raw.push_str(chunk);
        }
        let lines = split_lines_preserve_eol(&raw);
        let parsed = parse_conflict_blocks(&lines, None);
        let mut covered_lines = vec![false; lines.len()];
        let mut results = Vec::new();

        for (idx, block) in parsed.iter().enumerate() {
            for line_slot in &mut covered_lines[block.output_start_line..block.output_end_line_excl]
            {
                *line_slot = true;
            }
            results.push(ConflictRegion {
                region_id: idx as u32,
                base_line_range: block
                    .base_content
                    .as_ref()
                    .map(|r| (r.start as u32)..(r.end as u32)),
                ours_line_range: (block.ours_content.start as u32)..(block.ours_content.end as u32),
                theirs_line_range: (block.theirs_content.start as u32)
                    ..(block.theirs_content.end as u32),
                output_line_range: (block.output_start_line as u32)
                    ..(block.output_end_line_excl as u32),
                ours_label: block.ours_label.clone(),
                base_label: block.base_label.clone(),
                theirs_label: block.theirs_label.clone(),
                resolution: ConflictResolution::Unresolved,
                is_resolved: false,
            });
        }

        // Only detect stray opening/base/closing conflict markers outside well-formed blocks.
        // Standalone `=======` lines (e.g., Markdown/reST headings) outside blocks are ignored.
        for (line_idx, line) in lines.iter().enumerate() {
            if !covered_lines[line_idx] && is_stray_outer_marker(line) {
                let next_id = u32::try_from(results.len()).unwrap_or(u32::MAX);
                results.push(ConflictRegion {
                    region_id: next_id,
                    base_line_range: None,
                    ours_line_range: 0..0,
                    theirs_line_range: 0..0,
                    output_line_range: (line_idx as u32)..((line_idx + 1) as u32),
                    ours_label: CompactString::const_new("stray-marker"),
                    base_label: None,
                    theirs_label: CompactString::const_new("stray-marker"),
                    resolution: ConflictResolution::Unresolved,
                    is_resolved: false,
                });
            }
        }

        results
    }
}

#[inline]
fn check_input_size(path: &str, bytes: &[u8]) -> DiffMergeResult<()> {
    if bytes.len() > MAX_TEXT_DIFF_BYTES {
        return Err(DiffMergeError::InputTooLarge {
            path: path.to_string(),
            byte_len: bytes.len(),
            max_bytes: MAX_TEXT_DIFF_BYTES,
        });
    }
    Ok(())
}

#[inline]
fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

#[inline]
fn decode_utf8<'a>(path: &str, bytes: &'a [u8]) -> DiffMergeResult<&'a str> {
    std::str::from_utf8(bytes).map_err(|_| DiffMergeError::UnsupportedEncoding {
        path: path.to_string(),
    })
}

fn count_eol_stats(text: &str) -> EolStats {
    let bytes = text.as_bytes();
    let mut stats = EolStats::default();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if idx > 0 && bytes[idx - 1] == b'\r' {
                stats.crlf += 1;
            } else {
                stats.lf += 1;
            }
        }
    }
    stats
}

#[inline]
fn trim_line_eol(line: &str) -> &str {
    let no_lf = line.strip_suffix('\n').unwrap_or(line);
    no_lf.strip_suffix('\r').unwrap_or(no_lf)
}

fn split_lines_preserve_eol(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        if let Some(rel_nl) = text[start..].find('\n') {
            let end = start + rel_nl + 1;
            lines.push(&text[start..end]);
            start = end;
        } else {
            lines.push(&text[start..]);
            break;
        }
    }
    lines
}

fn parse_ours_marker(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("<<<<<<<")?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ').map(str::trim)
    }
}

fn parse_base_marker(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("|||||||")?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ').map(str::trim)
    }
}

#[inline]
fn is_separator_marker(trimmed: &str) -> bool {
    trimmed == "======="
}

fn parse_theirs_marker(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix(">>>>>>>")?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix(' ').map(str::trim)
    }
}

#[inline]
fn is_stray_outer_marker(line: &str) -> bool {
    let trimmed = trim_line_eol(line);
    parse_ours_marker(trimmed).is_some()
        || parse_base_marker(trimmed).is_some()
        || parse_theirs_marker(trimmed).is_some()
}

fn contains_trimmed_subslice(haystack: &[&str], needle: &[&str]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|win| {
        win.iter()
            .zip(needle.iter())
            .all(|(h, n)| trim_line_eol(h) == trim_line_eol(n))
    })
}

fn select_separator_line(
    lines: &[&str],
    ours_start: usize,
    pipe_line: Option<usize>,
    end_line: usize,
    sep_candidates: &[usize],
    hints: Option<&StageMatchHints<'_>>,
) -> usize {
    if sep_candidates.len() == 1 {
        return sep_candidates[0];
    }

    if let Some(hints) = hints {
        let mut best_sep = sep_candidates[0];
        let mut best_score = -1isize;
        for &sep in sep_candidates {
            let mut score = 0isize;
            if let Some(p) = pipe_line {
                let cand_base = &lines[(p + 1)..sep];
                if let Some(base_stage) = hints.base
                    && contains_trimmed_subslice(base_stage, cand_base)
                {
                    score += 2 + cand_base.len() as isize;
                }
            } else {
                let cand_ours = &lines[ours_start..sep];
                if contains_trimmed_subslice(hints.ours, cand_ours) {
                    score += 2 + cand_ours.len() as isize;
                }
            }
            let cand_theirs = &lines[(sep + 1)..end_line];
            if contains_trimmed_subslice(hints.theirs, cand_theirs) {
                score += 2 + cand_theirs.len() as isize;
            }
            if score > best_score {
                best_score = score;
                best_sep = sep;
            }
        }
        return best_sep;
    }

    sep_candidates[0]
}

fn parse_conflict_blocks(
    lines: &[&str],
    hints: Option<&StageMatchHints<'_>>,
) -> Vec<ParsedMarkerBlock> {
    let mut blocks = Vec::new();
    let mut idx = 0usize;

    while idx < lines.len() {
        let trimmed = trim_line_eol(lines[idx]);
        let Some(ours_label_raw) = parse_ours_marker(trimmed) else {
            idx += 1;
            continue;
        };

        let ours_label = if ours_label_raw.is_empty() {
            CompactString::const_new("HEAD")
        } else {
            CompactString::from(ours_label_raw)
        };

        let start_line = idx;
        let mut end_marker_line = None;
        let mut theirs_label = CompactString::const_new("incoming");
        let mut nested_ours = false;

        for (cursor, line) in lines.iter().enumerate().skip(start_line + 1) {
            let cur_trimmed = trim_line_eol(line);
            if parse_ours_marker(cur_trimmed).is_some() {
                nested_ours = true;
                break;
            }
            if let Some(lbl) = parse_theirs_marker(cur_trimmed) {
                if !lbl.is_empty() {
                    theirs_label = CompactString::from(lbl);
                }
                end_marker_line = Some(cursor);
                break;
            }
        }

        let Some(end_line) = (!nested_ours).then_some(end_marker_line).flatten() else {
            idx += 1;
            continue;
        };

        let ours_start = start_line + 1;
        let mut pipe_line = None;
        let mut base_label = None;
        for (cursor, line) in lines[..end_line].iter().enumerate().skip(ours_start) {
            let cur_trimmed = trim_line_eol(line);
            if let Some(lbl) = parse_base_marker(cur_trimmed) {
                pipe_line = Some(cursor);
                base_label = Some(if lbl.is_empty() {
                    CompactString::const_new("base")
                } else {
                    CompactString::from(lbl)
                });
                break;
            }
        }

        // In diff3 blocks (`<<<<<<< ... ||||||| ... ======= ... >>>>>>>`), any `=======` before
        // `|||||||` belongs to `ours` (e.g., Markdown heading underline) and cannot be the separator.
        let sep_search_start = pipe_line.map_or(ours_start, |p| p + 1);
        let sep_candidates: Vec<usize> = (sep_search_start..end_line)
            .filter(|&cursor| is_separator_marker(trim_line_eol(lines[cursor])))
            .collect();

        if sep_candidates.is_empty() {
            idx += 1;
            continue;
        }

        let sep_line = select_separator_line(
            lines,
            ours_start,
            pipe_line,
            end_line,
            &sep_candidates,
            hints,
        );

        let (ours_content, base_content) = if let Some(p) = pipe_line {
            (ours_start..p, Some((p + 1)..sep_line))
        } else {
            (ours_start..sep_line, None)
        };

        blocks.push(ParsedMarkerBlock {
            output_start_line: start_line,
            output_end_line_excl: end_line + 1,
            ours_content,
            base_content,
            theirs_content: (sep_line + 1)..end_line,
            ours_label,
            base_label,
            theirs_label,
        });
        idx = end_line + 1;
    }

    blocks
}

#[allow(clippy::too_many_arguments)]
fn open_from_worktree_markers(
    path: branvio_core::paths::RepoRelPath,
    ours_label: &str,
    theirs_label: &str,
    base_str: Option<&str>,
    ours_str: &str,
    theirs_str: &str,
    wt_str: &str,
    use_crlf: bool,
    adapt_lf_to_crlf: bool,
) -> DiffMergeResult<ThreeWayMergeDocument> {
    let wt_lines = split_lines_preserve_eol(wt_str);
    let base_actual = base_str.map(split_lines_preserve_eol);
    let ours_actual = split_lines_preserve_eol(ours_str);
    let theirs_actual = split_lines_preserve_eol(theirs_str);

    let synth_from_markers = ours_str.is_empty() && theirs_str.is_empty();
    let hints = (!synth_from_markers).then_some(StageMatchHints {
        base: base_actual.as_deref(),
        ours: &ours_actual,
        theirs: &theirs_actual,
    });
    let blocks = parse_conflict_blocks(&wt_lines, hints.as_ref());

    // Fast path: when stage inputs are provided and 3-way conflict synthesis matches every parsed
    // worktree block 1:1 (ignoring EOL differences), use exact stage coordinates directly.
    let exact_stage_coords = (!synth_from_markers && !blocks.is_empty())
        .then(|| {
            base_actual.as_deref().and_then(|b_lines| {
                let stage_coords =
                    compute_three_way_stage_regions(b_lines, &ours_actual, &theirs_actual);
                if stage_coords.len() == blocks.len()
                    && stage_coords
                        .iter()
                        .zip(blocks.iter())
                        .all(|((_br, or, tr), blk)| {
                            slices_eq_trim_eol(
                                &ours_actual[or.start..or.end],
                                &wt_lines[blk.ours_content.clone()],
                            ) && slices_eq_trim_eol(
                                &theirs_actual[tr.start..tr.end],
                                &wt_lines[blk.theirs_content.clone()],
                            )
                        })
                {
                    Some(stage_coords)
                } else {
                    None
                }
            })
        })
        .flatten();

    let (proj_base_str, proj_base_ranges) = project_wt_side(&wt_lines, &blocks, ConflictSide::Base);
    let (proj_ours_str, proj_ours_ranges) = project_wt_side(&wt_lines, &blocks, ConflictSide::Ours);
    let (proj_theirs_str, proj_theirs_ranges) =
        project_wt_side(&wt_lines, &blocks, ConflictSide::Theirs);

    let (base_rope, base_ranges): (Rope, Vec<Option<Range<u32>>>) = if let Some(b_text) = base_str {
        let actual_lines = base_actual.as_deref().unwrap_or(&[]);
        let mapped = if let Some(ref exact) = exact_stage_coords {
            exact
                .iter()
                .map(|(br, _, _)| Some((br.start as u32)..(br.end as u32)))
                .collect()
        } else {
            let proj_lines = split_lines_preserve_eol(&proj_base_str);
            let has_any_base = blocks.iter().any(|b| b.base_content.is_some());
            if has_any_base {
                align_projected_ranges(&proj_lines, actual_lines, &proj_base_ranges)
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| blocks[i].base_content.as_ref().map(|_| r))
                    .collect()
            } else {
                align_projected_ranges(&proj_lines, actual_lines, &proj_base_ranges)
                    .into_iter()
                    .map(Some)
                    .collect()
            }
        };
        (Rope::from_str(b_text), mapped)
    } else if synth_from_markers && blocks.iter().any(|b| b.base_content.is_some()) {
        let mapped = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| b.base_content.as_ref().map(|_| proj_base_ranges[i].clone()))
            .collect();
        (Rope::from_str(&proj_base_str), mapped)
    } else {
        (Rope::new(), vec![None; blocks.len()])
    };

    let (ours_rope, ours_ranges): (Rope, Vec<Range<u32>>) =
        if synth_from_markers && !blocks.is_empty() {
            (Rope::from_str(&proj_ours_str), proj_ours_ranges)
        } else if let Some(ref exact) = exact_stage_coords {
            (
                Rope::from_str(ours_str),
                exact
                    .iter()
                    .map(|(_, or, _)| (or.start as u32)..(or.end as u32))
                    .collect(),
            )
        } else {
            let proj_lines = split_lines_preserve_eol(&proj_ours_str);
            let mapped = align_projected_ranges(&proj_lines, &ours_actual, &proj_ours_ranges);
            (Rope::from_str(ours_str), mapped)
        };

    let (theirs_rope, theirs_ranges): (Rope, Vec<Range<u32>>) =
        if synth_from_markers && !blocks.is_empty() {
            (Rope::from_str(&proj_theirs_str), proj_theirs_ranges)
        } else if let Some(ref exact) = exact_stage_coords {
            (
                Rope::from_str(theirs_str),
                exact
                    .iter()
                    .map(|(_, _, tr)| (tr.start as u32)..(tr.end as u32))
                    .collect(),
            )
        } else {
            let proj_lines = split_lines_preserve_eol(&proj_theirs_str);
            let mapped = align_projected_ranges(&proj_lines, &theirs_actual, &proj_theirs_ranges);
            (Rope::from_str(theirs_str), mapped)
        };

    let mut regions = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.into_iter().enumerate() {
        let ours_lbl = if block.ours_label.as_str() == "HEAD" && !ours_label.is_empty() {
            CompactString::from(ours_label)
        } else {
            block.ours_label
        };
        let theirs_lbl = if block.theirs_label.as_str() == "incoming" && !theirs_label.is_empty() {
            CompactString::from(theirs_label)
        } else {
            block.theirs_label
        };
        regions.push(ConflictRegion {
            region_id: idx as u32,
            base_line_range: base_ranges[idx].clone(),
            ours_line_range: ours_ranges[idx].clone(),
            theirs_line_range: theirs_ranges[idx].clone(),
            output_line_range: (block.output_start_line as u32)
                ..(block.output_end_line_excl as u32),
            ours_label: ours_lbl,
            base_label: block.base_label,
            theirs_label: theirs_lbl,
            resolution: ConflictResolution::Unresolved,
            is_resolved: false,
        });
    }

    Ok(ThreeWayMergeDocument::new(
        path,
        base_rope,
        ours_rope,
        theirs_rope,
        Rope::from_str(wt_str),
        regions,
        use_crlf,
    )
    .with_crlf_stage_adaptation(adapt_lf_to_crlf))
}

#[inline]
fn slices_eq_trim_eol(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| trim_line_eol(x) == trim_line_eol(y))
}

fn project_wt_side(
    wt_lines: &[&str],
    blocks: &[ParsedMarkerBlock],
    side: ConflictSide,
) -> (String, Vec<Range<u32>>) {
    let mut out = String::new();
    let mut ranges = Vec::with_capacity(blocks.len());
    let mut line_cursor = 0usize;
    let mut proj_line = 0u32;

    for block in blocks {
        while line_cursor < block.output_start_line {
            out.push_str(wt_lines[line_cursor]);
            line_cursor += 1;
            proj_line += 1;
        }
        let side_slice = match side {
            ConflictSide::Base => block.base_content.clone().unwrap_or(0..0),
            ConflictSide::Ours => block.ours_content.clone(),
            ConflictSide::Theirs => block.theirs_content.clone(),
        };
        let r_start = proj_line;
        for idx in side_slice {
            out.push_str(wt_lines[idx]);
            proj_line += 1;
        }
        ranges.push(r_start..proj_line);
        line_cursor = block.output_end_line_excl;
    }

    while line_cursor < wt_lines.len() {
        out.push_str(wt_lines[line_cursor]);
        line_cursor += 1;
    }

    (out, ranges)
}

fn align_projected_ranges(
    projected: &[&str],
    actual: &[&str],
    proj_ranges: &[Range<u32>],
) -> Vec<Range<u32>> {
    if proj_ranges.is_empty() {
        return Vec::new();
    }
    let input = InternedInput::new(TrimEolTokenSource(projected), TrimEolTokenSource(actual));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let mut before_map = vec![0u32; projected.len() + 1];
    let mut after_map = vec![0u32; projected.len() + 1];
    let mut p = 0usize;
    let mut a = 0usize;
    for hunk in diff.hunks() {
        while p < hunk.before.start as usize {
            p += 1;
            a += 1;
            before_map[p] = a as u32;
            after_map[p] = a as u32;
        }
        if hunk.before.is_empty() {
            // Pure insertion at `p` in `actual`: `before_map[p]` stays before inserted base hunk,
            // while `after_map[p]` advances past it so zero-length 2-way base projections expand!
            before_map[p] = a as u32;
            a = hunk.after.end as usize;
            after_map[p] = a as u32;
        } else {
            before_map[p] = a as u32;
            let hunk_len = hunk.after.len();
            while p < hunk.before.end as usize {
                p += 1;
                let rel = (p - hunk.before.start as usize).min(hunk_len);
                let pos = (hunk.after.start as usize + rel) as u32;
                before_map[p] = pos;
                after_map[p] = pos;
            }
            a = hunk.after.end as usize;
            before_map[p] = a as u32;
            after_map[p] = a as u32;
        }
    }
    while p < projected.len() {
        p += 1;
        a += 1;
        before_map[p] = a as u32;
        after_map[p] = a as u32;
    }

    let max_actual = actual.len() as u32;
    proj_ranges
        .iter()
        .map(|r| {
            let s = before_map[(r.start as usize).min(projected.len())].min(max_actual);
            let e = after_map[(r.end as usize).min(projected.len())]
                .min(max_actual)
                .max(s);
            s..e
        })
        .collect()
}

enum StageMergeSegment {
    /// Identical base slice copied cleanly to all sides (`base[base_range]`).
    CommonBase(Range<usize>),
    /// Uncontested change on `ours` (`ours[range]`).
    CleanOurs(Range<usize>),
    /// Uncontested change on `theirs` (`theirs[range]`).
    CleanTheirs(Range<usize>),
    /// Divergent conflict across `(base, ours, theirs)` line ranges.
    Conflict {
        base: Range<usize>,
        ours: Range<usize>,
        theirs: Range<usize>,
    },
}

fn compute_three_way_merge_segments(
    base_lines: &[&str],
    ours_lines: &[&str],
    theirs_lines: &[&str],
) -> Vec<StageMergeSegment> {
    let in_o = InternedInput::new(
        ExactLineTokenSource(base_lines),
        ExactLineTokenSource(ours_lines),
    );
    let in_t = InternedInput::new(
        ExactLineTokenSource(base_lines),
        ExactLineTokenSource(theirs_lines),
    );
    let mut diff_o = Diff::compute(Algorithm::Myers, &in_o);
    let mut diff_t = Diff::compute(Algorithm::Myers, &in_t);
    // `git merge-file` (`xdiff/xmerge.c`) compacts sliders downward without `XDF_INDENT_HEURISTIC`.
    diff_o.postprocess_no_heuristic(&in_o);
    diff_t.postprocess_no_heuristic(&in_t);
    let hunks_o: Vec<imara_diff::Hunk> = diff_o.hunks().collect();
    let hunks_t: Vec<imara_diff::Hunk> = diff_t.hunks().collect();

    let mut io = 0usize;
    let mut it = 0usize;
    let mut b_pos = 0usize;
    let mut o_pos = 0usize;
    let mut t_pos = 0usize;
    let mut segments = Vec::new();

    loop {
        let next_b = hunks_o
            .get(io)
            .map_or(base_lines.len(), |h| h.before.start as usize)
            .min(
                hunks_t
                    .get(it)
                    .map_or(base_lines.len(), |h| h.before.start as usize),
            );
        if b_pos < next_b && b_pos < base_lines.len() {
            let end_b = next_b.min(base_lines.len());
            let skip = end_b - b_pos;
            segments.push(StageMergeSegment::CommonBase(b_pos..end_b));
            b_pos = end_b;
            o_pos += skip;
            t_pos += skip;
        }
        if io >= hunks_o.len() && it >= hunks_t.len() {
            break;
        }

        let b_lo = b_pos;
        let o_lo = o_pos;
        let t_lo = t_pos;
        let mut b_hi = b_lo;
        let mut last_b_end_o = b_lo;
        let mut last_o_end = o_lo;
        let mut last_b_end_t = b_lo;
        let mut last_t_end = t_lo;
        let mut took_o = false;
        let mut took_t = false;

        loop {
            // `s <= b_hi` (rather than `< b_hi`) matches `git merge-file --diff3` by treating
            // adjacent edits (e.g., ours modifying line N and theirs modifying line N+1) as conflicts.
            let take_o = hunks_o
                .get(io)
                .is_some_and(|h| (h.before.start as usize) <= b_hi);
            let take_t = hunks_t
                .get(it)
                .is_some_and(|h| (h.before.start as usize) <= b_hi);
            if !take_o && !take_t {
                break;
            }
            if take_o {
                let h = &hunks_o[io];
                io += 1;
                took_o = true;
                b_hi = b_hi.max(h.before.end as usize);
                last_b_end_o = h.before.end as usize;
                last_o_end = h.after.end as usize;
            }
            if take_t {
                let h = &hunks_t[it];
                it += 1;
                took_t = true;
                b_hi = b_hi.max(h.before.end as usize);
                last_b_end_t = h.before.end as usize;
                last_t_end = h.after.end as usize;
            }
        }

        let o_hi = last_o_end + (b_hi - last_b_end_o);
        let t_hi = last_t_end + (b_hi - last_b_end_t);

        if took_o && !took_t {
            segments.push(StageMergeSegment::CleanOurs(o_lo..o_hi));
        } else if took_t && !took_o {
            segments.push(StageMergeSegment::CleanTheirs(t_lo..t_hi));
        } else if ours_lines[o_lo..o_hi] == theirs_lines[t_lo..t_hi] {
            segments.push(StageMergeSegment::CleanOurs(o_lo..o_hi));
        } else {
            segments.push(StageMergeSegment::Conflict {
                base: b_lo..b_hi,
                ours: o_lo..o_hi,
                theirs: t_lo..t_hi,
            });
        }

        b_pos = b_hi;
        o_pos = o_hi;
        t_pos = t_hi;
    }

    segments
}

fn compute_three_way_stage_regions(
    base_lines: &[&str],
    ours_lines: &[&str],
    theirs_lines: &[&str],
) -> Vec<(Range<usize>, Range<usize>, Range<usize>)> {
    compute_three_way_merge_segments(base_lines, ours_lines, theirs_lines)
        .into_iter()
        .filter_map(|seg| match seg {
            StageMergeSegment::Conflict { base, ours, theirs } => Some((base, ours, theirs)),
            _ => None,
        })
        .collect()
}

fn synthesize_from_three_stages(
    path: branvio_core::paths::RepoRelPath,
    ours_label: &str,
    theirs_label: &str,
    base_str: Option<&str>,
    ours_str: &str,
    theirs_str: &str,
    use_crlf: bool,
) -> ThreeWayMergeDocument {
    let marker_eol = if use_crlf { "\r\n" } else { "\n" };
    let ours_lines = split_lines_preserve_eol(ours_str);
    let theirs_lines = split_lines_preserve_eol(theirs_str);

    let mut output = String::with_capacity(ours_str.len().max(theirs_str.len()) + 256);
    let mut regions = Vec::new();
    let mut out_line = 0u32;

    if let Some(base) = base_str {
        let base_lines = split_lines_preserve_eol(base);
        for seg in compute_three_way_merge_segments(&base_lines, &ours_lines, &theirs_lines) {
            match seg {
                StageMergeSegment::CommonBase(range) => {
                    for line in &base_lines[range] {
                        push_output_line(&mut output, line, &mut out_line);
                    }
                }
                StageMergeSegment::CleanOurs(range) => {
                    for line in &ours_lines[range] {
                        push_output_line(&mut output, line, &mut out_line);
                    }
                }
                StageMergeSegment::CleanTheirs(range) => {
                    for line in &theirs_lines[range] {
                        push_output_line(&mut output, line, &mut out_line);
                    }
                }
                StageMergeSegment::Conflict {
                    base: b_range,
                    ours: o_range,
                    theirs: t_range,
                } => {
                    let region_id = u32::try_from(regions.len()).unwrap_or(u32::MAX);
                    let out_start = out_line;
                    emit_diff3_markers(
                        &mut output,
                        &mut out_line,
                        marker_eol,
                        ours_label,
                        &ours_lines[o_range.clone()],
                        Some("base"),
                        Some(&base_lines[b_range.clone()]),
                        theirs_label,
                        &theirs_lines[t_range.clone()],
                    );
                    regions.push(ConflictRegion {
                        region_id,
                        base_line_range: Some((b_range.start as u32)..(b_range.end as u32)),
                        ours_line_range: (o_range.start as u32)..(o_range.end as u32),
                        theirs_line_range: (t_range.start as u32)..(t_range.end as u32),
                        output_line_range: out_start..out_line,
                        ours_label: CompactString::from(ours_label),
                        base_label: Some(CompactString::const_new("base")),
                        theirs_label: CompactString::from(theirs_label),
                        resolution: ConflictResolution::Unresolved,
                        is_resolved: false,
                    });
                }
            }
        }
    } else {
        // Two-way conflict without common ancestor (`BothAdded`).
        let input = InternedInput::new(
            ExactLineTokenSource(&ours_lines),
            ExactLineTokenSource(&theirs_lines),
        );
        let mut diff = Diff::compute(Algorithm::Myers, &input);
        diff.postprocess_no_heuristic(&input);
        let mut o_pos = 0usize;
        for hunk in diff.hunks() {
            while o_pos < hunk.before.start as usize {
                push_output_line(&mut output, ours_lines[o_pos], &mut out_line);
                o_pos += 1;
            }
            let o_lo = hunk.before.start as usize;
            let o_hi = hunk.before.end as usize;
            let t_lo = hunk.after.start as usize;
            let t_hi = hunk.after.end as usize;

            let region_id = u32::try_from(regions.len()).unwrap_or(u32::MAX);
            let out_start = out_line;
            emit_diff3_markers(
                &mut output,
                &mut out_line,
                marker_eol,
                ours_label,
                &ours_lines[o_lo..o_hi],
                None,
                None,
                theirs_label,
                &theirs_lines[t_lo..t_hi],
            );
            regions.push(ConflictRegion {
                region_id,
                base_line_range: None,
                ours_line_range: (o_lo as u32)..(o_hi as u32),
                theirs_line_range: (t_lo as u32)..(t_hi as u32),
                output_line_range: out_start..out_line,
                ours_label: CompactString::from(ours_label),
                base_label: None,
                theirs_label: CompactString::from(theirs_label),
                resolution: ConflictResolution::Unresolved,
                is_resolved: false,
            });
            o_pos = o_hi;
        }
        while o_pos < ours_lines.len() {
            push_output_line(&mut output, ours_lines[o_pos], &mut out_line);
            o_pos += 1;
        }
    }

    let base_rope = base_str.map_or_else(Rope::new, Rope::from_str);

    ThreeWayMergeDocument::new(
        path,
        base_rope,
        Rope::from_str(ours_str),
        Rope::from_str(theirs_str),
        Rope::from_str(&output),
        regions,
        use_crlf,
    )
}

#[inline]
fn push_output_line(out: &mut String, line: &str, out_line: &mut u32) {
    out.push_str(line);
    *out_line += 1;
}

#[allow(clippy::too_many_arguments)]
fn emit_diff3_markers(
    out: &mut String,
    out_line: &mut u32,
    marker_eol: &str,
    ours_label: &str,
    ours_lines: &[&str],
    base_label: Option<&str>,
    base_lines: Option<&[&str]>,
    theirs_label: &str,
    theirs_lines: &[&str],
) {
    out.push_str(&format!("<<<<<<< {ours_label}{marker_eol}"));
    *out_line += 1;
    for &l in ours_lines {
        out.push_str(l);
        if !l.ends_with('\n') {
            out.push_str(marker_eol);
        }
        *out_line += 1;
    }
    if let Some(b_lines) = base_lines {
        let lbl = base_label.unwrap_or("base");
        out.push_str(&format!("||||||| {lbl}{marker_eol}"));
        *out_line += 1;
        for &l in b_lines {
            out.push_str(l);
            if !l.ends_with('\n') {
                out.push_str(marker_eol);
            }
            *out_line += 1;
        }
    }
    out.push_str(&format!("======={marker_eol}"));
    *out_line += 1;
    for &l in theirs_lines {
        out.push_str(l);
        if !l.ends_with('\n') {
            out.push_str(marker_eol);
        }
        *out_line += 1;
    }
    out.push_str(&format!(">>>>>>> {theirs_label}{marker_eol}"));
    *out_line += 1;
}

fn format_stage_line_for_doc(line: &str, adapt_lf_to_crlf: bool) -> String {
    if adapt_lf_to_crlf
        && !line.ends_with("\r\n")
        && let Some(stem) = line.strip_suffix('\n')
    {
        return format!("{stem}\r\n");
    }
    line.to_string()
}

/// Joins resolved stage lines while preserving whether the final line terminated with a newline.
///
/// Only intermediate lines lacking a trailing newline receive `doc.line_ending` as a separator so
/// resolving a conflict at EOF preserves missing-trailing-newline fidelity.
fn join_lines_preserving_last_eol(lines: Vec<String>, doc: &ThreeWayMergeDocument) -> String {
    let len = lines.len();
    let mut s = String::new();
    for (idx, line) in lines.into_iter().enumerate() {
        let adapted = format_stage_line_for_doc(&line, doc.adapt_lf_to_crlf_on_resolve);
        s.push_str(&adapted);
        if idx + 1 < len && !adapted.ends_with('\n') {
            s.push_str(doc.line_ending);
        }
    }
    s
}

fn build_resolution_text(
    doc: &ThreeWayMergeDocument,
    region: &ConflictRegion,
    resolution: &ConflictResolution,
) -> DiffMergeResult<(String, bool)> {
    match resolution {
        ConflictResolution::TakeOurs => Ok((
            join_lines_preserving_last_eol(doc.extract_region_lines(region, true, false), doc),
            true,
        )),
        ConflictResolution::TakeTheirs => Ok((
            join_lines_preserving_last_eol(doc.extract_region_lines(region, false, false), doc),
            true,
        )),
        ConflictResolution::TakeBase => {
            if region.base_line_range.is_none() {
                return Err(DiffMergeError::InvalidSelection {
                    hunk_index: region.region_id,
                    reason: "cannot resolve with TakeBase when conflict region has no common ancestor",
                });
            }
            Ok((
                join_lines_preserving_last_eol(doc.extract_region_lines(region, false, true), doc),
                true,
            ))
        }
        ConflictResolution::TakeBothOursFirst => {
            let mut combined = doc.extract_region_lines(region, true, false);
            combined.extend(doc.extract_region_lines(region, false, false));
            Ok((join_lines_preserving_last_eol(combined, doc), true))
        }
        ConflictResolution::TakeBothTheirsFirst => {
            let mut combined = doc.extract_region_lines(region, false, false);
            combined.extend(doc.extract_region_lines(region, true, false));
            Ok((join_lines_preserving_last_eol(combined, doc), true))
        }
        ConflictResolution::LineLevelSelection(picks) => {
            let ours = doc.extract_region_lines(region, true, false);
            let theirs = doc.extract_region_lines(region, false, false);
            let has_base = region.base_line_range.is_some();
            let base = if has_base {
                doc.extract_region_lines(region, false, true)
            } else {
                Vec::new()
            };
            let mut chosen = Vec::with_capacity(picks.len());
            for &ConflictLinePick {
                side,
                relative_line_idx,
            } in picks
            {
                if side == ConflictSide::Base && !has_base {
                    return Err(DiffMergeError::InvalidSelection {
                        hunk_index: region.region_id,
                        reason: "cannot pick Base line when conflict region has no common ancestor",
                    });
                }
                let line = match side {
                    ConflictSide::Ours => ours.get(relative_line_idx as usize),
                    ConflictSide::Theirs => theirs.get(relative_line_idx as usize),
                    ConflictSide::Base => base.get(relative_line_idx as usize),
                }
                .ok_or(DiffMergeError::InvalidSelection {
                    hunk_index: region.region_id,
                    reason: "line-level merge pick index out of bounds",
                })?;
                chosen.push(line.clone());
            }
            Ok((join_lines_preserving_last_eol(chosen, doc), true))
        }
        ConflictResolution::Unresolved => {
            let ours = doc.extract_region_lines(region, true, false);
            let theirs = doc.extract_region_lines(region, false, false);
            let base = region
                .base_line_range
                .is_some()
                .then(|| doc.extract_region_lines(region, false, true));
            let mut s = String::new();
            let mut dummy_line = 0u32;
            let o_refs: Vec<&str> = ours.iter().map(String::as_str).collect();
            let t_refs: Vec<&str> = theirs.iter().map(String::as_str).collect();
            let b_refs_owned: Option<Vec<&str>> = base
                .as_ref()
                .map(|vec| vec.iter().map(String::as_str).collect());
            emit_diff3_markers(
                &mut s,
                &mut dummy_line,
                doc.line_ending,
                &region.ours_label,
                &o_refs,
                region.base_label.as_deref(),
                b_refs_owned.as_deref(),
                &region.theirs_label,
                &t_refs,
            );
            Ok((s, false))
        }
        ConflictResolution::ManualEdit => {
            let start = (region.output_line_range.start as usize).min(doc.output.len_lines());
            let end = (region.output_line_range.end as usize).min(doc.output.len_lines());
            let mut s = String::new();
            for line_idx in start..end {
                s.push_str(&doc.output.line(line_idx).to_string());
            }
            let has_markers = slice_has_marker_lines(&doc.output, region.output_line_range.clone());
            Ok((s, !has_markers))
        }
    }
}

fn char_span_to_line_range(rope: &Rope, start_char: usize, end_char: usize) -> Range<u32> {
    let max_lines = logical_line_count(rope);
    let start_clamped = start_char.min(rope.len_chars());
    let end_clamped = end_char.min(rope.len_chars()).max(start_clamped);
    let s_line = (rope.char_to_line(start_clamped)).min(max_lines);
    let e_line = if start_clamped == end_clamped {
        s_line
    } else {
        let mut e = rope.char_to_line(end_clamped);
        if end_clamped > rope.line_to_char(e) {
            e += 1;
        }
        e.min(max_lines).max(s_line)
    };
    (s_line as u32)..(e_line as u32)
}

fn logical_line_count(rope: &Rope) -> usize {
    if rope.len_chars() == 0 {
        return 0;
    }
    let raw_lines = rope.len_lines();
    if rope.char(rope.len_chars() - 1) == '\n' {
        raw_lines.saturating_sub(1)
    } else {
        raw_lines
    }
}

fn count_spanned_lines(text: &str, ends_before_eof: bool) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.as_bytes().iter().filter(|&&b| b == b'\n').count();
    if text.ends_with('\n') || ends_before_eof {
        newlines.max(1)
    } else {
        newlines + 1
    }
}

fn slice_has_marker_lines(rope: &Rope, line_range: Range<u32>) -> bool {
    let total = logical_line_count(rope);
    let start = (line_range.start as usize).min(total);
    let end = (line_range.end as usize).min(total);
    if start >= end {
        return false;
    }
    let owned: Vec<String> = (start..end).map(|idx| rope.line(idx).to_string()).collect();
    if owned.iter().any(|l| is_stray_outer_marker(l)) {
        return true;
    }
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    !parse_conflict_blocks(&refs, None).is_empty()
}

fn shift_downstream_regions(tail: &mut [ConflictRegion], line_delta: i64) {
    if line_delta == 0 {
        return;
    }
    for region in tail {
        let s = (region.output_line_range.start as i64 + line_delta).max(0) as u32;
        let e = (region.output_line_range.end as i64 + line_delta).max(s as i64) as u32;
        region.output_line_range = s..e;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use branvio_core::paths::RepoRelPath;

    fn path() -> RepoRelPath {
        RepoRelPath::normalize("src/merge_target.rs").unwrap()
    }

    #[test]
    fn parses_two_way_and_diff3_markers_with_earlier_edits_and_resolves_regions() {
        let mut engine = RopeConflictEngine::new();
        let base = b"hdr\na1\na2\nmid\nb1\nend\n";
        // Notice stage 2 (`ours`) has an extra non-conflicting line `inserted_above` before conflict 0.
        let ours = b"inserted_above\nhdr\no1\nmid\nb_ours\nend\n";
        let theirs = b"hdr\nt1\nt2\nmid\nb_theirs\nend\n";
        let wt = b"\
inserted_above
hdr
<<<<<<< HEAD
o1
||||||| ancestor
a1
a2
=======
t1
t2
>>>>>>> feature/y
mid
<<<<<<< HEAD
b_ours
=======
b_theirs
>>>>>>> feature/y
end
";

        let mut doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(base),
                ours_bytes: ours,
                theirs_bytes: theirs,
                worktree_with_markers: Some(wt),
                ours_label: "HEAD",
                theirs_label: "feature/y",
            })
            .unwrap();

        assert_eq!(doc.regions.len(), 2);
        assert_eq!(doc.unresolved_count(), 2);
        assert_eq!(
            doc.extract_region_lines(&doc.regions[0], true, false),
            vec!["o1\n"]
        );
        assert_eq!(
            doc.extract_region_lines(&doc.regions[0], false, true),
            vec!["a1\n", "a2\n"]
        );

        // Cannot export while unresolved regions remain.
        assert!(matches!(
            engine.export_merged_bytes(&doc, true),
            Err(DiffMergeError::UnresolvedConflictsRemaining { remaining: 2 })
        ));

        // Resolve region 0 using line-level selection [Theirs(1), Ours(0)].
        engine
            .resolve_region(
                &mut doc,
                0,
                ConflictResolution::LineLevelSelection(vec![
                    ConflictLinePick {
                        side: ConflictSide::Theirs,
                        relative_line_idx: 1,
                    },
                    ConflictLinePick {
                        side: ConflictSide::Ours,
                        relative_line_idx: 0,
                    },
                ]),
            )
            .unwrap();
        assert_eq!(doc.unresolved_count(), 1);
        // Region 1 output range must have shifted dynamically upward!
        assert!(doc.regions[1].output_line_range.start < 12);

        // Revert region 0 back to Unresolved, then apply TakeBothTheirsFirst.
        engine
            .resolve_region(&mut doc, 0, ConflictResolution::Unresolved)
            .unwrap();
        assert_eq!(doc.unresolved_count(), 2);

        engine
            .resolve_region(&mut doc, 0, ConflictResolution::TakeBothTheirsFirst)
            .unwrap();
        // Region 1 used 2-way markers in worktree, yet stage 1 (`base_bytes`) supplies `b1\n` cleanly!
        assert_eq!(
            doc.extract_region_lines(&doc.regions[1], false, true),
            vec!["b1\n"]
        );
        engine
            .resolve_region(&mut doc, 1, ConflictResolution::TakeBase)
            .unwrap();
        assert_eq!(
            String::from_utf8(engine.export_merged_bytes(&doc, true).unwrap()).unwrap(),
            "inserted_above\nhdr\nt1\nt2\no1\nmid\nb1\nend\n"
        );
        // Re-resolve region 1 to TakeTheirs cleanly:
        engine
            .resolve_region(&mut doc, 1, ConflictResolution::TakeTheirs)
            .unwrap();

        let bytes = engine.export_merged_bytes(&doc, true).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            "inserted_above\nhdr\nt1\nt2\no1\nmid\nb_theirs\nend\n"
        );

        // Inserting a non-marker line right above region 1 shifts region 1 cleanly without losing edits.
        let r1_start = doc
            .output
            .line_to_byte(doc.regions[1].output_line_range.start as usize);
        engine
            .edit_output_buffer(&mut doc, r1_start..r1_start, "// Xin chào 🦀\n")
            .unwrap();
        assert!(doc.regions[1].is_resolved);

        // Editing inside region 1 with a stray marker marks region 1 unresolved and blocks export.
        let r1_mid = doc
            .output
            .line_to_byte(doc.regions[1].output_line_range.start as usize)
            + 2;
        engine
            .edit_output_buffer(&mut doc, r1_mid..r1_mid, "\n<<<<<<< stray\n")
            .unwrap();
        assert!(!doc.regions[1].is_resolved);
        assert!(matches!(
            engine.export_merged_bytes(&doc, true),
            Err(DiffMergeError::UnresolvedConflictsRemaining { .. })
        ));
    }

    #[test]
    fn stage_only_three_way_synthesis_merges_clean_edits_and_preserves_crlf() {
        let engine = RopeConflictEngine::new();
        let base = b"line1\r\nline2\r\nline3\r\nline4\r\n";
        // Ours cleanly changed line 1, both changed line 3 differently -> line 1 merges cleanly!
        let ours = b"line1_ours\r\nline2\r\nline3_ours\r\nline4\r\n";
        let theirs = b"line1\r\nline2\r\nline3_theirs\r\nline4\r\n";

        let mut doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(base),
                ours_bytes: ours,
                theirs_bytes: theirs,
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();

        assert_eq!(doc.line_ending, "\r\n");
        assert_eq!(doc.regions.len(), 1);
        engine
            .resolve_region(&mut doc, 0, ConflictResolution::TakeBothOursFirst)
            .unwrap();

        let exported = engine.export_merged_bytes(&doc, true).unwrap();
        assert_eq!(
            String::from_utf8(exported).unwrap(),
            "line1_ours\r\nline2\r\nline3_ours\r\nline3_theirs\r\nline4\r\n"
        );
    }

    #[test]
    fn ropey_handles_bare_cr_form_feed_vertical_tab_and_unicode_line_separators() {
        let mut engine = RopeConflictEngine::new();
        let weird_ours =
            "top\rinline\x0Cformfeed\x0Bvt\u{0085}nel\u{2028}ls\u{2029}ps\nours_val\ntail\r_end\n";
        let weird_base =
            "top\rinline\x0Cformfeed\x0Bvt\u{0085}nel\u{2028}ls\u{2029}ps\nbase_val\ntail\r_end\n";
        let weird_theirs = "top\rinline\x0Cformfeed\x0Bvt\u{0085}nel\u{2028}ls\u{2029}ps\ntheirs_val\ntail\r_end\n";

        let mut doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(weird_base.as_bytes()),
                ours_bytes: weird_ours.as_bytes(),
                theirs_bytes: weird_theirs.as_bytes(),
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();

        assert_eq!(doc.regions.len(), 1);
        assert_eq!(doc.regions[0].output_line_range, 1..8);
        engine
            .resolve_region(&mut doc, 0, ConflictResolution::TakeTheirs)
            .unwrap();

        // User edits first weird line; downstream indices stay exact because Rope splits only on `\n`.
        engine.edit_output_buffer(&mut doc, 0..3, "TOP").unwrap();
        engine
            .resolve_region(&mut doc, 0, ConflictResolution::TakeOurs)
            .unwrap();

        let exported = engine.export_merged_bytes(&doc, true).unwrap();
        let expected = format!("TOP{}", &weird_ours[3..]);
        assert_eq!(String::from_utf8(exported).unwrap(), expected);
    }

    #[test]
    fn ignores_markdown_setext_headings_and_malformed_marker_lengths() {
        let engine = RopeConflictEngine::new();
        let md_path = RepoRelPath::normalize("README.md").unwrap();
        let base = b"Intro\n=======\nBase body\n";
        let ours = b"Title\n=======\nOurs body\n========\n";
        let theirs =
            b"Intro\n=======\nTheirs body\n=======   \n<<<<<<<notamarker\n>>>>>>>>too_long\n";
        let wt = b"\
<<<<<<< HEAD
Title
=======
Ours body
========
||||||| base
Intro
=======
Base body
=======
Intro
=======
Theirs body
=======   
<<<<<<<notamarker
>>>>>>>>too_long
>>>>>>> topic
";

        let mut doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: md_path,
                base_bytes: Some(base),
                ours_bytes: ours,
                theirs_bytes: theirs,
                worktree_with_markers: Some(wt),
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();

        assert_eq!(doc.regions.len(), 1);
        engine
            .resolve_region(&mut doc, 0, ConflictResolution::TakeOurs)
            .unwrap();
        assert!(doc.is_fully_resolved());
        assert!(engine.scan_remaining_markers(&doc).is_empty());

        let exported = engine.export_merged_bytes(&doc, true).unwrap();
        assert_eq!(exported, ours);

        // Clean Markdown document with `=======` has zero remaining markers and exports cleanly.
        let doc_theirs = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: RepoRelPath::normalize("NOTES.md").unwrap(),
                base_bytes: Some(theirs),
                ours_bytes: theirs,
                theirs_bytes: theirs,
                worktree_with_markers: Some(theirs),
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        assert!(doc_theirs.regions.is_empty());
        assert!(engine.scan_remaining_markers(&doc_theirs).is_empty());
        assert_eq!(
            engine.export_merged_bytes(&doc_theirs, true).unwrap(),
            theirs
        );
    }

    #[test]
    fn rejects_non_utf8_and_preserves_mixed_eol_and_eof_no_newline() {
        let mut engine = RopeConflictEngine::new();
        // Shift-JIS / Latin-1 rejected explicitly without lossy U+FFFD rewrite.
        let latin1_err = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(b"ok\n"),
                ours_bytes: b"caf\xe9\n",
                theirs_bytes: b"ok\n",
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap_err();
        assert!(matches!(
            latin1_err,
            DiffMergeError::UnsupportedEncoding { .. }
        ));

        // Mixed EOL file (mostly LF with one CRLF line) keeps per-line EOLs untouched on export!
        let mixed_base = b"l1\nl2\nl3\n";
        let mixed_ours = b"l1\nl2_ours\r\nl3\n";
        let mixed_theirs = b"l1\nl2_theirs\nl3\n";
        let mut mixed_doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(mixed_base),
                ours_bytes: mixed_ours,
                theirs_bytes: mixed_theirs,
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        engine
            .resolve_region(&mut mixed_doc, 0, ConflictResolution::TakeOurs)
            .unwrap();
        assert_eq!(
            engine.export_merged_bytes(&mixed_doc, true).unwrap(),
            mixed_ours
        );

        // Conflict at EOF where ours has NO trailing newline and theirs HAS trailing newline.
        let mut eof_doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(b"hdr\nbase"),
                ours_bytes: b"hdr\nours_no_nl",
                theirs_bytes: b"hdr\ntheirs_with_nl\n",
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        engine
            .resolve_region(&mut eof_doc, 0, ConflictResolution::TakeOurs)
            .unwrap();
        assert_eq!(
            engine.export_merged_bytes(&eof_doc, true).unwrap(),
            b"hdr\nours_no_nl"
        );
        // Re-resolving the same region to TakeTheirs restores the trailing newline.
        engine
            .resolve_region(&mut eof_doc, 0, ConflictResolution::TakeTheirs)
            .unwrap();
        assert_eq!(
            engine.export_merged_bytes(&eof_doc, true).unwrap(),
            b"hdr\ntheirs_with_nl\n"
        );

        // BothAdded (no base) rejects TakeBase cleanly.
        let mut add_add = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: None,
                ours_bytes: b"a\n",
                theirs_bytes: b"b\n",
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        assert!(matches!(
            engine.resolve_region(&mut add_add, 0, ConflictResolution::TakeBase),
            Err(DiffMergeError::InvalidSelection { .. })
        ));

        // Cross-boundary edit above conflict does NOT get swallowed by subsequent resolve_region.
        let mut cross_doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(b"top\nmid\nend\n"),
                ours_bytes: b"top\nours\nend\n",
                theirs_bytes: b"top\ntheirs\nend\n",
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        // Replace "top\n<<<<<<< HEAD\nours\n" (bytes 0..22) with "kept_manual\n".
        engine
            .edit_output_buffer(&mut cross_doc, 0..22, "kept_manual\n")
            .unwrap();
        engine
            .resolve_region(&mut cross_doc, 0, ConflictResolution::TakeTheirs)
            .unwrap();
        let resolved_cross =
            String::from_utf8(engine.export_merged_bytes(&cross_doc, true).unwrap()).unwrap();
        assert_eq!(resolved_cross, "kept_manual\ntheirs\nend\n");
    }

    #[test]
    fn adjacent_edits_conflict_like_git_and_crlf_autocrlf_aligns_repeated_lines() {
        let engine = RopeConflictEngine::new();
        // Adjacent edits on lines 2 and 3 must conflict cleanly just like `git merge-file`.
        let adj_doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(b"l1\nl2\nl3\nl4\n"),
                ours_bytes: b"l1\nOURS2\nl3\nl4\n",
                theirs_bytes: b"l1\nl2\nTHEIRS3\nl4\n",
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        assert_eq!(adj_doc.regions.len(), 1);
        assert_eq!(
            adj_doc.extract_region_lines(&adj_doc.regions[0], true, false),
            vec!["OURS2\n", "l3\n"]
        );
        assert_eq!(
            adj_doc.extract_region_lines(&adj_doc.regions[0], false, false),
            vec!["l2\n", "THEIRS3\n"]
        );

        // Worktree has CRLF + repeated closing braces while index stages have LF (`core.autocrlf`).
        let base_lf = b"fn a() {\n}\nfn b() {\n    v = 0;\n}\n";
        let ours_lf = b"fn a() {\n}\nfn b() {\n    v = 1;\n}\n";
        let theirs_lf = b"fn a() {\n}\nfn b() {\n    v = 2;\n}\n";
        let wt_crlf = b"fn a() {\r\n}\r\nfn b() {\r\n<<<<<<< HEAD\r\n    v = 1;\r\n||||||| base\r\n    v = 0;\r\n=======\r\n    v = 2;\r\n>>>>>>> topic\r\n}\r\n";

        let mut crlf_doc = engine
            .open_conflicted_file(ConflictedFileInputs {
                path: path(),
                base_bytes: Some(base_lf),
                ours_bytes: ours_lf,
                theirs_bytes: theirs_lf,
                worktree_with_markers: Some(wt_crlf),
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        engine
            .resolve_region(&mut crlf_doc, 0, ConflictResolution::TakeTheirs)
            .unwrap();
        let out = engine.export_merged_bytes(&crlf_doc, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "fn a() {\r\n}\r\nfn b() {\r\n    v = 2;\r\n}\r\n"
        );
    }
}
