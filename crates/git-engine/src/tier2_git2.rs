//! **Tier 2 (`git2`):** Pure local in-memory `.git/index` manipulation & predictive merge simulation.
//!
//! `git2::Repository` is `!Sync`, so every method runs inside `tokio::task::spawn_blocking`
//! opening a fresh repository handle. `git2` is compiled with `default-features = false`
//! (zero network/SSH/OpenSSL transports), and lock contention on `.git/index.lock` maps
//! cleanly to [`GitEngineError::IndexLocked`].

use crate::error::{GitEngineError, GitEngineResult};
use crate::oid_conv::{git_oid_from_git2, git_oid_to_git2};
use crate::traits::GitIndexEngine;
use crate::types::{MergePrediction, PredictiveConflictEntry};
use async_trait::async_trait;
use branvio_core::domain::ConflictSubkind;
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use std::path::Path;

/// Tier 2 in-memory index and predictive conflict engine backed by `git2`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Git2IndexEngine;

impl Git2IndexEngine {
    /// Creates a new [`Git2IndexEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GitIndexEngine for Git2IndexEngine {
    async fn stage_files(
        &self,
        repo_root: &HostPath,
        paths: &[RepoRelPath],
    ) -> GitEngineResult<()> {
        let root = repo_root.as_path().to_path_buf();
        let rels: Vec<RepoRelPath> = paths.to_vec();
        run_blocking("stage_files", move || stage_files_sync(&root, &rels)).await
    }

    async fn unstage_files(
        &self,
        repo_root: &HostPath,
        paths: &[RepoRelPath],
    ) -> GitEngineResult<()> {
        let root = repo_root.as_path().to_path_buf();
        let rels: Vec<RepoRelPath> = paths.to_vec();
        run_blocking("unstage_files", move || unstage_files_sync(&root, &rels)).await
    }

    async fn apply_synthetic_patch_to_index(
        &self,
        repo_root: &HostPath,
        unified_patch: &[u8],
    ) -> GitEngineResult<()> {
        let root = repo_root.as_path().to_path_buf();
        let patch_bytes = unified_patch.to_vec();
        run_blocking("apply_synthetic_patch_to_index", move || {
            apply_patch_sync(&root, &patch_bytes, git2::ApplyLocation::Index, None)
        })
        .await
    }

    async fn apply_reverse_patch_to_worktree(
        &self,
        repo_root: &HostPath,
        target_path: &RepoRelPath,
        reverse_unified_patch: &[u8],
    ) -> GitEngineResult<()> {
        let root = repo_root.as_path().to_path_buf();
        let target = target_path.clone();
        let patch_bytes = reverse_unified_patch.to_vec();
        run_blocking("apply_reverse_patch_to_worktree", move || {
            apply_patch_sync(
                &root,
                &patch_bytes,
                git2::ApplyLocation::WorkDir,
                Some(&target),
            )
        })
        .await
    }

    async fn snapshot_index_tree_oid(&self, repo_root: &HostPath) -> GitEngineResult<GitOid> {
        let root = repo_root.as_path().to_path_buf();
        run_blocking("snapshot_index_tree_oid", move || {
            let repo = open_git2_repo(&root)?;
            let mut index = repo
                .index()
                .map_err(|e| map_git2_err("open_index", &repo, e))?;
            let tree_id = index
                .write_tree_to(&repo)
                .map_err(|e| map_git2_err("write_tree", &repo, e))?;
            git_oid_from_git2(tree_id)
        })
        .await
    }

    async fn restore_index_from_tree_oid(
        &self,
        repo_root: &HostPath,
        tree_oid: GitOid,
    ) -> GitEngineResult<()> {
        let root = repo_root.as_path().to_path_buf();
        run_blocking("restore_index_from_tree_oid", move || {
            let repo = open_git2_repo(&root)?;
            let git2_tree_oid = git_oid_to_git2(tree_oid)?;
            let tree = repo
                .find_tree(git2_tree_oid)
                .map_err(|e| map_git2_err("find_tree", &repo, e))?;
            let mut index = repo
                .index()
                .map_err(|e| map_git2_err("open_index", &repo, e))?;
            index
                .read_tree(&tree)
                .map_err(|e| map_git2_err("read_tree", &repo, e))?;
            index
                .write()
                .map_err(|e| map_git2_err("write_index", &repo, e))?;
            Ok(())
        })
        .await
    }

