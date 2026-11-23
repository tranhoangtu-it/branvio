//! Lock-free MVCC reactive state pipeline, cancellation tokens, and watcher storm suppression.
//!
//! Data flows strictly one-way from background workers (`fs-watcher` -> `git-engine` -> `graph-layout`)
//! into [`MvccRepoStateStore`] via monotonic [`RepoEpoch`] comparison. The UI thread reads
//! immutable `Arc<RepoViewSnapshot>` states wait-free without locking any `Mutex`.

use crate::domain::{
    CommitNode, FileDelta, GraphRow, RefBadge, RefBadgeMap, RepoOperationState, VirtualWipRow,
};
use crate::ids::{GitOid, RepoEpoch, RepoId};
use crate::paths::HostPath;
use arc_swap::ArcSwap;
use compact_str::CompactString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Cooperative cancellation token passable directly as `&AtomicBool` to `gix` interruptible traversals.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancelToken {
    /// Creates a fresh uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals cancellation across all background workers observing this token.
    #[inline]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Completes asynchronously once cancellation has been requested.
    ///
    /// Uses `Notify::enable` before re-checking `is_cancelled()` so concurrent
    /// `cancel()` calls never cause missed wakeups.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    /// Returns `true` if cancellation was requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Exposes the inner [`AtomicBool`] reference for `gix::interrupt::Iter` and `gix` probes.
    #[inline]
    pub fn as_atomic_flag(&self) -> &AtomicBool {
        &self.cancelled
    }

    /// Clones the inner [`Arc<AtomicBool>`] for APIs such as `gix::status::Platform::should_interrupt_owned`.
    #[inline]
    pub fn clone_arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

/// Bitmask coalescing dirty scopes emitted by `fs-watcher` while suppressed during an internal Git command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyScopeFlags(u8);

impl DirtyScopeFlags {
    /// No dirty notifications pending.
    pub const NONE: Self = Self(0);
    /// Working tree files changed.
    pub const WORKTREE: Self = Self(1 << 0);
    /// `.git/index` changed.
    pub const INDEX: Self = Self(1 << 1);
    /// `.git/HEAD`, `.git/refs/**`, or `.git/packed-refs` changed.
    pub const REFS_OR_HEAD: Self = Self(1 << 2);

    /// Merges two dirty masks.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns `true` when at least one dirty flag is set.
    #[inline]
    pub const fn any(self) -> bool {
        self.0 != 0
    }

    /// Returns `true` if `REFS_OR_HEAD` is dirty.
    #[inline]
    pub const fn has_refs_or_head(self) -> bool {
        (self.0 & Self::REFS_OR_HEAD.0) != 0
    }
}

/// Coordinates Branvio-initiated mutations with `fs-watcher` to avoid mid-operation refresh storms.
///
/// When Branvio runs `rebase`, `checkout`, or `stash pop`, it acquires an [`InternalOpLease`].
/// While active (`active_leases > 0`), filesystem watcher events are coalesced into `pending_dirty`
/// rather than triggering premature Git reads; when the final lease drops, exactly one coalesced
/// refresh is emitted.
#[derive(Debug, Default)]
pub struct InternalOpController {
    active_leases: AtomicUsize,
    pending_dirty: std::sync::atomic::AtomicU8,
}

impl InternalOpController {
    /// Acquires a scoped suppression lease. Nestable across sub-operations.
    pub fn acquire_lease(self: &Arc<Self>) -> InternalOpLease {
        self.active_leases.fetch_add(1, Ordering::AcqRel);
        InternalOpLease {
            controller: Arc::clone(self),
            released: false,
        }
    }

    /// Returns `true` while an internal Branvio mutation owns the repository lock.
    #[inline]
    pub fn is_mutating(&self) -> bool {
        self.active_leases.load(Ordering::Acquire) > 0
    }

    /// Records a dirty scope from `fs-watcher`. Returns `Some(flags)` if the caller should
    /// trigger an immediate refresh, or `None` if suppressed until the active lease finishes.
    pub fn record_or_coalesce(&self, flags: DirtyScopeFlags) -> Option<DirtyScopeFlags> {
        if self.is_mutating() {
            self.pending_dirty.fetch_or(flags.0, Ordering::AcqRel);
            None
        } else {
            Some(flags)
        }
    }

    fn release_one(&self) -> DirtyScopeFlags {
        let prev = self.active_leases.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            DirtyScopeFlags(self.pending_dirty.swap(0, Ordering::AcqRel))
        } else {
            DirtyScopeFlags::NONE
        }
    }
}

