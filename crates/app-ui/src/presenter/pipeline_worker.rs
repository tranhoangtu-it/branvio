//! Background MVCC pipeline worker bridging async Core crates (`git-engine`, `graph-layout`,
//! `diff-merge`) with reactive UI state updates via bounded channels + [`UiWakeupSink`].

use crate::ports::CoreAppHost;
use crate::presenter::sidebar_tree::{SubmoduleSummary, load_submodules_from_repo};
use branvio_core::domain::{
    CommitDetails, CommitNode, DeltaArea, DeltaStatus, FileDelta, FileDiffPatch, FileMode,
    RefBadgeMap, RepoOperationState,
};
use branvio_core::ids::{GitOid, RepoEpoch, RepoId};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{
    CancelToken, DirtyScopeFlags, GraphSnapshotView, InternalOpController, MvccRepoStateStore,
    RepoViewSnapshot, UiWakeupSink, WorkingTreeStatusView,
};
use compact_str::CompactString;
use diff_merge::traits::MAX_TEXT_DIFF_BYTES;
use diff_merge::{
    ConflictedFileInputs, DiffMergeError, DiffOptions, LineSelectionSpec, PatchDirection,
    ThreeWayMergeDocument,
};
use git_engine::{
    CliCommandHandle, CliStreamEvent, CommitOptions, MergeOptions, MergePrediction,
    PickaxeSearchRequest, RepoSummary, RevwalkSession, RevwalkSessionParams, WorktreeInfo,
};
use graph_layout::{GraphFilterSpec, IncrementalLayoutSession, LayoutSessionConfig};
use oplog_undo::{OpLogStore, UndoOutcome, UndoRedoEngine, compute_repo_root_hash};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Number of commits pulled per viewport revwalk chunk.
pub const DEFAULT_REVWALK_CHUNK_SIZE: usize = 500;
/// Maximum retained terminal/CLI output lines per repository drawer.
pub const MAX_CLI_LOG_LINES: usize = 2000;

/// Single line logged in the bottom streaming CLI drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLogEntry {
    /// True when line originated from `stderr`, git progress, or error diagnostics.
    pub is_stderr: bool,
    /// Log text content.
    pub text: String,
}

/// Non-UTF-8 / binary / LFS / oversized diff state surfaced cleanly to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffPreviewNotice {
    /// Non-UTF-8 text file: disable line-level partial staging/editor and offer whole-file actions.
    UnsupportedEncoding {
        /// Repository-relative file path whose bytes failed strict UTF-8 validation.
        path: RepoRelPath,
        /// Optional conflict labels `(ours_label, theirs_label)` if this arose opening a 3-way conflict.
        conflict_labels: Option<(String, String)>,
    },
    /// Informational or diagnostic notice (Loading, Binary file, LFS pointer, I/O error).
    Message(String),
}

/// Structured failure when opening a 3-way merge conflict editor session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictLoadError {
    /// Stage blobs or worktree file contain non-UTF-8 bytes; offer raw byte whole-file side choice.
    UnsupportedEncoding {
        /// Conflicted file path.
        path: RepoRelPath,
        /// Display label for stage 2 (`Ours` / HEAD).
        ours_label: String,
        /// Display label for stage 3 (`Theirs` / Incoming).
        theirs_label: String,
    },
    /// Other failure loading stages or opening conflict document.
    Message(String),
}

/// Side to accept wholesale as raw bytes for non-UTF-8 conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawConflictSide {
    /// Stage 2 (`Ours` / current `HEAD`).
    Ours,
    /// Stage 3 (`Theirs` / incoming branch).
    Theirs,
}

/// Active 3-way merge conflict editing state loaded for one conflicted file.
#[derive(Debug, Clone)]
pub struct ConflictEditorSession {
    /// Live 4-buffer `ropey::Rope` document (`Base`, `Ours`, `Theirs`, `Output`).
    pub document: ThreeWayMergeDocument,
    /// Display label for stage 2 (`Ours` / current `HEAD`).
    pub ours_label: String,
    /// Display label for stage 3 (`Theirs` / incoming branch).
    pub theirs_label: String,
}

/// Combined revwalk + incremental layout cursor state retained across chunked `load_more_history` calls.
pub struct PaginationSessionState {
    /// Live stateful topological revwalk cursor.
    pub revwalk: Box<dyn RevwalkSession>,
    /// Incremental Metro-line layout session.
    pub layout: Box<dyn IncrementalLayoutSession>,
    /// Commit metadata cache keyed by OID for building ordered [`GraphSnapshotView`] slices.
    pub commits_by_oid: HashMap<GitOid, CommitNode>,
    /// Complete repository ref map passed on every `push_commits_chunk` call.
    pub ref_map: RefBadgeMap,
    /// Optional pickaxe OID filter set (`git log -S` / `-G` hits).
    pub pickaxe_oids: Option<HashSet<GitOid>>,
    /// Latest working-tree status needed when refreshing WIP row across pagination chunks.
    pub show_wip_node: bool,
}

