//! Time-Machine Undo / Redo operation log entry and repository snapshot metadata.

use crate::ids::GitOid;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// Reset strategy recorded in operation history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResetMode {
    /// `git reset --soft` (moves `HEAD`/branch only; preserves index and worktree).
    Soft,
    /// `git reset --mixed` (moves `HEAD`/branch and resets index; preserves worktree).
    Mixed,
    /// `git reset --hard` (moves `HEAD`/branch and resets index and worktree).
    Hard,
    /// `git reset --keep` (aborts if local changes conflict).
    Keep,
}

/// Classification of mutating Git operations tracked by `oplog-undo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OpKind {
    /// New commit created from staged index.
    Commit,
    /// Previous commit amended.
    Amend,
    /// Checked out a branch, tag, or detached commit.
    Checkout,
    /// Created a branch reference.
    CreateBranch,
    /// Deleted a branch reference.
    DeleteBranch,
    /// Reset current branch/HEAD to another commit.
    Reset {
        /// Reset mode used.
        mode: ResetMode,
    },
    /// Merged a branch/commit.
    Merge,
    /// Non-interactive rebase.
    Rebase,
    /// Interactive rebase (`GIT_SEQUENCE_EDITOR`).
    InteractiveRebase,
    /// Cherry-picked one or more commits.
    CherryPick,
    /// Reverted one or more commits.
    Revert,
    /// Pushed dirty working tree/index to stash.
    StashPush,
    /// Popped or applied a stash entry.
    StashPop,
    /// Dropped a stash entry.
    StashDrop,
    /// Staged hunk/lines into `.git/index`.
    StagePatch,
    /// Unstaged hunk/lines out of `.git/index`.
    UnstagePatch,
    /// Discarded working tree hunk/lines.
    DiscardWorktreePatch,
}

/// Exact state of `HEAD` before or after a mutating operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadTargetSnapshot {
    /// `HEAD` was a symbolic reference (e.g. `refs/heads/main`, possibly unborn).
    Symbolic {
        /// Full symbolic target ref name.
        full_ref: CompactString,
        /// Commit OID the branch resolved to, or `None` if unborn.
        target_oid: Option<GitOid>,
    },
    /// `HEAD` was detached directly on a commit OID.
    Detached {
        /// Detached commit OID.
        oid: GitOid,
    },
}

/// Recorded change to a single reference (`refs/heads/*`, `refs/tags/*`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefMutationRecord {
    /// Canonical full ref path.
    pub full_ref: CompactString,
    /// OID before operation (`None` if newly created).
    pub old_oid: Option<GitOid>,
    /// OID after operation (`None` if deleted).
    pub new_oid: Option<GitOid>,
}

/// In-progress multi-step sequencer state inside `.git/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RepoOperationState {
    /// Normal idle repository state.
    Clean,
    /// Merge in progress (`MERGE_HEAD` exists).
    Merging,
    /// Standard rebase in progress (`rebase-merge` or `rebase-apply`).
    Rebasing,
    /// Interactive rebase in progress.
    InteractiveRebasing,
    /// Cherry-pick in progress (`CHERRY_PICK_HEAD`).
    CherryPicking,
    /// Revert in progress (`REVERT_HEAD`).
    Reverting,
    /// Bisect session in progress (`BISECT_LOG`).
    Bisecting,
}

/// Lightweight snapshot of repository pointers and tree OIDs captured atomically around mutations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStateSnapshot {
    /// `HEAD` symbolic or detached state.
    pub head: HeadTargetSnapshot,
    /// Snapshot of affected references.
    pub refs_snapshot: Vec<RefMutationRecord>,
    /// Root tree OID written from `.git/index` via `git2` in-memory `write_tree`, if available.
    pub index_tree_oid: Option<GitOid>,
    /// Hidden safety commit OID created via `git stash create` (does not pollute `refs/stash`!)
    /// when protecting dirty working-tree files prior to destructive mutations.
    pub worktree_safety_stash_oid: Option<GitOid>,
    /// Sequencer state at capture time.
    pub operation_state: RepoOperationState,
}

/// Safety classification indicating whether undoing/redoing rewrites commits already pushed to remotes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteExposureState {
    /// Operation only affected local-only history or refs; safe to undo cleanly.
    LocalOnly,
    /// Target commits are already reachable from one or more remote-tracking branches.
    TouchesPushedHistory {
        /// Remote-tracking refs that contain the commits being rewound.
        remote_branches: Vec<CompactString>,
        /// Specific commit OIDs exposed to remote tracking branches.
        diverged_oids: Vec<GitOid>,
    },
}

/// Persistent transaction entry stored in `oplog-undo` (`redb`) and linked to Git Reflog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogEntry {
    /// Monotonic per-repository operation ID.
    pub op_id: u64,
    /// 64-bit hash of the canonical repository `.git` path for multi-repo isolation.
    pub repo_root_hash: u64,
    /// Unix timestamp in milliseconds when the operation began.
    pub timestamp_unix_ms: i64,
    /// Operation classification.
    pub kind: OpKind,
    /// Human-readable summary shown in Undo/Redo tooltip & history panel.
    pub summary: CompactString,
    /// Pre-mutation state snapshot (used for `undo()`).
    pub before: RepoStateSnapshot,
    /// Post-mutation state snapshot (populated once command completes; used for `redo()`).
    pub after: Option<RepoStateSnapshot>,
    /// Optional Git reflog selector anchor (e.g., `HEAD@{2}`) for cross-checking external mutations.
    pub reflog_selector: Option<CompactString>,
    /// Remote push exposure warning state.
    pub remote_exposure: RemoteExposureState,
    /// Whether this entry has currently been undone (available on the Redo stack).
    pub undone: bool,
}
