//! Transactional Time-Machine Undo/Redo orchestrator combining [`OpLogStore`] and [`HybridGitEngine`].

use crate::store::compute_repo_root_hash;
use crate::traits::{
    OpLogError, OpLogResult, OpLogStore, PushSafetyCheck, UndoOutcome, UndoRedoEngine,
};
use async_trait::async_trait;
use branvio_core::domain::{
    HeadTargetSnapshot, OpKind, OpLogEntry, RefMutationRecord, RemoteExposureState,
    RepoOperationState, RepoStateSnapshot, ResetMode,
};
use branvio_core::ids::GitOid;
use branvio_core::paths::HostPath;
use branvio_core::platform::{HydratedGitEnv, SubprocessEnvOptions};
use compact_str::CompactString;
use git_engine::{GitEngineError, HybridGitEngine};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

/// Ref namespace pinning full worktree safety snapshots against `gc`.
///
/// Pins are named `refs/branvio/undo/<op-id>-<timestamp-ms>-<snapshot-oid>` (see
/// [`snapshot_pin_ref`]) so a fresh journal whose op ids restart at 1 can never collide with,
/// let alone overwrite, an older pin. Entries recorded before this scheme used
/// `refs/branvio/undo/<op-id>`; those legacy pins are still honoured while their entry lives.
pub const UNDO_SNAPSHOT_REF_PREFIX: &str = "refs/branvio/undo/";

/// Returns the pin ref protecting `entry`'s worktree snapshot, if it has one.
pub fn snapshot_pin_ref(entry: &OpLogEntry) -> Option<String> {
    entry
        .before
        .worktree_safety_stash_oid
        .map(|oid| pin_ref_name(entry.op_id, entry.timestamp_unix_ms, oid))
}

fn pin_ref_name(op_id: u64, timestamp_unix_ms: i64, snapshot: GitOid) -> String {
    format!(
        "{UNDO_SNAPSHOT_REF_PREFIX}{op_id}-{timestamp_unix_ms}-{}",
        snapshot.to_hex()
    )
}

fn legacy_pin_ref_name(op_id: u64) -> String {
    format!("{UNDO_SNAPSHOT_REF_PREFIX}{op_id}")
}

/// Trailer identifying a full worktree snapshot commit (tracked + untracked files).
const WORKTREE_SNAPSHOT_MARKER: &str = "branvio-snapshot-format: worktree-v2";
const WORKTREE_SNAPSHOT_MESSAGE: &str =
    "branvio-undo-safety\n\nbranvio-snapshot-format: worktree-v2\n";
const SNAPSHOT_IDENT_NAME: &str = "Branvio Undo";
const SNAPSHOT_IDENT_EMAIL: &str = "undo@branvio.invalid";

/// Production Time-Machine Undo/Redo engine persisting state snapshots to [`OpLogStore`]
/// and executing safe local rollbacks / redos with remote-exposure & divergence guards.
pub struct BranvioUndoEngine {
    store: Arc<dyn OpLogStore>,
    git_engine: Arc<dyn HybridGitEngine>,
    git_env: Arc<HydratedGitEnv>,
}

impl BranvioUndoEngine {
    /// Creates a new [`BranvioUndoEngine`] backed by `store`, `git_engine`, and `git_env`.
    pub fn new(
        store: Arc<dyn OpLogStore>,
        git_engine: Arc<dyn HybridGitEngine>,
        git_env: Arc<HydratedGitEnv>,
    ) -> Self {
        Self {
            store,
            git_engine,
            git_env,
        }
    }

    async fn run_git(
        &self,
        repo_root: &HostPath,
        args: &[&str],
        read_only: bool,
    ) -> OpLogResult<std::process::Output> {
        self.run_git_with_env(repo_root, args, read_only, &[]).await
    }

    async fn run_git_with_env(
        &self,
        repo_root: &HostPath,
        args: &[&str],
        read_only: bool,
        extra_env: &[(&str, &OsStr)],
    ) -> OpLogResult<std::process::Output> {
        let git_bin = self
            .git_env
            .git_executable
            .as_deref()
            .unwrap_or_else(|| Path::new("git"));
        let envs = self.git_env.build_subprocess_env(SubprocessEnvOptions {
            read_only_no_locks: read_only,
            force_c_locale: true,
            ..SubprocessEnvOptions::default()
        });
        Command::new(git_bin)
            .args(args)
            .current_dir(repo_root.as_path())
            .envs(envs)
            .envs(extra_env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| OpLogError::Storage(format!("failed to execute git {args:?}: {e}")))
    }

