//! Synthetic Git-compatible unified patch builder for hunk-level and discrete
//! line-level Staging (`StageToIndex`), Unstaging (`UnstageFromIndex`), and Discarding (`DiscardInWorktree`).

use crate::diff_engine::format_hunk_header;
use crate::traits::{
    DiffMergeError, DiffMergeResult, LineSelectionSpec, PatchBuilder, PatchDirection,
};
use branvio_core::domain::{DeltaStatus, DiffLineOrigin, FileDiffPatch, FileMode};
use std::collections::{BTreeMap, BTreeSet};

/// Production [`PatchBuilder`] generating synthetic unified patches accepted cleanly by `git2::Diff::from_buffer`
/// and `git apply`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyntheticPatchBuilder;

impl SyntheticPatchBuilder {
    /// Creates a new [`SyntheticPatchBuilder`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

struct EmittedLine<'a> {
    prefix: u8,
    text: &'a str,
    has_trailing_newline: bool,
}

impl PatchBuilder for SyntheticPatchBuilder {
    fn build_hunk_patch(
        &self,
        patch: &FileDiffPatch,
        hunk_index: u32,
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>> {
        let Some(hunk) = patch.hunks.iter().find(|h| h.hunk_index == hunk_index) else {
            return Err(DiffMergeError::InvalidSelection {
                hunk_index,
                reason: "hunk index not found in FileDiffPatch",
            });
        };

        let selected_line_indices: BTreeSet<usize> = hunk
            .lines
            .iter()
            .enumerate()
            .filter_map(|(idx, line)| {
                matches!(
                    line.origin,
                    DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                )
                .then_some(idx)
            })
            .collect();

        let spec = LineSelectionSpec {
            hunk_index,
            selected_line_indices,
        };
        self.build_line_selection_patch(patch, &[spec], direction)
    }

    fn build_line_selection_patch(
        &self,
        patch: &FileDiffPatch,
        selections: &[LineSelectionSpec],
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>> {
        let path_str = patch.delta.new_path.as_str();
        if patch.delta.is_binary {
            return Err(DiffMergeError::BinaryFileUnsupported(path_str.to_string()));
        }

        validate_patchable_delta(patch)?;

        if selections.is_empty() {
            return Err(DiffMergeError::InvalidSelection {
                hunk_index: 0,
                reason: "no hunk or line selections provided",
            });
        }

        // Merge duplicate hunk specs and validate bounds before emitting patch bytes.
        let mut by_hunk: BTreeMap<u32, BTreeSet<usize>> = BTreeMap::new();
        for spec in selections {
            let Some(hunk) = patch.hunks.iter().find(|h| h.hunk_index == spec.hunk_index) else {
                return Err(DiffMergeError::InvalidSelection {
                    hunk_index: spec.hunk_index,
                    reason: "hunk index not found in FileDiffPatch",
                });
            };
            let entry = by_hunk.entry(spec.hunk_index).or_default();
            for &line_idx in &spec.selected_line_indices {
                if line_idx >= hunk.lines.len() {
                    return Err(DiffMergeError::InvalidSelection {
                        hunk_index: spec.hunk_index,
                        reason: "selected line index out of bounds for hunk",
                    });
                }
                entry.insert(line_idx);
            }
        }

        for (&hunk_idx, line_indices) in &by_hunk {
            let hunk = patch
                .hunks
                .iter()
                .find(|h| h.hunk_index == hunk_idx)
                .ok_or(DiffMergeError::InvalidSelection {
                    hunk_index: hunk_idx,
                    reason: "hunk index missing",
                })?;
            let has_actionable = line_indices.iter().any(|&idx| {
                matches!(
                    hunk.lines[idx].origin,
                    DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                )
            });
            if !has_actionable {
                return Err(DiffMergeError::InvalidSelection {
                    hunk_index: hunk_idx,
                    reason: "selection must include at least one added or deleted line",
                });
            }
        }

        let is_forward = matches!(direction, PatchDirection::StageToIndex);
        let base_is_empty = if is_forward {
            matches!(
                patch.delta.status,
                DeltaStatus::Added | DeltaStatus::Untracked
            ) || patch.delta.old_mode == FileMode::Absent
        } else {
            patch.delta.status == DeltaStatus::Deleted || patch.delta.new_mode == FileMode::Absent
        };

        let all_changes_selected = patch.hunks.iter().all(|h| {
            by_hunk.get(&h.hunk_index).is_some_and(|chosen| {
                h.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| {
                        matches!(
                            l.origin,
                            DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                        )
                    })
                    .all(|(i, _)| chosen.contains(&i))
            })
        });

        let result_becomes_empty = if is_forward {
            patch.delta.status == DeltaStatus::Deleted
                && patch.hunks.iter().all(|h| {
                    by_hunk.get(&h.hunk_index).is_some_and(|chosen| {
                        h.lines
                            .iter()
                            .enumerate()
                            .filter(|(_, l)| l.origin == DiffLineOrigin::Deletion)
                            .all(|(i, _)| chosen.contains(&i))
                    })
                })
        } else {
            matches!(
                patch.delta.status,
                DeltaStatus::Added | DeltaStatus::Untracked
            ) && patch.hunks.iter().all(|h| {
                by_hunk.get(&h.hunk_index).is_some_and(|chosen| {
                    h.lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| l.origin == DiffLineOrigin::Addition)
                        .all(|(i, _)| chosen.contains(&i))
                })
            })
        };

        let new_rel = patch.delta.new_path.as_str();
        let old_rel = patch
            .delta
            .old_path
            .as_ref()
            .map_or(new_rel, |p| p.as_str());
        let (header_old, header_new) = match direction {
            PatchDirection::StageToIndex => (old_rel, new_rel),
            PatchDirection::UnstageFromIndex => (new_rel, old_rel),
            PatchDirection::DiscardInWorktree => (new_rel, new_rel),
        };

        let (src_mode, dst_mode) = if is_forward {
            (patch.delta.old_mode, patch.delta.new_mode)
        } else {
            (patch.delta.new_mode, patch.delta.old_mode)
        };

        let mut out = Vec::with_capacity(512);
        emit_patch_file_header(
            &mut out,
            path_str,
            header_old,
            header_new,
            base_is_empty,
            result_becomes_empty,
            src_mode,
            dst_mode,
            all_changes_selected,
        )?;

        let mut cumulative_line_delta: i64 = 0;

        for (&hunk_idx, chosen) in &by_hunk {
            let hunk = patch
                .hunks
                .iter()
                .find(|h| h.hunk_index == hunk_idx)
                .ok_or(DiffMergeError::InvalidSelection {
                    hunk_index: hunk_idx,
                    reason: "hunk index missing",
                })?;

            let (src_start, mut emitted) = build_hunk_lines(patch, hunk, chosen, is_forward);
            normalize_emitted_eof_newlines(&mut emitted);

            let mut src_count = 0u32;
            let mut dst_count = 0u32;
            for line in &emitted {
                match line.prefix {
                    b' ' => {
                        src_count += 1;
                        dst_count += 1;
                    }
                    b'-' => src_count += 1,
                    b'+' => dst_count += 1,
                    _ => {}
                }
            }

            let orig_src_lines = if is_forward {
                hunk.old_lines
            } else {
                hunk.new_lines
            };
            let src_0: i64 = if orig_src_lines == 0 {
                i64::from(src_start)
            } else {
                i64::from(src_start).saturating_sub(1)
            };
            let dst_0: i64 = (src_0 + cumulative_line_delta).max(0);

            let eff_src_start = if src_count == 0 {
                u32::try_from(src_0).unwrap_or(u32::MAX)
            } else {
                u32::try_from(src_0 + 1).unwrap_or(u32::MAX)
            };
            let eff_dst_start = if dst_count == 0 {
                u32::try_from(dst_0).unwrap_or(u32::MAX)
            } else {
                u32::try_from(dst_0 + 1).unwrap_or(u32::MAX)
            };

            let hunk_header =
                format_hunk_header(eff_src_start, src_count, eff_dst_start, dst_count);
            out.extend_from_slice(hunk_header.as_bytes());
            out.push(b'\n');

            for line in emitted {
                out.push(line.prefix);
                out.extend_from_slice(line.text.as_bytes());
                out.push(b'\n');
                if !line.has_trailing_newline {
                    out.extend_from_slice(b"\\ No newline at end of file\n");
                }
            }

            cumulative_line_delta += i64::from(dst_count) - i64::from(src_count);
        }

        Ok(out)
    }
}

