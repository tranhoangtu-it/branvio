//! UI-agnostic Presenter ports decoupling all Core Crates from the `gpui` rendering layer.
//!
//! Any alternative UI stack (`Iced`, `Tauri v2`, or headless test harness) only needs to
//! implement [`UiRuntimeBackend`] and interact with [`CoreAppHost`].

use branvio_core::domain::{ConflictResolution, ResetMode};
use branvio_core::ids::{GitOid, RepoEpoch, RepoId};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{MvccRepoStateStore, UiWakeupSink};
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use git_engine::{CommitOptions, HybridGitEngine, RebasePlanPayload};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Errors raised at the UI runtime shell boundary.
#[derive(Debug, Error)]
pub enum UiBackendError {
    /// Failed to initialize native GPU/windowing surface.
    #[error("UI backend initialization failed: {0}")]
    InitFailed(String),

    /// Command dispatch failed in an underlying core service.
    #[error("action dispatch failed: {0}")]
    DispatchFailed(String),
}

/// Center pane rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffViewMode {
    /// Interactive Commit Graph canvas.
    CommitGraph,
    /// Unified single-column diff viewer.
    UnifiedDiff,
    /// Side-by-side split diff viewer.
    SideBySideDiff,
    /// 4-buffer 3-Way Merge Conflict Editor (`Base`, `Ours`, `Theirs`, `Output`).
    ThreeWayMergeEditor,
}

/// Right Inspector display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InspectorMode {
    /// Staging panel (Unstaged / Staged / Conflicted files + Commit Composer).
    WorkingTreeStaging,
    /// Read-only commit metadata + changed files tree for a selected historical commit.
    CommitDetail {
        /// Selected commit OID.
        oid: GitOid,
    },
    /// Active 3-way conflict summary panel.
    MergeConflictSummary,
}

/// Drag-and-Drop and context-menu operations triggered directly from the Commit Graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphInteractionAction {
    /// Dragged branch/commit dropped onto another branch to merge (`git merge`).
    MergeIntoHead {
        /// Dragged source ref or OID.
        source_rev: CompactString,
    },
    /// Dragged branch dropped onto target commit/branch to rebase (`git rebase`).
    RebaseCurrentOnto {
        /// Target base ref or OID.
        onto_rev: CompactString,
    },
    /// Launches Interactive Rebase modal from `base_oid..HEAD`.
    OpenInteractiveRebase {
        /// Base commit OID (`git rebase -i <base_oid>`).
        base_oid: GitOid,
    },
    /// Executes an edited interactive rebase plan via `GIT_SEQUENCE_EDITOR`.
    ExecuteInteractiveRebase {
        /// Prepared interactive rebase plan.
        plan: RebasePlanPayload,
    },
    /// Cherry-picks selected commit(s) onto current `HEAD`.
    CherryPickCommits {
        /// Ordered commit OIDs.
        commits: Vec<GitOid>,
    },
    /// Resets current branch/`HEAD` to target commit (`--soft`, `--mixed`, `--hard`).
    ResetCurrentBranchTo {
        /// Target revision/OID.
        target_rev: CompactString,
        /// Reset mode.
        mode: ResetMode,
    },
    /// Checks out a branch or commit.
    Checkout {
        /// Ref name or OID.
        target_rev: CompactString,
    },
    /// Creates a commit with options.
    Commit(CommitOptions),
    /// Applies a 3-way merge conflict resolution in the active merge session.
    ResolveConflictRegion {
        /// Target conflicted file path.
        path: RepoRelPath,
        /// Conflict region index.
        region_id: u32,
        /// Resolution choice.
        resolution: ConflictResolution,
    },
}

/// Entry searchable via `nucleo` inside the `Cmd/Ctrl + P` Command Palette.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteCommandItem {
    /// Stable command identifier.
    pub id: CompactString,
    /// Primary label matched fuzzily by `nucleo`.
    pub title: CompactString,
    /// Category badge (`Git`, `Branch`, `Worktree`, `View`, `AI`).
    pub category: CompactString,
    /// Pre-formatted OS shortcut string (`⌘P` vs `Ctrl+P`).
    pub shortcut_hint: Option<CompactString>,
}