    async fn run_git_checked(
        &self,
        repo_root: &HostPath,
        args: &[&str],
        read_only: bool,
    ) -> OpLogResult<String> {
        let out = self.run_git(repo_root, args, read_only).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(OpLogError::Git(GitEngineError::CliFailed {
                subcommand: format!("git {}", args.join(" ")),
                exit_code: out.status.code(),
                stderr_summary: stderr,
            }))
        }
    }

    async fn capture_head_snapshot(&self, repo_root: &HostPath) -> OpLogResult<HeadTargetSnapshot> {
        let sym_out = self
            .run_git(repo_root, &["symbolic-ref", "-q", "HEAD"], true)
            .await?;
        let head_oid_out = self
            .run_git(repo_root, &["rev-parse", "--verify", "-q", "HEAD"], true)
            .await?;
        let target_oid =
            if head_oid_out.status.success() {
                let hex = String::from_utf8_lossy(&head_oid_out.stdout)
                    .trim()
                    .to_string();
                if hex.is_empty() {
                    None
                } else {
                    Some(GitOid::from_hex(&hex).map_err(|e| {
                        OpLogError::Storage(format!("invalid HEAD oid '{hex}': {e}"))
                    })?)
                }
            } else {
                None
            };

        if sym_out.status.success() {
            let full_ref = CompactString::from(String::from_utf8_lossy(&sym_out.stdout).trim());
            return Ok(HeadTargetSnapshot::Symbolic {
                full_ref,
                target_oid,
            });
        }

        if let Some(oid) = target_oid {
            Ok(HeadTargetSnapshot::Detached { oid })
        } else {
            Ok(HeadTargetSnapshot::Symbolic {
                full_ref: CompactString::const_new("refs/heads/main"),
                target_oid: None,
            })
        }
    }

    async fn read_local_refs(
        &self,
        repo_root: &HostPath,
    ) -> OpLogResult<BTreeMap<CompactString, GitOid>> {
        let raw = self
            .run_git_checked(
                repo_root,
                &[
                    "for-each-ref",
                    "--format=%(refname)%09%(objectname)",
                    "refs/heads",
                    "refs/tags",
                ],
                true,
            )
            .await?;
        let mut map = BTreeMap::new();
        for line in raw.lines() {
            if let Some((refname, hex)) = line.split_once('\t')
                && let Ok(oid) = GitOid::from_hex(hex.trim())
            {
                map.insert(CompactString::from(refname.trim()), oid);
            }
        }
        Ok(map)
    }

    async fn capture_snapshot(
        &self,
        repo_root: &HostPath,
        op_kind_for_stash: Option<&OpKind>,
    ) -> OpLogResult<(RepoStateSnapshot, BTreeMap<CompactString, GitOid>)> {
        let summary = self.git_engine.open_repo(repo_root.as_path()).await?;
        let head = self.capture_head_snapshot(repo_root).await?;
        let local_refs = self.read_local_refs(repo_root).await?;
        let index_tree_oid = match self.git_engine.snapshot_index_tree_oid(repo_root).await {
            Ok(oid) => Some(oid),
            // Pre-mutation: fail closed. Without the staged tree, undo would silently lose the
            // index (e.g. an unmerged index during a conflicted merge cannot be written as a tree).
            Err(err) if op_kind_for_stash.is_some() => {
                return Err(OpLogError::SnapshotIncomplete {
                    reason: format!(
                        "the staged index could not be captured (are there unresolved merge \
                         conflicts?): {err}"
                    ),
                });
            }
            // Post-mutation: the operation already ran; recording it without an index tree
            // (redo then restores the index from `HEAD`) beats losing the undo point entirely.
            Err(_) => None,
        };

        let refs_snapshot = local_refs
            .iter()
            .map(|(full_ref, oid)| RefMutationRecord {
                full_ref: full_ref.clone(),
                old_oid: Some(*oid),
                new_oid: Some(*oid),
            })
            .collect();

        let worktree_safety_stash_oid = match op_kind_for_stash {
            Some(kind) if op_mutates_worktree(kind) => {
                let dirty_out = self
                    .run_git_checked(
                        repo_root,
                        &["status", "--porcelain", "--untracked-files=normal"],
                        true,
                    )
                    .await?;
                if dirty_out.trim().is_empty() {
                    None
                } else {
                    Some(
                        self.create_worktree_snapshot(repo_root, head_oid(&head), index_tree_oid)
                            .await?,
                    )
                }
            }
            _ => None,
        };

        Ok((
            RepoStateSnapshot {
                head,
                refs_snapshot,
                index_tree_oid,
                worktree_safety_stash_oid,
                operation_state: summary.operation_state,
            },
            local_refs,
        ))
    }

    /// Captures the complete working tree — tracked modifications/deletions AND untracked
    /// (non-ignored) files — as a commit object, without touching the real index or worktree.
    ///
    /// `git stash create` silently drops untracked files, so destructive operations could lose
    /// them forever while the UI promised Undo. Instead this builds the tree in a throw-away
    /// index (`GIT_INDEX_FILE` inside a temp dir that is deleted on return), seeded with the
    /// staged tree (so force-added ignored files stay captured), then `git add -A`.
    ///
    /// The commit's message carries [`WORKTREE_SNAPSHOT_MARKER`]; parents are `HEAD` (if born)
    /// and a commit wrapping the staged index tree (keeping it reachable for `gc`). The caller
    /// pins the result under [`UNDO_SNAPSHOT_REF_PREFIX`]`<op-id>`.
    async fn create_worktree_snapshot(
        &self,
        repo_root: &HostPath,
        head: Option<GitOid>,
        index_tree: Option<GitOid>,
    ) -> OpLogResult<GitOid> {
        let temp_dir = tempfile::Builder::new()
            .prefix("branvio-undo-index")
            .tempdir()
            .map_err(|e| OpLogError::Storage(format!("failed to create temp index dir: {e}")))?;
        // Must not exist yet: git treats a missing index file as empty (a 0-byte one is corrupt).
        let temp_index = temp_dir.path().join("index");
        let index_env = [("GIT_INDEX_FILE", temp_index.as_os_str())];

        let seed = index_tree.or(head).map(|oid| oid.to_hex());
        if let Some(seed_hex) = seed.as_deref() {
            self.run_git_env_checked(
                repo_root,
                &["read-tree", "--end-of-options", seed_hex],
                &index_env,
            )
            .await?;
        }
        self.run_git_env_checked(repo_root, &["add", "-A"], &index_env)
            .await?;
        let tree_hex = self
            .run_git_env_checked(repo_root, &["write-tree"], &index_env)
            .await?;
        let tree_hex = tree_hex.trim();

        let index_commit_hex = match index_tree {
            Some(tree_oid) => Some(
                self.commit_tree(repo_root, &tree_oid.to_hex(), &[], "branvio-undo-index")
                    .await?,
            ),
            None => None,
        };
        let mut parents = Vec::new();
        if let Some(h) = head {
            parents.push(h.to_hex());
        }
        parents.extend(index_commit_hex);
        let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();
        let snapshot_hex = self
            .commit_tree(repo_root, tree_hex, &parent_refs, WORKTREE_SNAPSHOT_MESSAGE)
            .await?;

        // Explicit close so a failed temp-dir removal is not silently ignored.
        temp_dir
            .close()
            .map_err(|e| OpLogError::Storage(format!("failed to remove temp index: {e}")))?;

        GitOid::from_hex(&snapshot_hex)
            .map_err(|e| OpLogError::Storage(format!("invalid snapshot oid '{snapshot_hex}': {e}")))
    }

    /// Runs a read-only (`GIT_OPTIONAL_LOCKS=0`) git command with extra env, failing on non-zero exit.
    async fn run_git_env_checked(
        &self,
        repo_root: &HostPath,
        args: &[&str],
        extra_env: &[(&str, &OsStr)],
    ) -> OpLogResult<String> {
        let out = self
            .run_git_with_env(repo_root, args, true, extra_env)
            .await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(OpLogError::Git(GitEngineError::CliFailed {
                subcommand: format!("git {}", args.join(" ")),
                exit_code: out.status.code(),
                stderr_summary: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }))
        }
    }

    /// Creates an unsigned internal commit object (fixed identity so it never depends on user config).
    async fn commit_tree(
        &self,
        repo_root: &HostPath,
        tree_hex: &str,
        parents: &[&str],
        message: &str,
    ) -> OpLogResult<String> {
        let mut args = vec!["commit-tree", "--no-gpg-sign", "-m", message];
        for parent in parents {
            args.push("-p");
            args.push(parent);
        }
        args.push("--end-of-options");
        args.push(tree_hex);
        let identity = [
            ("GIT_AUTHOR_NAME", OsStr::new(SNAPSHOT_IDENT_NAME)),
            ("GIT_AUTHOR_EMAIL", OsStr::new(SNAPSHOT_IDENT_EMAIL)),
            ("GIT_COMMITTER_NAME", OsStr::new(SNAPSHOT_IDENT_NAME)),
            ("GIT_COMMITTER_EMAIL", OsStr::new(SNAPSHOT_IDENT_EMAIL)),
        ];
        let out = self
            .run_git_env_checked(repo_root, &args, &identity)
            .await?;
        Ok(out.trim().to_string())
    }

    /// Pins a worktree snapshot under `ref_name` so `git gc` never prunes it.
    ///
    /// Created with an empty expected old value, so this fails instead of overwriting if the
    /// ref already exists.
    async fn pin_worktree_snapshot(
        &self,
        repo_root: &HostPath,
        ref_name: &str,
        snapshot: GitOid,
    ) -> OpLogResult<()> {
        let hex = snapshot.to_hex();
        self.run_git_checked(
            repo_root,
            &["update-ref", "--end-of-options", ref_name, &hex, ""],
            false,
        )
        .await?;
        Ok(())
    }

    /// Deletes every snapshot pin under [`UNDO_SNAPSHOT_REF_PREFIX`] that no longer belongs to
    /// a live journal entry of this repository (redo tail dropped, entry discarded, retention
    /// cap reached, or left behind by an older/reset journal). Returns how many were deleted.
    ///
    /// Refs are listed BEFORE the journal is read: a pin is only created after its entry is
    /// recorded, so a concurrently created pin is either not listed yet or already live.
    pub async fn prune_snapshot_pins(&self, repo_root: &HostPath) -> OpLogResult<usize> {
        let namespace = UNDO_SNAPSHOT_REF_PREFIX.trim_end_matches('/');
        let raw = self
            .run_git_checked(
                repo_root,
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)",
                    "--end-of-options",
                    namespace,
                ],
                true,
            )
            .await?;

        let repo_hash = compute_repo_root_hash(repo_root);
        let mut live: BTreeSet<(String, GitOid)> = BTreeSet::new();
        for entry in self.store.list_recent(repo_hash, usize::MAX)? {
            if let Some(snapshot) = entry.before.worktree_safety_stash_oid {
                live.insert((
                    pin_ref_name(entry.op_id, entry.timestamp_unix_ms, snapshot),
                    snapshot,
                ));
                live.insert((legacy_pin_ref_name(entry.op_id), snapshot));
            }
        }

        let mut deleted = 0usize;
        let mut failures = Vec::new();
        for line in raw.lines() {
            let Some((name, hex)) = line.split_once('\0') else {
                continue;
            };
            let hex = hex.trim();
            let Ok(oid) = GitOid::from_hex(hex) else {
                failures.push(format!("{name}: unexpected object id '{hex}'"));
                continue;
            };
            if live.contains(&(name.to_string(), oid)) {
                continue;
            }
            // Expected old value: never delete a pin that was concurrently re-pointed.
            match self
                .run_git_checked(
                    repo_root,
                    &["update-ref", "-d", "--end-of-options", name, hex],
                    false,
                )
                .await
            {
                Ok(_) => deleted += 1,
                Err(err) => failures.push(format!("{name}: {err}")),
            }
        }
        if failures.is_empty() {
            Ok(deleted)
        } else {
            Err(OpLogError::Storage(format!(
                "failed to delete stale undo snapshot pins: {}",
                failures.join("; ")
            )))
        }
    }

    /// Returns `true` if `oid` is a full worktree snapshot (vs. a legacy `git stash create` commit).
    async fn is_worktree_snapshot(&self, repo_root: &HostPath, oid: GitOid) -> OpLogResult<bool> {
        let hex = oid.to_hex();
        let body = self
            .run_git_checked(repo_root, &["cat-file", "commit", &hex], true)
            .await?;
        Ok(body.contains(WORKTREE_SNAPSHOT_MARKER))
    }

    /// Restores worktree + index from a snapshot made by [`Self::create_worktree_snapshot`].
    ///
    /// Expects the worktree to already be reset to `head`. Writes every snapshot path
    /// (overwriting, so untracked files come back even if a same-named file reappeared),
    /// deletes paths the user had deleted, then puts the staged tree back into the index so
    /// formerly-untracked files show up as untracked again.
    ///
    /// If writing the snapshot fails midway, the pre-restore index is put back before the error
    /// is returned (so formerly-untracked files never end up staged). Files that could not be
    /// deleted do not abort the restore; they are returned as warnings.
    async fn restore_worktree_snapshot(
        &self,
        repo_root: &HostPath,
        snapshot: GitOid,
        head: Option<GitOid>,
        index_tree: Option<GitOid>,
    ) -> OpLogResult<Vec<String>> {
        // The worktree was just reset to `head`, so the index is fully merged and writable.
        let pre_restore_hex = self
            .run_git_checked(repo_root, &["write-tree"], false)
            .await?
            .trim()
            .to_string();
        let pre_restore = GitOid::from_hex(&pre_restore_hex).map_err(|e| {
            OpLogError::Storage(format!("invalid index tree oid '{pre_restore_hex}': {e}"))
        })?;

        let warnings = match self
            .write_snapshot_to_worktree(repo_root, snapshot, head)
            .await
        {
            Ok(warnings) => warnings,
            Err(err) => {
                let hex = pre_restore.to_hex();
                if let Err(rollback) = self
                    .run_git_checked(
                        repo_root,
                        &["read-tree", "--reset", "--end-of-options", &hex],
                        false,
                    )
                    .await
                {
                    return Err(OpLogError::Storage(format!(
                        "restoring the worktree snapshot failed ({err}); putting back the \
                         original index also failed ({rollback})"
                    )));
                }
                return Err(err);
            }
        };

        match index_tree.or(head) {
            Some(oid) => {
                let hex = oid.to_hex();
                self.run_git_checked(
                    repo_root,
                    &["read-tree", "--reset", "--end-of-options", &hex],
                    false,
                )
                .await?;
            }
            None => {
                self.run_git_checked(repo_root, &["read-tree", "--empty"], false)
                    .await?;
            }
        }
        // Refresh stat info; non-zero merely means some entries differ from the worktree.
        let _ = self
            .run_git(repo_root, &["update-index", "-q", "--refresh"], false)
            .await?;
        Ok(warnings)
    }

    /// Loads the snapshot into the index, checks every path out and removes paths that were
    /// deleted in the snapshot relative to `head`. Leaves the index holding the snapshot tree.
    async fn write_snapshot_to_worktree(
        &self,
        repo_root: &HostPath,
        snapshot: GitOid,
        head: Option<GitOid>,
    ) -> OpLogResult<Vec<String>> {
        let snap_hex = snapshot.to_hex();
        self.run_git_checked(
            repo_root,
            &["read-tree", "--reset", "--end-of-options", &snap_hex],
            false,
        )
        .await?;
        self.run_git_checked(repo_root, &["checkout-index", "--all", "--force"], false)
            .await?;

        let mut warnings = Vec::new();
        if let Some(h) = head {
            let h_hex = h.to_hex();
            let deleted = self
                .run_git_checked(
                    repo_root,
                    &[
                        "diff-tree",
                        "-r",
                        "-z",
                        "--name-only",
                        "--no-renames",
                        "--diff-filter=D",
                        "--end-of-options",
                        &h_hex,
                        &snap_hex,
                    ],
                    true,
                )
                .await?;
            for rel in deleted.split('\0').filter(|p| !p.is_empty()) {
                let rel_path = Path::new(rel);
                if !rel_path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                {
                    warnings.push(format!("skipped deleting suspicious path '{rel}'"));
                    continue;
                }
                match std::fs::remove_file(repo_root.as_path().join(rel_path)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warnings.push(format!("could not delete '{rel}': {e}")),
                }
            }
        }
        Ok(warnings)
    }

    async fn verify_no_external_divergence(
        &self,
        repo_root: &HostPath,
        op_id: u64,
        expected: &RepoStateSnapshot,
        is_undo: bool,
    ) -> OpLogResult<()> {
        let live_head = self.capture_head_snapshot(repo_root).await?;
        if live_head != expected.head {
            return Err(OpLogError::ExternalDivergence {
                op_id,
                reason: format!(
                    "expected HEAD {:?} before rollback, found {:?}",
                    expected.head, live_head
                ),
            });
        }

        let live_refs = self.read_local_refs(repo_root).await?;
        for rec in &expected.refs_snapshot {
            let expected_oid = if is_undo { rec.new_oid } else { rec.old_oid };
            let actual_oid = live_refs.get(&rec.full_ref).copied();
            if actual_oid != expected_oid {
                return Err(OpLogError::ExternalDivergence {
                    op_id,
                    reason: format!(
                        "ref '{}' changed externally (expected {:?}, found {:?})",
                        rec.full_ref, expected_oid, actual_oid
                    ),
                });
            }
        }
        Ok(())
    }

    async fn compute_remote_exposure(
        &self,
        repo_root: &HostPath,
        from_snapshot: &RepoStateSnapshot,
        to_snapshot: &RepoStateSnapshot,
    ) -> OpLogResult<RemoteExposureState> {
        let remotes_out = self
            .run_git(
                repo_root,
                &["for-each-ref", "--format=%(refname:short)", "refs/remotes"],
                true,
            )
            .await?;
        if String::from_utf8_lossy(&remotes_out.stdout)
            .trim()
            .is_empty()
        {
            return Ok(RemoteExposureState::LocalOnly);
        }

        let mut candidate_lost_commits = Vec::new();
        collect_lost_commits_for_head(
            self,
            repo_root,
            head_oid(&from_snapshot.head),
            head_oid(&to_snapshot.head),
            &mut candidate_lost_commits,
        )
        .await?;

        for diff in &from_snapshot.refs_snapshot {
            collect_lost_commits_for_head(
                self,
                repo_root,
                diff.new_oid,
                diff.old_oid,
                &mut candidate_lost_commits,
            )
            .await?;
        }
        for diff in &to_snapshot.refs_snapshot {
            collect_lost_commits_for_head(
                self,
                repo_root,
                diff.new_oid,
                diff.old_oid,
                &mut candidate_lost_commits,
            )
            .await?;
        }

        let mut exposed_branches = BTreeSet::new();
        let mut diverged_oids = Vec::new();
        let mut seen_oids = BTreeSet::new();

        for oid in candidate_lost_commits {
            if !seen_oids.insert(oid) {
                continue;
            }
            let hex = oid.to_hex();
            let contains = self
                .run_git(repo_root, &["branch", "-r", "--contains", &hex], true)
                .await?;
            if contains.status.success() {
                let text = String::from_utf8_lossy(&contains.stdout);
                let mut hit = false;
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        exposed_branches.insert(CompactString::from(trimmed));
                        hit = true;
                    }
                }
                if hit {
                    diverged_oids.push(oid);
                }
            }
        }

        if exposed_branches.is_empty() {
            Ok(RemoteExposureState::LocalOnly)
        } else {
            Ok(RemoteExposureState::TouchesPushedHistory {
                remote_branches: exposed_branches.into_iter().collect(),
                diverged_oids,
            })
        }
    }

    async fn apply_restore(
        &self,
        repo_root: &HostPath,
        op_kind: &OpKind,
        ref_diffs: &[RefMutationRecord],
        target: &RepoStateSnapshot,
        is_undo: bool,
    ) -> OpLogResult<Vec<String>> {
        let mut warnings = Vec::new();
        for rec in ref_diffs {
            let desired = if is_undo { rec.old_oid } else { rec.new_oid };
            match desired {
                Some(oid) => {
                    let hex = oid.to_hex();
                    self.run_git_checked(
                        repo_root,
                        &["update-ref", rec.full_ref.as_str(), &hex],
                        false,
                    )
                    .await?;
                }
                None => {
                    let _ = self
                        .run_git(
                            repo_root,
                            &["update-ref", "-d", rec.full_ref.as_str()],
                            false,
                        )
                        .await?;
                }
            }
        }

        match &target.head {
            HeadTargetSnapshot::Symbolic {
                full_ref,
                target_oid,
            } => {
                if let Some(oid) = target_oid {
                    let hex = oid.to_hex();
                    self.run_git_checked(
                        repo_root,
                        &["update-ref", full_ref.as_str(), &hex],
                        false,
                    )
                    .await?;
                } else {
                    let _ = self
                        .run_git(repo_root, &["update-ref", "-d", full_ref.as_str()], false)
                        .await?;
                }
                self.run_git_checked(
                    repo_root,
                    &["symbolic-ref", "HEAD", full_ref.as_str()],
                    false,
                )
                .await?;
            }
            HeadTargetSnapshot::Detached { oid } => {
                let hex = oid.to_hex();
                self.run_git_checked(
                    repo_root,
                    &["update-ref", "--no-deref", "HEAD", &hex],
                    false,
                )
                .await?;
            }
        }

        let summary = self.git_engine.open_repo(repo_root.as_path()).await?;
        if target.operation_state == RepoOperationState::Clean
            && summary.operation_state != RepoOperationState::Clean
        {
            clear_sequencer_files(summary.git_dir.as_path());
        }

        let restore_worktree = op_mutates_worktree(op_kind);
        if restore_worktree {
            if is_undo && let Some(stash_oid) = target.worktree_safety_stash_oid {
                if let Some(h_oid) = head_oid(&target.head) {
                    let hex = h_oid.to_hex();
                    self.run_git_checked(repo_root, &["read-tree", "--reset", "-u", &hex], false)
                        .await?;
                }
                if self.is_worktree_snapshot(repo_root, stash_oid).await? {
                    warnings = self
                        .restore_worktree_snapshot(
                            repo_root,
                            stash_oid,
                            head_oid(&target.head),
                            target.index_tree_oid,
                        )
                        .await?;
                } else {
                    // Legacy entries recorded with `git stash create` (tracked changes only).
                    let s_hex = stash_oid.to_hex();
                    let res = self
                        .run_git(
                            repo_root,
                            &["stash", "apply", "--index", "--quiet", &s_hex],
                            false,
                        )
                        .await?;
                    if !res.status.success() {
                        let _ = self
                            .run_git(repo_root, &["stash", "apply", "--quiet", &s_hex], false)
                            .await?;
                    }
                }
            } else if let Some(tree_oid) = target.index_tree_oid {
                let hex = tree_oid.to_hex();
                self.run_git_checked(repo_root, &["read-tree", "--reset", "-u", &hex], false)
                    .await?;
            } else if let Some(h_oid) = head_oid(&target.head) {
                let hex = h_oid.to_hex();
                self.run_git_checked(repo_root, &["read-tree", "--reset", "-u", &hex], false)
                    .await?;
            }
        } else if let Some(tree_oid) = target.index_tree_oid {
            self.git_engine
                .restore_index_from_tree_oid(repo_root, tree_oid)
                .await?;
        }

        Ok(warnings)
    }
}

