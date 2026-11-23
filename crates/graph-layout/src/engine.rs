//! Concrete Metro-line DAG commit graph layout engine with O(1) detached WIP
//! mapping and cross-chunk `UNRESOLVED_COMMIT_ROW` edge backpatching.

use crate::filter::GraphFilterSpec;
use crate::palette::{
    METRO_PALETTE, deterministic_anonymous_lane_color, deterministic_branch_color,
};
use crate::traits::{
    GraphLayoutEngine, IncrementalLayoutSession, LayoutError, LayoutResult, LayoutSessionConfig,
};
use branvio_core::domain::{
    BranchLane, CommitFlags, CommitNode, DeterministicColor, EdgeCurveKind, GraphEdge,
    GraphNodeKind, GraphRow, LanePriority, LaneSegment, RefBadge, RefBadgeMap, RefKind,
    UNRESOLVED_COMMIT_ROW, VirtualWipRow,
};
use branvio_core::ids::GitOid;
use branvio_core::pipeline::WorkingTreeStatusView;
use compact_str::CompactString;
use smallvec::SmallVec;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

/// Default [`GraphLayoutEngine`] producing compact Metro-line DAG layouts.
#[derive(Debug, Default, Clone, Copy)]
pub struct MetroLayoutEngine;

impl MetroLayoutEngine {
    /// Creates a new [`MetroLayoutEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

impl GraphLayoutEngine for MetroLayoutEngine {
    fn start_session(
        &self,
        config: LayoutSessionConfig,
        head_oid: Option<GitOid>,
        filter: GraphFilterSpec,
    ) -> Box<dyn IncrementalLayoutSession> {
        Box::new(MetroLayoutSession::new(config, head_oid, filter))
    }
}

/// Bookkeeping entry allowing `O(1)` backpatching of `GraphEdge::target_row` once a parent
/// commit arrives in the current or a subsequent revwalk chunk.
#[derive(Debug, Clone, Copy)]
struct PendingEdgeRef {
    child_row: u32,
    outgoing_edge_idx: u16,
    lane_at_parent_top: u16,
    palette_slot: u8,
    is_merge_edge: bool,
}

/// Stateful Metro-line DAG session implementing incremental streaming, strict topology validation,
/// `UNRESOLVED_COMMIT_ROW` backpatching, and detached [`VirtualWipRow`] offsets.
///
/// ## Reference Map Contract
/// Callers MUST supply the complete repository-wide `RefBadgeMap` starting from the very first
/// `push_commits_chunk` call (and on all subsequent chunk calls) so primary trunk reservation on
/// lane `0` and deterministic branch lineage colors are established upfront from chunk 0.
pub struct MetroLayoutSession {
    config: LayoutSessionConfig,
    head_oid: Option<GitOid>,
    filter: GraphFilterSpec,
    commit_rows: Vec<GraphRow>,
    commit_index_by_oid: HashMap<GitOid, usize>,
    seen_oids: HashSet<GitOid>,
    wip_row: Option<VirtualWipRow>,
    lanes: Vec<Option<BranchLane>>,
    frontier_cache: Vec<BranchLane>,
    pending_by_parent: HashMap<GitOid, SmallVec<[PendingEdgeRef; 2]>>,
    saw_primary_trunk: bool,
    max_lane_width: u16,
}

impl MetroLayoutSession {
    /// Initializes an empty layout session.
    pub fn new(
        config: LayoutSessionConfig,
        head_oid: Option<GitOid>,
        filter: GraphFilterSpec,
    ) -> Self {
        Self {
            config,
            head_oid,
            filter,
            commit_rows: Vec::new(),
            commit_index_by_oid: HashMap::new(),
            seen_oids: HashSet::new(),
            wip_row: None,
            lanes: Vec::new(),
            frontier_cache: Vec::new(),
            pending_by_parent: HashMap::new(),
            saw_primary_trunk: false,
            max_lane_width: 0,
        }
    }

    fn allocate_free_lane_excluding(&mut self, min_lane: usize, exclude: Option<usize>) -> usize {
        for idx in min_lane..self.lanes.len() {
            if Some(idx) != exclude && self.lanes[idx].is_none() {
                return idx;
            }
        }
        while self.lanes.len() < min_lane {
            self.lanes.push(None);
        }
        if exclude == Some(self.lanes.len()) {
            self.lanes.push(None);
        }
        self.lanes.push(None);
        self.lanes.len() - 1
    }

    fn evict_non_trunk_from_lane_zero(&mut self) {
        let needs_eviction = self
            .lanes
            .first()
            .and_then(|s| s.as_ref())
            .is_some_and(|l| !l.is_primary_trunk);
        if !needs_eviction {
            return;
        }

        let new_lane = self.allocate_free_lane_excluding(1, None);
        let new_lane_u16 = new_lane as u16;
        let Some(mut displaced) = self.lanes[0].take() else {
            return;
        };
        displaced.lane_index = new_lane_u16;
        let target_oid = displaced.target_oid;
        self.lanes[new_lane] = Some(displaced);

        if let Some(waiting) = self.pending_by_parent.get_mut(&target_oid) {
            for pending in waiting.iter_mut() {
                if pending.lane_at_parent_top != 0 {
                    continue;
                }
                pending.lane_at_parent_top = new_lane_u16;
                let child_idx = pending.child_row as usize;
                if let Some(child_row) = self.commit_rows.get_mut(child_idx)
                    && let Some(edge) = child_row
                        .outgoing_edges
                        .get_mut(pending.outgoing_edge_idx as usize)
                {
                    edge.to_lane = new_lane_u16;
                    if edge.curve != EdgeCurveKind::OctopusSpan {
                        edge.curve = classify_outgoing_curve(
                            edge.from_lane,
                            new_lane_u16,
                            edge.is_merge_edge,
                        );
                    }
                    child_row.max_active_lane = compute_row_max_lane(
                        child_row.commit_lane,
                        &child_row.passing_lanes,
                        &child_row.incoming_edges,
                        &child_row.outgoing_edges,
                    );
                }

                for row in self.commit_rows.iter_mut().skip(child_idx + 1) {
                    let mut touched = false;
                    for seg in &mut row.passing_lanes {
                        if seg.lane_index == 0 {
                            seg.lane_index = new_lane_u16;
                            touched = true;
                        }
                    }
                    if touched {
                        row.max_active_lane = compute_row_max_lane(
                            row.commit_lane,
                            &row.passing_lanes,
                            &row.incoming_edges,
                            &row.outgoing_edges,
                        );
                    }
                }
            }
        }

        self.max_lane_width = self.max_lane_width.max(new_lane_u16);
    }

    fn cleanup_filtered_root(&mut self, root_oid: GitOid) {
        for slot in &mut self.lanes {
            if slot.as_ref().is_some_and(|l| l.target_oid == root_oid) {
                *slot = None;
            }
        }

        let Some(mut waiting) = self.pending_by_parent.remove(&root_oid) else {
            return;
        };

        // Remove highest edge index first per child_row so earlier indices remain stable within each row.
        waiting.sort_by_key(|p| (p.child_row, Reverse(p.outgoing_edge_idx)));
        for pending in waiting {
            let child_idx = pending.child_row as usize;
            let removed_idx = pending.outgoing_edge_idx;

            if let Some(child_row) = self.commit_rows.get_mut(child_idx)
                && (removed_idx as usize) < child_row.outgoing_edges.len()
            {
                child_row.outgoing_edges.remove(removed_idx as usize);
                child_row.max_active_lane = compute_row_max_lane(
                    child_row.commit_lane,
                    &child_row.passing_lanes,
                    &child_row.incoming_edges,
                    &child_row.outgoing_edges,
                );
            }

            // Re-index any remaining pending edges on the same child row whose index shifted down.
            for other in self.pending_by_parent.values_mut().flatten() {
                if other.child_row == pending.child_row && other.outgoing_edge_idx > removed_idx {
                    other.outgoing_edge_idx -= 1;
                }
            }

            let lane_has_other_pending = self
                .pending_by_parent
                .values()
                .flatten()
                .any(|p| p.lane_at_parent_top == pending.lane_at_parent_top);
            if !lane_has_other_pending {
                for row in self.commit_rows.iter_mut().skip(child_idx + 1) {
                    let prev_len = row.passing_lanes.len();
                    row.passing_lanes
                        .retain(|seg| seg.lane_index != pending.lane_at_parent_top);
                    if row.passing_lanes.len() != prev_len {
                        row.max_active_lane = compute_row_max_lane(
                            row.commit_lane,
                            &row.passing_lanes,
                            &row.incoming_edges,
                            &row.outgoing_edges,
                        );
                    }
                }
            }
        }
    }