/// Background pipeline completion events delivered asynchronously back to [`super::BranvioPresenter`].
pub enum PipelineEvent {
    /// Full or scoped repository refresh published a fresh MVCC snapshot.
    RefreshCompleted {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Monotonic MVCC epoch published by this refresh.
        epoch: RepoEpoch,
        /// Refresh summary metadata.
        summary: RepoSummary,
        /// Discovered linked worktrees.
        worktrees: Vec<WorktreeInfo>,
        /// Parsed `.gitmodules` submodules.
        submodules: Vec<SubmoduleSummary>,
        /// Shared pagination session for incremental `load_more_history` calls.
        pagination: Option<Arc<Mutex<PaginationSessionState>>>,
        /// True when all reachable commits have been walked.
        revwalk_exhausted: bool,
    },
    /// Additional history chunk was appended and published to MVCC store.
    HistoryChunkAppended {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Monotonic MVCC epoch published by this history chunk.
        epoch: RepoEpoch,
        /// True once `RevwalkSession` reached exhaustion.
        revwalk_exhausted: bool,
    },
    /// Refresh failed (and was not superseded/cancelled).
    RefreshFailed {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Diagnostic error string.
        error: String,
    },
    /// Commit metadata and file deltas finished loading asynchronously for `oid`.
    CommitDetailsLoaded {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Commit OID loaded.
        oid: GitOid,
        /// Full commit details payload.
        details: CommitDetails,
    },
    /// Diff computation and `tree-sitter` syntax highlighting completed for `delta`.
    DeltaDiffLoaded {
        /// Repository tab ID.
        repo_id: RepoId,
        /// File delta requested.
        delta: FileDelta,
        /// Highlighted diff patch result or structured user-facing notice (binary, encoding, too large).
        result: Result<FileDiffPatch, DiffPreviewNotice>,
    },
    /// 3-way conflict session opened with stage 1/2/3 index blobs and worktree markers.
    ConflictSessionLoaded {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Opened conflict session (or structured error/encoding notice).
        result: Result<ConflictEditorSession, ConflictLoadError>,
    },
    /// Tier 2 predictive merge check finished for an active drag-and-drop hover.
    DragDropPredictionReady {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Source commit OID dragged.
        source_oid: GitOid,
        /// Target commit OID evaluated.
        target_oid: GitOid,
        /// Non-mutating prediction outcome.
        prediction: MergePrediction,
    },
    /// Registers the active CLI cancel trigger with [`super::BranvioPresenter`] once spawned.
    CliStartedCancel {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Cancellation token linked to the child subprocess.
        cancel: CancelToken,
    },
    /// Incremental streaming CLI output chunk or completion event.
    CliEvent {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Stream event from Tier 3 subprocess.
        event: CliStreamEvent,
    },
    /// Tier 2 index mutation finished (`stage_files`, `unstage_files`, synthetic patch, etc.).
    IndexMutationFinished {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Error message if mutation failed (e.g., `IndexLocked`).
        error: Option<String>,
        /// Optional feedback status toast upon success.
        success_banner: Option<String>,
    },
    /// Conflict file was written cleanly back to disk (and optionally staged).
    ConflictSaved {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Resolved file path.
        path: RepoRelPath,
        /// Error message if save or stage failed.
        error: Option<String>,
    },
    /// Time-Machine Undo or Redo transaction completed.
    UndoRedoFinished {
        /// Repository tab ID.
        repo_id: RepoId,
        /// `true` for Redo, `false` for Undo.
        is_redo: bool,
        /// Outcome or structured confirmation requirement / error.
        result: Result<UndoOutcome, UndoRedoUiError>,
    },
    /// Debounced filesystem watcher signal arrived from `fs-watcher`.
    WatcherTriggered {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Coalesced dirty repository layers.
        scopes: DirtyScopeFlags,
        /// True if coalesced after a bulk worktree storm.
        storm_coalesced: bool,
    },
    /// Embedded shell PTY session initialized and ready for interactive input.
    TerminalSessionReady {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Live PTY handle.
        session: Arc<dyn terminal_pty::TerminalSession>,
        /// Shell title label.
        title: CompactString,
    },
    /// Terminal PTY grid damaged (new shell output arrived).
    TerminalDamaged {
        /// Repository tab ID.
        repo_id: RepoId,
    },
    /// Remote Pull/Merge Requests finished loading via `forge-client`.
    PullRequestsLoaded {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Display label of target forge remote.
        remote_label: String,
        /// Fetched PR/MR summaries or error string.
        result: Result<Vec<forge_client::PullRequestSummary>, String>,
    },
    /// Creating a new Pull/Merge Request completed.
    PullRequestCreated {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Created PR/MR summary or error string.
        result: Result<forge_client::PullRequestSummary, String>,
    },
    /// AI Copilot finished generating a Conventional Commit message draft.
    AiCommitSuggested {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Generated commit subject/body or a typed UI error.
        result: Result<ai_copilot::CommitMessageSuggestion, AiUiError>,
    },
    /// AI Copilot finished generating 3-way conflict resolution advice.
    AiConflictSuggested {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Target conflict region index.
        region_id: u32,
        /// Resolution recommendation or a typed UI error.
        result: Result<ai_copilot::ConflictResolutionAdvice, AiUiError>,
    },
    /// The remote-tracking OID for a pending force push was captured (the lease value).
    ForcePushLeaseCaptured {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Local branch the user asked to force push.
        branch: CompactString,
        /// `origin/<branch>` OID at capture time (`Ok(None)` if the tracking ref is absent).
        result: Result<Option<GitOid>, String>,
    },
    /// Saving the Time-Machine undo point before a mutation failed, so the mutation was **not**
    /// run. The presenter asks the user whether to continue without undo.
    UndoCaptureFailed {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Short description of the held-back operation.
        summary: String,
        /// Why the undo point could not be saved.
        reason: String,
        /// The held-back mutation, runnable without undo protection after confirmation.
        pending: PendingUnprotectedMutation,
    },
}

/// Boxed launcher for a Tier 3 CLI mutation (see `BranvioPresenter::spawn_cli_command`).
pub type BoxedCliLaunch = Box<
    dyn FnOnce() -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = git_engine::GitEngineResult<CliCommandHandle>>
                    + Send,
            >,
        > + Send,
>;