async fn collect_lost_commits_for_head(
    engine: &BranvioUndoEngine,
    repo_root: &HostPath,
    source: Option<GitOid>,
    target: Option<GitOid>,
    out: &mut Vec<GitOid>,
) -> OpLogResult<()> {
    let Some(src_oid) = source else {
        return Ok(());
    };
    if Some(src_oid) == target {
        return Ok(());
    }
    let src_hex = src_oid.to_hex();
    let rev_list_out = match target {
        Some(tgt_oid) => {
            let tgt_hex = tgt_oid.to_hex();
            engine
                .run_git(
                    repo_root,
                    &["rev-list", "--max-count=64", &src_hex, "--not", &tgt_hex],
                    true,
                )
                .await?
        }
        None => {
            engine
                .run_git(repo_root, &["rev-list", "--max-count=64", &src_hex], true)
                .await?
        }
    };
    if rev_list_out.status.success() {
        for line in String::from_utf8_lossy(&rev_list_out.stdout).lines() {
            if let Ok(oid) = GitOid::from_hex(line.trim()) {
                out.push(oid);
            }
        }
    }
    Ok(())
}

fn head_oid(head: &HeadTargetSnapshot) -> Option<GitOid> {
    match head {
        HeadTargetSnapshot::Symbolic { target_oid, .. } => *target_oid,
        HeadTargetSnapshot::Detached { oid } => Some(*oid),
    }
}

