//! Request, response, streaming, and predictive-merge DTOs for `git-engine`.

use branvio_core::domain::{
    CommitNode, ConflictSubkind, RefBadge, RefBadgeMap, RepoOperationState, ResetMode,
};
use branvio_core::ids::GitOid;
use branvio_core::paths::{FsCaseSensitivity, HostPath, RepoRelPath};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Static & structural summary returned upon opening a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSummary {
    /// Canonical working-tree root (or `.git` root for bare repositories).
    pub workdir_root: HostPath,
    /// Path to the `.git` directory (resolves worktree gitdir automatically).
    pub git_dir: HostPath,
    /// Common shared `.git` directory across linked worktrees.
    pub common_git_dir: HostPath,
    /// Whether the repository is bare.
    pub is_bare: bool,
    /// Detected filesystem case-sensitivity semantics.
    pub case_sensitivity: FsCaseSensitivity,
    /// Active sequencer state (`Clean`, `Merging`, `Rebasing`, etc.).
    pub operation_state: RepoOperationState,
    /// Current `HEAD` display label.
    pub head_label: CompactString,
    /// Current `HEAD` OID (`None` if unborn branch).
    pub head_oid: Option<GitOid>,
}

/// Parameters used to initialize a stateful topological [`crate::RevwalkSession`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevwalkSessionParams {
    /// Starting tips; if empty, walks all local/remote branch heads + `HEAD`.
    pub start_tips: Vec<GitOid>,
    /// Restrict traversal to first-parent backbone only.
    pub first_parent_only: bool,
    /// Optional pathspecs restricting commits to those modifying matching files.
    pub path_filters: Vec<RepoRelPath>,
}

/// One streamed chunk of commits emitted by [`crate::RevwalkSession::next_chunk`].
///
/// ## Strict Topological Ordering Invariant
/// Commits are yielded strictly parent-after-child (every child is emitted before any of its
/// parents, even when author/committer timestamps contain clock skew). Timestamps act solely
/// as priority tie-breakers among topologically ready nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevwalkChunk {
    /// Commits in strict topological order ready to be fed to `graph-layout`.
    pub commits: Vec<CommitNode>,
    /// True once the stateful `gix` walker has exhausted all reachable commits.
    pub exhausted: bool,
}

/// Request routed to Tier 3 (`git log -S` / `-G`) for high-speed native pickaxe code search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickaxeSearchRequest {
    /// Search term or regex pattern matching added/removed content lines.
    pub query: String,
    /// When `true`, uses `-G` (regex diff search); when `false`, uses `-S` (count-change pickaxe).
    pub is_regex: bool,
    /// Maximum matching commit OIDs to return (`--max-count`).
    pub limit: usize,
    /// Starting tips; empty defaults to `--all`.
    pub start_tips: Vec<GitOid>,
    /// Optional pathspecs scoping the search.
    pub path_filters: Vec<RepoRelPath>,
}

/// Single line entry returned by `gix` file blame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    /// 1-based line number in the blamed target revision.
    pub line_number: u32,
    /// 1-based line number in the original commit where the line was introduced.
    pub orig_line_number: u32,
    /// Originating commit OID.
    pub commit_oid: GitOid,
    /// Originating author name.
    pub author_name: CompactString,
    /// Originating author Unix timestamp in seconds.
    pub author_time_secs: i64,
    /// Blamed path in the originating commit (captures historical renames).
    pub orig_path: RepoRelPath,
}

/// Full blame result for a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameResult {
    /// Blamed path.
    pub path: RepoRelPath,
    /// Base commit OID (`None` if blamed against working tree).
    pub base_oid: Option<GitOid>,
    /// Line-by-line attribution records.
    pub lines: Vec<BlameLine>,
}

/// Raw blob contents read from unmerged index stages (`1=Base`, `2=Ours`, `3=Theirs`) in Tier 1 (`gix`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictStageBlobs {
    /// Common ancestor (`stage 1`) bytes (`None` if absent, e.g., both-added conflicts).
    pub base: Option<Vec<u8>>,
    /// Current `HEAD` (`stage 2`) bytes (`None` if deleted by us).
    pub ours: Option<Vec<u8>>,
    /// Incoming branch (`stage 3`) bytes (`None` if deleted by them).
    pub theirs: Option<Vec<u8>>,
}

