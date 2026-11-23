//! Deterministic classifier separating `.git/` internal control files, ephemeral lockfiles,
//! and working-tree paths across Windows, macOS, and Linux.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Specific `.git/` internal change triggering targeted refresh pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitInternalChangeKind {
    /// `.git/HEAD` changed (branch switch, commit, detached HEAD).
    Head,
    /// `.git/index` changed (stage, unstage, checkout, merge).
    Index,
    /// `.git/refs/**` or `.git/packed-refs` changed (commit, branch, tag, fetch, pull).
    Refs,
    /// Sequencer state file (`MERGE_HEAD`, `REBASE_HEAD`, `rebase-merge/`, `CHERRY_PICK_HEAD`, etc.).
    OperationState,
    /// Repository configuration (`.git/config`, `.git/info/exclude`).
    Config,
    /// Loose/packed objects added (`.git/objects/**`); triggers background refresh only when paired with refs.
    Objects,
}

/// Classification outcome for a filesystem notification path inside a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WatcherPathClass {
    /// Ephemeral Git lockfiles (`.git/*.lock`) or OS noise (`.DS_Store`) — strictly ignored to
    /// eliminate premature refreshes while another `git` process holds a lock.
    IgnoreEphemeral,
    /// Change inside `.git/` requiring selective Git state reload.
    GitInternal(GitInternalChangeKind),
    /// Change inside the working tree (subject to `.gitignore` filtering).
    WorktreeFile,
}

/// Classifies any modified host path relative to `repo_root` and `git_dir` (which may live outside
/// `repo_root` for linked Git worktrees!).
pub fn classify_repo_event_path(
    event_path: &Path,
    repo_root: &Path,
    git_dir: &Path,
) -> WatcherPathClass {
    if let Some(name) = event_path.file_name().and_then(|n| n.to_str()) {
        if name == ".DS_Store" || name == "Thumbs.db" {
            return WatcherPathClass::IgnoreEphemeral;
        }
        if name.ends_with(".lock") && event_path.starts_with(git_dir) {
            return WatcherPathClass::IgnoreEphemeral;
        }
    }

    if let Ok(rel_git) = event_path.strip_prefix(git_dir) {
        let normalized = normalize_slashes(rel_git);
        if normalized.is_empty() {
            return WatcherPathClass::IgnoreEphemeral;
        }
        if normalized == "HEAD" {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::Head);
        }
        if normalized == "index" {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::Index);
        }
        if normalized == "packed-refs"
            || normalized == "refs"
            || normalized.starts_with("refs/")
            || normalized == "logs/HEAD"
            || normalized.starts_with("logs/refs/")
        {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::Refs);
        }
        if is_sequencer_state_path(&normalized) {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::OperationState);
        }
        if normalized == "config" || normalized == "info/exclude" {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::Config);
        }
        if normalized == "objects" || normalized.starts_with("objects/") {
            return WatcherPathClass::GitInternal(GitInternalChangeKind::Objects);
        }
        return WatcherPathClass::IgnoreEphemeral;
    }

    if event_path.starts_with(repo_root) {
        return WatcherPathClass::WorktreeFile;
    }

    WatcherPathClass::IgnoreEphemeral
}

fn is_sequencer_state_path(rel_git: &str) -> bool {
    matches!(
        rel_git,
        "MERGE_HEAD"
            | "MERGE_MSG"
            | "REBASE_HEAD"
            | "CHERRY_PICK_HEAD"
            | "REVERT_HEAD"
            | "BISECT_LOG"
            | "ORIG_HEAD"
    ) || rel_git.starts_with("rebase-merge")
        || rel_git.starts_with("rebase-apply")
        || rel_git.starts_with("sequencer")
}

fn normalize_slashes(path: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.components() {
        if let std::path::Component::Normal(seg) = comp
            && let Some(s) = seg.to_str()
        {
            parts.push(s);
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classifies_git_internals_locks_and_worktree_correctly() {
        let root = PathBuf::from("/workspace/branvio");
        let git_dir = root.join(".git");

        assert_eq!(
            classify_repo_event_path(&git_dir.join("index.lock"), &root, &git_dir),
            WatcherPathClass::IgnoreEphemeral
        );
        assert_eq!(
            classify_repo_event_path(&git_dir.join("HEAD"), &root, &git_dir),
            WatcherPathClass::GitInternal(GitInternalChangeKind::Head)
        );
        assert_eq!(
            classify_repo_event_path(&git_dir.join("index"), &root, &git_dir),
            WatcherPathClass::GitInternal(GitInternalChangeKind::Index)
        );
        assert_eq!(
            classify_repo_event_path(&git_dir.join("refs/heads/main"), &root, &git_dir),
            WatcherPathClass::GitInternal(GitInternalChangeKind::Refs)
        );
        assert_eq!(
            classify_repo_event_path(
                &git_dir.join("rebase-merge/git-rebase-todo"),
                &root,
                &git_dir
            ),
            WatcherPathClass::GitInternal(GitInternalChangeKind::OperationState)
        );
        assert_eq!(
            classify_repo_event_path(&root.join("crates/app-ui/src/main.rs"), &root, &git_dir),
            WatcherPathClass::WorktreeFile
        );
        assert_eq!(
            classify_repo_event_path(&root.join(".DS_Store"), &root, &git_dir),
            WatcherPathClass::IgnoreEphemeral
        );
    }

    #[test]
    fn supports_linked_worktrees_with_external_gitdir() {
        let worktree_root = PathBuf::from("/workspace/branvio-feature");
        let linked_git_dir = PathBuf::from("/workspace/branvio/.git/worktrees/branvio-feature");

        assert_eq!(
            classify_repo_event_path(
                &linked_git_dir.join("HEAD"),
                &worktree_root,
                &linked_git_dir
            ),
            WatcherPathClass::GitInternal(GitInternalChangeKind::Head)
        );
        assert_eq!(
            classify_repo_event_path(
                &worktree_root.join("src/lib.rs"),
                &worktree_root,
                &linked_git_dir
            ),
            WatcherPathClass::WorktreeFile
        );
    }
}