fn op_mutates_worktree(kind: &OpKind) -> bool {
    matches!(
        kind,
        OpKind::Checkout
            | OpKind::Reset {
                mode: ResetMode::Hard | ResetMode::Keep
            }
            | OpKind::Merge
            | OpKind::Rebase
            | OpKind::InteractiveRebase
            | OpKind::CherryPick
            | OpKind::Revert
            | OpKind::StashPush
            | OpKind::StashPop
            | OpKind::DiscardWorktreePatch
    )
}

fn diff_refs(
    before: &[RefMutationRecord],
    after: &BTreeMap<CompactString, GitOid>,
) -> Vec<RefMutationRecord> {
    let before_map: BTreeMap<&str, Option<GitOid>> = before
        .iter()
        .map(|r| (r.full_ref.as_str(), r.old_oid))
        .collect();
    let mut all_keys = BTreeSet::new();
    for r in before {
        all_keys.insert(r.full_ref.clone());
    }
    for k in after.keys() {
        all_keys.insert(k.clone());
    }

    let mut diffs = Vec::new();
    for k in all_keys {
        let old_oid = before_map.get(k.as_str()).copied().flatten();
        let new_oid = after.get(&k).copied();
        if old_oid != new_oid {
            diffs.push(RefMutationRecord {
                full_ref: k,
                old_oid,
                new_oid,
            });
        }
    }
    diffs
}