/// A mutation held back because its undo point could not be saved.
pub enum PendingUnprotectedMutation {
    /// Tier 3 CLI command.
    Cli {
        /// Launcher for the command.
        launch: BoxedCliLaunch,
    },
    /// Tier 2 worktree-discarding patch.
    Patch {
        /// Repository root.
        repo_root: HostPath,
        /// File patch the selection refers to.
        patch: Box<FileDiffPatch>,
        /// Selected hunks/lines.
        selections: Vec<LineSelectionSpec>,
        /// Patch direction.
        direction: PatchDirection,
    },
}

impl std::fmt::Debug for PendingUnprotectedMutation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli { .. } => f.write_str("PendingUnprotectedMutation::Cli"),
            Self::Patch { direction, .. } => f
                .debug_struct("PendingUnprotectedMutation::Patch")
                .field("direction", direction)
                .finish_non_exhaustive(),
        }
    }
}

/// Which AI Copilot feature produced an [`AiUiError`] (used to phrase the user-facing message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiFeature {
    /// Conventional Commit message drafting from the staged diff.
    SuggestCommitMessage,
    /// 3-way conflict resolution advice for one conflict region.
    SuggestConflictResolution,
}

impl AiFeature {
    /// Short action label used at the start of user-facing messages.
    pub const fn action_label(self) -> &'static str {
        match self {
            Self::SuggestCommitMessage => "Suggest message",
            Self::SuggestConflictResolution => "Suggest resolution",
        }
    }

    /// What content the feature would upload (used in privacy messages).
    pub const fn payload_label(self) -> &'static str {
        match self {
            Self::SuggestCommitMessage => "your diff",
            Self::SuggestConflictResolution => "the conflicting code",
        }
    }
}

/// UI-friendly AI Copilot error that keeps privacy refusals distinguishable from other failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiUiError {
    /// The configured provider is off-device and the user has not allowed cloud uploads.
    CloudUploadNotAllowed {
        /// Provider display label (e.g. `Anthropic`).
        provider: String,
    },
    /// The file looks like it holds secrets and was never sent to the provider.
    SensitivePathExcluded {
        /// Repository-relative path that was withheld.
        path: String,
    },
    /// Any other failure, already formatted for display.
    Message(String),
}

impl From<ai_copilot::AiCopilotError> for AiUiError {
    fn from(err: ai_copilot::AiCopilotError) -> Self {
        match err {
            ai_copilot::AiCopilotError::CloudUploadNotAllowed { provider } => {
                Self::CloudUploadNotAllowed { provider }
            }
            ai_copilot::AiCopilotError::SensitivePathExcluded { path } => {
                Self::SensitivePathExcluded { path }
            }
            other => Self::Message(other.to_string()),
        }
    }
}

impl AiUiError {
    /// Clear, actionable message shown in the status banner.
    pub fn user_message(&self, feature: AiFeature) -> String {
        match self {
            Self::CloudUploadNotAllowed { provider } => format!(
                "{} would send {} to {provider}. Turn on 'Allow sending diffs to cloud AI' \
                 (Command Palette), allow it for this repository, or use a local Ollama model.",
                feature.action_label(),
                feature.payload_label(),
            ),
            Self::SensitivePathExcluded { path } => {
                format!("Skipped AI help for {path} because it may contain secrets.")
            }
            Self::Message(message) => message.clone(),
        }
    }
}

/// UI-friendly Time-Machine Undo/Redo error distinguishing confirmation-required warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoRedoUiError {
    /// Target commits were already pushed to remote-tracking branch(es); confirm rewrite before proceeding.
    RemoteConfirmationRequired {
        /// Remote branches containing commits being rewound.
        remote_branches: Vec<CompactString>,
    },
    /// Unrecoverable error message.
    Message(String),
}

/// Optional transaction context tracking pre/post Undo/Redo snapshots around mutating ops.
#[derive(Clone)]
pub struct UndoTxnContext {
    /// Time-Machine Undo/Redo engine.
    pub engine: Arc<dyn UndoRedoEngine>,
    /// Optional journal store for abort cleanup.
    pub store: Option<Arc<dyn OpLogStore>>,
    /// Repository root path.
    pub repo_root: HostPath,
    /// Captured `op_id`.
    pub op_id: u64,
}

/// Builds an initial epoch-0 [`RepoViewSnapshot`] for opening a new repository tab.
pub fn make_empty_repo_snapshot(repo_id: RepoId, repo_root: HostPath) -> RepoViewSnapshot {
    RepoViewSnapshot {
        repo_id,
        epoch: RepoEpoch(0),
        repo_root,
        head_label: CompactString::const_new("HEAD"),
        head_oid: None,
        operation_state: RepoOperationState::Clean,
        refs: Arc::from([]),
        working_tree: Arc::default(),
        graph: Arc::default(),
        can_undo: false,
        can_redo: false,
    }
}

/// Parameters controlling an asynchronous refresh task.
#[derive(Clone)]
pub struct RefreshRequest {
    /// Repository ID.
    pub repo_id: RepoId,
    /// Repository root path.
    pub repo_root: HostPath,
    /// MVCC store owned by this tab.
    pub mvcc_store: Arc<MvccRepoStateStore>,
    /// Cancellation token allowing newer refreshes to abort superseded runs.
    pub cancel: CancelToken,
    /// First-parent filter flag.
    pub first_parent_only: bool,
    /// Active graph filter parameters.
    pub filter_spec: GraphFilterSpec,
    /// Pickaxe query text (empty when pickaxe disabled).
    pub pickaxe_query: String,
    /// True to run pickaxe regex (`-G`), false for count change (`-S`).
    pub pickaxe_is_regex: bool,
    /// Initial chunk limit to load immediately.
    pub initial_chunk_size: usize,
}