/// RAII guard suppressing `fs-watcher` event storms while Branvio mutates the repository.
#[derive(Debug)]
pub struct InternalOpLease {
    controller: Arc<InternalOpController>,
    released: bool,
}

impl InternalOpLease {
    /// Explicitly finishes the lease and returns any dirty scopes coalesced during the operation.
    pub fn finish(mut self) -> DirtyScopeFlags {
        self.released = true;
        self.controller.release_one()
    }
}

impl Drop for InternalOpLease {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.controller.release_one();
        }
    }
}

/// Immutable working-tree and index status slice shared inside [`RepoViewSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkingTreeStatusView {
    /// Files modified/untracked in working tree (`DeltaArea::Unstaged`).
    pub unstaged: Arc<[FileDelta]>,
    /// Files staged in `.git/index` (`DeltaArea::Staged`).
    pub staged: Arc<[FileDelta]>,
    /// Unmerged conflicted files (`DeltaArea::Conflicted`).
    pub conflicted: Arc<[FileDelta]>,
}

impl WorkingTreeStatusView {
    /// Total dirty entries across staged, unstaged, and conflicted buckets.
    #[inline]
    pub fn total_dirty_count(&self) -> usize {
        self.unstaged.len() + self.staged.len() + self.conflicted.len()
    }
}

/// Immutable computed DAG graph slice shared inside [`RepoViewSnapshot`].
///
/// Separates [`VirtualWipRow`] from historical [`GraphSnapshotView::commit_rows`] so toggling
/// uncommitted working-tree changes never shifts the commit array (`O(1)` display mapping).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphSnapshotView {
    /// Detached virtual WIP row displayed at visual row `0` when working tree is dirty.
    pub wip_row: Option<Arc<VirtualWipRow>>,
    /// Ordered commits matching `commit_rows` 1-to-1.
    pub commits: Arc<[CommitNode]>,
    /// Pre-computed metro-line layout rows aligned index-for-index with `commits`.
    pub commit_rows: Arc<[GraphRow]>,
    /// Sparse map of ref badges keyed by commit OID (`O(1)` lookup during row paint).
    pub refs_by_oid: Arc<RefBadgeMap>,
    /// Maximum lane width across loaded rows for canvas column sizing.
    pub max_lane_width: u16,
    /// `true` if more historical commits exist beyond the currently loaded chunk.
    pub has_more_history: bool,
}

impl GraphSnapshotView {
    /// Total rows visible in the virtualized graph list (`+1` when `wip_row` is active).
    #[inline]
    pub fn display_row_count(&self) -> usize {
        self.commit_rows.len() + usize::from(self.wip_row.is_some())
    }

    /// Maps a visual row index (`0..display_row_count()`) to an index inside `commit_rows` (`O(1)`).
    /// Returns `None` for visual row `0` when `wip_row` is present or if out of bounds.
    #[inline]
    pub fn display_to_commit_index(&self, display_row: usize) -> Option<usize> {
        let offset = usize::from(self.wip_row.is_some());
        if display_row < offset {
            return None;
        }
        let commit_idx = display_row - offset;
        (commit_idx < self.commit_rows.len()).then_some(commit_idx)
    }

    /// Maps a historical `commit_rows` index (or `GraphEdge::target_row`) to visual display index (`O(1)`).
    #[inline]
    pub fn commit_to_display_index(&self, commit_row: usize) -> usize {
        commit_row + usize::from(self.wip_row.is_some())
    }

    /// Looks up reference badges attached to `oid` in `O(1)`.
    #[inline]
    pub fn badges_for(&self, oid: &GitOid) -> &[RefBadge] {
        self.refs_by_oid.get(oid).map_or(&[], |v| v.as_slice())
    }
}