fn clear_sequencer_files(git_dir: &Path) {
    for name in [
        "MERGE_HEAD",
        "MERGE_MODE",
        "MERGE_MSG",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
        "BISECT_LOG",
    ] {
        let _ = std::fs::remove_file(git_dir.join(name));
    }
    for dir in ["rebase-merge", "rebase-apply", "sequencer"] {
        let _ = std::fs::remove_dir_all(git_dir.join(dir));
    }
}

#[async_trait]
impl UndoRedoEngine for BranvioUndoEngine {
    async fn capture_before_mutation(
        &self,
        repo_root: &HostPath,
        kind: OpKind,
        summary: CompactString,
    ) -> OpLogResult<u64> {
        let repo_hash = compute_repo_root_hash(repo_root);
        let (before, _) = self.capture_snapshot(repo_root, Some(&kind)).await?;
        let worktree_snapshot = before.worktree_safety_stash_oid;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let entry = OpLogEntry {
            op_id: 0,
            repo_root_hash: repo_hash,
            timestamp_unix_ms: now_ms,
            kind,
            summary,
            before,
            after: None,
            reflog_selector: None,
            remote_exposure: RemoteExposureState::LocalOnly,
            undone: false,
        };
        let op_id = self.store.record_begin(entry)?;
        if let Some(snapshot) = worktree_snapshot {
            let ref_name = pin_ref_name(op_id, now_ms, snapshot);
            if let Err(err) = self
                .pin_worktree_snapshot(repo_root, &ref_name, snapshot)
                .await
            {
                // Without the pin the snapshot may be pruned; refuse to start an op we can't undo.
                let _ = self.store.discard_incomplete(repo_hash, op_id);
                return Err(err);
            }
        }
        // `record_begin` may have dropped the redo tail / entries beyond the retention cap:
        // release their pins. Not fatal for this operation (its own entry and pin are in
        // place); a failed cleanup is retried on every subsequent capture/discard.
        let _ = self.prune_snapshot_pins(repo_root).await;
        Ok(op_id)
    }