/// Single predicted conflict entry computed in-memory via Tier 2 (`git2::Repository::merge_commits`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictiveConflictEntry {
    /// Conflicting path.
    pub path: RepoRelPath,
    /// Specific conflict stage nature.
    pub kind: ConflictSubkind,
}

/// Non-mutating prediction of whether merging/cherry-picking two commits will succeed cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePrediction {
    /// Source commit being merged/rebased/cherry-picked.
    pub source_oid: GitOid,
    /// Destination target (`HEAD` or dragged target branch).
    pub target_oid: GitOid,
    /// True if merge completes in-memory without index conflicts.
    pub will_succeed_cleanly: bool,
    /// Fast-forward possible without creating a merge commit.
    pub is_fast_forward: bool,
    /// Predicted conflicted files (empty when `will_succeed_cleanly` is `true`).
    pub conflicts: Vec<PredictiveConflictEntry>,
}

/// Options for Tier 3 `git commit` execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitOptions {
    /// Full commit message (subject + optional body/trailers).
    pub message: String,
    /// Amend the previous commit (`git commit --amend`).
    pub amend: bool,
    /// Explicitly request GPG/SSH commit signing (`-S`), `None` respects `.gitconfig` `commit.gpgsign`.
    pub gpg_sign: Option<bool>,
    /// Optional explicit signing key fingerprint (`-S<keyid>`).
    pub signing_key_id: Option<CompactString>,
    /// Bypass `pre-commit` and `commit-msg` hooks (`--no-verify`).
    pub no_verify: bool,
    /// Allow creating an empty commit (`--allow-empty`).
    pub allow_empty: bool,
}

/// Options for Tier 3 `git merge` execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOptions {
    /// Target ref or commit OID to merge into current `HEAD`.
    pub target_rev: CompactString,
    /// Force creation of a merge commit even if fast-forward is possible (`--no-ff`).
    pub no_ff: bool,
    /// Fail unless merge can fast-forward (`--ff-only`).
    pub ff_only: bool,
    /// Squash changes into index without creating a merge commit (`--squash`).
    pub squash: bool,
    /// Optional custom merge commit message.
    pub message: Option<String>,
}

/// Rebase instruction verb for Interactive Rebase modal & `GIT_SEQUENCE_EDITOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RebaseAction {
    /// Keep commit as-is (`pick`).
    Pick,
    /// Keep commit changes, prompt/apply edited commit message (`reword`).
    Reword,
    /// Pause rebase to amend commit contents/splits (`edit`).
    Edit,
    /// Meld commit into previous commit, combining commit messages (`squash`).
    Squash,
    /// Meld commit into previous commit, discarding this commit's message (`fixup`).
    Fixup,
    /// Discard commit completely (`drop`).
    Drop,
}

impl RebaseAction {
    /// Returns the exact Git rebase-todo keyword (`pick`, `reword`, `edit`, `squash`, `fixup`, `drop`).
    #[inline]
    pub const fn todo_keyword(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }
}

/// Single ordered row in an Interactive Rebase plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasePlanEntry {
    /// Original commit OID.
    pub oid: GitOid,
    /// Chosen interactive rebase action.
    pub action: RebaseAction,
    /// Replacement message used when `action` is `Reword` or `Squash`.
    pub updated_message: Option<String>,
}

/// Serialized JSON plan written to a secure temp file read by `branvio --branvio-git-sequence-editor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasePlanPayload {
    /// Onto commit/branch target (`git rebase -i <onto>`).
    pub onto_rev: CompactString,
    /// Ordered sequence of commits from oldest (top) to newest (bottom).
    pub entries: Vec<RebasePlanEntry>,
}

/// Descriptor for a linked or main Git worktree (`git worktree list --porcelain`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Working tree directory path on host filesystem.
    pub path: HostPath,
    /// Current `HEAD` commit OID (`None` if bare or unborn).
    pub head_oid: Option<GitOid>,
    /// Checked-out branch name (`None` if detached).
    pub branch: Option<CompactString>,
    /// True if this is the primary repository working tree.
    pub is_main: bool,
    /// True if the worktree is locked (`git worktree lock`).
    pub is_locked: bool,
    /// True if the directory is missing on disk and prunable.
    pub is_prunable: bool,
}