    fn resolve_tip_identity(
        &self,
        commit: &CommitNode,
        refs_by_oid: &RefBadgeMap,
    ) -> (
        LanePriority,
        DeterministicColor,
        Option<CompactString>,
        bool,
    ) {
        let badges = refs_by_oid
            .get(&commit.oid)
            .map_or(&[][..], |v| v.as_slice());

        if commit.flags.contains(CommitFlags::ON_PRIMARY_TRUNK)
            || badges
                .iter()
                .any(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names))
        {
            let trunk_label = badges
                .iter()
                .find(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names))
                .map_or_else(|| CompactString::const_new("main"), |b| b.name.clone());
            return (
                LanePriority::PrimaryTrunk,
                METRO_PALETTE[0],
                Some(trunk_label),
                true,
            );
        }

        if let Some(local) = badges
            .iter()
            .find(|b| matches!(b.kind, RefKind::LocalBranch))
        {
            let prio = if local.is_head || self.head_oid == Some(commit.oid) {
                LanePriority::ActiveCheckedOutBranch
            } else {
                LanePriority::LocalBranch
            };
            return (
                prio,
                deterministic_branch_color(&local.full_ref),
                Some(local.name.clone()),
                false,
            );
        }

        if let Some(remote) = badges
            .iter()
            .find(|b| matches!(b.kind, RefKind::RemoteBranch { .. }))
        {
            return (
                LanePriority::RemoteBranch,
                deterministic_branch_color(&remote.full_ref),
                Some(remote.name.clone()),
                false,
            );
        }

        if commit.flags.contains(CommitFlags::IS_STASH)
            || badges.iter().any(|b| matches!(b.kind, RefKind::Stash))
        {
            return (
                LanePriority::Stash,
                deterministic_anonymous_lane_color(commit.oid),
                None,
                false,
            );
        }