    async fn discard_incomplete(&self, repo_root: &HostPath, op_id: u64) -> OpLogResult<()> {
        let repo_hash = compute_repo_root_hash(repo_root);
        self.store.discard_incomplete(repo_hash, op_id)?;
        self.prune_snapshot_pins(repo_root).await?;
        Ok(())
    }

    async fn finalize_after_mutation(&self, repo_root: &HostPath, op_id: u64) -> OpLogResult<()> {
        let repo_hash = compute_repo_root_hash(repo_root);
        let recent = self.store.list_recent(repo_hash, 128)?;
        let before_entry = recent
            .into_iter()
            .find(|e| e.op_id == op_id)
            .ok_or_else(|| OpLogError::Storage(format!("oplog entry {op_id} not found")))?;

        let (mut after, after_map) = self.capture_snapshot(repo_root, None).await?;
        after.refs_snapshot = diff_refs(&before_entry.before.refs_snapshot, &after_map);

        let reflog_out = self
            .run_git(repo_root, &["reflog", "-1", "--format=%gd %gs"], true)
            .await?;
        let reflog_selector = if reflog_out.status.success() {
            let line = String::from_utf8_lossy(&reflog_out.stdout)
                .trim()
                .to_string();
            if line.is_empty() {
                None
            } else {
                Some(CompactString::from(line))
            }
        } else {
            None
        };

        self.store
            .record_commit(repo_hash, op_id, after, reflog_selector)
    }