fn validate_patchable_delta(patch: &FileDiffPatch) -> DiffMergeResult<()> {
    let path_str = patch.delta.new_path.as_str();
    match patch.delta.status {
        DeltaStatus::Added
        | DeltaStatus::Modified
        | DeltaStatus::Deleted
        | DeltaStatus::Untracked => {}
        DeltaStatus::Renamed { .. }
        | DeltaStatus::Copied { .. }
        | DeltaStatus::TypeChanged
        | DeltaStatus::Conflicted(_)
        | DeltaStatus::Ignored => {
            return Err(DiffMergeError::UnsupportedPatchOperation {
                path: path_str.to_string(),
                reason: "renamed, copied, type-changed, conflicted, or ignored entries do not support line-level patching",
            });
        }
    }

    if patch
        .delta
        .old_path
        .as_ref()
        .is_some_and(|old_p| old_p != &patch.delta.new_path)
    {
        return Err(DiffMergeError::UnsupportedPatchOperation {
            path: path_str.to_string(),
            reason: "renamed or copied paths must be staged or unstaged as whole files",
        });
    }

    for mode in [patch.delta.old_mode, patch.delta.new_mode] {
        if matches!(
            mode,
            FileMode::Symlink | FileMode::GitlinkSubmodule | FileMode::Tree
        ) {
            return Err(DiffMergeError::UnsupportedPatchOperation {
                path: path_str.to_string(),
                reason: "symlinks (120000), submodules (160000), and trees do not support line-level patching",
            });
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_patch_file_header(
    out: &mut Vec<u8>,
    path_str: &str,
    header_old: &str,
    header_new: &str,
    base_is_empty: bool,
    result_becomes_empty: bool,
    src_mode: FileMode,
    dst_mode: FileMode,
    all_changes_selected: bool,
) -> DiffMergeResult<()> {
    let qa = format_git_patch_path("a/", header_old);
    let qb = format_git_patch_path("b/", header_new);
    out.extend_from_slice(format!("diff --git {qa} {qb}\n").as_bytes());
    if base_is_empty {
        let mode_str = format_regular_mode(dst_mode).ok_or_else(|| {
            DiffMergeError::UnsupportedPatchOperation {
                path: path_str.to_string(),
                reason: "new file has non-regular target mode",
            }
        })?;
        out.extend_from_slice(format!("new file mode {mode_str}\n").as_bytes());
        out.extend_from_slice(b"--- /dev/null\n");
        out.extend_from_slice(format!("+++ {qb}\n").as_bytes());
    } else if result_becomes_empty {
        let mode_str = format_regular_mode(src_mode).ok_or_else(|| {
            DiffMergeError::UnsupportedPatchOperation {
                path: path_str.to_string(),
                reason: "deleted file has non-regular source mode",
            }
        })?;
        out.extend_from_slice(format!("deleted file mode {mode_str}\n").as_bytes());
        out.extend_from_slice(format!("--- {qa}\n").as_bytes());
        out.extend_from_slice(b"+++ /dev/null\n");
    } else {
        let src_mode_str = format_regular_mode(src_mode).ok_or_else(|| {
            DiffMergeError::UnsupportedPatchOperation {
                path: path_str.to_string(),
                reason: "modified file has non-regular source mode",
            }
        })?;
        if let Some(dst_mode_str) = format_regular_mode(dst_mode)
            && src_mode != dst_mode
            && all_changes_selected
        {
            out.extend_from_slice(format!("old mode {src_mode_str}\n").as_bytes());
            out.extend_from_slice(format!("new mode {dst_mode_str}\n").as_bytes());
        }
        out.extend_from_slice(format!("--- {qa}\n").as_bytes());
        out.extend_from_slice(format!("+++ {qb}\n").as_bytes());
    }
    Ok(())
}

fn format_regular_mode(mode: FileMode) -> Option<&'static str> {
    match mode {
        FileMode::Regular644 => Some("100644"),
        FileMode::Executable755 => Some("100755"),
        FileMode::Absent | FileMode::Symlink | FileMode::GitlinkSubmodule | FileMode::Tree => None,
    }
}

fn format_git_patch_path(prefix: &str, rel: &str) -> String {
    let mut raw = Vec::with_capacity(prefix.len() + rel.len());
    raw.extend_from_slice(prefix.as_bytes());
    raw.extend_from_slice(rel.as_bytes());

    let needs_quoting = raw
        .iter()
        .any(|&b| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\');
    if !needs_quoting {
        return format!("{prefix}{rel}");
    }

    let mut out = String::with_capacity(raw.len() + 8);
    out.push('"');
    for b in raw {
        match b {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b'\r' => out.push_str("\\r"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "\\{b:03o}");
            }
        }
    }
    out.push('"');
    out
}

fn build_hunk_lines<'a>(
    patch: &'a FileDiffPatch,
    hunk: &branvio_core::domain::DiffHunk,
    selected: &BTreeSet<usize>,
    is_forward: bool,
) -> (u32, Vec<EmittedLine<'a>>) {
    let src_start = if is_forward {
        hunk.old_start
    } else {
        hunk.new_start
    };
    let mut emitted = Vec::with_capacity(hunk.lines.len());
    let mut i = 0usize;

    while i < hunk.lines.len() {
        let line = &hunk.lines[i];
        match line.origin {
            DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew => {
                i += 1;
            }
            DiffLineOrigin::Context => {
                let ctx_text = if is_forward {
                    patch.old_line_text(line)
                } else {
                    patch.line_text(line)
                };
                emitted.push(EmittedLine {
                    prefix: b' ',
                    text: ctx_text,
                    has_trailing_newline: line.has_trailing_newline,
                });
                i += 1;
            }
            DiffLineOrigin::Deletion | DiffLineOrigin::Addition => {
                // Collect contiguous edit block (Deletion*/Addition* ignoring EOF marker rows).
                let mut dels: Vec<usize> = Vec::new();
                let mut adds: Vec<usize> = Vec::new();
                while i < hunk.lines.len() {
                    match hunk.lines[i].origin {
                        DiffLineOrigin::Deletion => {
                            dels.push(i);
                            i += 1;
                        }
                        DiffLineOrigin::Addition => {
                            adds.push(i);
                            i += 1;
                        }
                        DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew => {
                            i += 1;
                        }
                        DiffLineOrigin::Context => break,
                    }
                }

                // In forward patching, `src` = deletions, `dst` = additions.
                // In reverse patching, `src` = additions, `dst` = deletions.
                let (src_indices, dst_indices) = if is_forward {
                    (&dels, &adds)
                } else {
                    (&adds, &dels)
                };

                let mut pending_rm: Vec<EmittedLine<'a>> = Vec::new();
                let mut pending_add: Vec<EmittedLine<'a>> = Vec::new();
                let flush = |out: &mut Vec<EmittedLine<'a>>,
                             rm: &mut Vec<EmittedLine<'a>>,
                             add: &mut Vec<EmittedLine<'a>>| {
                    out.append(rm);
                    out.append(add);
                };

                let max_pairs = src_indices.len().max(dst_indices.len());
                for k in 0..max_pairs {
                    if let Some(&src_idx) = src_indices.get(k) {
                        let s_line = &hunk.lines[src_idx];
                        if selected.contains(&src_idx) {
                            pending_rm.push(EmittedLine {
                                prefix: b'-',
                                text: patch.line_text(s_line),
                                has_trailing_newline: s_line.has_trailing_newline,
                            });
                        } else {
                            // Unselected target line acts as anchor context: only flush buffered `+`
                            // additions beforehand if an earlier `-` deletion paired with them.
                            if !pending_rm.is_empty() {
                                flush(&mut emitted, &mut pending_rm, &mut pending_add);
                            }
                            emitted.push(EmittedLine {
                                prefix: b' ',
                                text: patch.line_text(s_line),
                                has_trailing_newline: s_line.has_trailing_newline,
                            });
                        }
                    }

                    if let Some(&dst_idx) = dst_indices.get(k)
                        && selected.contains(&dst_idx)
                    {
                        let d_line = &hunk.lines[dst_idx];
                        pending_add.push(EmittedLine {
                            prefix: b'+',
                            text: patch.line_text(d_line),
                            has_trailing_newline: d_line.has_trailing_newline,
                        });
                    }
                }
                flush(&mut emitted, &mut pending_rm, &mut pending_add);
            }
        }
    }

    (src_start, emitted)
}