        let prio = if self.head_oid == Some(commit.oid) {
            LanePriority::ActiveCheckedOutBranch
        } else {
            LanePriority::TagOrDetached
        };
        (
            prio,
            deterministic_anonymous_lane_color(commit.oid),
            None,
            false,
        )
    }

    fn resolve_secondary_parent_identity(
        &self,
        parent_oid: GitOid,
        refs_by_oid: &RefBadgeMap,
    ) -> (
        LanePriority,
        DeterministicColor,
        Option<CompactString>,
        bool,
    ) {
        let badges = refs_by_oid
            .get(&parent_oid)
            .map_or(&[][..], |v| v.as_slice());
        if let Some(trunk) = badges
            .iter()
            .find(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names))
        {
            return (
                LanePriority::PrimaryTrunk,
                METRO_PALETTE[0],
                Some(trunk.name.clone()),
                true,
            );
        }
        if let Some(branch) = badges
            .iter()
            .find(|b| matches!(b.kind, RefKind::LocalBranch | RefKind::RemoteBranch { .. }))
        {
            let prio = match branch.kind {
                RefKind::LocalBranch => LanePriority::LocalBranch,
                RefKind::RemoteBranch { .. } => LanePriority::RemoteBranch,
                _ => LanePriority::TagOrDetached,
            };
            return (
                prio,
                deterministic_branch_color(&branch.full_ref),
                Some(branch.name.clone()),
                false,
            );
        }

        // Locked-in Phase 1 Review Rule (Option A1): pure deterministic hash of parent OID.
        (
            LanePriority::TagOrDetached,
            deterministic_anonymous_lane_color(parent_oid),
            None,
            false,
        )
    }

    fn matches_filter(&self, commit: &CommitNode, refs_by_oid: &RefBadgeMap) -> bool {
        if self.filter.is_unfiltered() {
            return true;
        }
        if self.filter.hide_merges && commit.parents.len() > 1 {
            return false;
        }
        if let Some(ref prefix) = self.filter.sha_prefix
            && !commit
                .oid
                .to_hex()
                .starts_with(prefix.to_ascii_lowercase().as_str())
        {
            return false;
        }
        if let Some(ref msg_q) = self.filter.message_query {
            let needle = msg_q.to_ascii_lowercase();
            if !commit
                .summary
                .to_ascii_lowercase()
                .contains(needle.as_str())
            {
                return false;
            }
        }
        if !self.filter.author_queries.is_empty() {
            let name_lc = commit.author.name.to_ascii_lowercase();
            let email_lc = commit.author.email.to_ascii_lowercase();
            let any_author = self.filter.author_queries.iter().any(|q| {
                let q_lc = q.to_ascii_lowercase();
                name_lc.contains(q_lc.as_str()) || email_lc.contains(q_lc.as_str())
            });
            if !any_author {
                return false;
            }
        }
        if !self.filter.branch_patterns.is_empty() {
            let badges = refs_by_oid
                .get(&commit.oid)
                .map_or(&[][..], |v| v.as_slice());
            let any_branch = badges.iter().any(|b| {
                self.filter
                    .branch_patterns
                    .iter()
                    .any(|pat| b.name.contains(pat.as_str()) || b.full_ref.contains(pat.as_str()))
            });
            if !any_branch {
                return false;
            }
        }
        true
    }

    fn rebuild_frontier_cache(&mut self) {
        self.frontier_cache.clear();
        for lane in self.lanes.iter().flatten() {
            self.frontier_cache.push(lane.clone());
        }
    }

    fn process_commit(
        &mut self,
        commit: &CommitNode,
        refs_by_oid: &RefBadgeMap,
    ) -> LayoutResult<bool> {
        self.seen_oids.insert(commit.oid);

        let mut unique_parents: SmallVec<[GitOid; 2]> = SmallVec::new();
        for &p_oid in &commit.parents {
            if !unique_parents.contains(&p_oid) {
                unique_parents.push(p_oid);
            }
        }

        if !self.matches_filter(commit, refs_by_oid) {
            if let Some(&first_parent) = unique_parents.first() {
                // Re-anchor any pending edges waiting on this skipped commit to its first parent
                // so filtered views never orphan downstream edges.
                if let Some(waiting) = self.pending_by_parent.remove(&commit.oid) {
                    self.pending_by_parent
                        .entry(first_parent)
                        .or_default()
                        .extend(waiting);
                }
                for lane in self.lanes.iter_mut().flatten() {
                    if lane.target_oid == commit.oid {
                        lane.target_oid = first_parent;
                    }
                }
            } else {
                self.cleanup_filtered_root(commit.oid);
            }
            return Ok(false);
        }

        let row_idx = self.commit_rows.len() as u32;
        self.commit_index_by_oid
            .insert(commit.oid, row_idx as usize);

        let badges = refs_by_oid
            .get(&commit.oid)
            .map_or(&[][..], |v| v.as_slice());
        let is_commit_trunk = commit.flags.contains(CommitFlags::ON_PRIMARY_TRUNK)
            || badges
                .iter()
                .any(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names));
        if is_commit_trunk {
            self.saw_primary_trunk = true;
        }

        let reserve_zero = self.config.pin_primary_trunk_to_lane_zero && self.saw_primary_trunk;
        let min_non_trunk_lane = usize::from(reserve_zero);

        if is_commit_trunk && self.config.pin_primary_trunk_to_lane_zero {
            let can_claim_zero = self
                .lanes
                .first()
                .and_then(|s| s.as_ref())
                .is_none_or(|l| l.target_oid == commit.oid || !l.is_primary_trunk);
            if can_claim_zero {
                self.evict_non_trunk_from_lane_zero();
            }
        }

        // Locate all existing lanes converging onto this commit OID.
        let matching_slots: SmallVec<[usize; 4]> = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                slot.as_ref()
                    .filter(|lane| lane.target_oid == commit.oid)
                    .map(|_| idx)
            })
            .collect();

        let can_claim_lane_zero = is_commit_trunk
            && self.config.pin_primary_trunk_to_lane_zero
            && self
                .lanes
                .first()
                .and_then(|s| s.as_ref())
                .is_none_or(|l| l.target_oid == commit.oid || !l.is_primary_trunk);

        let (commit_lane, mut active_lane_state) = if can_claim_lane_zero {
            if self.lanes.is_empty() {
                self.lanes.push(None);
            }
            let existing_zero = self.lanes[0].take();
            for &other_idx in &matching_slots {
                if other_idx != 0 {
                    self.lanes[other_idx] = None;
                }
            }
            let origin_ref = badges
                .iter()
                .find(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names))
                .map(|b| b.name.clone())
                .or_else(|| existing_zero.and_then(|l| l.origin_ref))
                .or_else(|| Some(CompactString::const_new("main")));
            (
                0u16,
                BranchLane {
                    lane_index: 0,
                    target_oid: commit.oid,
                    color: METRO_PALETTE[0],
                    priority: LanePriority::PrimaryTrunk,
                    origin_ref,
                    is_primary_trunk: true,
                },
            )
        } else if !matching_slots.is_empty() {
            let best_slot = matching_slots
                .iter()
                .copied()
                .min_by_key(|&idx| {
                    let lane = self.lanes[idx].as_ref();
                    let trunk_rank = u8::from(!lane.is_some_and(|l| l.is_primary_trunk));
                    let prio = lane.map_or(LanePriority::Stash, |l| l.priority);
                    (trunk_rank, prio, idx)
                })
                .unwrap_or(matching_slots[0]);

            let chosen = self.lanes[best_slot].take();
            for &other_idx in &matching_slots {
                if other_idx != best_slot {
                    self.lanes[other_idx] = None;
                }
            }
            if let Some(mut lane) = chosen {
                if is_commit_trunk && best_slot == 0 {
                    lane.is_primary_trunk = true;
                    lane.color = METRO_PALETTE[0];
                    lane.priority = LanePriority::PrimaryTrunk;
                }
                (best_slot as u16, lane)
            } else {
                let (priority, color, origin_ref, is_trunk) =
                    self.resolve_tip_identity(commit, refs_by_oid);
                (
                    best_slot as u16,
                    BranchLane {
                        lane_index: best_slot as u16,
                        target_oid: commit.oid,
                        color,
                        priority,
                        origin_ref,
                        is_primary_trunk: is_trunk && best_slot == 0,
                    },
                )
            }
        } else {
            let (mut priority, mut color, origin_ref, is_trunk) =
                self.resolve_tip_identity(commit, refs_by_oid);
            let alloc_idx = self.allocate_free_lane_excluding(min_non_trunk_lane, None);
            let effective_trunk = is_trunk && alloc_idx == 0;
            if is_trunk && !effective_trunk {
                priority = LanePriority::LocalBranch;
                color = origin_ref.as_deref().map_or_else(
                    || deterministic_anonymous_lane_color(commit.oid),
                    deterministic_branch_color,
                );
            }

            let lane = BranchLane {
                lane_index: alloc_idx as u16,
                target_oid: commit.oid,
                color,
                priority,
                origin_ref,
                is_primary_trunk: effective_trunk,
            };
            (alloc_idx as u16, lane)
        };

        let node_palette_slot = active_lane_state.color.palette_slot;

        // Backpatch all child rows whose outgoing edges pointed to UNRESOLVED_COMMIT_ROW waiting on this OID.
        let mut incoming_edges: SmallVec<[GraphEdge; 2]> = SmallVec::new();
        if let Some(waiting_children) = self.pending_by_parent.remove(&commit.oid) {
            for pending in waiting_children {
                if let Some(child_row) = self.commit_rows.get_mut(pending.child_row as usize)
                    && let Some(out_edge) = child_row
                        .outgoing_edges
                        .get_mut(pending.outgoing_edge_idx as usize)
                {
                    out_edge.target_row = row_idx;
                }
                let curve = classify_incoming_curve(pending.lane_at_parent_top, commit_lane);
                incoming_edges.push(GraphEdge {
                    target_row: pending.child_row,
                    from_lane: pending.lane_at_parent_top,
                    to_lane: commit_lane,
                    palette_slot: pending.palette_slot,
                    curve,
                    is_merge_edge: pending.is_merge_edge,
                });
            }
        }

        // Backpatch detached WIP node if HEAD points to this commit (updating lane & color on late HEAD load).
        if let Some(wip) = self.wip_row.as_mut()
            && wip.head_oid == Some(commit.oid)
        {
            wip.head_commit_row = Some(row_idx);
            wip.commit_lane = commit_lane;
            wip.palette_slot = node_palette_slot;
            wip.max_active_lane = wip.max_active_lane.max(commit_lane);
            if let Some(head_edge) = wip.outgoing_head_edge.as_mut() {
                head_edge.target_row = row_idx;
                head_edge.from_lane = commit_lane;
                head_edge.to_lane = commit_lane;
                head_edge.palette_slot = node_palette_slot;
                head_edge.curve = EdgeCurveKind::Straight;
            }
        }

        // Record uninterrupted vertical rails passing beside this commit node.
        let mut passing_lanes: SmallVec<[LaneSegment; 8]> = SmallVec::new();
        for (slot_idx, slot) in self.lanes.iter().enumerate() {
            if slot_idx != commit_lane as usize
                && let Some(passing) = slot
            {
                passing_lanes.push(LaneSegment {
                    lane_index: slot_idx as u16,
                    palette_slot: passing.color.palette_slot,
                    is_dotted_wip: false,
                });
            }
        }

        // Route deduplicated parents downward and record pending references for UNRESOLVED_COMMIT_ROW backpatching.
        let mut outgoing_edges: SmallVec<[GraphEdge; 2]> = SmallVec::new();
        let node_kind = if commit.flags.contains(CommitFlags::IS_STASH) {
            GraphNodeKind::StashNode { stash_index: 0 }
        } else {
            match unique_parents.len() {
                0 => GraphNodeKind::InitialRoot,
                1 => GraphNodeKind::Regular,
                2 => GraphNodeKind::Merge,
                count => GraphNodeKind::OctopusMerge {
                    parent_count: u8::try_from(count).unwrap_or(u8::MAX),
                },
            }
        };

        if unique_parents.is_empty() {
            self.lanes[commit_lane as usize] = None;
        } else {
            let p0 = unique_parents[0];
            let existing_p0_slot = self.lanes.iter().enumerate().find_map(|(idx, slot)| {
                (idx != commit_lane as usize)
                    .then(|| {
                        slot.as_ref()
                            .filter(|l| {
                                l.target_oid == p0
                                    && (!active_lane_state.is_primary_trunk || l.is_primary_trunk)
                            })
                            .map(|_| idx)
                    })
                    .flatten()
            });

            let mut vacated_lane: Option<usize> = None;

            if let Some(target_idx) = existing_p0_slot {
                // First parent already has an active lane (e.g. trunk or earlier sibling branch):
                // converge onto it and vacate `commit_lane` (disallowing immediate self-fork reuse below).
                let curve = if (target_idx as u16) < commit_lane {
                    EdgeCurveKind::MergeLeft
                } else {
                    EdgeCurveKind::MergeRight
                };
                let edge_idx = outgoing_edges.len() as u16;
                outgoing_edges.push(GraphEdge {
                    target_row: UNRESOLVED_COMMIT_ROW,
                    from_lane: commit_lane,
                    to_lane: target_idx as u16,
                    palette_slot: node_palette_slot,
                    curve,
                    is_merge_edge: false,
                });
                self.pending_by_parent
                    .entry(p0)
                    .or_default()
                    .push(PendingEdgeRef {
                        child_row: row_idx,
                        outgoing_edge_idx: edge_idx,
                        lane_at_parent_top: target_idx as u16,
                        palette_slot: node_palette_slot,
                        is_merge_edge: false,
                    });
                self.lanes[commit_lane as usize] = None;
                vacated_lane = Some(commit_lane as usize);
            } else {
                active_lane_state.target_oid = p0;
                active_lane_state.lane_index = commit_lane;
                self.lanes[commit_lane as usize] = Some(active_lane_state);

                let edge_idx = outgoing_edges.len() as u16;
                outgoing_edges.push(GraphEdge {
                    target_row: UNRESOLVED_COMMIT_ROW,
                    from_lane: commit_lane,
                    to_lane: commit_lane,
                    palette_slot: node_palette_slot,
                    curve: EdgeCurveKind::Straight,
                    is_merge_edge: false,
                });
                self.pending_by_parent
                    .entry(p0)
                    .or_default()
                    .push(PendingEdgeRef {
                        child_row: row_idx,
                        outgoing_edge_idx: edge_idx,
                        lane_at_parent_top: commit_lane,
                        palette_slot: node_palette_slot,
                        is_merge_edge: false,
                    });
            }

            // Secondary merge parents (`m >= 1`).
            let is_octopus = unique_parents.len() >= 3;
            for (parent_pos, &pm_oid) in unique_parents.iter().enumerate().skip(1) {
                let existing_pm_slot = self.lanes.iter().enumerate().find_map(|(idx, slot)| {
                    slot.as_ref()
                        .filter(|l| l.target_oid == pm_oid)
                        .map(|l| (idx, l.color.palette_slot))
                });

                let (target_lane, edge_slot, curve) =
                    if let Some((existing_idx, existing_palette)) = existing_pm_slot {
                        let target_u16 = existing_idx as u16;
                        let curve = if is_octopus && parent_pos >= 2 {
                            EdgeCurveKind::OctopusSpan
                        } else if target_u16 < commit_lane {
                            EdgeCurveKind::MergeLeft
                        } else {
                            EdgeCurveKind::MergeRight
                        };
                        (target_u16, existing_palette, curve)
                    } else {
                        let (mut prio, mut color, origin_ref, is_trunk) =
                            self.resolve_secondary_parent_identity(pm_oid, refs_by_oid);
                        if is_trunk {
                            self.saw_primary_trunk = true;
                            if self.config.pin_primary_trunk_to_lane_zero {
                                self.evict_non_trunk_from_lane_zero();
                            }
                        }
                        let alloc_idx = if is_trunk
                            && self.config.pin_primary_trunk_to_lane_zero
                            && vacated_lane != Some(0)
                            && self.lanes.first().and_then(|s| s.as_ref()).is_none()
                        {
                            if self.lanes.is_empty() {
                                self.lanes.push(None);
                            }
                            0
                        } else {
                            let min_sec = usize::from(
                                self.config.pin_primary_trunk_to_lane_zero
                                    && self.saw_primary_trunk,
                            );
                            self.allocate_free_lane_excluding(min_sec, vacated_lane)
                        };
                        let effective_trunk = is_trunk && alloc_idx == 0;
                        if is_trunk && !effective_trunk {
                            prio = LanePriority::LocalBranch;
                            color = deterministic_anonymous_lane_color(pm_oid);
                        }
                        let new_lane_u16 = alloc_idx as u16;
                        self.lanes[alloc_idx] = Some(BranchLane {
                            lane_index: new_lane_u16,
                            target_oid: pm_oid,
                            color,
                            priority: prio,
                            origin_ref,
                            is_primary_trunk: effective_trunk,
                        });
                        let curve = if is_octopus && parent_pos >= 2 {
                            EdgeCurveKind::OctopusSpan
                        } else if new_lane_u16 < commit_lane {
                            EdgeCurveKind::ForkLeft
                        } else {
                            EdgeCurveKind::ForkRight
                        };
                        (new_lane_u16, color.palette_slot, curve)
                    };

                let edge_idx = outgoing_edges.len() as u16;
                outgoing_edges.push(GraphEdge {
                    target_row: UNRESOLVED_COMMIT_ROW,
                    from_lane: commit_lane,
                    to_lane: target_lane,
                    palette_slot: edge_slot,
                    curve,
                    is_merge_edge: true,
                });
                self.pending_by_parent
                    .entry(pm_oid)
                    .or_default()
                    .push(PendingEdgeRef {
                        child_row: row_idx,
                        outgoing_edge_idx: edge_idx,
                        lane_at_parent_top: target_lane,
                        palette_slot: edge_slot,
                        is_merge_edge: true,
                    });
            }
        }

        let row_max_lane = compute_row_max_lane(
            commit_lane,
            &passing_lanes,
            &incoming_edges,
            &outgoing_edges,
        );
        self.max_lane_width = self.max_lane_width.max(row_max_lane);
        self.commit_rows.push(GraphRow {
            oid: commit.oid,
            commit_lane,
            max_active_lane: row_max_lane,
            node_kind,
            node_palette_slot,
            passing_lanes,
            incoming_edges,
            outgoing_edges,
        });

        Ok(true)
    }
}

