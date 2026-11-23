//! Metro-line DAG commit graph layout data types.
//!
//! Optimized for monorepo scale: `GraphRow` omits per-row inline `RefBadge`s and full `GitOid`
//! edge copies, replacing them with `u32` target row indices and `u8` palette slots.

use crate::ids::GitOid;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Sentinel target row index in [`GraphEdge::target_row`] when a parent commit lies beyond
/// the currently loaded chunk boundary and has not yet been assigned a row index.
pub const UNRESOLVED_COMMIT_ROW: u32 = u32::MAX;

/// Deterministic sRGB color derived from branch/lane identity with high contrast on dark & light themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeterministicColor {
    /// Red channel (0..=255).
    pub r: u8,
    /// Green channel (0..=255).
    pub g: u8,
    /// Blue channel (0..=255).
    pub b: u8,
    /// Index into the active palette theme table (0..=15) for theme-aware remapping.
    pub palette_slot: u8,
}

impl DeterministicColor {
    /// Returns the color packed as `0xRRGGBBFF` for GPU shaders and `gpui::Rgba`.
    #[inline]
    pub const fn to_rgba_u32(self) -> u32 {
        ((self.r as u32) << 24) | ((self.g as u32) << 16) | ((self.b as u32) << 8) | 0xFF
    }

    /// Formats the color as `#RRGGBB`.
    pub fn to_hex(self) -> CompactString {
        compact_str::format_compact!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Priority ordering controlling which branches claim inner leftmost straight lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum LanePriority {
    /// Virtual WIP node anchored directly above `HEAD`.
    WipHead = 0,
    /// Primary integration trunk (`main`, `master`, `develop`).
    PrimaryTrunk = 1,
    /// Currently checked-out branch if distinct from primary trunk.
    ActiveCheckedOutBranch = 2,
    /// Other local branches.
    LocalBranch = 3,
    /// Remote-tracking branches.
    RemoteBranch = 4,
    /// Tags or detached commits.
    TagOrDetached = 5,
    /// Stash branches.
    Stash = 6,
}

/// Active vertical lane tracked during DAG layout calculation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchLane {
    /// Zero-based horizontal column index (`0` is leftmost).
    pub lane_index: u16,
    /// Commit OID this lane is currently expecting next as the walk advances downward.
    pub target_oid: GitOid,
    /// Deterministic metro color bound to this branch lineage.
    pub color: DeterministicColor,
    /// Lane placement priority.
    pub priority: LanePriority,
    /// Originating branch name used for deterministic color hashing, if known.
    pub origin_ref: Option<CompactString>,
    /// True if this lane tracks the first-parent backbone of the primary trunk.
    pub is_primary_trunk: bool,
}

/// Geometric spline shape used by the GPU canvas renderer to connect commit rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum EdgeCurveKind {
    /// Straight vertical rail within the same lane.
    Straight = 0,
    /// Bezier curve entering/exiting to a lane on the left.
    MergeLeft = 1,
    /// Bezier curve entering/exiting to a lane on the right.
    MergeRight = 2,
    /// Branch fork diverging to a lane on the left.
    ForkLeft = 3,
    /// Branch fork diverging to a lane on the right.
    ForkRight = 4,
    /// Multi-parent octopus fan-out arc spanning multiple lanes.
    OctopusSpan = 5,
}

/// Compact 4-byte vertical pass-through rail segment drawn behind a commit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneSegment {
    /// Column index of the passing vertical rail.
    pub lane_index: u16,
    /// Palette slot index (`0..16`) resolved via `graph_layout::palette_color`.
    pub palette_slot: u8,
    /// Drawn dashed/dotted when connecting the virtual WIP node to `HEAD`.
    pub is_dotted_wip: bool,
}

/// Compact 12-byte directed edge connecting commit rows in the DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Zero-based index inside historical `commit_rows` (`UNRESOLVED_COMMIT_ROW` until target loads).
    pub target_row: u32,
    /// Source lane at the commit node or top/bottom row boundary.
    pub from_lane: u16,
    /// Destination lane at the target boundary or parent node.
    pub to_lane: u16,
    /// Palette slot index (`0..16`) resolved via `graph_layout::palette_color`.
    pub palette_slot: u8,
    /// Spline curve classification.
    pub curve: EdgeCurveKind,
    /// True if this edge links a secondary merge parent (`parent_index >= 1`).
    pub is_merge_edge: bool,
}