/// Ensures `\ No newline at end of file` never appears before another emitted line in a hunk.
///
/// For instance, if `old` lacked a trailing newline (`-b` unselected -> `' ' b`) and the user
/// selects `+c\n` after it, `' ' b` cannot legally emit `\ No newline` above `+c\n`; splitting it
/// into `-b` (`!nl`) + `+b\n` (`nl`) + `+c\n` produces a valid Git patch and exact resulting bytes.
fn normalize_emitted_eof_newlines(emitted: &mut Vec<EmittedLine<'_>>) {
    let last_dst_idx = emitted
        .iter()
        .rposition(|line| matches!(line.prefix, b' ' | b'+'));
    let Some(last_dst) = last_dst_idx else {
        return;
    };

    let has_invalid_mid_no_nl = emitted.iter().enumerate().any(|(idx, line)| {
        !line.has_trailing_newline && idx < last_dst && matches!(line.prefix, b' ' | b'+')
    });
    if !has_invalid_mid_no_nl {
        return;
    }

    let prev = std::mem::take(emitted);
    emitted.reserve(prev.len() + 1);
    for (idx, mut line) in prev.into_iter().enumerate() {
        if !line.has_trailing_newline && idx < last_dst {
            match line.prefix {
                b' ' => {
                    emitted.push(EmittedLine {
                        prefix: b'-',
                        text: line.text,
                        has_trailing_newline: false,
                    });
                    emitted.push(EmittedLine {
                        prefix: b'+',
                        text: line.text,
                        has_trailing_newline: true,
                    });
                }
                b'+' => {
                    line.has_trailing_newline = true;
                    emitted.push(line);
                }
                _ => emitted.push(line),
            }
        } else {
            emitted.push(line);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::diff_engine::ImaraDiffEngine;
    use crate::traits::{DiffEngine, DiffOptions};
    use branvio_core::domain::{DeltaArea, EolChange, FileDelta};
    use branvio_core::paths::RepoRelPath;

    fn delta_for(
        path: &str,
        status: DeltaStatus,
        old_mode: FileMode,
        new_mode: FileMode,
    ) -> FileDelta {
        FileDelta {
            old_path: None,
            new_path: RepoRelPath::normalize(path).unwrap(),
            status,
            area: DeltaArea::Unstaged,
            old_mode,
            new_mode,
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
    fn validates_empty_and_out_of_bounds_line_selections() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();
        let delta = delta_for(
            "a.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Regular644,
        );
        let patch = diff
            .diff_blobs(delta, b"one\ntwo\n", b"one\nTWO\n", &DiffOptions::default())
            .unwrap();

        assert!(matches!(
            builder.build_line_selection_patch(&patch, &[], PatchDirection::StageToIndex),
            Err(DiffMergeError::InvalidSelection { .. })
        ));
        // Context-only line selection (index 0) must be rejected.
        let ctx_only = LineSelectionSpec {
            hunk_index: 0,
            selected_line_indices: BTreeSet::from([0]),
        };
        assert!(matches!(
            builder.build_line_selection_patch(&patch, &[ctx_only], PatchDirection::StageToIndex),
            Err(DiffMergeError::InvalidSelection { .. })
        ));
        // Out of bounds hunk index rejected.
        assert!(matches!(
            builder.build_hunk_patch(&patch, 99, PatchDirection::StageToIndex),
            Err(DiffMergeError::InvalidSelection { .. })
        ));
    }

    #[test]
    fn partial_and_full_unstage_of_added_file_toggle_deleted_header_accurately() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();
        let delta = delta_for(
            "added.txt",
            DeltaStatus::Added,
            FileMode::Absent,
            FileMode::Regular644,
        );
        let patch = diff
            .diff_blobs(
                delta,
                b"",
                b"line 1\nline 2\nline 3\n",
                &DiffOptions::default(),
            )
            .unwrap();

        // Partial forward stage (lines 0 & 2 only) emits new file mode + /dev/null.
        let forward_partial = builder
            .build_line_selection_patch(
                &patch,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([0, 2]),
                }],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let fwd_str = std::str::from_utf8(&forward_partial).unwrap();
        assert!(fwd_str.contains("new file mode 100644\n--- /dev/null\n+++ b/added.txt\n@@ -0,0 +1,2 @@\n+line 1\n+line 3\n"));

        // Partial reverse unstage (only line 2) must keep file alive (--- a/added.txt, +++ b/added.txt).
        let reverse_partial = builder
            .build_line_selection_patch(
                &patch,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([1]),
                }],
                PatchDirection::UnstageFromIndex,
            )
            .unwrap();
        let rev_partial_str = std::str::from_utf8(&reverse_partial).unwrap();
        assert!(!rev_partial_str.contains("/dev/null"));
        assert!(rev_partial_str.contains("@@ -1,3 +1,2 @@\n line 1\n-line 2\n line 3\n"));

        // Full reverse unstage of added file emits deleted file mode + /dev/null.
        let reverse_full = builder
            .build_hunk_patch(&patch, 0, PatchDirection::UnstageFromIndex)
            .unwrap();
        let rev_full_str = std::str::from_utf8(&reverse_full).unwrap();
        assert!(rev_full_str.contains(
            "deleted file mode 100644\n--- a/added.txt\n+++ /dev/null\n@@ -1,3 +0,0 @@\n"
        ));
    }

    #[test]
    fn multi_hunk_selection_recalculates_cumulative_line_offsets() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        for i in 1..=25 {
            old_lines.push(format!("line {i}\n"));
            if i == 2 {
                new_lines.push("line 2 NEW A\n".to_string());
                new_lines.push("line 2 EXTRA B\n".to_string());
            } else if i == 23 {
                new_lines.push("line 23 NEW Z\n".to_string());
            } else {
                new_lines.push(format!("line {i}\n"));
            }
        }
        let delta = delta_for(
            "multi.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Regular644,
        );
        let patch = diff
            .diff_blobs(
                delta,
                old_lines.concat().as_bytes(),
                new_lines.concat().as_bytes(),
                &DiffOptions::default(),
            )
            .unwrap();
        assert_eq!(patch.hunks.len(), 2);
        // Selecting ONLY hunk 1 for StageToIndex uses old_start (20), untouched by hunk 0's +1 delta!
        let hunk1_del_add: BTreeSet<usize> = patch.hunks[1]
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                matches!(
                    l.origin,
                    DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                )
                .then_some(i)
            })
            .collect();
        let only_hunk1 = builder
            .build_line_selection_patch(
                &patch,
                &[LineSelectionSpec {
                    hunk_index: 1,
                    selected_line_indices: hunk1_del_add.clone(),
                }],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let s1 = std::str::from_utf8(&only_hunk1).unwrap();
        let expected_hunk1_old_start = patch.hunks[1].old_start;
        assert!(s1.contains(&format!("@@ -{expected_hunk1_old_start},")));
        // Note old_start == new_start when hunk 0 is skipped forward.
        assert!(s1.contains(&format!(" +{expected_hunk1_old_start},")));

        // Selecting BOTH hunk 0 (+1 net line) and hunk 1 shifts hunk 1's new_start forward by +1!
        let both_hunks = builder
            .build_line_selection_patch(
                &patch,
                &[
                    LineSelectionSpec {
                        hunk_index: 0,
                        selected_line_indices: patch.hunks[0]
                            .lines
                            .iter()
                            .enumerate()
                            .filter_map(|(i, l)| {
                                matches!(
                                    l.origin,
                                    DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                                )
                                .then_some(i)
                            })
                            .collect(),
                    },
                    LineSelectionSpec {
                        hunk_index: 1,
                        selected_line_indices: hunk1_del_add,
                    },
                ],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let s_both = std::str::from_utf8(&both_hunks).unwrap();
        let shifted = expected_hunk1_old_start + 1;
        assert!(s_both.contains(&format!("@@ -{expected_hunk1_old_start},")));
        assert!(s_both.contains(&format!(" +{shifted},")));
    }

    #[test]
    fn partial_replace_block_keeps_paired_addition_before_unselected_lines() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();
        let delta = delta_for(
            "rep.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Regular644,
        );
        let patch = diff
            .diff_blobs(
                delta,
                b"line 1\nline 2\nline 3\n",
                b"line 1\nline 2B\nline 3B\n",
                &DiffOptions::default(),
            )
            .unwrap();
        // Hunk lines: 0=ctx(line 1), 1=del(line 2), 2=del(line 3), 3=add(line 2B), 4=add(line 3B).
        let bytes = builder
            .build_line_selection_patch(
                &patch,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([1, 3]),
                }],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains(" line 1\n-line 2\n+line 2B\n line 3\n"));
    }

    #[test]
    fn partial_addition_in_larger_deletion_block_stays_after_unselected_lines() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();
        let delta = delta_for(
            "order.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Regular644,
        );
        let patch = diff
            .diff_blobs(delta, b"A\nB\n", b"X\n", &DiffOptions::default())
            .unwrap();
        // Hunk lines: 0=del(A), 1=del(B), 2=add(X). Select ONLY +X (2).
        let bytes = builder
            .build_line_selection_patch(
                &patch,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([2]),
                }],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("@@ -1,2 +1,3 @@\n A\n B\n+X\n"),
            "got:\n{text}"
        );
    }

    #[test]
    fn handles_eof_no_newline_partial_selection_c_escaped_paths_u0_and_mode_validation() {
        let diff = ImaraDiffEngine::new();
        let builder = SyntheticPatchBuilder::new();

        // 1. EOF no-newline partial selection: old="a\nb" (no \n), new="a\nb\nc\n", select ONLY +c\n.
        let delta_eof = delta_for(
            "eof.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Regular644,
        );
        let patch_eof = diff
            .diff_blobs(delta_eof, b"a\nb", b"a\nb\nc\n", &DiffOptions::default())
            .unwrap();
        let add_c_idx = patch_eof.hunks[0]
            .lines
            .iter()
            .position(|l| l.origin == DiffLineOrigin::Addition && patch_eof.line_text(l) == "c")
            .unwrap();
        let bytes_eof = builder
            .build_line_selection_patch(
                &patch_eof,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([add_c_idx]),
                }],
                PatchDirection::StageToIndex,
            )
            .unwrap();
        let str_eof = std::str::from_utf8(&bytes_eof).unwrap();
        assert!(
            str_eof.contains("@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+b\n+c\n"),
            "got:\n{str_eof}"
        );

        // Reverse direction when target has mid-hunk addition originally at EOF: must not emit \ No newline before context!
        let add_b_idx = patch_eof.hunks[0]
            .lines
            .iter()
            .position(|l| l.origin == DiffLineOrigin::Addition && patch_eof.line_text(l) == "b")
            .unwrap();
        let del_b_idx = patch_eof.hunks[0]
            .lines
            .iter()
            .position(|l| l.origin == DiffLineOrigin::Deletion && patch_eof.line_text(l) == "b")
            .unwrap();
        let rev_eof = builder
            .build_line_selection_patch(
                &patch_eof,
                &[LineSelectionSpec {
                    hunk_index: 0,
                    selected_line_indices: BTreeSet::from([add_b_idx, del_b_idx]),
                }],
                PatchDirection::DiscardInWorktree,
            )
            .unwrap();
        let rev_eof_str = std::str::from_utf8(&rev_eof).unwrap();
        assert!(!rev_eof_str.contains("\\ No newline"));
        assert!(rev_eof_str.contains("-b\n+b\n c\n"));

        // 2. Rejects Rename, Symlink (120000), GitlinkSubmodule (160000) cleanly!
        let mut ren_delta = delta_for(
            "new.rs",
            DeltaStatus::Renamed { similarity_pct: 90 },
            FileMode::Regular644,
            FileMode::Regular644,
        );
        ren_delta.old_path = Some(RepoRelPath::normalize("old.rs").unwrap());
        let ren_patch = diff
            .diff_blobs(ren_delta, b"a\n", b"b\n", &DiffOptions::default())
            .unwrap();
        assert!(matches!(
            builder.build_hunk_patch(&ren_patch, 0, PatchDirection::StageToIndex),
            Err(DiffMergeError::UnsupportedPatchOperation { .. })
        ));

        let sym_delta = delta_for(
            "link",
            DeltaStatus::Modified,
            FileMode::Symlink,
            FileMode::Symlink,
        );
        let sym_patch = diff
            .diff_blobs(
                sym_delta,
                b"target1\n",
                b"target2\n",
                &DiffOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            builder.build_hunk_patch(&sym_patch, 0, PatchDirection::StageToIndex),
            Err(DiffMergeError::UnsupportedPatchOperation { .. })
        ));

        let sub_delta = delta_for(
            "vendor/sub",
            DeltaStatus::Modified,
            FileMode::GitlinkSubmodule,
            FileMode::GitlinkSubmodule,
        );
        let sub_patch = diff
            .diff_blobs(
                sub_delta,
                b"Subproject commit a\n",
                b"Subproject commit b\n",
                &DiffOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            builder.build_hunk_patch(&sub_patch, 0, PatchDirection::StageToIndex),
            Err(DiffMergeError::UnsupportedPatchOperation { .. })
        ));

        // 3. C-quoted paths (direct backslash/UTF-8 check + RepoRelPath check) & mode transitions + 0-line hunk coordinates (`-U0`).
        assert_eq!(
            format_git_patch_path("a/", "dir/t\tq\"s\\n\n\x07é.txt"),
            "\"a/dir/t\\tq\\\"s\\\\n\\n\\a\\303\\251.txt\""
        );
        let cquote_delta = delta_for(
            "dir/t\tq\"s\n.txt",
            DeltaStatus::Modified,
            FileMode::Regular644,
            FileMode::Executable755,
        );
        let cquote_patch = diff
            .diff_blobs(
                cquote_delta,
                b"1\n2\n3\n4\n5\n",
                b"1\n2\nINS\n3\n4\n",
                &DiffOptions {
                    context_lines: 0,
                    ..DiffOptions::default()
                },
            )
            .unwrap();
        // Select ONLY hunk 1 (pure deletion of line 5) -> header must be `@@ -5 +4,0 @@` and omit mode change!
        let h1_patch = builder
            .build_hunk_patch(&cquote_patch, 1, PatchDirection::StageToIndex)
            .unwrap();
        let h1_str = std::str::from_utf8(&h1_patch).unwrap();
        assert!(
            h1_str.contains("diff --git \"a/dir/t\\tq\\\"s\\n.txt\" \"b/dir/t\\tq\\\"s\\n.txt\""),
            "got:\n{h1_str}"
        );
        assert!(!h1_str.contains("new mode 100755"));
        assert!(h1_str.contains("@@ -5 +4,0 @@\n-5\n"), "got:\n{h1_str}");

        // Select BOTH hunks -> includes `old mode 100644\nnew mode 100755` and shifted `@@ -5 +5,0 @@`!
        let all_specs: Vec<LineSelectionSpec> = cquote_patch
            .hunks
            .iter()
            .map(|h| LineSelectionSpec {
                hunk_index: h.hunk_index,
                selected_line_indices: h
                    .lines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, l)| {
                        matches!(
                            l.origin,
                            DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                        )
                        .then_some(i)
                    })
                    .collect(),
            })
            .collect();
        let all_patch = builder
            .build_line_selection_patch(&cquote_patch, &all_specs, PatchDirection::StageToIndex)
            .unwrap();
        let all_str = std::str::from_utf8(&all_patch).unwrap();
        assert!(all_str.contains("old mode 100644\nnew mode 100755\n"));
        assert!(all_str.contains("@@ -2,0 +3 @@\n+INS\n"));
        assert!(all_str.contains("@@ -5 +5,0 @@\n-5\n"));
    }
}