impl IncrementalLayoutSession for MetroLayoutSession {
    fn push_commits_chunk(
        &mut self,
        commits: &[CommitNode],
        refs_by_oid: &RefBadgeMap,
    ) -> LayoutResult<std::ops::Range<usize>> {
        // Atomic pre-validation: reject clock-skew / self-parent / duplicate OIDs before mutating session.
        let mut chunk_seen = HashSet::with_capacity(commits.len());
        for commit in commits {
            if self.seen_oids.contains(&commit.oid) || !chunk_seen.insert(commit.oid) {
                return Err(LayoutError::InvalidTopologyOrder {
                    oid: commit.oid,
                    reason: "duplicate commit OID encountered during topological layout",
                });
            }
            for parent_oid in &commit.parents {
                if *parent_oid == commit.oid {
                    return Err(LayoutError::InvalidTopologyOrder {
                        oid: commit.oid,
                        reason: "commit cannot list itself as a parent",
                    });
                }
                if self.seen_oids.contains(parent_oid) || chunk_seen.contains(parent_oid) {
                    return Err(LayoutError::InvalidTopologyOrder {
                        oid: commit.oid,
                        reason: "parent commit appeared before child commit in stream",
                    });
                }
            }
        }

        // Cache primary-trunk presence once per chunk instead of scanning `refs_by_oid` per commit.
        if !self.saw_primary_trunk
            && refs_by_oid
                .values()
                .flatten()
                .any(|b| is_primary_trunk_badge(b, &self.config.primary_trunk_names))
        {
            self.saw_primary_trunk = true;
        }

        let start_idx = self.commit_rows.len();
        for commit in commits {
            let _ = self.process_commit(commit, refs_by_oid)?;
        }
        self.rebuild_frontier_cache();
        Ok(start_idx..self.commit_rows.len())
    }

    fn upsert_virtual_wip(
        &mut self,
        head_oid: Option<GitOid>,
        status: &WorkingTreeStatusView,
        refs_by_oid: &RefBadgeMap,
    ) -> LayoutResult<()> {
        self.head_oid = head_oid;
        if status.total_dirty_count() == 0 {
            self.remove_virtual_wip();
            return Ok(());
        }

        let head_commit_row = head_oid.and_then(|oid| self.commit_index_by_oid.get(&oid).copied());
        let (commit_lane, palette_slot) = if let Some(idx) = head_commit_row {
            let row = &self.commit_rows[idx];
            (row.commit_lane, row.node_palette_slot)
        } else if let Some(head) = head_oid {
            let badges = refs_by_oid.get(&head).map_or(&[][..], |v| v.as_slice());
            let slot = badges
                .iter()
                .find(|b| b.is_head || matches!(b.kind, RefKind::LocalBranch))
                .map_or(METRO_PALETTE[0].palette_slot, |b| {
                    deterministic_branch_color(&b.full_ref).palette_slot
                });
            (0, slot)
        } else {
            (0, METRO_PALETTE[0].palette_slot)
        };

        let outgoing_head_edge = head_oid.map(|_| GraphEdge {
            target_row: head_commit_row.map_or(UNRESOLVED_COMMIT_ROW, |idx| idx as u32),
            from_lane: commit_lane,
            to_lane: commit_lane,
            palette_slot,
            curve: EdgeCurveKind::Straight,
            is_merge_edge: false,
        });

        self.wip_row = Some(VirtualWipRow {
            head_oid,
            head_commit_row: head_commit_row.map(|idx| idx as u32),
            commit_lane,
            max_active_lane: commit_lane,
            palette_slot,
            staged_count: status.staged.len() as u32,
            unstaged_count: status.unstaged.len() as u32,
            conflicted_count: status.conflicted.len() as u32,
            untracked_count: status
                .unstaged
                .iter()
                .filter(|d| matches!(d.status, branvio_core::domain::DeltaStatus::Untracked))
                .count() as u32,
            passing_lanes: SmallVec::new(),
            outgoing_head_edge,
        });

        Ok(())
    }

