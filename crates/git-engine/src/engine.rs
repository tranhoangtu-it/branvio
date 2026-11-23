//! Composite [`BranvioGitEngine`] unifying Tier 1 (`gix`), Tier 2 (`git2`), and Tier 3 (`git` CLI).

use crate::error::GitEngineResult;
use crate::tier1_gix::GixReadEngine;
use crate::tier2_git2::Git2IndexEngine;
use crate::tier3_cli::TokioGitCliExecutor;
use crate::traits::{GitCliExecutor, GitIndexEngine, GitReadEngine, RevwalkSession};
use crate::types::{
    BlameResult, CliCommandHandle, CommitOptions, ConflictStageBlobs, MergeOptions,
    MergePrediction, PickaxeSearchRequest, PushForce, RebasePlanPayload, RefsSnapshot, RepoSummary,
    RevwalkSessionParams, WorktreeInfo,
};
use async_trait::async_trait;
use branvio_core::domain::{CommitDetails, ResetMode};
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{CancelToken, WorkingTreeStatusView};
use branvio_core::platform::HydratedGitEnv;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Unified 3-Tier Hybrid Git Engine implementation (`HybridGitEngine`).
#[derive(Debug, Clone)]
pub struct BranvioGitEngine {
    tier1: GixReadEngine,
    tier2: Git2IndexEngine,
    tier3: TokioGitCliExecutor,
}

impl BranvioGitEngine {
    /// Constructs a new [`BranvioGitEngine`] backed by the hydrated OS Git/SSH/GPG environment.
    pub fn new(git_env: Arc<HydratedGitEnv>, self_helper_exe: Option<PathBuf>) -> Self {
        Self {
            tier1: GixReadEngine::new(),
            tier2: Git2IndexEngine::new(),
            tier3: TokioGitCliExecutor::new(git_env, self_helper_exe),
        }
    }
}

#[async_trait]
impl GitReadEngine for BranvioGitEngine {
    async fn open_repo(&self, path: &Path) -> GitEngineResult<RepoSummary> {
        self.tier1.open_repo(path).await
    }

