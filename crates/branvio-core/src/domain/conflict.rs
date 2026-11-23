//! 3-Way Merge conflict region descriptors shared between `diff-merge` and `app-ui`.

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Identifies one of the three ancestor/branch inputs to a 3-way merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictSide {
    /// Common ancestor (`stage 1` / `||||||| base` in diff3).
    Base,
    /// Current `HEAD` side (`stage 2` / `<<<<<<< ours`).
    Ours,
    /// Incoming branch/commit side (`stage 3` / `>>>>>>> theirs`).
    Theirs,
}

/// Picks a specific line from one of the merge sides for granular line-by-line merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConflictLinePick {
    /// Source side (`Base`, `Ours`, or `Theirs`).
    pub side: ConflictSide,
    /// Zero-based line offset relative to that side's range inside the conflict region.
    pub relative_line_idx: u32,
}

/// Resolution choice applied to a [`ConflictRegion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Conflict has not yet been resolved (blocks marking file resolved unless manually edited).
    Unresolved,
    /// Accept the entire `Ours` (`Current`) block.
    TakeOurs,
    /// Accept the entire `Theirs` (`Incoming`) block.
    TakeTheirs,
    /// Revert to the common `Base` ancestor block.
    TakeBase,
    /// Keep `Ours` followed immediately by `Theirs`.
    TakeBothOursFirst,
    /// Keep `Theirs` followed immediately by `Ours`.
    TakeBothTheirsFirst,
    /// Custom line-by-line sequence picked from `Ours`, `Theirs`, and/or `Base`.
    LineLevelSelection(Vec<ConflictLinePick>),
    /// User manually edited the output rope inside `output_line_range`.
    ManualEdit,
}

/// Single conflicted region tracked across the 4 `ropey::Rope` buffers (`Base`, `Ours`, `Theirs`, `Output`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRegion {
    /// Monotonic zero-based conflict identifier within the file.
    pub region_id: u32,
    /// Line range in `Base` rope (`None` when parsed from 2-way conflict markers without `diff3` base).
    pub base_line_range: Option<Range<u32>>,
    /// Line range in `Ours` (`Current`) rope.
    pub ours_line_range: Range<u32>,
    /// Line range in `Theirs` (`Incoming`) rope.
    pub theirs_line_range: Range<u32>,
    /// Live line range occupied inside the `Merged Output` rope (updated dynamically on edits).
    pub output_line_range: Range<u32>,
    /// Header label extracted from `<<<<<<< <label>`.
    pub ours_label: CompactString,
    /// Optional base label extracted from `||||||| <label>`.
    pub base_label: Option<CompactString>,
    /// Header label extracted from `>>>>>>> <label>`.
    pub theirs_label: CompactString,
    /// Active resolution strategy for this hunk.
    pub resolution: ConflictResolution,
    /// Whether this region currently contains clean resolved output without conflict markers.
    pub is_resolved: bool,
}