    fn remove_virtual_wip(&mut self) {
        self.wip_row = None;
    }

    #[inline]
    fn wip_row(&self) -> Option<&VirtualWipRow> {
        self.wip_row.as_ref()
    }

    fn commit_rows_slice(&self, range: std::ops::Range<usize>) -> &[GraphRow] {
        let len = self.commit_rows.len();
        let start = range.start.min(len);
        let end = range.end.min(len);
        if start >= end {
            &[]
        } else {
            &self.commit_rows[start..end]
        }
    }

    #[inline]
    fn commit_row_count(&self) -> usize {
        self.commit_rows.len()
    }

    #[inline]
    fn display_row_count(&self) -> usize {
        self.commit_rows.len() + usize::from(self.wip_row.is_some())
    }

    fn display_row_at(&self, display_index: usize) -> Option<crate::traits::ViewportRowRef<'_>> {
        if let Some(wip) = self.wip_row.as_ref() {
            if display_index == 0 {
                return Some(crate::traits::ViewportRowRef::Wip(wip));
            }
            let commit_index = display_index - 1;
            self.commit_rows
                .get(commit_index)
                .map(|row| crate::traits::ViewportRowRef::Commit { commit_index, row })
        } else {
            self.commit_rows
                .get(display_index)
                .map(|row| crate::traits::ViewportRowRef::Commit {
                    commit_index: display_index,
                    row,
                })
        }
    }

    #[inline]
    fn commit_index_for_oid(&self, oid: GitOid) -> Option<usize> {
        self.commit_index_by_oid.get(&oid).copied()
    }

    #[inline]
    fn display_row_for_oid(&self, oid: GitOid) -> Option<usize> {
        self.commit_index_by_oid
            .get(&oid)
            .map(|&idx| idx + usize::from(self.wip_row.is_some()))
    }

    #[inline]
    fn active_frontier_lanes(&self) -> &[BranchLane] {
        &self.frontier_cache
    }

    #[inline]
    fn max_lane_width(&self) -> u16 {
        self.max_lane_width
    }
}

#[inline]
fn classify_incoming_curve(from_lane: u16, to_lane: u16) -> EdgeCurveKind {
    if from_lane == to_lane {
        EdgeCurveKind::Straight
    } else if from_lane > to_lane {
        EdgeCurveKind::MergeLeft
    } else {
        EdgeCurveKind::MergeRight
    }
}

#[inline]
fn classify_outgoing_curve(from_lane: u16, to_lane: u16, is_merge_edge: bool) -> EdgeCurveKind {
    if from_lane == to_lane {
        EdgeCurveKind::Straight
    } else if is_merge_edge {
        if to_lane < from_lane {
            EdgeCurveKind::ForkLeft
        } else {
            EdgeCurveKind::ForkRight
        }
    } else if to_lane < from_lane {
        EdgeCurveKind::MergeLeft
    } else {
        EdgeCurveKind::MergeRight
    }
}

fn compute_row_max_lane(
    commit_lane: u16,
    passing: &[LaneSegment],
    incoming: &[GraphEdge],
    outgoing: &[GraphEdge],
) -> u16 {
    let mut max_lane = commit_lane;
    for seg in passing {
        max_lane = max_lane.max(seg.lane_index);
    }
    for edge in incoming {
        max_lane = max_lane.max(edge.from_lane).max(edge.to_lane);
    }
    for edge in outgoing {
        max_lane = max_lane.max(edge.from_lane).max(edge.to_lane);
    }
    max_lane
}

fn matches_trunk_short_name(branch_name: &str, configured_trunks: &[CompactString]) -> bool {
    matches!(branch_name, "main" | "master" | "trunk")
        || configured_trunks.iter().any(|t| t.as_str() == branch_name)
}