/// Spawns an asynchronous full Tier 1 + `graph-layout` refresh and publishes an [`RepoViewSnapshot`].
pub fn spawn_repo_refresh(
    host: &CoreAppHost,
    req: RefreshRequest,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    let graph_layout = Arc::clone(&host.graph_layout);
    let oplog_store = host.oplog_store.clone();

    host.tokio_handle.spawn(async move {
        let cancel = req.cancel;
        let epoch = req.mvcc_store.next_epoch();
        let run = execute_refresh(
            git_engine.as_ref(),
            graph_layout.as_ref(),
            req.repo_id,
            epoch,
            &req.repo_root,
            req.first_parent_only,
            &req.filter_spec,
            &req.pickaxe_query,
            req.pickaxe_is_regex,
            req.initial_chunk_size,
            oplog_store,
            &cancel,
        )
        .await;

        if cancel.is_cancelled() {
            return;
        }

        match run {
            Ok(out) => {
                let published = req.mvcc_store.try_publish(Arc::new(out.snapshot));
                if published {
                    let _ = event_tx.send(PipelineEvent::RefreshCompleted {
                        repo_id: req.repo_id,
                        epoch,
                        summary: out.summary,
                        worktrees: out.worktrees,
                        submodules: out.submodules,
                        pagination: Some(Arc::new(Mutex::new(out.pagination))),
                        revwalk_exhausted: out.revwalk_exhausted,
                    });
                    wakeup.notify_repo_updated(req.repo_id, epoch);
                }
            }
            Err(err_msg) => {
                let _ = event_tx.send(PipelineEvent::RefreshFailed {
                    repo_id: req.repo_id,
                    error: err_msg,
                });
                wakeup.notify_repo_updated(req.repo_id, epoch);
            }
        }
    });
}

struct ExecutedRefresh {
    summary: RepoSummary,
    worktrees: Vec<WorktreeInfo>,
    submodules: Vec<SubmoduleSummary>,
    pagination: PaginationSessionState,
    revwalk_exhausted: bool,
    snapshot: RepoViewSnapshot,
}

#[allow(clippy::too_many_arguments)]
async fn execute_refresh(
    git_engine: &dyn git_engine::HybridGitEngine,
    graph_layout: &dyn graph_layout::GraphLayoutEngine,
    repo_id: RepoId,
    epoch: RepoEpoch,
    repo_root: &HostPath,
    first_parent_only: bool,
    filter_spec: &GraphFilterSpec,
    pickaxe_query: &str,
    pickaxe_is_regex: bool,
    initial_chunk_size: usize,
    oplog_store: Option<Arc<dyn OpLogStore>>,
    cancel: &CancelToken,
) -> Result<ExecutedRefresh, String> {
    let summary = git_engine
        .open_repo(repo_root.as_path())
        .await
        .map_err(|e| e.to_string())?;
    let refs = git_engine
        .head_and_refs(repo_root, cancel.clone())
        .await
        .map_err(|e| e.to_string())?;
    let status = git_engine
        .working_tree_status(repo_root, cancel.clone())
        .await
        .map_err(|e| e.to_string())?;
    let worktrees = git_engine
        .worktree_list(repo_root)
        .await
        .unwrap_or_default();
    let submodules = load_submodules_from_repo(repo_root.as_path());

    let pickaxe_oids = if !pickaxe_query.trim().is_empty() {
        let req = PickaxeSearchRequest {
            query: pickaxe_query.trim().to_string(),
            is_regex: pickaxe_is_regex,
            limit: 2000,
            start_tips: Vec::new(),
            path_filters: filter_spec.file_paths.clone(),
        };
        let oids = git_engine
            .search_commits_pickaxe(repo_root, req, cancel.clone())
            .await
            .map_err(|e| e.to_string())?;
        Some(oids.into_iter().collect::<HashSet<GitOid>>())
    } else {
        None
    };

    let mut effective_filter = filter_spec.clone();
    effective_filter.first_parent_only = first_parent_only;
    if !pickaxe_query.trim().is_empty() {
        effective_filter.pickaxe_code_query = Some(pickaxe_query.trim().to_string());
    }

    let walk_params = RevwalkSessionParams {
        start_tips: Vec::new(),
        first_parent_only: effective_filter.first_parent_only,
        path_filters: filter_spec.file_paths.clone(),
    };
    let mut revwalk = git_engine
        .open_revwalk_session(repo_root, walk_params, cancel.clone())
        .await
        .map_err(|e| e.to_string())?;

    // Clear `pickaxe_code_query` from `MetroLayoutSession`'s own filter because we apply
    // exact Tier 3 pickaxe OID matching in `push_chunk_and_build_graph` directly.
    let mut layout_filter = effective_filter.clone();
    layout_filter.pickaxe_code_query = None;

    let layout = graph_layout.start_session(
        LayoutSessionConfig::default(),
        summary.head_oid,
        layout_filter,
    );

    let chunk = {
        let cancel_clone = cancel.clone();
        revwalk
            .next_chunk(initial_chunk_size.max(1), &cancel_clone)
            .map_err(|e| e.to_string())?
    };

    if cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }

    let mut pagination = PaginationSessionState {
        revwalk,
        layout,
        commits_by_oid: HashMap::with_capacity(chunk.commits.len()),
        ref_map: refs.by_oid,
        pickaxe_oids,
        show_wip_node: effective_filter.show_wip_node,
    };

    let graph_view = push_chunk_and_build_graph(
        &mut pagination,
        &chunk.commits,
        summary.head_oid,
        &status,
        chunk.exhausted,
    )?;

    let (can_undo, can_redo) = match oplog_store {
        Some(store) => {
            let repo_hash = compute_repo_root_hash(repo_root);
            let entries = store.list_recent(repo_hash, 128).unwrap_or_default();
            (
                entries.iter().any(|e| e.after.is_some() && !e.undone),
                entries.iter().any(|e| e.after.is_some() && e.undone),
            )
        }
        None => (false, false),
    };

    let snapshot = RepoViewSnapshot {
        repo_id,
        epoch,
        repo_root: repo_root.clone(),
        head_label: summary.head_label.clone(),
        head_oid: summary.head_oid,
        operation_state: summary.operation_state,
        refs: Arc::from(refs.badges),
        working_tree: Arc::new(status),
        graph: Arc::new(graph_view),
        can_undo,
        can_redo,
    };

    Ok(ExecutedRefresh {
        summary,
        worktrees,
        submodules,
        pagination,
        revwalk_exhausted: chunk.exhausted,
        snapshot,
    })
}