    async fn predict_merge_conflicts(
        &self,
        repo_root: &HostPath,
        ours_oid: GitOid,
        theirs_oid: GitOid,
    ) -> GitEngineResult<MergePrediction> {
        let root = repo_root.as_path().to_path_buf();
        run_blocking("predict_merge_conflicts", move || {
            predict_merge_conflicts_sync(&root, ours_oid, theirs_oid)
        })
        .await
    }
}

async fn run_blocking<F, T>(op: &'static str, f: F) -> GitEngineResult<T>
where
    F: FnOnce() -> GitEngineResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| GitEngineError::IndexOp {
            operation: op,
            message: err.to_string(),
        })?
}

fn open_git2_repo(root: &Path) -> GitEngineResult<git2::Repository> {
    git2::Repository::open(root).map_err(|err| GitEngineError::OpenRepoFailed {
        path: root.display().to_string(),
        message: err.to_string(),
    })
}

fn resolve_index_lock_path(repo: &git2::Repository) -> String {
    if let Ok(index) = repo.index()
        && let Some(idx_path) = index.path()
    {
        return format!("{}.lock", idx_path.display());
    }
    repo.path().join("index.lock").display().to_string()
}

fn map_git2_err(op: &'static str, repo: &git2::Repository, err: git2::Error) -> GitEngineError {
    if err.code() == git2::ErrorCode::Locked {
        return GitEngineError::IndexLocked {
            lock_path: resolve_index_lock_path(repo),
        };
    }
    GitEngineError::IndexOp {
        operation: op,
        message: err.message().to_string(),
    }
}

fn stage_files_sync(root: &Path, paths: &[RepoRelPath]) -> GitEngineResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let repo = open_git2_repo(root)?;
    let workdir = repo.workdir().ok_or_else(|| GitEngineError::IndexOp {
        operation: "stage_files",
        message: "cannot stage files in a bare repository".to_string(),
    })?;
    let mut index = repo
        .index()
        .map_err(|e| map_git2_err("open_index", &repo, e))?;

    for rel in paths {
        let disk_path = workdir.join(rel.as_str());
        // Use symlink_metadata so dangling symlinks are staged as symlinks rather than deletions.
        if disk_path.symlink_metadata().is_ok() {
            index
                .add_path(Path::new(rel.as_str()))
                .map_err(|e| map_git2_err("add_path", &repo, e))?;
        } else {
            index
                .remove_path(Path::new(rel.as_str()))
                .map_err(|e| map_git2_err("remove_path", &repo, e))?;
        }
    }
    index
        .write()
        .map_err(|e| map_git2_err("write_index", &repo, e))?;
    Ok(())
}

fn unstage_files_sync(root: &Path, paths: &[RepoRelPath]) -> GitEngineResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let repo = open_git2_repo(root)?;
    let raw_paths: Vec<&str> = paths.iter().map(RepoRelPath::as_str).collect();

    match repo.head() {
        Ok(head_ref) => {
            let head_obj = head_ref
                .peel(git2::ObjectType::Commit)
                .map_err(|e| map_git2_err("peel_head", &repo, e))?;
            repo.reset_default(Some(&head_obj), raw_paths)
                .map_err(|e| map_git2_err("reset_default", &repo, e))?;
        }
        Err(err) if err.code() == git2::ErrorCode::UnbornBranch => {
            // Unborn branch has no HEAD tree: simply remove staged paths from `.git/index`.
            let mut index = repo
                .index()
                .map_err(|e| map_git2_err("open_index", &repo, e))?;
            for rel in paths {
                let _ = index.remove_path(Path::new(rel.as_str()));
            }
            index
                .write()
                .map_err(|e| map_git2_err("write_index", &repo, e))?;
        }
        Err(err) => return Err(map_git2_err("read_head", &repo, err)),
    }
    Ok(())
}