fn is_primary_trunk_badge(badge: &RefBadge, configured_trunks: &[CompactString]) -> bool {
    match &badge.kind {
        RefKind::LocalBranch => matches_trunk_short_name(&badge.name, configured_trunks),
        RefKind::RemoteBranch { remote } => {
            let stripped = badge
                .name
                .strip_prefix(remote.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(badge.name.as_str());
            matches_trunk_short_name(stripped, configured_trunks)
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::traits::ViewportRowRef;
    use branvio_core::domain::{
        CommitSignatureState, CompactSignature, DeltaArea, DeltaStatus, FileDelta, FileMode,
        RefBadge,
    };
    use branvio_core::paths::RepoRelPath;
    use std::sync::Arc;

    fn oid(tag: u8) -> GitOid {
        let mut bytes = [0u8; 20];
        bytes[0] = tag;
        bytes[19] = tag.wrapping_mul(17).wrapping_add(1);
        GitOid::from_sha1_bytes(bytes)
    }

    fn make_commit(id: GitOid, parents: &[GitOid], ts: i64, flags: CommitFlags) -> CommitNode {
        CommitNode {
            oid: id,
            parents: SmallVec::from_slice(parents),
            tree_oid: id,
            author: CompactSignature {
                name: CompactString::const_new("Dev"),
                email: CompactString::const_new("dev@branvio.rs"),
                timestamp_secs: ts,
                tz_offset_mins: 0,
            },
            committer: CompactSignature {
                name: CompactString::const_new("Dev"),
                email: CompactString::const_new("dev@branvio.rs"),
                timestamp_secs: ts,
                tz_offset_mins: 0,
            },
            summary: CompactString::from("commit"),
            has_extended_body: false,
            signature_state: CommitSignatureState::Unsigned,
            flags,
            generation: 1,
        }
    }

    fn make_branch_badge(name: &str, full_ref: &str, is_head: bool) -> RefBadge {
        RefBadge {
            name: CompactString::from(name),
            full_ref: CompactString::from(full_ref),
            kind: RefKind::LocalBranch,
            is_head,
            upstream_divergence: None,
        }
    }

    #[test]
    fn linear_trunk_stays_pinned_on_lane_zero_and_backpatches_rows() {
        let c1 = oid(1);
        let c2 = oid(2);
        let c3 = oid(3);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            c3,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );

        let commits = vec![
            make_commit(
                c3,
                &[c2],
                300,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
            ),
            make_commit(c2, &[c1], 200, CommitFlags::ON_PRIMARY_TRUNK),
            make_commit(
                c1,
                &[],
                100,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(c3),
            GraphFilterSpec::default(),
        );
        let range = session.push_commits_chunk(&commits, &refs).unwrap();
        assert_eq!(range, 0..3);

        let rows = session.commit_rows_slice(0..3);
        assert!(rows.iter().all(|r| r.commit_lane == 0));
        assert!(
            rows.iter()
                .all(|r| r.node_palette_slot == METRO_PALETTE[0].palette_slot)
        );
        assert_eq!(rows[0].outgoing_edges[0].target_row, 1);
        assert_eq!(rows[1].outgoing_edges[0].target_row, 2);
        assert!(rows[2].outgoing_edges.is_empty());
        assert_eq!(rows[2].node_kind, GraphNodeKind::InitialRoot);
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn multi_chunk_streaming_backpatches_unresolved_commit_rows_across_chunks() {
        let c1 = oid(10);
        let c2 = oid(11);
        let c3 = oid(12);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            c3,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(c3),
            GraphFilterSpec::default(),
        );

        // Chunk 1 ends at c3; c2 has not arrived yet so outgoing target_row must be UNRESOLVED_COMMIT_ROW.
        session
            .push_commits_chunk(
                &[make_commit(c3, &[c2], 300, CommitFlags::ON_PRIMARY_TRUNK)],
                &refs,
            )
            .unwrap();
        assert_eq!(
            session.commit_rows_slice(0..1)[0].outgoing_edges[0].target_row,
            UNRESOLVED_COMMIT_ROW
        );

        // Chunk 2 arrives with c2 & c1 -> c3's outgoing edge MUST backpatch to row 1!
        session
            .push_commits_chunk(
                &[
                    make_commit(c2, &[c1], 200, CommitFlags::ON_PRIMARY_TRUNK),
                    make_commit(c1, &[], 100, CommitFlags::ON_PRIMARY_TRUNK),
                ],
                &refs,
            )
            .unwrap();

        let rows = session.commit_rows_slice(0..3);
        assert_eq!(rows[0].outgoing_edges[0].target_row, 1);
        assert_eq!(rows[1].outgoing_edges[0].target_row, 2);
        assert_eq!(rows[1].incoming_edges[0].target_row, 0);
        assert_eq!(rows[2].incoming_edges[0].target_row, 1);
    }

    #[test]
    fn octopus_merge_four_parents_allocates_lanes_and_reclaims_at_fork_base() {
        // Topology:
        //   M (parents: [B, P1, P2, P3]) -- octopus merge on main
        //   P1 -> B, P2 -> B, P3 -> B
        //   B (root)
        let base = oid(20);
        let p1 = oid(21);
        let p2 = oid(22);
        let p3 = oid(23);
        let merge = oid(24);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            merge,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );

        let commits = vec![
            make_commit(
                merge,
                &[base, p1, p2, p3],
                500,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_MERGE | CommitFlags::IS_OCTOPUS,
            ),
            make_commit(p3, &[base], 450, CommitFlags::empty()),
            make_commit(p2, &[base], 440, CommitFlags::empty()),
            make_commit(p1, &[base], 430, CommitFlags::empty()),
            make_commit(
                base,
                &[],
                100,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(merge),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..5);
        assert_eq!(rows[0].commit_lane, 0);
        assert_eq!(
            rows[0].node_kind,
            GraphNodeKind::OctopusMerge { parent_count: 4 }
        );
        assert_eq!(rows[0].outgoing_edges.len(), 4);

        // Verify anonymous secondary parents use deterministic_anonymous_lane_color(parent_oid) in slots 4..=15.
        assert_eq!(
            rows[0].outgoing_edges[1].palette_slot,
            deterministic_anonymous_lane_color(p1).palette_slot
        );
        assert_eq!(
            rows[0].outgoing_edges[2].palette_slot,
            deterministic_anonymous_lane_color(p2).palette_slot
        );
        assert_eq!(
            rows[0].outgoing_edges[3].palette_slot,
            deterministic_anonymous_lane_color(p3).palette_slot
        );
        assert_eq!(rows[0].outgoing_edges[2].curve, EdgeCurveKind::OctopusSpan);
        assert_eq!(rows[0].outgoing_edges[3].curve, EdgeCurveKind::OctopusSpan);

        // Every edge backpatched cleanly (zero UNRESOLVED_COMMIT_ROW remaining).
        for row in rows {
            for edge in &row.outgoing_edges {
                assert_ne!(edge.target_row, UNRESOLVED_COMMIT_ROW);
            }
        }
        // All 4 paths converge at root `base` -> zero open frontier lanes at bottom.
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn criss_cross_merge_reuses_shared_parent_lanes_without_duplicate_tracks() {
        // Criss-cross DAG:
        //       base
        //      /    \
        //     A1     B1
        //     |\    /|
        //     | \  / |
        //     |  \/  |
        //     |  /\  |
        //     M_A    M_B
        // M_A parents: [A1, B1], M_B parents: [B1, A1]
        let base = oid(30);
        let a1 = oid(31);
        let b1 = oid(32);
        let ma = oid(33);
        let mb = oid(34);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            ma,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        refs.insert(
            mb,
            SmallVec::from_elem(make_branch_badge("feat/cc", "refs/heads/feat/cc", false), 1),
        );

        let commits = vec![
            make_commit(
                ma,
                &[a1, b1],
                500,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_MERGE,
            ),
            make_commit(mb, &[b1, a1], 490, CommitFlags::IS_MERGE),
            make_commit(a1, &[base], 300, CommitFlags::ON_PRIMARY_TRUNK),
            make_commit(b1, &[base], 290, CommitFlags::empty()),
            make_commit(base, &[], 100, CommitFlags::ON_PRIMARY_TRUNK),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(ma),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..5);
        // M_A opened lane 0 (A1) & lane 1 (B1); M_B on lane 2 MUST reuse lane 1 for B1 and lane 0 for A1.
        assert!(session.max_lane_width() <= 2);
        assert_eq!(rows[0].outgoing_edges[0].target_row, 2); // M_A -> A1
        assert_eq!(rows[0].outgoing_edges[1].target_row, 3); // M_A -> B1
        assert_eq!(rows[1].outgoing_edges[0].target_row, 3); // M_B -> B1
        assert_eq!(rows[1].outgoing_edges[1].target_row, 2); // M_B -> A1
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn orphan_branches_layout_independently_and_terminate_cleanly() {
        let root_main = oid(40);
        let tip_main = oid(41);
        let root_orphan = oid(42);
        let tip_orphan = oid(43);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            tip_main,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        refs.insert(
            tip_orphan,
            SmallVec::from_elem(
                make_branch_badge("gh-pages", "refs/heads/gh-pages", false),
                1,
            ),
        );

        let commits = vec![
            make_commit(tip_main, &[root_main], 400, CommitFlags::ON_PRIMARY_TRUNK),
            make_commit(tip_orphan, &[root_orphan], 350, CommitFlags::empty()),
            make_commit(
                root_main,
                &[],
                200,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
            make_commit(root_orphan, &[], 150, CommitFlags::IS_ORPHAN_ROOT),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(tip_main),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..4);
        assert_eq!(rows[0].commit_lane, 0);
        assert_eq!(rows[1].commit_lane, 1);
        assert_eq!(rows[2].commit_lane, 0);
        assert_eq!(rows[2].node_kind, GraphNodeKind::InitialRoot);
        assert_eq!(rows[3].commit_lane, 1);
        assert_eq!(rows[3].node_kind, GraphNodeKind::InitialRoot);
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn clock_skew_topological_stream_succeeds_and_non_topological_stream_errors() {
        // Parent commit has a FUTURE timestamp (year 2030) while child has a PAST timestamp (year 2020).
        let parent_future = oid(50);
        let child_past = oid(51);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            child_past,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );

        let valid_topo_order = vec![
            make_commit(
                child_past,
                &[parent_future],
                1_577_836_800,
                CommitFlags::ON_PRIMARY_TRUNK,
            ),
            make_commit(
                parent_future,
                &[],
                1_893_456_000,
                CommitFlags::ON_PRIMARY_TRUNK,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut ok_session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(child_past),
            GraphFilterSpec::default(),
        );
        ok_session
            .push_commits_chunk(&valid_topo_order, &refs)
            .unwrap();
        assert_eq!(ok_session.commit_row_count(), 2);
        assert_eq!(
            ok_session.commit_rows_slice(0..2)[0].outgoing_edges[0].target_row,
            1
        );

        // Feeding parent BEFORE child (broken timestamp-only order) MUST fail with InvalidTopologyOrder.
        let broken_time_order = vec![
            make_commit(
                parent_future,
                &[],
                1_893_456_000,
                CommitFlags::ON_PRIMARY_TRUNK,
            ),
            make_commit(
                child_past,
                &[parent_future],
                1_577_836_800,
                CommitFlags::ON_PRIMARY_TRUNK,
            ),
        ];
        let mut err_session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(child_past),
            GraphFilterSpec::default(),
        );
        let err = err_session
            .push_commits_chunk(&broken_time_order, &refs)
            .unwrap_err();
        assert!(matches!(
            err,
            LayoutError::InvalidTopologyOrder { oid, .. } if oid == child_past
        ));
    }

    #[test]
    fn detached_virtual_wip_toggles_in_o1_without_mutating_commit_row_targets() {
        let c1 = oid(60);
        let c2 = oid(61);
        let mut refs = RefBadgeMap::new();
        refs.insert(
            c2,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );

        let commits = vec![
            make_commit(
                c2,
                &[c1],
                200,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
            ),
            make_commit(c1, &[], 100, CommitFlags::ON_PRIMARY_TRUNK),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(c2),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        assert_eq!(session.display_row_count(), 2);
        assert_eq!(session.display_row_for_oid(c2), Some(0));

        let dirty = WorkingTreeStatusView {
            unstaged: Arc::from([FileDelta {
                old_path: None,
                new_path: RepoRelPath::normalize("src/lib.rs").unwrap(),
                status: DeltaStatus::Modified,
                area: DeltaArea::Unstaged,
                old_mode: FileMode::Regular644,
                new_mode: FileMode::Regular644,
                old_oid: None,
                new_oid: None,
                is_binary: false,
                lfs: None,
                eol_change: branvio_core::domain::EolChange::Unchanged,
                additions: 1,
                deletions: 0,
            }]),
            staged: Arc::from([]),
            conflicted: Arc::from([]),
        };

        session.upsert_virtual_wip(Some(c2), &dirty, &refs).unwrap();
        assert_eq!(session.display_row_count(), 3);
        assert_eq!(session.display_row_for_oid(c2), Some(1));
        assert!(matches!(
            session.display_row_at(0),
            Some(ViewportRowRef::Wip(_))
        ));
        assert!(matches!(
            session.display_row_at(1),
            Some(ViewportRowRef::Commit {
                commit_index: 0,
                ..
            })
        ));
        // Historical edge target_row remains immutable (1 = c1 in commit_rows).
        assert_eq!(
            session.commit_rows_slice(0..2)[0].outgoing_edges[0].target_row,
            1
        );

        session.remove_virtual_wip();
        assert_eq!(session.display_row_count(), 2);
        assert_eq!(session.display_row_for_oid(c2), Some(0));
    }

    #[test]
    fn newer_feature_branch_ahead_of_main_keeps_main_pinned_to_lane_zero() {
        // Topology (ordered newest-first):
        //   F1 (feature, newer) -> B0
        //   M1 (main, older than F1) -> B0
        //   B0 (shared ancestor on main)
        let b0 = oid(70);
        let m1 = oid(71);
        let f1 = oid(72);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            m1,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        refs.insert(
            f1,
            SmallVec::from_elem(make_branch_badge("feat/ui", "refs/heads/feat/ui", false), 1),
        );

        let commits = vec![
            make_commit(f1, &[b0], 300, CommitFlags::empty()),
            make_commit(
                m1,
                &[b0],
                200,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
            ),
            make_commit(
                b0,
                &[],
                100,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(m1),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..3);
        assert_eq!(rows[0].commit_lane, 1); // F1 on lane 1
        assert_eq!(rows[1].commit_lane, 0); // M1 (main) on lane 0
        assert!(rows[1].passing_lanes.iter().any(|seg| seg.lane_index == 1));
        assert_eq!(rows[2].commit_lane, 0); // Shared base B0 stays pinned on lane 0!
        assert_eq!(rows[0].outgoing_edges[0].target_row, 2);
        assert_eq!(rows[1].outgoing_edges[0].target_row, 2);
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn filter_skipping_root_commit_cleans_ghost_lanes_and_unresolved_edges() {
        // Topology: a(keep) -> b(drop) -> r(drop, root); then c(keep, root)
        // Plus a merge case: m(keep, parents: [r, c]) so removal re-indexes remaining edge cleanly.
        let r = oid(80);
        let b = oid(81);
        let a = oid(82);
        let c = oid(83);
        let m = oid(84);

        let mk = |id, parents, ts, msg: &str| {
            let mut node = make_commit(id, parents, ts, CommitFlags::empty());
            node.summary = CompactString::from(msg);
            node
        };

        let commits = vec![
            mk(m, &[b, c], 500, "keep: merge m"),
            mk(a, &[b], 400, "keep: child a"),
            mk(b, &[r], 300, "drop: middle b"),
            mk(r, &[], 200, "drop: root r"),
            mk(c, &[], 100, "keep: root c"),
        ];

        let filter = GraphFilterSpec {
            message_query: Some(CompactString::const_new("keep")),
            ..GraphFilterSpec::default()
        };
        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(LayoutSessionConfig::default(), None, filter);
        session
            .push_commits_chunk(&commits, &RefBadgeMap::new())
            .unwrap();

        assert_eq!(session.commit_row_count(), 3);
        let rows = session.commit_rows_slice(0..3);
        // m's first parent edge originally waited on `b -> r` (which was dropped as root) and
        // MUST be removed while its second parent edge to `c` stays valid and backpatched to row 2!
        assert_eq!(rows[0].outgoing_edges.len(), 1);
        assert_eq!(rows[0].outgoing_edges[0].target_row, 2);
        // `a` waited on `b -> r` (dropped root), so `a` must have zero dangling UNRESOLVED edges.
        assert!(rows[1].outgoing_edges.is_empty());
        // `c` should have zero ghost passing lanes from dropped `r`.
        for edge in rows.iter().flat_map(|row| &row.outgoing_edges) {
            assert_ne!(edge.target_row, UNRESOLVED_COMMIT_ROW);
        }
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn feature_slash_main_and_tag_main_are_not_treated_as_primary_trunk() {
        let c1 = oid(90);
        let c2 = oid(91);
        let c3 = oid(92);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            c3,
            SmallVec::from_elem(
                make_branch_badge("feature/main", "refs/heads/feature/main", false),
                1,
            ),
        );
        refs.insert(
            c2,
            SmallVec::from_elem(
                RefBadge {
                    name: CompactString::const_new("origin/feature/main"),
                    full_ref: CompactString::const_new("refs/remotes/origin/feature/main"),
                    kind: RefKind::RemoteBranch {
                        remote: CompactString::const_new("origin"),
                    },
                    is_head: false,
                    upstream_divergence: None,
                },
                1,
            ),
        );
        refs.insert(
            c1,
            SmallVec::from_elem(
                RefBadge {
                    name: CompactString::const_new("main"),
                    full_ref: CompactString::const_new("refs/tags/main"),
                    kind: RefKind::Tag { is_annotated: true },
                    is_head: false,
                    upstream_divergence: None,
                },
                1,
            ),
        );

        let commits = vec![
            make_commit(c3, &[c1], 300, CommitFlags::empty()),
            make_commit(c2, &[c1], 200, CommitFlags::empty()),
            make_commit(c1, &[], 100, CommitFlags::empty()),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            None,
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..3);
        let expected_slot = deterministic_branch_color("refs/heads/feature/main").palette_slot;
        assert_ne!(expected_slot, METRO_PALETTE[0].palette_slot);
        // Trunkless repo still starts on lane 0, but uses feature color (never trunk slot 0).
        assert_eq!(rows[0].commit_lane, 0);
        assert_eq!(rows[0].node_palette_slot, expected_slot);
        assert_ne!(rows[1].node_palette_slot, METRO_PALETTE[0].palette_slot);
    }

    #[test]
    fn trunk_arriving_in_later_chunk_evicts_lane_zero_cleanly() {
        let b0 = oid(100);
        let f1 = oid(101);
        let f2 = oid(102);
        let m1 = oid(103);

        // Chunk 1 has no trunk refs in refs_by_oid yet -> f2 starts on lane 0.
        let mut refs_chunk1 = RefBadgeMap::new();
        refs_chunk1.insert(
            f2,
            SmallVec::from_elem(make_branch_badge("feat/a", "refs/heads/feat/a", false), 1),
        );

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(m1),
            GraphFilterSpec::default(),
        );
        session
            .push_commits_chunk(
                &[make_commit(f2, &[f1], 400, CommitFlags::empty())],
                &refs_chunk1,
            )
            .unwrap();
        assert_eq!(session.commit_rows_slice(0..1)[0].commit_lane, 0);

        // Chunk 2 introduces `main` -> feature lane MUST be evicted from lane 0 to lane 1 so `main` claims lane 0!
        let mut refs_chunk2 = refs_chunk1;
        refs_chunk2.insert(
            m1,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        session
            .push_commits_chunk(
                &[
                    make_commit(
                        m1,
                        &[b0],
                        300,
                        CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
                    ),
                    make_commit(f1, &[b0], 200, CommitFlags::empty()),
                    make_commit(
                        b0,
                        &[],
                        100,
                        CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
                    ),
                ],
                &refs_chunk2,
            )
            .unwrap();

        let rows = session.commit_rows_slice(0..4);
        assert_eq!(rows[0].outgoing_edges[0].from_lane, 0);
        assert_eq!(rows[0].outgoing_edges[0].to_lane, 1);
        assert_eq!(rows[0].outgoing_edges[0].curve, EdgeCurveKind::MergeRight);
        assert_eq!(rows[0].max_active_lane, 1);

        assert_eq!(rows[1].commit_lane, 0);
        assert_eq!(rows[1].node_palette_slot, METRO_PALETTE[0].palette_slot);
        assert!(rows[1].passing_lanes.iter().any(|seg| seg.lane_index == 1));
        assert_eq!(rows[2].commit_lane, 1);
        assert_eq!(rows[2].max_active_lane, 1);
        assert_eq!(rows[2].outgoing_edges[0].from_lane, 1);
        assert_eq!(rows[2].outgoing_edges[0].to_lane, 0);
        assert_eq!(rows[2].outgoing_edges[0].curve, EdgeCurveKind::MergeLeft);
        assert_eq!(rows[3].commit_lane, 0);
        for row in rows {
            for edge in &row.outgoing_edges {
                assert_ne!(edge.target_row, UNRESOLVED_COMMIT_ROW);
            }
        }
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn converging_lanes_include_freed_incoming_lane_in_max_active_lane() {
        let b0 = oid(110);
        let m1 = oid(111);
        let f1 = oid(112);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            m1,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        refs.insert(
            f1,
            SmallVec::from_elem(make_branch_badge("feat/x", "refs/heads/feat/x", false), 1),
        );

        let commits = vec![
            make_commit(f1, &[b0], 300, CommitFlags::empty()),
            make_commit(
                m1,
                &[b0],
                200,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
            ),
            make_commit(
                b0,
                &[],
                100,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(m1),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..3);
        assert_eq!(rows[2].commit_lane, 0);
        assert!(rows[2].passing_lanes.is_empty());
        assert_eq!(rows[2].incoming_edges.len(), 2);
        assert!(rows[2].incoming_edges.iter().any(|e| {
            e.from_lane == 1 && e.to_lane == 0 && e.curve == EdgeCurveKind::MergeLeft
        }));
        assert!(
            rows[2].incoming_edges.iter().any(|e| {
                e.from_lane == 0 && e.to_lane == 0 && e.curve == EdgeCurveKind::Straight
            })
        );
        assert_eq!(rows[2].max_active_lane, 1);
    }

    #[test]
    fn merge_converging_p0_does_not_fork_p1_onto_same_vacated_lane() {
        // Topology: main has b0 <- side <- m0 (so b0 is already on lane 0 when `fm` arrives on lane 1);
        // `fm` has parents [b0, side] (p0 converges 1 -> 0; p1 must fork onto lane 2, NEVER lane 1!).
        let b0 = oid(120);
        let side = oid(121);
        let fm = oid(122);
        let m0 = oid(123);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            m0,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", true), 1),
        );
        refs.insert(
            fm,
            SmallVec::from_elem(make_branch_badge("feat/m", "refs/heads/feat/m", false), 1),
        );

        let commits = vec![
            make_commit(
                m0,
                &[b0],
                400,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_HEAD,
            ),
            make_commit(fm, &[b0, side], 300, CommitFlags::IS_MERGE),
            make_commit(side, &[b0], 200, CommitFlags::empty()),
            make_commit(
                b0,
                &[],
                100,
                CommitFlags::ON_PRIMARY_TRUNK | CommitFlags::IS_ORPHAN_ROOT,
            ),
        ];

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(m0),
            GraphFilterSpec::default(),
        );
        session.push_commits_chunk(&commits, &refs).unwrap();

        let rows = session.commit_rows_slice(0..4);
        assert_eq!(rows[1].commit_lane, 1);
        assert_eq!(rows[1].outgoing_edges[0].from_lane, 1);
        assert_eq!(rows[1].outgoing_edges[0].to_lane, 0);
        assert_eq!(rows[1].outgoing_edges[0].curve, EdgeCurveKind::MergeLeft);
        // Secondary fork MUST NOT reuse lane 1 (from_lane != to_lane).
        assert_ne!(
            rows[1].outgoing_edges[1].from_lane,
            rows[1].outgoing_edges[1].to_lane
        );
        assert_eq!(rows[1].outgoing_edges[1].to_lane, 2);
        assert_eq!(rows[1].outgoing_edges[1].curve, EdgeCurveKind::ForkRight);
        assert!(session.active_frontier_lanes().is_empty());
    }

    #[test]
    fn wip_row_updates_lane_and_color_when_head_loads_in_later_chunk() {
        let b0 = oid(130);
        let m1 = oid(131);
        let f1 = oid(132);

        let mut refs = RefBadgeMap::new();
        refs.insert(
            m1,
            SmallVec::from_elem(make_branch_badge("main", "refs/heads/main", false), 1),
        );
        refs.insert(
            f1,
            SmallVec::from_elem(
                make_branch_badge("feat/wip", "refs/heads/feat/wip", true),
                1,
            ),
        );

        let dirty = WorkingTreeStatusView {
            unstaged: Arc::from([FileDelta {
                old_path: None,
                new_path: RepoRelPath::normalize("a.rs").unwrap(),
                status: DeltaStatus::Modified,
                area: DeltaArea::Unstaged,
                old_mode: FileMode::Regular644,
                new_mode: FileMode::Regular644,
                old_oid: None,
                new_oid: None,
                is_binary: false,
                lfs: None,
                eol_change: branvio_core::domain::EolChange::Unchanged,
                additions: 1,
                deletions: 0,
            }]),
            staged: Arc::from([]),
            conflicted: Arc::from([]),
        };

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            Some(f1),
            GraphFilterSpec::default(),
        );

        // Set WIP before f1 has arrived; chunk 1 only loads m1 on lane 0.
        session
            .push_commits_chunk(
                &[make_commit(m1, &[b0], 300, CommitFlags::ON_PRIMARY_TRUNK)],
                &refs,
            )
            .unwrap();
        session
            .upsert_virtual_wip(Some(f1), &dirty, &RefBadgeMap::new())
            .unwrap();
        assert_eq!(session.wip_row().unwrap().head_commit_row, None);

        // Chunk 2 loads f1 on lane 1 -> WIP row MUST update lane, color, and outgoing edge!
        session
            .push_commits_chunk(
                &[
                    make_commit(f1, &[b0], 200, CommitFlags::IS_HEAD),
                    make_commit(b0, &[], 100, CommitFlags::ON_PRIMARY_TRUNK),
                ],
                &refs,
            )
            .unwrap();

        let wip = session.wip_row().unwrap();
        let f1_row = &session.commit_rows_slice(0..3)[1];
        assert_eq!(f1_row.commit_lane, 1);
        assert_eq!(wip.head_commit_row, Some(1));
        assert_eq!(wip.commit_lane, 1);
        assert_eq!(wip.palette_slot, f1_row.node_palette_slot);
        let head_edge = wip.outgoing_head_edge.unwrap();
        assert_eq!(head_edge.target_row, 1);
        assert_eq!(head_edge.from_lane, 1);
        assert_eq!(head_edge.to_lane, 1);
        assert_eq!(head_edge.palette_slot, f1_row.node_palette_slot);
    }

    #[test]
    fn invalid_mid_chunk_commit_leaves_session_atomic_and_rejects_self_parent() {
        let c0 = oid(140);
        let c1 = oid(141);
        let c2 = oid(142);

        let engine = MetroLayoutEngine::new();
        let mut session = engine.start_session(
            LayoutSessionConfig::default(),
            None,
            GraphFilterSpec::default(),
        );
        session
            .push_commits_chunk(
                &[make_commit(c2, &[c1], 300, CommitFlags::empty())],
                &RefBadgeMap::new(),
            )
            .unwrap();
        assert_eq!(session.commit_row_count(), 1);

        // Mid-chunk error (c1 valid, c0 lists itself as parent) must not commit c1!
        let bad_chunk = vec![
            make_commit(c1, &[c0], 200, CommitFlags::empty()),
            make_commit(c0, &[c0], 100, CommitFlags::empty()),
        ];
        assert!(
            session
                .push_commits_chunk(&bad_chunk, &RefBadgeMap::new())
                .is_err()
        );
        assert_eq!(session.commit_row_count(), 1);

        // Now pushing a clean chunk (even with duplicate parent [c0, c0]) succeeds atomically.
        session
            .push_commits_chunk(
                &[
                    make_commit(c1, &[c0, c0], 200, CommitFlags::empty()),
                    make_commit(c0, &[], 100, CommitFlags::empty()),
                ],
                &RefBadgeMap::new(),
            )
            .unwrap();
        assert_eq!(session.commit_row_count(), 3);
        assert_eq!(session.commit_rows_slice(0..3)[1].outgoing_edges.len(), 1);
    }
}