/// Spawns incremental pagination pulling `chunk_size` more commits into the active layout session.
pub fn spawn_load_more_history(
    host: &CoreAppHost,
    repo_id: RepoId,
    mvcc_store: Arc<MvccRepoStateStore>,
    pagination: Arc<Mutex<PaginationSessionState>>,
    chunk_size: usize,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    host.tokio_handle.spawn(async move {
        let cancel = CancelToken::new();
        let current = mvcc_store.load_snapshot();
        let epoch = mvcc_store.next_epoch();

        let (exhausted, new_graph) = {
            let mut state = pagination.lock().await;
            let chunk = match state.revwalk.next_chunk(chunk_size.max(1), &cancel) {
                Ok(c) => c,
                Err(err) => {
                    let _ = event_tx.send(PipelineEvent::RefreshFailed {
                        repo_id,
                        error: err.to_string(),
                    });
                    wakeup.notify_repo_updated(repo_id, epoch);
                    return;
                }
            };
            match push_chunk_and_build_graph(
                &mut state,
                &chunk.commits,
                current.head_oid,
                current.working_tree.as_ref(),
                chunk.exhausted,
            ) {
                Ok(view) => (chunk.exhausted, view),
                Err(err) => {
                    let _ = event_tx.send(PipelineEvent::RefreshFailed {
                        repo_id,
                        error: err,
                    });
                    wakeup.notify_repo_updated(repo_id, epoch);
                    return;
                }
            }
        };

        let updated = RepoViewSnapshot {
            repo_id: current.repo_id,
            epoch,
            repo_root: current.repo_root.clone(),
            head_label: current.head_label.clone(),
            head_oid: current.head_oid,
            operation_state: current.operation_state,
            refs: Arc::clone(&current.refs),
            working_tree: Arc::clone(&current.working_tree),
            graph: Arc::new(new_graph),
            can_undo: current.can_undo,
            can_redo: current.can_redo,
        };
        if mvcc_store.try_publish(Arc::new(updated)) {
            let _ = event_tx.send(PipelineEvent::HistoryChunkAppended {
                repo_id,
                epoch,
                revwalk_exhausted: exhausted,
            });
            wakeup.notify_repo_updated(repo_id, epoch);
        }
    });
}

fn push_chunk_and_build_graph(
    state: &mut PaginationSessionState,
    new_commits: &[CommitNode],
    head_oid: Option<GitOid>,
    status: &WorkingTreeStatusView,
    exhausted: bool,
) -> Result<GraphSnapshotView, String> {
    for c in new_commits {
        state.commits_by_oid.insert(c.oid, c.clone());
    }

    if let Some(ref pickaxe_set) = state.pickaxe_oids {
        let mut filtered = Vec::new();
        for c in new_commits {
            if pickaxe_set.contains(&c.oid) {
                let mut cloned = c.clone();
                cloned.parents.retain(|p| pickaxe_set.contains(p));
                filtered.push(cloned);
            }
        }
        state
            .layout
            .push_commits_chunk(&filtered, &state.ref_map)
            .map_err(|e| e.to_string())?;
    } else {
        state
            .layout
            .push_commits_chunk(new_commits, &state.ref_map)
            .map_err(|e| e.to_string())?;
    }

    if status.total_dirty_count() > 0 && state.show_wip_node {
        state
            .layout
            .upsert_virtual_wip(head_oid, status, &state.ref_map)
            .map_err(|e| e.to_string())?;
    } else {
        state.layout.remove_virtual_wip();
    }

    let row_count = state.layout.commit_row_count();
    let commit_rows = state.layout.commit_rows_slice(0..row_count).to_vec();
    let mut commits = Vec::with_capacity(commit_rows.len());
    for row in &commit_rows {
        if let Some(node) = state.commits_by_oid.get(&row.oid) {
            commits.push(node.clone());
        }
    }

    Ok(GraphSnapshotView {
        wip_row: state.layout.wip_row().cloned().map(Arc::new),
        commits: Arc::from(commits),
        commit_rows: Arc::from(commit_rows),
        refs_by_oid: Arc::new(state.ref_map.clone()),
        max_lane_width: state.layout.max_lane_width(),
        has_more_history: !exhausted,
    })
}

/// Spawns asynchronous loading of [`CommitDetails`] for a clicked commit node.
pub fn spawn_load_commit_details(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    oid: GitOid,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    host.tokio_handle.spawn(async move {
        if let Ok(details) = git_engine
            .load_commit_details(&repo_root, oid, CancelToken::new())
            .await
        {
            let _ = event_tx.send(PipelineEvent::CommitDetailsLoaded {
                repo_id,
                oid,
                details,
            });
            let epoch = mvcc_store.committed_epoch();
            wakeup.notify_repo_updated(repo_id, epoch);
        }
    });
}