fn apply_patch_sync(
    root: &Path,
    patch_bytes: &[u8],
    location: git2::ApplyLocation,
    target_path: Option<&RepoRelPath>,
) -> GitEngineResult<()> {
    let repo = open_git2_repo(root)?;
    let diff =
        git2::Diff::from_buffer(patch_bytes).map_err(|err| GitEngineError::PatchConflict {
            reason: format!("invalid unified patch buffer: {}", err.message()),
        })?;

    let mut matched_target = target_path.is_none();
    let apply_res = {
        let mut opts = git2::ApplyOptions::new();
        if let Some(target_rel) = target_path {
            let target_os_path = Path::new(target_rel.as_str());
            opts.delta_callback(|delta| {
                let Some(d) = delta else {
                    return false;
                };
                let is_match = d.new_file().path() == Some(target_os_path)
                    || d.old_file().path() == Some(target_os_path);
                if is_match {
                    matched_target = true;
                }
                is_match
            });
        }
        repo.apply(&diff, location, Some(&mut opts))
    };

    apply_res.map_err(|err| {
        if err.code() == git2::ErrorCode::Locked {
            GitEngineError::IndexLocked {
                lock_path: resolve_index_lock_path(&repo),
            }
        } else if matches!(
            err.class(),
            git2::ErrorClass::Os | git2::ErrorClass::Filesystem
        ) {
            GitEngineError::Io {
                operation: "apply_patch",
                source: std::io::Error::other(err.message().to_string()),
            }
        } else {
            GitEngineError::PatchConflict {
                reason: err.message().to_string(),
            }
        }
    })?;

    if !matched_target {
        return Err(GitEngineError::PatchConflict {
            reason: format!(
                "patch contains no delta matching target path '{}'",
                target_path.map_or("", RepoRelPath::as_str)
            ),
        });
    }

    Ok(())
}

fn predict_merge_conflicts_sync(
    root: &Path,
    ours_oid: GitOid,
    theirs_oid: GitOid,
) -> GitEngineResult<MergePrediction> {
    let repo = open_git2_repo(root)?;
    let our_id = git_oid_to_git2(ours_oid)?;
    let their_id = git_oid_to_git2(theirs_oid)?;

    let our_commit = repo
        .find_commit(our_id)
        .map_err(|e| map_git2_err("find_ours_commit", &repo, e))?;
    let their_commit = repo
        .find_commit(their_id)
        .map_err(|e| map_git2_err("find_theirs_commit", &repo, e))?;

    let is_fast_forward = our_id == their_id
        || repo
            .graph_descendant_of(their_id, our_id)
            .map_err(|e| map_git2_err("graph_descendant_of", &repo, e))?;

    let merged_index = repo
        .merge_commits(&our_commit, &their_commit, None)
        .map_err(|e| map_git2_err("merge_commits", &repo, e))?;

    if !merged_index.has_conflicts() {
        return Ok(MergePrediction {
            source_oid: theirs_oid,
            target_oid: ours_oid,
            will_succeed_cleanly: true,
            is_fast_forward,
            conflicts: Vec::new(),
        });
    }

    let mut conflicts = Vec::new();
    let iter = merged_index
        .conflicts()
        .map_err(|e| map_git2_err("iterate_conflicts", &repo, e))?;

    for conflict_res in iter {
        let entry = conflict_res.map_err(|e| map_git2_err("read_conflict", &repo, e))?;
        let raw_path = entry
            .our
            .as_ref()
            .or(entry.their.as_ref())
            .or(entry.ancestor.as_ref())
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .unwrap_or_default();
        if raw_path.is_empty() {
            continue;
        }
        let kind = match (
            entry.ancestor.is_some(),
            entry.our.is_some(),
            entry.their.is_some(),
        ) {
            (true, true, true) => ConflictSubkind::BothModified,
            (false, true, true) => ConflictSubkind::BothAdded,
            (true, false, true) => ConflictSubkind::DeletedByUs,
            (true, true, false) => ConflictSubkind::DeletedByThem,
            (false, true, false) => ConflictSubkind::AddedByUs,
            (false, false, true) => ConflictSubkind::AddedByThem,
            _ => ConflictSubkind::BothDeleted,
        };
        let Ok(normalized_path) = RepoRelPath::normalize(&raw_path) else {
            tracing::warn!("skipping unnormalizable conflict path: {raw_path}");
            continue;
        };
        conflicts.push(PredictiveConflictEntry {
            path: normalized_path,
            kind,
        });
    }

    Ok(MergePrediction {
        source_oid: theirs_oid,
        target_oid: ours_oid,
        will_succeed_cleanly: false,
        is_fast_forward,
        conflicts,
    })
}