    async fn head_and_refs(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<RefsSnapshot> {
        self.tier1.head_and_refs(repo_root, cancel).await
    }

    async fn open_revwalk_session(
        &self,
        repo_root: &HostPath,
        params: RevwalkSessionParams,
        cancel: CancelToken,
    ) -> GitEngineResult<Box<dyn RevwalkSession>> {
        self.tier1
            .open_revwalk_session(repo_root, params, cancel)
            .await
    }

    async fn load_commit_details(
        &self,
        repo_root: &HostPath,
        oid: GitOid,
        cancel: CancelToken,
    ) -> GitEngineResult<CommitDetails> {
        self.tier1.load_commit_details(repo_root, oid, cancel).await
    }

    async fn working_tree_status(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<WorkingTreeStatusView> {
        self.tier1.working_tree_status(repo_root, cancel).await
    }

    async fn read_blob(
        &self,
        repo_root: &HostPath,
        blob_oid: GitOid,
        max_bytes: Option<usize>,
    ) -> GitEngineResult<Vec<u8>> {
        self.tier1.read_blob(repo_root, blob_oid, max_bytes).await
    }

    async fn read_conflict_stages(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        max_bytes: usize,
    ) -> GitEngineResult<ConflictStageBlobs> {
        self.tier1
            .read_conflict_stages(repo_root, path, max_bytes)
            .await
    }

    async fn blame_file(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        at_commit: Option<GitOid>,
        cancel: CancelToken,
    ) -> GitEngineResult<BlameResult> {
        self.tier1
            .blame_file(repo_root, path, at_commit, cancel)
            .await
    }
}

#[async_trait]
impl GitIndexEngine for BranvioGitEngine {
    async fn stage_files(
        &self,
        repo_root: &HostPath,
        paths: &[RepoRelPath],
    ) -> GitEngineResult<()> {
        self.tier2.stage_files(repo_root, paths).await
    }

    async fn unstage_files(
        &self,
        repo_root: &HostPath,
        paths: &[RepoRelPath],
    ) -> GitEngineResult<()> {
        self.tier2.unstage_files(repo_root, paths).await
    }

    async fn apply_synthetic_patch_to_index(
        &self,
        repo_root: &HostPath,
        unified_patch: &[u8],
    ) -> GitEngineResult<()> {
        self.tier2
            .apply_synthetic_patch_to_index(repo_root, unified_patch)
            .await
    }

    async fn apply_reverse_patch_to_worktree(
        &self,
        repo_root: &HostPath,
        target_path: &RepoRelPath,
        reverse_unified_patch: &[u8],
    ) -> GitEngineResult<()> {
        self.tier2
            .apply_reverse_patch_to_worktree(repo_root, target_path, reverse_unified_patch)
            .await
    }

    async fn snapshot_index_tree_oid(&self, repo_root: &HostPath) -> GitEngineResult<GitOid> {
        self.tier2.snapshot_index_tree_oid(repo_root).await
    }

    async fn restore_index_from_tree_oid(
        &self,
        repo_root: &HostPath,
        tree_oid: GitOid,
    ) -> GitEngineResult<()> {
        self.tier2
            .restore_index_from_tree_oid(repo_root, tree_oid)
            .await
    }

    async fn predict_merge_conflicts(
        &self,
        repo_root: &HostPath,
        ours_oid: GitOid,
        theirs_oid: GitOid,
    ) -> GitEngineResult<MergePrediction> {
        self.tier2
            .predict_merge_conflicts(repo_root, ours_oid, theirs_oid)
            .await
    }
}

#[async_trait]
impl GitCliExecutor for BranvioGitEngine {
    async fn commit(
        &self,
        repo_root: &HostPath,
        options: CommitOptions,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.commit(repo_root, options).await
    }

    async fn checkout_ref(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        create_branch: Option<&str>,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .checkout_ref(repo_root, target_rev, create_branch)
            .await
    }

    async fn merge(
        &self,
        repo_root: &HostPath,
        options: MergeOptions,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.merge(repo_root, options).await
    }

    async fn rebase(
        &self,
        repo_root: &HostPath,
        onto_rev: &str,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.rebase(repo_root, onto_rev).await
    }

    async fn interactive_rebase(
        &self,
        repo_root: &HostPath,
        plan: RebasePlanPayload,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.interactive_rebase(repo_root, plan).await
    }

    async fn cherry_pick(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.cherry_pick(repo_root, commits).await
    }

    async fn revert(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.revert(repo_root, commits).await
    }

    async fn reset(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        mode: ResetMode,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.reset(repo_root, target_rev, mode).await
    }

    async fn fetch(
        &self,
        repo_root: &HostPath,
        remote: Option<&str>,
        prune: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.fetch(repo_root, remote, prune).await
    }

    async fn pull(
        &self,
        repo_root: &HostPath,
        rebase: bool,
        ff_only: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3.pull(repo_root, rebase, ff_only).await
    }

    async fn push(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
        force: PushForce,
        set_upstream: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .push(repo_root, remote, branch, force, set_upstream)
            .await
    }

    async fn remote_tracking_oid(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
    ) -> GitEngineResult<Option<GitOid>> {
        self.tier3
            .remote_tracking_oid(repo_root, remote, branch)
            .await
    }

    async fn stash_push(
        &self,
        repo_root: &HostPath,
        message: Option<&str>,
        include_untracked: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .stash_push(repo_root, message, include_untracked)
            .await
    }

    async fn stash_pop(
        &self,
        repo_root: &HostPath,
        stash_index: usize,
        drop_after_apply: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .stash_pop(repo_root, stash_index, drop_after_apply)
            .await
    }

    async fn worktree_list(&self, repo_root: &HostPath) -> GitEngineResult<Vec<WorktreeInfo>> {
        self.tier3.worktree_list(repo_root).await
    }

    async fn worktree_add(
        &self,
        repo_root: &HostPath,
        target_dir: &HostPath,
        branch_or_oid: &str,
        create_new_branch: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .worktree_add(repo_root, target_dir, branch_or_oid, create_new_branch)
            .await
    }

    async fn worktree_remove(
        &self,
        repo_root: &HostPath,
        worktree_path: &HostPath,
        force: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .worktree_remove(repo_root, worktree_path, force)
            .await
    }

    async fn discard_worktree_paths(
        &self,
        repo_root: &HostPath,
        tracked_paths: &[RepoRelPath],
        untracked_paths: &[RepoRelPath],
    ) -> GitEngineResult<CliCommandHandle> {
        self.tier3
            .discard_worktree_paths(repo_root, tracked_paths, untracked_paths)
            .await
    }

    async fn search_commits_pickaxe(
        &self,
        repo_root: &HostPath,
        request: PickaxeSearchRequest,
        cancel: CancelToken,
    ) -> GitEngineResult<Vec<GitOid>> {
        self.tier3
            .search_commits_pickaxe(repo_root, request, cancel)
            .await
    }
}