/// Complete, immutable, wait-free repository state snapshot consumed by any UI backend
/// (`gpui`, `Iced`, or `Tauri v2`).
#[derive(Debug, Clone)]
pub struct RepoViewSnapshot {
    /// Repository tab identifier.
    pub repo_id: RepoId,
    /// Monotonic generation stamp proving freshness of this snapshot.
    pub epoch: RepoEpoch,
    /// Normalized host filesystem path to the repository working tree root.
    pub repo_root: HostPath,
    /// Current `HEAD` display label (branch name or abbreviated detached SHA).
    pub head_label: CompactString,
    /// Current `HEAD` commit OID if not unborn.
    pub head_oid: Option<GitOid>,
    /// Active sequencer state (`Clean`, `Merging`, `Rebasing`, etc.).
    pub operation_state: RepoOperationState,
    /// All known references (`refs/heads`, `refs/remotes`, `refs/tags`, stashes).
    pub refs: Arc<[RefBadge]>,
    /// Working-tree and index status snapshot.
    pub working_tree: Arc<WorkingTreeStatusView>,
    /// Computed commit graph snapshot.
    pub graph: Arc<GraphSnapshotView>,
    /// Whether `oplog-undo` currently has an undoable or redoable entry.
    pub can_undo: bool,
    /// Whether an undone entry can be redone.
    pub can_redo: bool,
}

/// UI-agnostic notification port called when a newer [`RepoViewSnapshot`] is committed.
///
/// Allows `branvio-core` to wake `gpui::AsyncApp`, an `Iced` subscription stream,
/// or a `Tauri v2` event emitter without depending on any UI framework crate.
pub trait UiWakeupSink: Send + Sync {
    /// Signals the UI layer that `repo_id` advanced to `epoch` and should re-read [`MvccRepoStateStore`].
    fn notify_repo_updated(&self, repo_id: RepoId, epoch: RepoEpoch);
}

/// Lock-free MVCC state store preventing race conditions and UI-thread stalls.
///
/// - **Readers (120 FPS UI thread):** call [`MvccRepoStateStore::load_snapshot`] (`ArcSwap::load_full`
///   or `load`), which is wait-free and never blocks on `git` I/O.
/// - **Writers (Tokio background workers):** allocate epochs via [`MvccRepoStateStore::next_epoch`]
///   and commit via [`MvccRepoStateStore::try_publish`]. Out-of-order stale finishes (`epoch <= committed`)
///   are rejected atomically without corrupting newer UI state.
pub struct MvccRepoStateStore {
    latest_issued_epoch: AtomicU64,
    committed_epoch: AtomicU64,
    snapshot: ArcSwap<RepoViewSnapshot>,
}

impl MvccRepoStateStore {
    /// Initializes the store with an initial epoch-0 snapshot.
    pub fn new(initial: RepoViewSnapshot) -> Self {
        let initial_epoch = initial.epoch.0;
        Self {
            latest_issued_epoch: AtomicU64::new(initial_epoch),
            committed_epoch: AtomicU64::new(initial_epoch),
            snapshot: ArcSwap::from_pointee(initial),
        }
    }

    /// Issues a fresh monotonically increasing [`RepoEpoch`] for an incoming background refresh.
    #[inline]
    pub fn next_epoch(&self) -> RepoEpoch {
        let prev = self.latest_issued_epoch.fetch_add(1, Ordering::AcqRel);
        RepoEpoch(prev.saturating_add(1))
    }

    /// Returns the highest epoch currently issued (workers can check whether they were superseded).
    #[inline]
    pub fn latest_issued_epoch(&self) -> RepoEpoch {
        RepoEpoch(self.latest_issued_epoch.load(Ordering::Acquire))
    }

    /// Returns the epoch currently visible to the UI.
    #[inline]
    pub fn committed_epoch(&self) -> RepoEpoch {
        RepoEpoch(self.committed_epoch.load(Ordering::Acquire))
    }

    /// Wait-free snapshot read for the 120 FPS UI render loop.
    #[inline]
    pub fn load_snapshot(&self) -> Arc<RepoViewSnapshot> {
        self.snapshot.load_full()
    }