    async fn inspect_next_undo(&self, repo_root: &HostPath) -> OpLogResult<PushSafetyCheck> {
        let repo_hash = compute_repo_root_hash(repo_root);
        let entry = self
            .store
            .list_recent(repo_hash, 256)?
            .into_iter()
            .find(|e| e.after.is_some() && !e.undone)
            .ok_or(OpLogError::StackEmpty { action: "undo" })?;
        let after = entry
            .after
            .as_ref()
            .ok_or_else(|| OpLogError::Storage("missing after snapshot".to_string()))?;
        let exposure = self
            .compute_remote_exposure(repo_root, after, &entry.before)
            .await?;
        Ok(PushSafetyCheck {
            op_id: entry.op_id,
            summary: entry.summary,
            remote_exposure: exposure,
            preserves_worktree: !op_mutates_worktree(&entry.kind)
                || entry.before.worktree_safety_stash_oid.is_some(),
        })
    }

    async fn undo(
        &self,
        repo_root: &HostPath,
        allow_rewrite_pushed: bool,
    ) -> OpLogResult<UndoOutcome> {
        let repo_hash = compute_repo_root_hash(repo_root);
        let entry = self
            .store
            .list_recent(repo_hash, 256)?
            .into_iter()
            .find(|e| e.after.is_some() && !e.undone)
            .ok_or(OpLogError::StackEmpty { action: "undo" })?;
        let after = entry
            .after
            .as_ref()
            .ok_or_else(|| OpLogError::Storage("missing after snapshot".to_string()))?;

        self.verify_no_external_divergence(repo_root, entry.op_id, after, true)
            .await?;

        let exposure = self
            .compute_remote_exposure(repo_root, after, &entry.before)
            .await?;
        if !allow_rewrite_pushed
            && matches!(exposure, RemoteExposureState::TouchesPushedHistory { .. })
        {
            return Err(OpLogError::RemoteExposureConfirmationRequired { exposure });
        }

        let warnings = self
            .apply_restore(
                repo_root,
                &entry.kind,
                &after.refs_snapshot,
                &entry.before,
                true,
            )
            .await?;
        self.store.set_undone_state(repo_hash, entry.op_id, true)?;
        if !warnings.is_empty() {
            return Err(OpLogError::RestoreIncomplete {
                op_id: entry.op_id,
                details: warnings,
            });
        }

        Ok(UndoOutcome {
            op_id: entry.op_id,
            restored_head_oid: head_oid(&entry.before.head),
            status_message: CompactString::from(format!("Undid: {}", entry.summary)),
        })
    }