/// Spawns asynchronous blob reading + `imara-diff` + `tree-sitter` syntax highlighting + enclosing symbol lookup.
#[allow(clippy::too_many_arguments)]
pub fn spawn_load_delta_diff(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    delta: FileDelta,
    options: DiffOptions,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    let diff_merge = Arc::clone(&host.diff_merge);
    let tokio_handle = host.tokio_handle.clone();

    host.tokio_handle.spawn(async move {
        let res = compute_delta_diff(
            git_engine.as_ref(),
            diff_merge,
            &tokio_handle,
            &repo_root,
            delta.clone(),
            options,
        )
        .await;
        let _ = event_tx.send(PipelineEvent::DeltaDiffLoaded {
            repo_id,
            delta,
            result: res,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

async fn compute_delta_diff(
    git_engine: &dyn git_engine::HybridGitEngine,
    diff_merge: Arc<dyn crate::ports::DiffMergePort>,
    tokio_handle: &tokio::runtime::Handle,
    repo_root: &HostPath,
    delta: FileDelta,
    options: DiffOptions,
) -> Result<FileDiffPatch, DiffPreviewNotice> {
    if let Some(ref lfs) = delta.lfs {
        return Err(DiffPreviewNotice::Message(format!(
            "Git LFS pointer ({} bytes, sha256:{})",
            lfs.byte_size,
            &lfs.oid_sha256[..12.min(lfs.oid_sha256.len())]
        )));
    }
    if delta.is_binary {
        return Err(DiffPreviewNotice::Message(format!(
            "Binary file '{}'",
            delta.new_path.as_str()
        )));
    }

    let old_bytes = match delta.old_oid {
        Some(oid) => git_engine
            .read_blob(repo_root, oid, Some(MAX_TEXT_DIFF_BYTES))
            .await
            .map_err(|e| DiffPreviewNotice::Message(e.to_string()))?,
        None => Vec::new(),
    };

    let new_bytes = match delta.area {
        DeltaArea::Unstaged | DeltaArea::Conflicted => {
            if delta.status == DeltaStatus::Deleted {
                Vec::new()
            } else {
                read_worktree_file_capped(repo_root.as_path(), &delta.new_path, delta.new_mode)
                    .await
                    .map_err(DiffPreviewNotice::Message)?
            }
        }
        DeltaArea::Staged | DeltaArea::CommitDiff => match delta.new_oid {
            Some(oid) => git_engine
                .read_blob(repo_root, oid, Some(MAX_TEXT_DIFF_BYTES))
                .await
                .map_err(|e| DiffPreviewNotice::Message(e.to_string()))?,
            None => Vec::new(),
        },
    };

    let rel_path = delta.new_path.clone();
    tokio_handle
        .spawn_blocking(move || {
            let mut patch = diff_merge
                .diff_blobs(delta, &old_bytes, &new_bytes, &options)
                .map_err(|err| match err {
                    DiffMergeError::UnsupportedEncoding { .. } => {
                        DiffPreviewNotice::UnsupportedEncoding {
                            path: rel_path.clone(),
                            conflict_labels: None,
                        }
                    }
                    other => DiffPreviewNotice::Message(other.to_string()),
                })?;
            let _ = diff_merge.highlight_patch(&mut patch, Some(&old_bytes), Some(&new_bytes));
            for hunk in &mut patch.hunks {
                if hunk.enclosing_symbol.is_none() && !new_bytes.is_empty() {
                    hunk.enclosing_symbol = diff_merge.extract_enclosing_symbol(
                        &rel_path,
                        &new_bytes,
                        hunk.new_start.max(1),
                    );
                }
            }
            Ok(patch)
        })
        .await
        .map_err(|e| DiffPreviewNotice::Message(e.to_string()))?
}

async fn read_worktree_file_capped(
    repo_root: &Path,
    rel: &RepoRelPath,
    expected_mode: FileMode,
) -> Result<Vec<u8>, String> {
    let full = repo_root.join(rel.as_str());
    let meta = tokio::fs::symlink_metadata(&full)
        .await
        .map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || expected_mode == FileMode::Symlink {
        let target = tokio::fs::read_link(&full)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(target.to_string_lossy().into_owned().into_bytes());
    }
    if meta.len() > MAX_TEXT_DIFF_BYTES as u64 {
        return Err(format!(
            "File '{}' exceeds 64 MiB preview limit ({} bytes)",
            rel.as_str(),
            meta.len()
        ));
    }
    tokio::fs::read(&full).await.map_err(|e| e.to_string())
}

/// Opens an interactive 3-way conflict session for `path` using Tier 1 index stage blobs.
#[allow(clippy::too_many_arguments)]
pub fn spawn_open_conflict_session(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    path: RepoRelPath,
    ours_label: String,
    theirs_label: String,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    let diff_merge = Arc::clone(&host.diff_merge);
    let tokio_handle = host.tokio_handle.clone();

    host.tokio_handle.spawn(async move {
        let stages_res = git_engine
            .read_conflict_stages(&repo_root, &path, MAX_TEXT_DIFF_BYTES)
            .await
            .map_err(|e| ConflictLoadError::Message(e.to_string()));
        let wt_bytes = tokio::fs::read(repo_root.as_path().join(path.as_str()))
            .await
            .ok();

        let result = match stages_res {
            Err(err) => Err(err),
            Ok(stages) => {
                let ours_label_clone = ours_label.clone();
                let theirs_label_clone = theirs_label.clone();
                let path_for_err = path.clone();
                tokio_handle
                    .spawn_blocking(move || {
                        let ours_slice = stages.ours.as_deref().unwrap_or(b"");
                        let theirs_slice = stages.theirs.as_deref().unwrap_or(b"");
                        let doc = diff_merge
                            .open_conflicted_file(ConflictedFileInputs {
                                path,
                                base_bytes: stages.base.as_deref(),
                                ours_bytes: ours_slice,
                                theirs_bytes: theirs_slice,
                                worktree_with_markers: wt_bytes.as_deref(),
                                ours_label: &ours_label_clone,
                                theirs_label: &theirs_label_clone,
                            })
                            .map_err(|err| match err {
                                DiffMergeError::UnsupportedEncoding { .. } => {
                                    ConflictLoadError::UnsupportedEncoding {
                                        path: path_for_err,
                                        ours_label: ours_label_clone.clone(),
                                        theirs_label: theirs_label_clone.clone(),
                                    }
                                }
                                other => ConflictLoadError::Message(other.to_string()),
                            })?;
                        Ok(ConflictEditorSession {
                            document: doc,
                            ours_label: ours_label_clone,
                            theirs_label: theirs_label_clone,
                        })
                    })
                    .await
                    .map_err(|e| ConflictLoadError::Message(e.to_string()))
                    .and_then(|r| r)
            }
        };

        let _ = event_tx.send(PipelineEvent::ConflictSessionLoaded { repo_id, result });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

fn finish_conflict_save(
    repo_id: RepoId,
    epoch: RepoEpoch,
    path: RepoRelPath,
    error: Option<String>,
    event_tx: &mpsc::UnboundedSender<PipelineEvent>,
    wakeup: &dyn UiWakeupSink,
) {
    let _ = event_tx.send(PipelineEvent::ConflictSaved {
        repo_id,
        path,
        error,
    });
    wakeup.notify_repo_updated(repo_id, epoch);
}

/// Saves a resolved [`ThreeWayMergeDocument`] to the worktree and optionally stages it via Tier 2.
#[allow(clippy::too_many_arguments)]
pub fn spawn_save_conflict_resolution(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    doc: ThreeWayMergeDocument,
    stage_after_save: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    let diff_merge = Arc::clone(&host.diff_merge);
    let path = doc.path.clone();

    host.tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let epoch = mvcc_store.committed_epoch();
        let exported = match diff_merge.export_merged_bytes(&doc, true) {
            Ok(bytes) => bytes,
            Err(err) => {
                finish_conflict_save(
                    repo_id,
                    epoch,
                    path,
                    Some(err.to_string()),
                    &event_tx,
                    wakeup.as_ref(),
                );
                return;
            }
        };

        let dest = repo_root.as_path().join(path.as_str());
        if let Err(err) = tokio::fs::write(&dest, &exported).await {
            finish_conflict_save(
                repo_id,
                epoch,
                path,
                Some(err.to_string()),
                &event_tx,
                wakeup.as_ref(),
            );
            return;
        }

        if stage_after_save
            && let Err(err) = git_engine
                .stage_files(&repo_root, std::slice::from_ref(&path))
                .await
        {
            finish_conflict_save(
                repo_id,
                epoch,
                path,
                Some(err.to_string()),
                &event_tx,
                wakeup.as_ref(),
            );
            return;
        }

        finish_conflict_save(repo_id, epoch, path, None, &event_tx, wakeup.as_ref());
    });
}

/// Resolves a non-UTF-8 or binary conflict by copying stage 2 (`Ours`) or stage 3 (`Theirs`) raw
/// bytes directly to the worktree without lossy UTF-8 decoding, then optionally stages the path.
#[allow(clippy::too_many_arguments)]
pub fn spawn_resolve_raw_conflict_side(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    path: RepoRelPath,
    side: RawConflictSide,
    stage_after_save: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);

    host.tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let epoch = mvcc_store.committed_epoch();

        let stages = match git_engine
            .read_conflict_stages(&repo_root, &path, MAX_TEXT_DIFF_BYTES)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                finish_conflict_save(
                    repo_id,
                    epoch,
                    path,
                    Some(err.to_string()),
                    &event_tx,
                    wakeup.as_ref(),
                );
                return;
            }
        };

        let chosen = match side {
            RawConflictSide::Ours => stages.ours,
            RawConflictSide::Theirs => stages.theirs,
        };
        let dest = repo_root.as_path().join(path.as_str());
        let io_res = match chosen {
            Some(raw_bytes) => tokio::fs::write(&dest, &raw_bytes).await,
            None => match tokio::fs::remove_file(&dest).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(err) = io_res {
            finish_conflict_save(
                repo_id,
                epoch,
                path,
                Some(err.to_string()),
                &event_tx,
                wakeup.as_ref(),
            );
            return;
        }

        if stage_after_save
            && let Err(err) = git_engine
                .stage_files(&repo_root, std::slice::from_ref(&path))
                .await
        {
            finish_conflict_save(
                repo_id,
                epoch,
                path,
                Some(err.to_string()),
                &event_tx,
                wakeup.as_ref(),
            );
            return;
        }

        finish_conflict_save(repo_id, epoch, path, None, &event_tx, wakeup.as_ref());
    });
}

/// Runs Tier 2 `predict_merge_conflicts` asynchronously when hovering over a drop target.
#[allow(clippy::too_many_arguments)]
pub fn spawn_drag_drop_prediction(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    source_oid: GitOid,
    target_oid: GitOid,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    host.tokio_handle.spawn(async move {
        if let Ok(prediction) = git_engine
            .predict_merge_conflicts(&repo_root, target_oid, source_oid)
            .await
        {
            let _ = event_tx.send(PipelineEvent::DragDropPredictionReady {
                repo_id,
                source_oid,
                target_oid,
                prediction,
            });
            let epoch = mvcc_store.committed_epoch();
            wakeup.notify_repo_updated(repo_id, epoch);
        }
    });
}

/// Spawns a Tier 2 surgical line-level or hunk-level index/worktree patch mutation.
#[allow(clippy::too_many_arguments)]
pub fn spawn_patch_mutation(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    patch: FileDiffPatch,
    selections: Vec<LineSelectionSpec>,
    direction: PatchDirection,
    allow_without_undo: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    let diff_merge = Arc::clone(&host.diff_merge);
    let undo_engine = host.undo_engine.clone();

    host.tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let epoch = mvcc_store.committed_epoch();
        let target_path = patch.delta.new_path.clone();

        let patch_bytes = if selections.len() == 1 && selections[0].selected_line_indices.is_empty()
        {
            diff_merge.build_hunk_patch(&patch, selections[0].hunk_index, direction)
        } else {
            diff_merge.build_line_selection_patch(&patch, &selections, direction)
        };

        let bytes = match patch_bytes {
            Ok(b) => b,
            Err(err) => {
                let _ = event_tx.send(PipelineEvent::IndexMutationFinished {
                    repo_id,
                    error: Some(err.to_string()),
                    success_banner: None,
                });
                wakeup.notify_repo_updated(repo_id, epoch);
                return;
            }
        };

        let undo_op_id = if let Some(ref ue) = undo_engine {
            let kind = match direction {
                PatchDirection::StageToIndex => branvio_core::domain::OpKind::StagePatch,
                PatchDirection::UnstageFromIndex => branvio_core::domain::OpKind::UnstagePatch,
                PatchDirection::DiscardInWorktree => {
                    branvio_core::domain::OpKind::DiscardWorktreePatch
                }
            };
            let summary = format!("Patch {}", target_path.as_str());
            match ue
                .capture_before_mutation(&repo_root, kind, CompactString::from(summary.as_str()))
                .await
            {
                Ok(op_id) => Some(op_id),
                // Discarding worktree changes is destructive: never proceed silently without an
                // undo point. Staging/unstaging is trivially reversible and proceeds unprotected.
                Err(err)
                    if direction == PatchDirection::DiscardInWorktree && !allow_without_undo =>
                {
                    let _ = event_tx.send(PipelineEvent::UndoCaptureFailed {
                        repo_id,
                        summary: format!("Discard selected changes in {}", target_path.as_str()),
                        reason: err.to_string(),
                        pending: PendingUnprotectedMutation::Patch {
                            repo_root,
                            patch: Box::new(patch),
                            selections,
                            direction,
                        },
                    });
                    wakeup.notify_repo_updated(repo_id, epoch);
                    return;
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let op_res = match direction {
            PatchDirection::StageToIndex | PatchDirection::UnstageFromIndex => {
                git_engine
                    .apply_synthetic_patch_to_index(&repo_root, &bytes)
                    .await
            }
            PatchDirection::DiscardInWorktree => {
                git_engine
                    .apply_reverse_patch_to_worktree(&repo_root, &target_path, &bytes)
                    .await
            }
        };

        if let (Some(ue), Some(op_id)) = (undo_engine, undo_op_id) {
            if op_res.is_ok() {
                let _ = ue.finalize_after_mutation(&repo_root, op_id).await;
            } else {
                // Also deletes the entry's snapshot pin ref.
                let _ = ue.discard_incomplete(&repo_root, op_id).await;
            }
        }

        let (error, success_banner) = match op_res {
            Ok(()) => (
                None,
                Some(format!(
                    "Applied surgical patch to {}",
                    target_path.as_str()
                )),
            ),
            Err(err) => (Some(err.to_string()), None),
        };
        let _ = event_tx.send(PipelineEvent::IndexMutationFinished {
            repo_id,
            error,
            success_banner,
        });
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Spawns Tier 2 whole-file `stage_files` or `unstage_files`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_whole_file_index_op(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    paths: Vec<RepoRelPath>,
    stage: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let git_engine = Arc::clone(&host.git_engine);
    host.tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let res = if stage {
            git_engine.stage_files(&repo_root, &paths).await
        } else {
            git_engine.unstage_files(&repo_root, &paths).await
        };
        let _ = event_tx.send(PipelineEvent::IndexMutationFinished {
            repo_id,
            error: res.err().map(|e| e.to_string()),
            success_banner: None,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Pumps events from a [`CliCommandHandle`] into `event_tx` while holding `InternalOpLease`
/// and finalizing optional Time-Machine Undo/Redo snapshots before emitting terminal `Finished`.
#[allow(clippy::too_many_arguments)]
pub fn pump_cli_handle(
    tokio_handle: &tokio::runtime::Handle,
    repo_id: RepoId,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    mut handle: CliCommandHandle,
    undo_ctx: Option<UndoTxnContext>,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) -> CancelToken {
    let cancel = handle.cancel.clone();
    tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let mut undo_opt = undo_ctx;
        while let Some(ev) = handle.events.recv().await {
            let is_terminal = matches!(ev, CliStreamEvent::Finished { .. });
            if let CliStreamEvent::Finished { success, .. } = ev
                && let Some(ctx) = undo_opt.take()
            {
                if success {
                    let _ = ctx
                        .engine
                        .finalize_after_mutation(&ctx.repo_root, ctx.op_id)
                        .await;
                } else {
                    // Also deletes the entry's snapshot pin ref.
                    let _ = ctx
                        .engine
                        .discard_incomplete(&ctx.repo_root, ctx.op_id)
                        .await;
                }
            }
            let _ = event_tx.send(PipelineEvent::CliEvent { repo_id, event: ev });
            let epoch = mvcc_store.committed_epoch();
            wakeup.notify_repo_updated(repo_id, epoch);
            if is_terminal {
                break;
            }
        }
    });
    cancel
}

/// Helper to construct default [`CommitOptions`] cleanly from composer drafts.
pub fn make_commit_options(
    message: String,
    amend: bool,
    gpg_sign: Option<bool>,
    no_verify: bool,
) -> CommitOptions {
    CommitOptions {
        message,
        amend,
        gpg_sign,
        signing_key_id: None,
        no_verify,
        allow_empty: false,
    }
}

/// Helper to construct [`MergeOptions`] for drag-and-drop or toolbar merge actions.
pub fn make_merge_options(target_rev: &str, no_ff: bool) -> MergeOptions {
    MergeOptions {
        target_rev: CompactString::from(target_rev),
        no_ff,
        ff_only: false,
        squash: false,
        message: None,
    }
}
