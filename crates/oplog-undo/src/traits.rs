//! Traits for persistent snapshot logging (`redb`) and safe transactional Undo/Redo.

use async_trait::async_trait;
use branvio_core::domain::{OpKind, OpLogEntry, RemoteExposureState, RepoStateSnapshot};
use branvio_core::ids::GitOid;
use branvio_core::paths::HostPath;
use compact_str::CompactString;
use git_engine::GitEngineError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias for `oplog-undo`.
pub type OpLogResult<T> = Result<T, OpLogError>;

/// Errors produced by operation journal persistence or rollback execution.
#[derive(Debug, Error)]
pub enum OpLogError {
    /// Embedded `redb` database I/O or serialization failure.
    #[error("operation journal storage failure: {0}")]
    Storage(String),

    /// Underlying Git engine error while capturing or restoring repository state.
    #[error(transparent)]
    Git(#[from] GitEngineError),

    /// No operation is available on the undo or redo stack.
    #[error("nothing to {action} for repository")]
    StackEmpty {
        /// Either `"undo"` or `"redo"`.
        action: &'static str,
    },

    /// Rollback aborted because external CLI commands modified `HEAD`/refs after the logged operation.
    #[error("external reflog divergence detected since operation {op_id}: {reason}")]
    ExternalDivergence {
        /// Conflicting operation ID.
        op_id: u64,
        /// Explanation of the mismatch.
        reason: String,
    },

    /// The pre-mutation snapshot could not capture the complete repository state (e.g. the
    /// index has unresolved merge conflicts), so no undo point was recorded. Callers should not
    /// run the destructive operation as "undoable".
    #[error("cannot capture a complete undo snapshot: {reason}")]
    SnapshotIncomplete {
        /// Explanation of what could not be captured.
        reason: String,
    },

    /// Undo/redo restored refs, `HEAD`, worktree and index, but some cleanup steps failed
    /// (e.g. files deleted in the snapshot could not be removed). The entry's undo state was
    /// still updated; `details` lists what needs manual attention.
    #[error("operation {op_id} was restored with problems: {}", .details.join("; "))]
    RestoreIncomplete {
        /// Operation that was undone or redone.
        op_id: u64,
        /// One message per failed cleanup step.
        details: Vec<String>,
    },

    /// Rollback requires explicit user confirmation because it rewrites already-pushed commits.
    #[error("undo touches commits already pushed to remote(s)")]
    RemoteExposureConfirmationRequired {
        /// Exposure report detailing affected remote tracking refs.
        exposure: RemoteExposureState,
    },
}

/// Evaluation result before executing Undo/Redo on a target operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSafetyCheck {
    /// Entry that would be undone or redone.
    pub op_id: u64,
    /// Human-readable description of the rollback effect.
    pub summary: CompactString,
    /// Indicates whether commits touched by rollback already exist on remotes.
    pub remote_exposure: RemoteExposureState,
    /// True if dirty working-tree files will be safely preserved or restored via safety stash.
    pub preserves_worktree: bool,
}

/// Result returned after completing an `undo()` or `redo()` transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoOutcome {
    /// Operation entry ID that was reversed or re-applied.
    pub op_id: u64,
    /// Updated `HEAD` OID after rollback, if non-unborn.
    pub restored_head_oid: Option<GitOid>,
    /// Human-readable status banner message.
    pub status_message: CompactString,
}

/// Persistent embedded store (`redb`) indexing lightweight repository snapshots per repository.
pub trait OpLogStore: Send + Sync {
    /// Records a pre-mutation [`OpLogEntry`] before a mutating Git command starts.
    ///
    /// Also drops the undone (redo) tail and the oldest entries beyond the retention cap. The
    /// store has no repository access: the undo engine removes snapshot pins whose entry is gone.
    fn record_begin(&self, entry: OpLogEntry) -> OpLogResult<u64>;

    /// Marks the transaction finalized by attaching the post-mutation snapshot and reflog anchor.
    fn record_commit(
        &self,
        repo_root_hash: u64,
        op_id: u64,
        after: RepoStateSnapshot,
        reflog_selector: Option<CompactString>,
    ) -> OpLogResult<()>;

    /// Rolls back an aborted/incomplete pending entry if a command failed before mutating refs.
    ///
    /// Prefer [`UndoRedoEngine::discard_incomplete`], which also deletes the entry's snapshot pin
    /// ref immediately (otherwise it is only garbage-collected on the next captured operation).
    fn discard_incomplete(&self, repo_root_hash: u64, op_id: u64) -> OpLogResult<()>;

    /// Lists recent history entries newest-first for UI inspection.
    fn list_recent(&self, repo_root_hash: u64, limit: usize) -> OpLogResult<Vec<OpLogEntry>>;

    /// Marks an entry as undone or re-applied (`undone = true/false`).
    fn set_undone_state(&self, repo_root_hash: u64, op_id: u64, undone: bool) -> OpLogResult<()>;
}

/// High-level Time-Machine Undo/Redo orchestrator coordinating `OpLogStore` and `HybridGitEngine`.
#[async_trait]
pub trait UndoRedoEngine: Send + Sync {
    /// Captures a lightweight snapshot (`HEAD`, refs, index tree OID, optional safety stash)
    /// right before executing a mutating operation.
    async fn capture_before_mutation(
        &self,
        repo_root: &HostPath,
        kind: OpKind,
        summary: CompactString,
    ) -> OpLogResult<u64>;

    /// Discards an incomplete entry (the mutation failed or never ran) and deletes its pinned
    /// worktree snapshot ref, if any.
    async fn discard_incomplete(&self, repo_root: &HostPath, op_id: u64) -> OpLogResult<()>;

    /// Finalizes the active transaction after the Git mutation succeeds.
    async fn finalize_after_mutation(&self, repo_root: &HostPath, op_id: u64) -> OpLogResult<()>;

    /// Inspects the top undoable operation for remote push exposure before rolling back.
    async fn inspect_next_undo(&self, repo_root: &HostPath) -> OpLogResult<PushSafetyCheck>;

    /// Safely reverts the most recent non-undone operation.
    /// When `allow_rewrite_pushed == false`, refuses with
    /// [`OpLogError::RemoteExposureConfirmationRequired`] if remote-tracking branches contain
    /// the target commits.
    async fn undo(
        &self,
        repo_root: &HostPath,
        allow_rewrite_pushed: bool,
    ) -> OpLogResult<UndoOutcome>;

    /// Re-applies the most recently undone operation.
    async fn redo(
        &self,
        repo_root: &HostPath,
        allow_rewrite_pushed: bool,
    ) -> OpLogResult<UndoOutcome>;
}