/// Layout mode for changed files in Staging and Commit Detail file lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FileListLayoutMode {
    /// Flat repository-relative path list.
    #[default]
    FlatList,
    /// Nested directory tree grouped by path segments.
    FolderTree,
}

/// Live hover/prediction state displayed during drag-and-drop on branches or graph nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragDropPreviewState {
    /// Display label of the dragged source branch/commit.
    pub source_label: compact_str::CompactString,
    /// Source commit OID (`None` if dragging an unborn branch).
    pub source_oid: Option<branvio_core::ids::GitOid>,
    /// Display label of the hovered drop target (`None` when not over a valid target).
    pub target_label: Option<compact_str::CompactString>,
    /// Target commit OID (`None` when not over a valid commit/branch target).
    pub target_oid: Option<branvio_core::ids::GitOid>,
    /// In-memory Tier 2 merge prediction for `(source_oid, target_oid)`.
    pub prediction: Option<git_engine::MergePrediction>,
}

/// Combined port unifying `DiffEngine`, `SyntaxHighlighter`, `PatchBuilder`, and `MergeConflictEngine`.
pub trait DiffMergePort:
    diff_merge::DiffEngine
    + diff_merge::SyntaxHighlighter
    + diff_merge::PatchBuilder
    + diff_merge::MergeConflictEngine
    + Send
    + Sync
{
    /// Applies a direct UTF-8 byte edit to `doc.output` without requiring `&mut` on the shared port.
    fn apply_output_edit(
        &self,
        doc: &mut diff_merge::ThreeWayMergeDocument,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) -> diff_merge::DiffMergeResult<()>;
}

impl DiffMergePort for diff_merge::BranvioDiffMergeEngine {
    fn apply_output_edit(
        &self,
        doc: &mut diff_merge::ThreeWayMergeDocument,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) -> diff_merge::DiffMergeResult<()> {
        let mut engine = *self;
        diff_merge::MergeConflictEngine::edit_output_buffer(
            &mut engine,
            doc,
            byte_range,
            replacement_utf8,
        )
    }
}

/// Dependency container injected into any UI runtime (`gpui`, `Iced`, or `Tauri v2`).
pub struct CoreAppHost {
    /// Async Tokio runtime shared across all non-UI background workers.
    pub tokio_handle: tokio::runtime::Handle,
    /// Hydrated OS Git/SSH/GPG environment snapshot.
    pub git_env: Arc<HydratedGitEnv>,
    /// 3-Tier Hybrid Git Engine instance.
    pub git_engine: Arc<dyn HybridGitEngine>,
    /// Diff computation, AST highlighting, synthetic patch building, and 3-way merge engine.
    pub diff_merge: Arc<dyn DiffMergePort>,
    /// Metro-line Commit Graph DAG layout engine.
    pub graph_layout: Arc<dyn graph_layout::GraphLayoutEngine>,
    /// Optional Phase 5 Time-Machine Undo/Redo engine.
    pub undo_engine: Option<Arc<dyn oplog_undo::UndoRedoEngine>>,
    /// Optional Phase 5 persistent `redb` operation journal store (used for accurate `can_undo`/`can_redo`).
    pub oplog_store: Option<Arc<dyn oplog_undo::OpLogStore>>,
    /// Optional Phase 5 adaptive filesystem watcher.
    pub repo_watcher: Option<Arc<dyn fs_watcher::RepoWatcher>>,
    /// Optional Phase 5 cross-platform embedded shell PTY backend.
    pub terminal_backend: Option<Arc<dyn terminal_pty::TerminalBackend>>,
    /// Optional Phase 5 OS keyring token vault.
    pub token_vault: Option<Arc<dyn forge_client::TokenVault>>,
    /// Optional Phase 5 Git hosting forge provider.
    pub forge_provider: Option<Arc<dyn forge_client::ForgeProvider>>,
    /// Optional Phase 5 AI Copilot provider.
    pub ai_provider: Option<Arc<dyn ai_copilot::AiCopilotProvider>>,
    /// Optional AI Copilot endpoint preset when AI services are configured.
    pub ai_config: Option<ai_copilot::AiEndpointConfig>,
    /// Lock-free MVCC repository state stores indexed by active tab [`RepoId`].
    pub active_repos: Vec<(RepoId, HostPath, Arc<MvccRepoStateStore>)>,
}