    /// Atomically publishes `candidate` ONLY if `candidate.epoch > committed_epoch`.
    ///
    /// Uses a CAS loop on `committed_epoch` so concurrent background tasks finishing out of order
    /// can never overwrite a newer state with stale data. Returns `true` if published.
    pub fn try_publish(&self, candidate: Arc<RepoViewSnapshot>) -> bool {
        let candidate_epoch = candidate.epoch.0;
        let mut current = self.committed_epoch.load(Ordering::Acquire);
        loop {
            if candidate_epoch <= current {
                return false;
            }
            match self.committed_epoch.compare_exchange_weak(
                current,
                candidate_epoch,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.snapshot.store(candidate);
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_snapshot(epoch: u64, head_label: &str) -> RepoViewSnapshot {
        RepoViewSnapshot {
            repo_id: RepoId(1),
            epoch: RepoEpoch(epoch),
            repo_root: HostPath::from_path_buf(PathBuf::from("/tmp/repo")),
            head_label: CompactString::from(head_label),
            head_oid: None,
            operation_state: RepoOperationState::Clean,
            refs: Arc::from([]),
            working_tree: Arc::default(),
            graph: Arc::default(),
            can_undo: false,
            can_redo: false,
        }
    }

    #[test]
    fn mvcc_store_rejects_stale_out_of_order_snapshots() {
        let store = MvccRepoStateStore::new(dummy_snapshot(0, "init"));
        let e1 = store.next_epoch();
        let e2 = store.next_epoch();
        assert_eq!(e1, RepoEpoch(1));
        assert_eq!(e2, RepoEpoch(2));

        // Task 2 finishes first and commits epoch 2
        assert!(store.try_publish(Arc::new(dummy_snapshot(e2.0, "main-v2"))));
        assert_eq!(store.load_snapshot().head_label, "main-v2");

        // Task 1 finishes late with stale epoch 1 -> MUST be rejected
        assert!(!store.try_publish(Arc::new(dummy_snapshot(e1.0, "stale-v1"))));
        assert_eq!(store.load_snapshot().head_label, "main-v2");
        assert_eq!(store.committed_epoch(), RepoEpoch(2));
    }

    #[test]
    fn internal_op_lease_coalesces_watcher_events() {
        let ctrl = Arc::new(InternalOpController::default());
        assert_eq!(
            ctrl.record_or_coalesce(DirtyScopeFlags::WORKTREE),
            Some(DirtyScopeFlags::WORKTREE)
        );

        let outer = ctrl.acquire_lease();
        let inner = ctrl.acquire_lease();

        assert_eq!(ctrl.record_or_coalesce(DirtyScopeFlags::WORKTREE), None);
        assert_eq!(ctrl.record_or_coalesce(DirtyScopeFlags::REFS_OR_HEAD), None);

        // Inner lease finishes, outer still held -> still empty
        assert_eq!(inner.finish(), DirtyScopeFlags::NONE);
        // Outer lease finishes -> yields coalesced dirty scopes
        let coalesced = outer.finish();
        assert!(coalesced.any());
        assert!(coalesced.has_refs_or_head());
    }

    #[test]
    fn graph_snapshot_view_maps_display_rows_in_o1_with_detached_wip() {
        use crate::domain::{GraphNodeKind, GraphRow};
        use smallvec::SmallVec;

        let commit_oid = GitOid::from_sha1_bytes([7u8; 20]);
        let row = GraphRow {
            oid: commit_oid,
            commit_lane: 0,
            max_active_lane: 0,
            node_kind: GraphNodeKind::InitialRoot,
            node_palette_slot: 0,
            passing_lanes: SmallVec::new(),
            incoming_edges: SmallVec::new(),
            outgoing_edges: SmallVec::new(),
        };

        let mut view = GraphSnapshotView {
            wip_row: None,
            commits: Arc::from([]),
            commit_rows: Arc::from([row]),
            refs_by_oid: Arc::default(),
            max_lane_width: 0,
            has_more_history: false,
        };

        assert_eq!(view.display_row_count(), 1);
        assert_eq!(view.display_to_commit_index(0), Some(0));
        assert_eq!(view.commit_to_display_index(0), 0);

        view.wip_row = Some(Arc::new(VirtualWipRow {
            head_oid: Some(commit_oid),
            head_commit_row: Some(0),
            commit_lane: 0,
            max_active_lane: 0,
            palette_slot: 0,
            staged_count: 1,
            unstaged_count: 2,
            conflicted_count: 0,
            untracked_count: 0,
            passing_lanes: SmallVec::new(),
            outgoing_head_edge: None,
        }));

        assert_eq!(view.display_row_count(), 2);
        assert_eq!(view.display_to_commit_index(0), None);
        assert_eq!(view.display_to_commit_index(1), Some(0));
        assert_eq!(view.commit_to_display_index(0), 1);
        assert!(view.badges_for(&commit_oid).is_empty());
    }
}