/// Visual glyph classification for a historical commit row in the Commit Graph (4 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphNodeKind {
    /// Standard single-parent commit circle.
    Regular,
    /// Two-parent merge commit ring.
    Merge,
    /// Three-or-more parent octopus merge badge.
    OctopusMerge {
        /// Total parent count (`>= 3`, saturated to `u8::MAX`).
        parent_count: u8,
    },
    /// Root / orphan commit with no parents.
    InitialRoot,
    /// Stash entry node.
    StashNode {
        /// Index in `refs/stash` stack (`stash@{n}`).
        stash_index: u16,
    },
}

/// Detached synthetic Work-In-Progress row kept outside `Vec<GraphRow>`.
///
/// Keeping WIP separate prevents O(N) vector shifts and row index invalidation whenever
/// working-tree status toggles between clean and dirty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualWipRow {
    /// Commit OID of `HEAD` that this WIP node sits on (`None` if unborn branch).
    pub head_oid: Option<GitOid>,
    /// Zero-based index of `HEAD` inside historical `commit_rows` (`None` if unborn or unloaded).
    pub head_commit_row: Option<u32>,
    /// Horizontal lane index where the WIP node is drawn.
    pub commit_lane: u16,
    /// Highest active lane index across display row 0.
    pub max_active_lane: u16,
    /// Palette slot (`0..16`) of the active `HEAD` branch.
    pub palette_slot: u8,
    /// Number of staged paths in `.git/index`.
    pub staged_count: u32,
    /// Number of modified/deleted tracked paths in working directory.
    pub unstaged_count: u32,
    /// Number of unmerged conflicted paths.
    pub conflicted_count: u32,
    /// Number of untracked non-ignored paths.
    pub untracked_count: u32,
    /// Straight vertical rails passing uninterrupted through display row 0.
    pub passing_lanes: SmallVec<[LaneSegment; 8]>,
    /// Dashed outgoing edge connecting WIP downward to `HEAD`.
    pub outgoing_head_edge: Option<GraphEdge>,
}

/// Compact precomputed layout instruction for one historical commit row in the Commit Graph.
///
/// Display indices are derived implicitly from slice position (`+1` when a [`VirtualWipRow`]
/// is present), while ref badges are looked up out-of-line in [`crate::domain::RefBadgeMap`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRow {
    /// Commit OID rendered on this row.
    pub oid: GitOid,
    /// Horizontal lane index where the commit circle/node is drawn.
    pub commit_lane: u16,
    /// Highest active lane index in this row (determines dynamic graph column width).
    pub max_active_lane: u16,
    /// Visual style of the commit node.
    pub node_kind: GraphNodeKind,
    /// Palette slot (`0..16`) of the node and its first-parent lineage.
    pub node_palette_slot: u8,
    /// Straight vertical rails passing uninterrupted through this row.
    pub passing_lanes: SmallVec<[LaneSegment; 8]>,
    /// Curves entering this row from child rows above.
    pub incoming_edges: SmallVec<[GraphEdge; 2]>,
    /// Curves leaving this row downward toward parent commit(s).
    pub outgoing_edges: SmallVec<[GraphEdge; 2]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::commit::{CommitNode, RefBadge};
    use std::mem::size_of;

    #[test]
    fn compact_graph_types_meet_memory_budget() {
        eprintln!(
            "MEASURED_SIZES: GitOid={}, CommitNode={}, GraphRow={}, GraphEdge={}, LaneSegment={}, GraphNodeKind={}, VirtualWipRow={}, RefBadge={}",
            size_of::<GitOid>(),
            size_of::<CommitNode>(),
            size_of::<GraphRow>(),
            size_of::<GraphEdge>(),
            size_of::<LaneSegment>(),
            size_of::<GraphNodeKind>(),
            size_of::<VirtualWipRow>(),
            size_of::<RefBadge>(),
        );

        assert_eq!(size_of::<GitOid>(), 33);
        assert_eq!(size_of::<LaneSegment>(), 4);
        assert_eq!(size_of::<GraphEdge>(), 12);
        assert_eq!(size_of::<GraphNodeKind>(), 4);
        assert!(size_of::<GraphRow>() <= 160);
    }
}
