//! 4-Buffer `ropey::Rope` container for the interactive 3-Way Merge Conflict Editor.

use branvio_core::domain::{ConflictRegion, ConflictResolution};
use branvio_core::paths::RepoRelPath;
use ropey::Rope;

/// Holds the four live text buffers (`Base`, `Ours`, `Theirs`, `Merged Output`) and conflict
/// region metadata for a conflicted file.
///
/// Using `ropey::Rope` ensures O(log N) insertions, deletions, and line-index lookups even on
/// multi-megabyte generated or monorepo files without reallocating contiguous strings on every edit.
#[derive(Debug, Clone)]
pub struct ThreeWayMergeDocument {
    /// Repository-relative path of the conflicted file.
    pub path: RepoRelPath,
    /// Common ancestor buffer (`stage 1` / `|||||||`). Empty if created via both-added conflict.
    pub base: Rope,
    /// Current `HEAD` buffer (`stage 2` / `<<<<<<<`).
    pub ours: Rope,
    /// Incoming branch buffer (`stage 3` / `>>>>>>>`).
    pub theirs: Rope,
    /// Live editable output buffer updated immediately as regions are accepted or hand-edited.
    pub output: Rope,
    /// Ordered conflict regions tracking dynamic line ranges inside `output`.
    pub regions: Vec<ConflictRegion>,
    /// Line ending convention used when emitting new conflict marker lines (`"\n"` vs `"\r\n"`).
    /// Individual lines inside `base`/`ours`/`theirs`/`output` preserve their exact byte-level EOLs.
    pub line_ending: &'static str,
    /// When `worktree_with_markers` is pure CRLF (e.g. `core.autocrlf = true`) while index stages
    /// are pure LF, adapt bare `\n` on resolved stage lines to `\r\n` when splicing into `output`.
    pub(crate) adapt_lf_to_crlf_on_resolve: bool,
}

impl ThreeWayMergeDocument {
    /// Constructs a new 4-buffer merge document.
    pub fn new(
        path: RepoRelPath,
        base: Rope,
        ours: Rope,
        theirs: Rope,
        output: Rope,
        regions: Vec<ConflictRegion>,
        use_crlf: bool,
    ) -> Self {
        Self {
            path,
            base,
            ours,
            theirs,
            output,
            regions,
            line_ending: if use_crlf { "\r\n" } else { "\n" },
            adapt_lf_to_crlf_on_resolve: false,
        }
    }

    #[inline]
    pub(crate) fn with_crlf_stage_adaptation(mut self, adapt: bool) -> Self {
        self.adapt_lf_to_crlf_on_resolve = adapt;
        self
    }

    /// Returns the count of conflict regions still marked unresolved.
    pub fn unresolved_count(&self) -> usize {
        self.regions.iter().filter(|r| !r.is_resolved).count()
    }

    /// Returns `true` when all conflict regions are resolved and no residual conflict markers remain.
    pub fn is_fully_resolved(&self) -> bool {
        self.unresolved_count() == 0
    }

    /// Returns a contiguous line slice from the requested side of a specific region.
    pub fn extract_region_lines(
        &self,
        region: &ConflictRegion,
        from_ours: bool,
        from_base: bool,
    ) -> Vec<String> {
        let (rope, range) = if from_base {
            let Some(ref base_range) = region.base_line_range else {
                return Vec::new();
            };
            (&self.base, base_range.clone())
        } else if from_ours {
            (&self.ours, region.ours_line_range.clone())
        } else {
            (&self.theirs, region.theirs_line_range.clone())
        };

        let start = (range.start as usize).min(rope.len_lines());
        let end = (range.end as usize).min(rope.len_lines());
        (start..end).map(|idx| rope.line(idx).to_string()).collect()
    }

    /// Returns the total byte size of the live output buffer.
    #[inline]
    pub fn output_byte_len(&self) -> usize {
        self.output.len_bytes()
    }

    /// Summarizes how many regions were resolved automatically vs manually edited.
    pub fn resolution_summary(&self) -> (usize, usize, usize) {
        let mut resolved = 0usize;
        let mut manual = 0usize;
        let mut unresolved = 0usize;
        for region in &self.regions {
            if !region.is_resolved {
                unresolved += 1;
            } else if matches!(region.resolution, ConflictResolution::ManualEdit) {
                manual += 1;
            } else {
                resolved += 1;
            }
        }
        (resolved, manual, unresolved)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use compact_str::CompactString;

    #[test]
    fn four_rope_document_tracks_unresolved_counts() {
        let path = RepoRelPath::normalize("src/main.rs").unwrap();
        let region = ConflictRegion {
            region_id: 0,
            base_line_range: Some(1..2),
            ours_line_range: 1..3,
            theirs_line_range: 1..2,
            output_line_range: 1..6,
            ours_label: CompactString::const_new("HEAD"),
            base_label: Some(CompactString::const_new("ancestor")),
            theirs_label: CompactString::const_new("feature/x"),
            resolution: ConflictResolution::Unresolved,
            is_resolved: false,
        };
        let doc = ThreeWayMergeDocument::new(
            path,
            Rope::from_str("fn a()\nbase\n"),
            Rope::from_str("fn a()\nours1\nours2\n"),
            Rope::from_str("fn a()\ntheirs\n"),
            Rope::from_str("fn a()\n<<<<<<< HEAD\nours1\n=======\ntheirs\n>>>>>>> feature/x\n"),
            vec![region],
            false,
        );
        assert_eq!(doc.unresolved_count(), 1);
        assert!(!doc.is_fully_resolved());
        let ours_lines = doc.extract_region_lines(&doc.regions[0], true, false);
        assert_eq!(ours_lines, vec!["ours1\n", "ours2\n"]);
    }
}
