//! Core async traits defining the 3-Tier Hybrid Git Engine contract.

use crate::error::GitEngineResult;
use crate::types::{
    BlameResult, CliCommandHandle, CommitOptions, ConflictStageBlobs, MergeOptions,
    MergePrediction, PickaxeSearchRequest, PushForce, RebasePlanPayload, RefsSnapshot, RepoSummary,
    RevwalkChunk, RevwalkSessionParams, WorktreeInfo,
};
use async_trait::async_trait;
use branvio_core::domain::{CommitDetails, ResetMode};
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{CancelToken, WorkingTreeStatusView};
use std::path::Path;

/// Stateful topological revwalk session preserving `gix` traversal frontier across chunks.
///
/// Replaces stateless cursor arrays so multi-parent topological ordering (Kahn / commit-graph)
/// streams arbitrarily deep monorepo history incrementally without skipping or re-walking commits.
pub trait RevwalkSession: Send {
    /// Pulls up to `max_commits` commits in strict topological parent-after-child order.
    fn next_chunk(
        &mut self,
        max_commits: usize,
        cancel: &CancelToken,
    ) -> GitEngineResult<RevwalkChunk>;

    /// Returns `true` once all reachable commits have been emitted.
    fn is_exhausted(&self) -> bool;
}

/// **Tier 1 (`gix` / `gitoxide`):** Zero-lock, memory-mapped, cooperative-cancellable read operations.
///
/// Implementations must NEVER take `.git/index.lock` and must check `cancel.is_cancelled()`
/// frequently inside long loops (e.g. monorepo revwalks or blame scans).
#[async_trait]
pub trait GitReadEngine: Send + Sync {
    /// Opens and validates a Git repository (resolving linked worktrees and `.git` pointers).
    async fn open_repo(&self, path: &Path) -> GitEngineResult<RepoSummary>;