impl CoreAppHost {
    /// Constructs a [`CoreAppHost`] wired with standard core diff-merge and metro-graph engines.
    pub fn new(
        tokio_handle: tokio::runtime::Handle,
        git_env: Arc<HydratedGitEnv>,
        git_engine: Arc<dyn HybridGitEngine>,
        active_repos: Vec<(RepoId, HostPath, Arc<MvccRepoStateStore>)>,
    ) -> Self {
        Self {
            tokio_handle,
            git_env,
            git_engine,
            diff_merge: Arc::new(diff_merge::BranvioDiffMergeEngine::new()),
            graph_layout: Arc::new(graph_layout::MetroLayoutEngine::new()),
            undo_engine: None,
            oplog_store: None,
            repo_watcher: None,
            terminal_backend: None,
            token_vault: None,
            forge_provider: None,
            ai_provider: None,
            ai_config: None,
            active_repos,
        }
    }

    /// Attaches optional Phase 5 services (`oplog-undo`, `forge-client`, `ai-copilot`).
    #[must_use]
    pub fn with_optional_services(
        mut self,
        undo_engine: Option<Arc<dyn oplog_undo::UndoRedoEngine>>,
        forge_provider: Option<Arc<dyn forge_client::ForgeProvider>>,
        ai_provider: Option<Arc<dyn ai_copilot::AiCopilotProvider>>,
    ) -> Self {
        self.undo_engine = undo_engine;
        self.forge_provider = forge_provider;
        self.ai_provider = ai_provider;
        self
    }

    /// Attaches additional Phase 5 runtime backends (`oplog_store`, `repo_watcher`, `terminal_backend`,
    /// `token_vault`, and optional default [`ai_copilot::AiEndpointConfig`]).
    #[must_use]
    pub fn with_phase5_runtime(
        mut self,
        oplog_store: Option<Arc<dyn oplog_undo::OpLogStore>>,
        repo_watcher: Option<Arc<dyn fs_watcher::RepoWatcher>>,
        terminal_backend: Option<Arc<dyn terminal_pty::TerminalBackend>>,
        token_vault: Option<Arc<dyn forge_client::TokenVault>>,
        ai_config: Option<ai_copilot::AiEndpointConfig>,
    ) -> Self {
        self.oplog_store = oplog_store;
        self.repo_watcher = repo_watcher;
        self.terminal_backend = terminal_backend;
        self.token_vault = token_vault;
        self.ai_config = ai_config;
        self
    }
}

/// Abstract UI Runtime Backend trait enabling swap between `gpui`, `Iced`, and `Tauri v2`
/// without altering any code inside `branvio-core`, `git-engine`, `graph-layout`, or `diff-merge`.
pub trait UiRuntimeBackend: Send + Sync {
    /// Human-readable backend identifier (e.g., `"PURE_RUST_GPUI (gpui + wgpu)"`).
    fn backend_name(&self) -> &'static str;

    /// Returns a [`UiWakeupSink`] wired to the UI event loop so background MVCC commits
    /// wake the window cleanly without polling.
    fn create_wakeup_sink(&self) -> Arc<dyn UiWakeupSink>;

    /// Launches the main event loop and opens the native desktop window.
    fn run_event_loop(self: Box<Self>, host: CoreAppHost) -> Result<(), UiBackendError>;
}

/// Minimal no-op wakeup sink useful for headless tests or CLI helper mode.
#[derive(Debug, Default)]
pub struct NoopWakeupSink;

impl UiWakeupSink for NoopWakeupSink {
    fn notify_repo_updated(&self, _repo_id: RepoId, _epoch: RepoEpoch) {}
}