/// Real-time streaming event emitted by Tier 3 (`tokio::process` native `git`) to the UI progress drawer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CliStreamEvent {
    /// Command started with the resolved command line summary (secrets redacted).
    Started {
        /// Human-readable command summary (e.g. `git push origin main`).
        command_summary: String,
    },
    /// Raw stdout line.
    StdoutLine(String),
    /// Raw stderr line (Git writes progress and hook output to stderr).
    StderrLine(String),
    /// Parsed network or checkout transfer percentage (`0..=100`).
    TransferProgress {
        /// Phase name (e.g. `Counting objects`, `Receiving objects`, `Resolving deltas`).
        phase: CompactString,
        /// Percentage `0..=100`.
        percent: u8,
    },
    /// Indicates a native Git hook (`pre-commit`, `commit-msg`, `pre-push`) has started executing.
    HookStarted {
        /// Hook filename.
        hook_name: CompactString,
    },
    /// Terminal completion event.
    Finished {
        /// OS exit code (`None` if killed/cancelled).
        exit_code: Option<i32>,
        /// True when `exit_code == Some(0)`.
        success: bool,
    },
}

/// Handle returned when launching a streaming Tier 3 Git command.
pub struct CliCommandHandle {
    /// Live stream of stdout, stderr, progress, and completion events.
    pub events: mpsc::Receiver<CliStreamEvent>,
    /// Cooperative cancel trigger (sends SIGTERM/TerminateProcess when triggered).
    pub cancel: branvio_core::pipeline::CancelToken,
}

impl Drop for CliCommandHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Strategy flag for `git reset`.
pub type GitResetMode = ResetMode;

/// Force policy for [`crate::GitCliExecutor::push`].
///
/// A bare `--force-with-lease` compares against the local remote-tracking ref, which background
/// auto-fetch silently advances — turning the "lease" into an unconditional force that can
/// overwrite other people's commits. [`PushForce::WithLease`] therefore always pins an explicit
/// expected value captured when the user confirmed the force push (see
/// [`crate::GitCliExecutor::remote_tracking_oid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushForce {
    /// Regular push; the remote rejects non-fast-forward updates.
    None,
    /// Explicit-value lease: `--force-with-lease=refs/heads/<branch>:<expected>` plus
    /// `--force-if-includes`.
    ///
    /// The explicit `<expected>` value is what closes the auto-fetch hole. `--force-if-includes`
    /// only takes effect for tracking-ref based leases, so with an explicit value it is inert
    /// and does not depend on reflogs (verified with `core.logAllRefUpdates=false` and a deleted
    /// tracking reflog); it is kept only as defence in depth.
    WithLease {
        /// Remote branch OID the user saw when confirming. `None` means the remote branch did
        /// not exist at that time, so the push is rejected if it exists now.
        expected_remote_oid: Option<GitOid>,
    },
    /// Unconditional `--force` (overwrites whatever is on the remote).
    Force,
}

/// Reference snapshot returned by Tier 1 (`gix`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefsSnapshot {
    /// Current `HEAD` display label.
    pub head_label: CompactString,
    /// Current `HEAD` OID (`None` if unborn).
    pub head_oid: Option<GitOid>,
    /// Ordered list of all local, remote, tag, and stash badges (for sidebar tree).
    pub badges: Vec<RefBadge>,
    /// Sparse `O(1)` lookup map indexed by commit OID (for graph layout & rendering).
    pub by_oid: RefBadgeMap,
}

/// Guards Tier 3 CLI argument lists against option-injection when a user branch or revision
/// begins with `-` (e.g., `-weird-branch` or `--cached`), inserting `--end-of-options` before
/// revision operands as supported by modern Git (`>= 2.24`).
pub fn build_safe_revision_args<'a>(revs: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut args = vec!["--end-of-options".to_string()];
    for rev in revs {
        args.push(rev.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_dash_prefixed_branch_names_with_end_of_options() {
        let args = build_safe_revision_args(["-feature-x", "--orphan-lookalike"]);
        assert_eq!(
            args,
            vec!["--end-of-options", "-feature-x", "--orphan-lookalike",]
        );
    }
}