    /// Reads `HEAD`, local branches, remote tracking branches, tags, and ahead/behind divergence.
    async fn head_and_refs(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<RefsSnapshot>;

    /// Opens a stateful topological revwalk session ready to stream chunks incrementally.
    async fn open_revwalk_session(
        &self,
        repo_root: &HostPath,
        params: RevwalkSessionParams,
        cancel: CancelToken,
    ) -> GitEngineResult<Box<dyn RevwalkSession>>;

    /// Lazily loads full commit body, trailers, cryptographic signature status, and changed files.
    async fn load_commit_details(
        &self,
        repo_root: &HostPath,
        oid: GitOid,
        cancel: CancelToken,
    ) -> GitEngineResult<CommitDetails>;

    /// Computes staged, unstaged, untracked, and conflicted working-tree deltas honoring `.gitignore`
    /// and `core.autocrlf`.
    async fn working_tree_status(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<WorkingTreeStatusView>;

    /// Reads raw object bytes for a blob OID (with hard safety cap option for gigantic binary blobs).
    async fn read_blob(
        &self,
        repo_root: &HostPath,
        blob_oid: GitOid,
        max_bytes: Option<usize>,
    ) -> GitEngineResult<Vec<u8>>;

    /// Reads unmerged index stage blobs (`1=Base`, `2=Ours`, `3=Theirs`) for a conflicted path.
    async fn read_conflict_stages(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        max_bytes: usize,
    ) -> GitEngineResult<ConflictStageBlobs>;

    /// Computes file blame attribution against an optional commit OID (or working tree when `None`).
    async fn blame_file(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        at_commit: Option<GitOid>,
        cancel: CancelToken,
    ) -> GitEngineResult<BlameResult>;
}

/// **Tier 2 (`git2`):** Local in-memory `.git/index` manipulation & predictive merge conflict check.
///
/// ## Concurrency & Locking Contract
/// `git2::Repository` is `!Sync`, and writing `.git/index` acquires `.git/index.lock`.
/// Implementations MUST execute `git2` work on `tokio::task::spawn_blocking` (opening a fresh
/// repo handle or per-call pool worker) and map `.git/index.lock` contention to
/// [`crate::GitEngineError::IndexLocked`].
/// Must NEVER invoke network or credential callbacks (`git2` compiled with `default-features = false`).
#[async_trait]
pub trait GitIndexEngine: Send + Sync {
    /// Stages entire file paths into `.git/index`.
    async fn stage_files(&self, repo_root: &HostPath, paths: &[RepoRelPath])
    -> GitEngineResult<()>;

    /// Unstages entire file paths back to `HEAD` tree state in `.git/index` (handles unborn `HEAD`).
    async fn unstage_files(
        &self,
        repo_root: &HostPath,
        paths: &[RepoRelPath],
    ) -> GitEngineResult<()>;

    /// Applies a synthetic unified patch directly in-memory onto `.git/index` without temp files on disk.
    /// Used for surgical hunk-level and line-level Staging and Unstaging.
    async fn apply_synthetic_patch_to_index(
        &self,
        repo_root: &HostPath,
        unified_patch: &[u8],
    ) -> GitEngineResult<()>;

    /// Applies a reverse patch to the working directory for surgical hunk/line Discard.
    async fn apply_reverse_patch_to_worktree(
        &self,
        repo_root: &HostPath,
        target_path: &RepoRelPath,
        reverse_unified_patch: &[u8],
    ) -> GitEngineResult<()>;

    /// Writes the current `.git/index` contents to an ODB tree object and returns its OID
    /// (used by `oplog-undo` for lightweight index snapshots).
    async fn snapshot_index_tree_oid(&self, repo_root: &HostPath) -> GitEngineResult<GitOid>;

    /// Restores `.git/index` deterministically from a previously captured tree OID.
    async fn restore_index_from_tree_oid(
        &self,
        repo_root: &HostPath,
        tree_oid: GitOid,
    ) -> GitEngineResult<()>;

    /// Runs `git2::Repository::merge_commits` strictly in memory to preview conflicts before
    /// executing a drag-and-drop merge, rebase, or cherry-pick.
    async fn predict_merge_conflicts(
        &self,
        repo_root: &HostPath,
        ours_oid: GitOid,
        theirs_oid: GitOid,
    ) -> GitEngineResult<MergePrediction>;
}

/// **Tier 3 (`tokio::process` native `git` CLI):** State-mutating, pickaxe search, and network operations.
///
/// ## Safety & Compatibility Invariants
/// - Preserves 100% compatibility with `.gitconfig`, SSH Agents, GPG/SSH signing, Git LFS, and Hooks.
/// - All revision arguments MUST be guarded via [`crate::build_safe_revision_args`] (`--end-of-options`)
///   so branch names beginning with `-` cannot inject CLI flags.
/// - Subprocesses MUST be spawned with `GIT_TERMINAL_PROMPT=0` and (`LC_ALL=C`, `LANG=C`) whenever
///   stdout/stderr porcelain output is parsed programmatically.
#[async_trait]
pub trait GitCliExecutor: Send + Sync {
    /// Creates or amends a commit, streaming hook stdout/stderr live.
    async fn commit(
        &self,
        repo_root: &HostPath,
        options: CommitOptions,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Checks out a branch, tag, or commit OID.
    async fn checkout_ref(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        create_branch: Option<&str>,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs `git merge` with configured fast-forward/squash flags.
    async fn merge(
        &self,
        repo_root: &HostPath,
        options: MergeOptions,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs `git rebase <onto>`.
    async fn rebase(
        &self,
        repo_root: &HostPath,
        onto_rev: &str,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs automated `git rebase -i` using Branvio itself via `GIT_SEQUENCE_EDITOR`.
    async fn interactive_rebase(
        &self,
        repo_root: &HostPath,
        plan: RebasePlanPayload,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs `git cherry-pick <oids...>`.
    async fn cherry_pick(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs `git revert <oids...>`.
    async fn revert(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle>;

    /// Runs `git reset --soft|--mixed|--hard|--keep <target>`.
    async fn reset(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        mode: ResetMode,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Fetches from a specific remote or `--all` remotes (`prune` optional).
    async fn fetch(
        &self,
        repo_root: &HostPath,
        remote: Option<&str>,
        prune: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Pulls changes (`rebase` or `ff-only`).
    async fn pull(
        &self,
        repo_root: &HostPath,
        rebase: bool,
        ff_only: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Pushes `branch` to `remote` (`refs/heads/<branch>` on both sides).
    ///
    /// `branch` must be a plain branch name valid under `git check-ref-format` (no refspec
    /// syntax such as `+x`, `a:b`, `:x`, no `@{`, `HEAD`, ...) for every force policy;
    /// otherwise the call fails before anything is spawned.
    ///
    /// `force` selects the overwrite policy; see [`PushForce`] for why leases always carry an
    /// explicit expected OID.
    async fn push(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
        force: PushForce,
        set_upstream: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Reads the current local remote-tracking OID of `refs/remotes/<remote>/<branch>`
    /// (`None` if the tracking ref does not exist).
    ///
    /// Callers capture this when a force-push confirmation opens and pass it as
    /// [`PushForce::WithLease`]`{ expected_remote_oid }` so later auto-fetches cannot
    /// silently widen the lease.
    async fn remote_tracking_oid(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
    ) -> GitEngineResult<Option<GitOid>>;

    /// Pushes working changes to a new stash entry (`--include-untracked` optional).
    async fn stash_push(
        &self,
        repo_root: &HostPath,
        message: Option<&str>,
        include_untracked: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Applies or pops a stash entry (`stash@{idx}`).
    async fn stash_pop(
        &self,
        repo_root: &HostPath,
        stash_index: usize,
        drop_after_apply: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Lists all linked Git worktrees (`git worktree list --porcelain`).
    async fn worktree_list(&self, repo_root: &HostPath) -> GitEngineResult<Vec<WorktreeInfo>>;

    /// Creates a new Git worktree at `target_dir` checked out to `branch_or_oid`.
    async fn worktree_add(
        &self,
        repo_root: &HostPath,
        target_dir: &HostPath,
        branch_or_oid: &str,
        create_new_branch: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Removes a linked Git worktree.
    async fn worktree_remove(
        &self,
        repo_root: &HostPath,
        worktree_path: &HostPath,
        force: bool,
    ) -> GitEngineResult<CliCommandHandle>;

    /// Discards uncommitted working-directory changes by restoring tracked paths from the index
    /// and cleaning explicitly selected untracked paths.
    async fn discard_worktree_paths(
        &self,
        repo_root: &HostPath,
        tracked_paths: &[RepoRelPath],
        untracked_paths: &[RepoRelPath],
    ) -> GitEngineResult<CliCommandHandle>;

    /// Executes native `git log -S` / `-G` pickaxe code search across history (with `GIT_OPTIONAL_LOCKS=0`).
    async fn search_commits_pickaxe(
        &self,
        repo_root: &HostPath,
        request: PickaxeSearchRequest,
        cancel: CancelToken,
    ) -> GitEngineResult<Vec<GitOid>>;
}

/// Combined supertrait exposing the complete 3-Tier Hybrid Git Engine to application services.
pub trait HybridGitEngine: GitReadEngine + GitIndexEngine + GitCliExecutor + Send + Sync {}

impl<T> HybridGitEngine for T where T: GitReadEngine + GitIndexEngine + GitCliExecutor + Send + Sync {}
