//! Core traits and session interfaces for incremental Metro-line DAG Commit Graph computation.

use crate::filter::GraphFilterSpec;
use branvio_core::domain::{BranchLane, CommitNode, GraphRow, RefBadgeMap, VirtualWipRow};
use branvio_core::ids::GitOid;
use branvio_core::pipeline::WorkingTreeStatusView;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias for `graph-layout`.
pub type LayoutResult<T> = Result<T, LayoutError>;

/// Errors arising from incremental DAG layout construction.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// Commits were fed out of strict topological parent-after-child order.
    #[error("topological layout invariant violated at commit {oid}: {reason}")]
    InvalidTopologyOrder {
        /// Commit OID that violated layout assumptions.
        oid: GitOid,
        /// Explanation of the invariant failure.
        reason: &'static str,
    },
}

/// Configuration knobs controlling trunk anchoring and lane allocation behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSessionConfig {
    /// Primary trunk branch names ordered by priority (`["main", "master", "develop"]`).
    pub primary_trunk_names: Vec<CompactString>,
    /// Keep primary trunk pinned on lane `0` even across feature merges.
    pub pin_primary_trunk_to_lane_zero: bool,
    /// Maximum number of commits retained before aggressive chunk eviction (0 = unbounded).
    pub max_resident_rows: usize,
}

impl Default for LayoutSessionConfig {
    fn default() -> Self {
        Self {
            primary_trunk_names: vec![
                CompactString::const_new("main"),
                CompactString::const_new("master"),
                CompactString::const_new("develop"),
            ],
            pin_primary_trunk_to_lane_zero: true,
            max_resident_rows: 0,
        }
    }
}

/// Zero-copy view of one visual row in the virtualized Commit Graph viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportRowRef<'a> {
    /// Synthetic WIP node rendered at display row `0` when working tree has changes.
    Wip(&'a VirtualWipRow),
    /// Historical commit row rendered after any active WIP row.
    Commit {
        /// Zero-based index inside historical `commit_rows`.
        commit_index: usize,
        /// Precomputed layout geometry for this commit.
        row: &'a GraphRow,
    },
}

/// Stateful incremental Metro-line DAG layout builder supporting chunked viewport streaming.
///
/// ## Topological Invariant
/// Callers MUST supply `commits` in strict topological parent-after-child order (produced by
/// `git_engine::RevwalkSession`), even when commit timestamps suffer from clock skew.
///
/// ## Reference Map Contract
/// Callers MUST supply the complete repository-wide `RefBadgeMap` starting from the very first
/// `push_commits_chunk` call (and on all subsequent chunk calls) so primary trunk reservation on
/// lane `0` and deterministic branch lineage colors are established upfront from chunk 0.
///
/// ## Detached WIP Invariant
/// [`VirtualWipRow`] lives outside the historical `commit_rows` vector so toggling working-tree
/// cleanliness is `O(1)` without mutating stored edge `target_row` indices.
pub trait IncrementalLayoutSession: Send + Sync {
    /// Appends the next topological chunk of commits and returns the newly added historical row range.
    ///
    /// ## Reference Map Contract
    /// Callers MUST supply the complete repository-wide `RefBadgeMap` starting from chunk 0 (and
    /// on all subsequent chunk calls) so primary trunk reservation on lane `0` and deterministic
    /// branch lineage colors are established upfront from the first chunk.
    fn push_commits_chunk(
        &mut self,
        commits: &[CommitNode],
        refs_by_oid: &RefBadgeMap,
    ) -> LayoutResult<std::ops::Range<usize>>;

    /// Inserts or updates the detached [`VirtualWipRow`] anchored above `HEAD` when working-tree
    /// changes exist (`staged`, `unstaged`, `conflicted`, or `untracked`).
    fn upsert_virtual_wip(
        &mut self,
        head_oid: Option<GitOid>,
        status: &WorkingTreeStatusView,
        refs_by_oid: &RefBadgeMap,
    ) -> LayoutResult<()>;

    /// Removes the detached [`VirtualWipRow`] in `O(1)` when the working tree becomes clean.
    fn remove_virtual_wip(&mut self);

    /// Returns the detached `WIP` row if currently active.
    fn wip_row(&self) -> Option<&VirtualWipRow>;

    /// Returns a zero-copy slice of historical commit rows `[start..end)` (excluding WIP).
    fn commit_rows_slice(&self, range: std::ops::Range<usize>) -> &[GraphRow];

    /// Total historical commit rows currently laid out (excluding WIP).
    fn commit_row_count(&self) -> usize;

    /// Total visual display rows (`commit_row_count() + usize::from(wip_row().is_some())`).
    fn display_row_count(&self) -> usize;

    /// Resolves a visual display index (`0..display_row_count()`) in `O(1)`.
    fn display_row_at(&self, display_index: usize) -> Option<ViewportRowRef<'_>>;

    /// Looks up the immutable historical commit index of `oid`.
    fn commit_index_for_oid(&self, oid: GitOid) -> Option<usize>;

    /// Looks up the current visual display row index of `oid` (`+1` if `WIP` is active).
    fn display_row_for_oid(&self, oid: GitOid) -> Option<usize>;

    /// Returns the currently open vertical lanes at the bottom frontier of the loaded chunk.
    fn active_frontier_lanes(&self) -> &[BranchLane];

    /// Maximum lane index used across all laid-out rows (for dynamic left-margin sizing).
    fn max_lane_width(&self) -> u16;
}

/// Factory creating incremental Metro-line DAG layout sessions.
pub trait GraphLayoutEngine: Send + Sync {
    /// Creates a new incremental layout session configured with repository refs and filter spec.
    fn start_session(
        &self,
        config: LayoutSessionConfig,
        head_oid: Option<GitOid>,
        filter: GraphFilterSpec,
    ) -> Box<dyn IncrementalLayoutSession>;
}