    async fn redo(
        &self,
        repo_root: &HostPath,
        allow_rewrite_pushed: bool,
    ) -> OpLogResult<UndoOutcome> {
        let repo_hash = compute_repo_root_hash(repo_root);
        // Redo entries are newest-first, wait: undo popped newest (!undone) first,
        // so the most recently undone entry in linear chronological undo-chain is the OLDEST `undone == true` op_id!
        let entry = self
            .store
            .list_recent(repo_hash, 256)?
            .into_iter()
            .rev()
            .find(|e| e.after.is_some() && e.undone)
            .ok_or(OpLogError::StackEmpty { action: "redo" })?;
        let after = entry
            .after
            .as_ref()
            .ok_or_else(|| OpLogError::Storage("missing after snapshot".to_string()))?;

        let expected_before = RepoStateSnapshot {
            refs_snapshot: after.refs_snapshot.clone(),
            ..entry.before.clone()
        };
        self.verify_no_external_divergence(repo_root, entry.op_id, &expected_before, false)
            .await?;

        let exposure = self
            .compute_remote_exposure(repo_root, &entry.before, after)
            .await?;
        if !allow_rewrite_pushed
            && matches!(exposure, RemoteExposureState::TouchesPushedHistory { .. })
        {
            return Err(OpLogError::RemoteExposureConfirmationRequired { exposure });
        }

        let warnings = self
            .apply_restore(repo_root, &entry.kind, &after.refs_snapshot, after, false)
            .await?;
        self.store.set_undone_state(repo_hash, entry.op_id, false)?;
        if !warnings.is_empty() {
            return Err(OpLogError::RestoreIncomplete {
                op_id: entry.op_id,
                details: warnings,
            });
        }

        Ok(UndoOutcome {
            op_id: entry.op_id,
            restored_head_oid: head_oid(after.head()),
            status_message: CompactString::from(format!("Redid: {}", entry.summary)),
        })
    }
}

trait HeadAccess {
    fn head(&self) -> &HeadTargetSnapshot;
}
impl HeadAccess for RepoStateSnapshot {
    fn head(&self) -> &HeadTargetSnapshot {
        &self.head
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::store::RedbOpLogStore;

    fn sh(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn pinning_never_overwrites_an_existing_ref() {
        let repo = tempfile::TempDir::new().unwrap();
        let db = tempfile::TempDir::new().unwrap();
        sh(repo.path(), &["init", "-b", "main"]);
        sh(repo.path(), &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        sh(repo.path(), &["add", "a.txt"]);
        sh(repo.path(), &["commit", "-m", "one"]);
        let first = GitOid::from_hex(&sh(repo.path(), &["rev-parse", "HEAD"])).unwrap();
        std::fs::write(repo.path().join("a.txt"), "b\n").unwrap();
        sh(repo.path(), &["commit", "-am", "two"]);
        let second = GitOid::from_hex(&sh(repo.path(), &["rev-parse", "HEAD"])).unwrap();

        let root = HostPath::canonicalize_existing(repo.path()).unwrap();
        let env = Arc::new(HydratedGitEnv::from_current_process());
        let store = Arc::new(RedbOpLogStore::open_at(db.path().join("oplog.redb")).unwrap());
        let git = Arc::new(git_engine::BranvioGitEngine::new(Arc::clone(&env), None));
        let engine = BranvioUndoEngine::new(store, git, env);

        let name = legacy_pin_ref_name(1);
        engine
            .pin_worktree_snapshot(&root, &name, first)
            .await
            .unwrap();
        assert!(
            engine
                .pin_worktree_snapshot(&root, &name, second)
                .await
                .is_err(),
            "existing pin must not be overwritten"
        );
        assert_eq!(
            sh(repo.path(), &["rev-parse", "--verify", &name]),
            first.to_hex()
        );
        // Not referenced by any journal entry: garbage-collected.
        assert_eq!(engine.prune_snapshot_pins(&root).await.unwrap(), 1);
        assert!(sh(repo.path(), &["for-each-ref", "refs/branvio"]).is_empty());
    }
}
