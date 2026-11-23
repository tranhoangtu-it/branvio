//! Core UI-agnostic presenter managing multi-repo workspace tabs, selection state,
//! MVCC background synchronization, diff/patch workflows, 3-way conflict resolution, and modals.

pub mod command_palette;
pub mod destructive_guard;
pub mod graph_geometry;
pub mod phase5_worker;
pub mod pipeline_worker;
pub mod rebase_planner;
pub mod sidebar_tree;

use crate::ports::{
    CoreAppHost, DiffViewMode, DragDropPreviewState, FileListLayoutMode, InspectorMode,
};
use crate::presenter::command_palette::{
    PaletteActionKind, PaletteEntry, PaletteMatch, build_palette_entries, search_palette_entries,
};
pub use crate::presenter::pipeline_worker::{
    AiFeature, AiUiError, DiffPreviewNotice, RawConflictSide, UndoRedoUiError,
};
use crate::presenter::pipeline_worker::{
    CliLogEntry, ConflictEditorSession, ConflictLoadError, DEFAULT_REVWALK_CHUNK_SIZE,
    MAX_CLI_LOG_LINES, PaginationSessionState, PipelineEvent, RefreshRequest, UndoTxnContext,
    make_commit_options, make_empty_repo_snapshot, make_merge_options, pump_cli_handle,
    spawn_drag_drop_prediction, spawn_load_commit_details, spawn_load_delta_diff,
    spawn_load_more_history, spawn_open_conflict_session, spawn_patch_mutation, spawn_repo_refresh,
    spawn_resolve_raw_conflict_side, spawn_save_conflict_resolution, spawn_whole_file_index_op,
};
use crate::presenter::rebase_planner::InteractiveRebasePlanner;
use crate::presenter::sidebar_tree::{
    SidebarRefSections, SubmoduleSummary, build_sidebar_ref_sections,
};
use crate::theme::BranvioTheme;
use branvio_core::domain::{
    CommitDetails, ConflictResolution, DeltaArea, DeltaStatus, FileDelta, FileDiffPatch,
    RepoOperationState, ResetMode,
};
use branvio_core::ids::{GitOid, RepoId};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{
    CancelToken, DirtyScopeFlags, InternalOpController, MvccRepoStateStore, RepoViewSnapshot,
    UiWakeupSink,
};
use compact_str::CompactString;
use diff_merge::{DiffOptions, LineSelectionSpec, PatchDirection};
use git_engine::{
    CliCommandHandle, CliStreamEvent, PushForce, RebaseAction, RepoSummary, WorktreeInfo,
};
use graph_layout::GraphFilterSpec;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Remote used for push / force push from the toolbar and palette.
const DEFAULT_PUSH_REMOTE: &str = "origin";

/// Bytes a terminal sends for the Enter key when submitting a command line.
///
/// Windows ConPTY translates VT input into console key events: only CR becomes `VK_RETURN`,
/// while a bare LF is delivered as Ctrl+Enter, which PowerShell/PSReadLine and `cmd.exe` do not
/// treat as "submit". Unix PTYs keep LF (the line discipline / line editors accept it directly).
#[cfg(windows)]
const TERMINAL_ENTER_KEY: &[u8] = b"\r";
#[cfg(not(windows))]
const TERMINAL_ENTER_KEY: &[u8] = b"\n";

/// Environment variable that pre-enables cloud AI uploads when no setting UI is available.
///
/// Fallback only: accepted values are `1`/`true`/`yes`. The in-app toggle is the primary control.
pub const AI_CLOUD_UPLOAD_ENV: &str = "BRANVIO_AI_ALLOW_CLOUD_UPLOAD";

const ALL_DIRTY_SCOPES: DirtyScopeFlags = DirtyScopeFlags::WORKTREE
    .union(DirtyScopeFlags::INDEX)
    .union(DirtyScopeFlags::REFS_OR_HEAD);

/// Returns the checked-out local branch name of `tab` (`None` when `HEAD` is detached/unborn).
fn current_head_branch(tab: &RepoTabState) -> Option<String> {
    tab.snapshot
        .refs
        .iter()
        .find(|b| b.is_head && b.kind == branvio_core::domain::RefKind::LocalBranch)
        .map(|b| b.name.to_string())
}

/// Toast or status banner message surfaced at the top/bottom chrome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBanner {
    /// True if message indicates a failure requiring user attention.
    pub is_error: bool,
    /// Concise display message.
    pub message: String,
}

/// User privacy settings for AI Copilot uploads.
///
/// Off-device providers (anything not on `localhost`) receive (secret-masked) diffs only when
/// [`Self::allows_cloud_upload_for`] returns `true`. Defaults to deny. Settings live for the
/// lifetime of the presenter (the app has no persisted settings store yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiPrivacySettings {
    /// "Allow sending diffs to cloud AI" for every repository.
    pub allow_cloud_upload: bool,
    /// Repositories (by working tree root) where the user allowed cloud uploads individually.
    pub cloud_upload_repos: BTreeSet<HostPath>,
}

impl AiPrivacySettings {
    /// Builds defaults, honoring the [`AI_CLOUD_UPLOAD_ENV`] opt-in fallback.
    pub fn from_env() -> Self {
        let allow = std::env::var(AI_CLOUD_UPLOAD_ENV)
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
        Self {
            allow_cloud_upload: allow,
            cloud_upload_repos: BTreeSet::new(),
        }
    }

    /// Returns `true` when diffs from `repo_root` may be sent to an off-device AI provider.
    pub fn allows_cloud_upload_for(&self, repo_root: &HostPath) -> bool {
        self.allow_cloud_upload || self.cloud_upload_repos.contains(repo_root)
    }
}

/// Which variant of force push the user picked in the confirmation modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcePushChoice {
    /// "Force push safely": `--force-with-lease` pinned to the OID captured when the modal opened.
    WithLease,
    /// "Force push": unconditional `--force`.
    Force,
}

/// Title shown on the force-push confirmation modal.
pub fn force_push_title(branch: &str) -> String {
    format!("Force push {branch}?")
}

/// Explanation shown on the force-push confirmation modal.
pub fn force_push_explanation(branch: &str, expected_remote_oid: Option<GitOid>) -> String {
    let remote_state = expected_remote_oid.map_or_else(
        || format!("{DEFAULT_PUSH_REMOTE}/{branch} does not exist yet."),
        |oid| format!("{DEFAULT_PUSH_REMOTE}/{branch} is at {}.", oid.short_hex(8)),
    );
    format!(
        "Commits on the remote that are not on your local {branch} will be removed. {remote_state} \
         \"Force push safely\" fails if the remote changed since this dialog opened; \
         \"Force push\" overwrites it regardless."
    )
}

/// Active tab inside the bottom expandable workspace drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BottomDrawerTab {
    /// Streaming native Tier 3 Git CLI & hook output.
    #[default]
    GitOutput,
    /// Interactive cross-platform OS shell PTY backed by `portable-pty` + `alacritty_terminal`.
    Terminal,
}

/// Modal dialog state owned centrally by [`BranvioPresenter`].
#[derive(Debug, Clone)]
pub enum ActiveModal {
    /// No modal currently open.
    None,
    /// Fuzzy Command Palette (`Cmd/Ctrl+P`).
    CommandPalette {
        /// Active search string.
        query: String,
        /// Highlighted index inside filtered results.
        selected_index: usize,
    },
    /// Interactive Rebase plan editor (`git rebase -i`).
    InteractiveRebase {
        /// Stateful rebase planner holding ordered commits and actions.
        planner: InteractiveRebasePlanner,
        /// Row currently editing a `Reword`/`Squash` message.
        editing_message_row: Option<usize>,
        /// Temporary textarea content for edited message.
        message_draft: String,
        /// Last validation error string shown inside modal banner.
        validation_error: Option<String>,
    },
    /// Drag-and-drop action popover (`Merge`, `Rebase`, `Reset`) with Tier 2 conflict prediction.
    DropBranchAction {
        /// Live preview + predicted conflicts.
        preview: DragDropPreviewState,
        /// Whether to force `--no-ff` on merge.
        merge_no_ff: bool,
    },
    /// Create branch dialog targeting a branch or commit.
    CreateBranch {
        /// Revision at which the branch will be created.
        base_rev: String,
        /// New branch name draft.
        branch_name_draft: String,
        /// Immediately switch to the newly created branch (`git checkout -b`).
        checkout_immediately: bool,
        /// Inline validation error.
        error: Option<String>,
    },
    /// Create linked Git worktree dialog.
    CreateWorktree {
        /// Target filesystem directory path.
        target_dir_draft: String,
        /// Existing branch/OID or new branch name.
        branch_or_oid_draft: String,
        /// Check `-b` to create a new branch inside the worktree.
        create_new_branch: bool,
        /// Inline validation error.
        error: Option<String>,
    },
    /// Open another repository workspace tab by local path.
    OpenRepository {
        /// Filesystem path input.
        path_draft: String,
        /// Validation/open error.
        error: Option<String>,
    },
    /// Confirmation prompt when Time-Machine Undo/Redo touches commits already pushed to remotes.
    ConfirmRewritePushedUndo {
        /// True when confirming `redo`, false for `undo`.
        is_redo: bool,
        /// Remote-tracking branches containing commits being rewound.
        remote_branches: Vec<CompactString>,
    },
    /// Confirmation before force pushing `branch` to `origin`.
    ///
    /// `expected_remote_oid` is captured once, when this modal opens, and is the exact lease value
    /// used on confirmation (it is never re-read, so background fetches cannot widen the lease).
    ConfirmForcePush {
        /// Repository tab the push targets.
        repo_id: RepoId,
        /// Local branch being force pushed.
        branch: CompactString,
        /// `origin/<branch>` OID when the user opened the dialog (`None` = no remote branch).
        expected_remote_oid: Option<GitOid>,
    },
    /// Consent prompt shown when an AI request was refused because it would upload to the cloud.
    ConfirmAiCloudUpload {
        /// Repository tab whose content would be uploaded.
        repo_id: RepoId,
        /// Provider display label (e.g. `Anthropic`).
        provider: String,
        /// Feature that was refused.
        feature: AiFeature,
    },
    /// Confirmation before sending a destructive `git` command typed in the terminal box.
    ConfirmDestructiveTerminalCommand {
        /// Repository tab whose terminal receives the command.
        repo_id: RepoId,
        /// Full command line as typed.
        command_line: String,
        /// The destructive segment that triggered the prompt.
        command: String,
        /// What will be lost.
        reason: String,
    },
    /// Saving the undo point before a mutation failed; the mutation is held back until the user
    /// explicitly chooses to continue without undo (Cancel is the default).
    ConfirmContinueWithoutUndo {
        /// Repository tab the held-back mutation targets.
        repo_id: RepoId,
        /// Short description of the held-back operation.
        summary: String,
        /// Why the undo point could not be saved.
        reason: String,
    },
    /// Dialog for creating a new Pull/Merge Request via `forge-client`.
    CreatePullRequest {
        /// Source head branch already pushed to remote.
        head_branch: CompactString,
        /// Target base branch name.
        base_branch_draft: String,
        /// Pull Request title draft.
        title_draft: String,
        /// Pull Request description body draft.
        body_draft: String,
        /// Create as Draft PR/MR.
        draft: bool,
        /// Inline validation/API error.
        error: Option<String>,
    },
}

/// Per-repository workspace tab state synchronized asynchronously via MVCC + [`PipelineEvent`].
pub struct RepoTabState {
    /// Unique tab identifier.
    pub repo_id: RepoId,
    /// Canonical working tree root path.
    pub repo_root: HostPath,
    /// Tab bar short label (directory basename).
    pub display_name: CompactString,
    /// Lock-free MVCC snapshot store read at 120 FPS by UI canvases.
    pub mvcc_store: Arc<MvccRepoStateStore>,
    /// Internal-op lease controller suppressing external watcher storms.
    pub internal_ops: Arc<InternalOpController>,
    /// Last loaded repository metadata summary.
    pub summary: Option<RepoSummary>,
    /// Cached latest MVCC snapshot handle (refreshed on `drain_events`).
    pub snapshot: Arc<RepoViewSnapshot>,
    /// Linked worktrees (`git worktree list`).
    pub worktrees: Vec<WorktreeInfo>,
    /// Submodules parsed from `.gitmodules`.
    pub submodules: Vec<SubmoduleSummary>,
    /// Selected commit row OID in the graph (`None` if WIP or nothing selected).
    pub selected_commit_oid: Option<GitOid>,
    /// True when the virtual top **WIP** row is selected.
    pub selected_wip: bool,
    /// Cached commit details for `selected_commit_oid`.
    pub selected_commit_details: Option<CommitDetails>,
    /// Currently inspected changed file delta (staging or commit file).
    pub selected_delta: Option<FileDelta>,
    /// Computed AST-highlighted diff patch for `selected_delta`.
    pub active_diff_patch: Option<FileDiffPatch>,
    /// Notice or special-file banner (Loading, Binary, LFS, UnsupportedEncoding, or I/O error).
    pub diff_notice: Option<DiffPreviewNotice>,
    /// Per-hunk checked line indices for discrete line-level Stage / Unstage / Discard.
    pub selected_diff_lines: BTreeMap<u32, BTreeSet<usize>>,
    /// Active 4-buffer `ropey::Rope` 3-way conflict editor session.
    pub conflict_session: Option<ConflictEditorSession>,
    /// Commit composer subject line.
    pub commit_subject: String,
    /// Commit composer optional body.
    pub commit_body: String,
    /// Commit composer `--amend` toggle.
    pub commit_amend: bool,
    /// Commit composer explicit GPG/SSH sign override (`None` respects `.gitconfig`).
    pub commit_gpg_sign: Option<bool>,
    /// Commit composer `--no-verify` toggle.
    pub commit_no_verify: bool,
    /// Graph filter criteria.
    pub filter_spec: GraphFilterSpec,
    /// Filter bar search input text.
    pub search_query: String,
    /// When `true`, `search_query` executes Tier 3 pickaxe code search (`-S`/`-G`).
    pub search_is_pickaxe: bool,
    /// When `true`, pickaxe uses regex diff search (`-G`).
    pub search_is_regex: bool,
    /// Restrict history graph to first-parent lineage.
    pub first_parent_only: bool,
    /// True once stateful revwalk session reached initial root commit.
    pub revwalk_exhausted: bool,
    /// True while pulling another viewport chunk via `load_more_history`.
    pub is_loading_more: bool,
    /// True while a Tier 2 index or worktree patch mutation is in flight.
    pub is_mutating_index: bool,
    /// Collapsed branch folder keys preserved across refreshes.
    pub collapsed_branch_folders: HashSet<CompactString>,
    /// Collapsed file tree directory keys preserved across refreshes.
    pub collapsed_file_dirs: HashSet<CompactString>,
    /// Active cancellation token for in-flight background refresh.
    pub active_refresh_cancel: Option<CancelToken>,
    /// Active cancellation token for in-flight Tier 3 CLI process.
    pub active_cli_cancel: Option<CancelToken>,
    /// True while a Tier 3 CLI command is running.
    pub cli_running: bool,
    /// Live network/checkout transfer progress `(phase, percent)`.
    pub cli_transfer_progress: Option<(CompactString, u8)>,
    /// Bounded output lines streamed to the bottom CLI drawer.
    pub cli_logs: Vec<CliLogEntry>,
    /// Shared revwalk + layout session handle for incremental pagination.
    pagination: Option<Arc<Mutex<PaginationSessionState>>>,
    /// Active Phase 5 adaptive filesystem watcher handle (dropping cancels watch).
    pub watch_handle: Option<Box<dyn fs_watcher::RepoWatchHandle>>,
    /// Active Phase 5 embedded PTY terminal session handle.
    pub terminal_session: Option<Arc<dyn terminal_pty::TerminalSession>>,
    /// Latest rendered `alacritty_terminal` viewport damage snapshot.
    pub terminal_snapshot: Option<terminal_pty::TerminalDamageSnapshot>,
    /// Active terminal tab title.
    pub terminal_title: CompactString,
    /// Active Phase 5 remote Pull/Merge Requests fetched from `forge-client`.
    pub pull_requests: Vec<forge_client::PullRequestSummary>,
    /// Short remote label (e.g. `owner/repo`) shown above PR list.
    pub pull_requests_remote: Option<String>,
    /// Last Pull Request fetch diagnostic (when configured remote/token failed).
    pub pull_requests_error: Option<String>,
    /// True while fetching remote Pull Requests.
    pub pull_requests_loading: bool,
    /// True while AI Copilot commit generation request is in flight.
    pub ai_commit_busy: bool,
    /// Per-conflict-region AI Copilot resolution recommendations.
    pub ai_conflict_advice: BTreeMap<u32, ai_copilot::ConflictResolutionAdvice>,
}

impl RepoTabState {
    fn new(repo_id: RepoId, repo_root: HostPath, mvcc_store: Arc<MvccRepoStateStore>) -> Self {
        let dir_name = CompactString::from(
            repo_root
                .as_path()
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("repository"),
        );
        let snapshot = mvcc_store.load_snapshot();
        Self {
            repo_id,
            repo_root,
            display_name: dir_name,
            mvcc_store,
            internal_ops: Arc::new(InternalOpController::default()),
            summary: None,
            snapshot,
            worktrees: Vec::new(),
            submodules: Vec::new(),
            selected_commit_oid: None,
            selected_wip: true,
            selected_commit_details: None,
            selected_delta: None,
            active_diff_patch: None,
            diff_notice: None,
            selected_diff_lines: BTreeMap::new(),
            conflict_session: None,
            commit_subject: String::new(),
            commit_body: String::new(),
            commit_amend: false,
            commit_gpg_sign: None,
            commit_no_verify: false,
            filter_spec: GraphFilterSpec {
                show_wip_node: true,
                ..GraphFilterSpec::default()
            },
            search_query: String::new(),
            search_is_pickaxe: false,
            search_is_regex: false,
            first_parent_only: false,
            revwalk_exhausted: false,
            is_loading_more: false,
            is_mutating_index: false,
            collapsed_branch_folders: HashSet::new(),
            collapsed_file_dirs: HashSet::new(),
            active_refresh_cancel: None,
            active_cli_cancel: None,
            cli_running: false,
            cli_transfer_progress: None,
            cli_logs: Vec::new(),
            pagination: None,
            watch_handle: None,
            terminal_session: None,
            terminal_snapshot: None,
            terminal_title: CompactString::const_new("Terminal"),
            pull_requests: Vec::new(),
            pull_requests_remote: None,
            pull_requests_error: None,
            pull_requests_loading: false,
            ai_commit_busy: false,
            ai_conflict_advice: BTreeMap::new(),
        }
    }

    /// Computes sidebar sections (`Local`, grouped `Remotes`, `Tags`, `Stashes`) respecting collapsed folders.
    pub fn sidebar_sections(&self) -> SidebarRefSections {
        build_sidebar_ref_sections(&self.snapshot.refs, &self.collapsed_branch_folders)
    }

    /// Composes `commit_subject` + optional `commit_body` into a Git commit message.
    pub fn full_commit_message(&self) -> String {
        let subj = self.commit_subject.trim();
        let body = self.commit_body.trim();
        if body.is_empty() {
            subj.to_string()
        } else {
            format!("{subj}\n\n{body}")
        }
    }

    /// Checks whether the repository currently has staged changes or `--amend` is active.
    pub fn can_commit(&self) -> bool {
        !self.commit_subject.trim().is_empty()
            && (!self.snapshot.working_tree.staged.is_empty() || self.commit_amend)
    }

    /// Appends one line to `cli_logs` keeping bounded capacity [`MAX_CLI_LOG_LINES`].
    fn push_cli_log(&mut self, is_stderr: bool, text: String) {
        if self.cli_logs.len() >= MAX_CLI_LOG_LINES {
            self.cli_logs.remove(0);
        }
        self.cli_logs.push(CliLogEntry { is_stderr, text });
    }
}

/// Master application presenter driving all UI panels and background async workflows.
pub struct BranvioPresenter {
    /// Core engine ports (`git-engine`, `diff-merge`, `graph-layout`, Tokio handle).
    pub host: CoreAppHost,
    /// UI wakeup sink notified whenever background workers finish or emit CLI lines.
    pub wakeup: Arc<dyn UiWakeupSink>,
    /// Open workspace repository tabs.
    pub tabs: Vec<RepoTabState>,
    /// Currently active tab ID.
    pub active_repo_id: Option<RepoId>,
    /// Active visual color palette (`Dark` or `Light`).
    pub theme: BranvioTheme,
    /// Diff view polarity (`Unified` vs `SideBySide`).
    pub diff_view_mode: DiffViewMode,
    /// Changed files list polarity (`FlatList` vs `FolderTree`).
    pub file_list_layout: FileListLayoutMode,
    /// Right inspector panel mode (`WorkingTreeStaging` vs `CommitDetails`).
    pub inspector_mode: InspectorMode,
    /// Whether the left sidebar is visible.
    pub sidebar_visible: bool,
    /// Whether the right inspector pane is visible.
    pub inspector_visible: bool,
    /// Whether the bottom CLI output drawer is open.
    pub bottom_drawer_open: bool,
    /// Which tab is active inside the bottom drawer (`GitOutput` vs `Terminal`).
    pub bottom_drawer_tab: BottomDrawerTab,
    /// Height of bottom drawer in logical px.
    pub bottom_drawer_height_px: f32,
    /// Live drag-and-drop preview state (while dragging across sidebar/graph).
    pub drag_preview: Option<DragDropPreviewState>,
    /// Active modal overlay (`None` when closed).
    pub modal: ActiveModal,
    /// Latest feedback toast / error banner.
    pub status_banner: Option<StatusBanner>,
    /// Next monotonic [`RepoId`] allocated when opening additional repository tabs.
    next_repo_id: u64,
    /// Channel receiver collecting [`PipelineEvent`] completions from background tasks.
    event_rx: mpsc::UnboundedReceiver<PipelineEvent>,
    /// Channel sender cloned into spawned background tasks.
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    /// Active diff options shared across preview requests.
    pub diff_options: DiffOptions,
    /// Commit count loaded per initial refresh chunk (defaults to [`DEFAULT_REVWALK_CHUNK_SIZE`]).
    pub initial_chunk_size: usize,
    /// AI Copilot upload consent (deny by default).
    pub ai_privacy: AiPrivacySettings,
    /// Mutation held back by [`ActiveModal::ConfirmContinueWithoutUndo`].
    pending_unprotected: Option<(RepoId, pipeline_worker::PendingUnprotectedMutation)>,
}

impl BranvioPresenter {
    /// Initializes [`BranvioPresenter`] from injected [`CoreAppHost`] and [`UiWakeupSink`].
    pub fn new(host: CoreAppHost, wakeup: Arc<dyn UiWakeupSink>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut max_id = 0u64;
        let mut tabs = Vec::with_capacity(host.active_repos.len());
        for (repo_id, path, store) in &host.active_repos {
            max_id = max_id.max(repo_id.0);
            tabs.push(RepoTabState::new(*repo_id, path.clone(), Arc::clone(store)));
        }
        let active_repo_id = tabs.first().map(|t| t.repo_id);

        let mut presenter = Self {
            host,
            wakeup,
            tabs,
            active_repo_id,
            theme: BranvioTheme::dark(),
            diff_view_mode: DiffViewMode::UnifiedDiff,
            file_list_layout: FileListLayoutMode::FlatList,
            inspector_mode: InspectorMode::WorkingTreeStaging,
            sidebar_visible: true,
            inspector_visible: true,
            bottom_drawer_open: false,
            bottom_drawer_tab: BottomDrawerTab::GitOutput,
            bottom_drawer_height_px: 210.0,
            drag_preview: None,
            modal: ActiveModal::None,
            status_banner: None,
            next_repo_id: max_id + 1,
            event_rx,
            event_tx,
            diff_options: DiffOptions::default(),
            initial_chunk_size: DEFAULT_REVWALK_CHUNK_SIZE,
            ai_privacy: AiPrivacySettings::from_env(),
            pending_unprotected: None,
        };

        let tab_ids: Vec<RepoId> = presenter.tabs.iter().map(|t| t.repo_id).collect();
        for repo_id in tab_ids {
            presenter.trigger_refresh(repo_id, ALL_DIRTY_SCOPES);
        }
        presenter
    }

    /// Returns an immutable reference to the active repository tab, if any.
    pub fn active_tab(&self) -> Option<&RepoTabState> {
        let active_id = self.active_repo_id?;
        self.tabs.iter().find(|t| t.repo_id == active_id)
    }

    /// Returns a mutable reference to the active repository tab, if any.
    pub fn active_tab_mut(&mut self) -> Option<&mut RepoTabState> {
        let active_id = self.active_repo_id?;
        self.tabs.iter_mut().find(|t| t.repo_id == active_id)
    }

    /// Returns mutable reference to tab matching `repo_id`.
    pub fn tab_mut(&mut self, repo_id: RepoId) -> Option<&mut RepoTabState> {
        self.tabs.iter_mut().find(|t| t.repo_id == repo_id)
    }

    /// Opens a repository path in a new workspace tab (or focuses existing tab if already open)
    /// and immediately triggers background Tier 1 + layout loading.
    pub fn open_repository_tab(&mut self, path: &Path) -> Result<RepoId, String> {
        let canonical = HostPath::canonicalize_existing(path).map_err(|e| e.to_string())?;
        if let Some(existing) = self.tabs.iter().find(|t| t.repo_root == canonical) {
            let id = existing.repo_id;
            self.active_repo_id = Some(id);
            return Ok(id);
        }

        let repo_id = RepoId(self.next_repo_id);
        self.next_repo_id += 1;
        let initial = make_empty_repo_snapshot(repo_id, canonical.clone());
        let store = Arc::new(MvccRepoStateStore::new(initial));
        self.host
            .active_repos
            .push((repo_id, canonical.clone(), Arc::clone(&store)));
        self.tabs.push(RepoTabState::new(repo_id, canonical, store));
        self.active_repo_id = Some(repo_id);
        self.trigger_refresh(repo_id, ALL_DIRTY_SCOPES);
        Ok(repo_id)
    }

    /// Switches active workspace tab to `repo_id`.
    pub fn select_tab(&mut self, repo_id: RepoId) -> bool {
        if self.tabs.iter().any(|t| t.repo_id == repo_id) {
            self.active_repo_id = Some(repo_id);
            true
        } else {
            false
        }
    }

    /// Closes workspace tab `repo_id` (cancelling in-flight refresh/CLI jobs on that tab).
    pub fn close_tab(&mut self, repo_id: RepoId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.repo_id == repo_id) else {
            return false;
        };
        let removed = self.tabs.remove(pos);
        if let Some(cancel) = removed.active_refresh_cancel {
            cancel.cancel();
        }
        if let Some(cancel) = removed.active_cli_cancel {
            cancel.cancel();
        }
        self.host.active_repos.retain(|(id, _, _)| *id != repo_id);
        if self.active_repo_id == Some(repo_id) {
            self.active_repo_id = self
                .tabs
                .get(pos.min(self.tabs.len().saturating_sub(1)))
                .map(|t| t.repo_id);
        }
        true
    }

    /// Cancels any superseded refresh on `repo_id` and launches a new background MVCC refresh.
    pub fn trigger_refresh(&mut self, repo_id: RepoId, _scope: DirtyScopeFlags) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.repo_id == repo_id) else {
            return;
        };
        if let Some(prev) = tab.active_refresh_cancel.take() {
            prev.cancel();
        }
        let cancel = CancelToken::new();
        tab.active_refresh_cancel = Some(cancel.clone());

        let req = RefreshRequest {
            repo_id,
            repo_root: tab.repo_root.clone(),
            mvcc_store: Arc::clone(&tab.mvcc_store),
            cancel,
            first_parent_only: tab.first_parent_only,
            filter_spec: tab.filter_spec.clone(),
            pickaxe_query: if tab.search_is_pickaxe {
                tab.search_query.clone()
            } else {
                String::new()
            },
            pickaxe_is_regex: tab.search_is_regex,
            initial_chunk_size: self.initial_chunk_size,
        };
        spawn_repo_refresh(
            &self.host,
            req,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Requests another streamed revwalk chunk (`load_more_history`) for chunked viewport pagination.
    pub fn load_more_history(&mut self, chunk_size: usize) {
        let Some(repo_id) = self.active_repo_id else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.repo_id == repo_id) else {
            return;
        };
        if tab.revwalk_exhausted || tab.is_loading_more {
            return;
        }
        let Some(pagination) = tab.pagination.clone() else {
            return;
        };

        tab.is_loading_more = true;
        spawn_load_more_history(
            &self.host,
            repo_id,
            Arc::clone(&tab.mvcc_store),
            pagination,
            chunk_size,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Selects the virtual top WIP node in the commit graph.
    pub fn select_wip_row(&mut self) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        tab.selected_wip = true;
        tab.selected_commit_oid = None;
        tab.selected_commit_details = None;
        self.inspector_mode = InspectorMode::WorkingTreeStaging;
    }

    /// Selects commit `oid` in the commit graph and triggers async Tier 1 detail loading.
    pub fn select_commit_row(&mut self, oid: GitOid) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        tab.selected_wip = false;
        tab.selected_commit_oid = Some(oid);
        tab.selected_commit_details = None;
        self.inspector_mode = InspectorMode::CommitDetail { oid };

        spawn_load_commit_details(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            oid,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Selects a changed [`FileDelta`] and computes its AST-highlighted diff (or opens 3-way conflict editor).
    pub fn select_file_delta(&mut self, delta: FileDelta) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let head_label = tab.snapshot.head_label.to_string();
        let is_conflicted = delta.area == DeltaArea::Conflicted
            || matches!(delta.status, DeltaStatus::Conflicted(_));

        tab.selected_delta = Some(delta.clone());
        tab.selected_diff_lines.clear();
        tab.active_diff_patch = None;
        tab.diff_notice = Some(DiffPreviewNotice::Message("Loading diff...".to_string()));

        if is_conflicted {
            spawn_open_conflict_session(
                &self.host,
                repo_id,
                repo_root,
                mvcc_store,
                delta.new_path,
                format!("HEAD ({head_label})"),
                "Incoming".to_string(),
                self.event_tx.clone(),
                Arc::clone(&self.wakeup),
            );
            return;
        }

        tab.conflict_session = None;
        spawn_load_delta_diff(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            delta,
            self.diff_options.clone(),
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Toggles line checkbox `line_idx` inside hunk `hunk_idx` of the active diff.
    pub fn toggle_diff_line_selection(&mut self, hunk_idx: u32, line_idx: usize) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let entry = tab.selected_diff_lines.entry(hunk_idx).or_default();
        if !entry.remove(&line_idx) {
            entry.insert(line_idx);
        }
        if entry.is_empty() {
            tab.selected_diff_lines.remove(&hunk_idx);
        }
    }

    /// Applies currently checked lines inside `hunk_idx` (or whole hunk if no lines checked)
    /// using [`PatchDirection`] (`StageToIndex`, `UnstageFromIndex`, or `DiscardInWorktree`).
    pub fn apply_hunk_or_selected_lines(&mut self, hunk_idx: u32, direction: PatchDirection) {
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(patch) = tab.active_diff_patch.clone() else {
            return;
        };
        let checked = tab
            .selected_diff_lines
            .get(&hunk_idx)
            .cloned()
            .unwrap_or_default();

        tab.is_mutating_index = true;
        let selections = vec![LineSelectionSpec {
            hunk_index: hunk_idx,
            selected_line_indices: checked,
        }];
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);

        spawn_patch_mutation(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            internal_ops,
            patch,
            selections,
            direction,
            false,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Stages explicit repository-relative paths via Tier 2 (`git2`).
    pub fn stage_paths(&mut self, paths: Vec<RepoRelPath>) {
        self.whole_file_index_op(paths, true);
    }

    /// Unstages explicit repository-relative paths via Tier 2 (`git2`).
    pub fn unstage_paths(&mut self, paths: Vec<RepoRelPath>) {
        self.whole_file_index_op(paths, false);
    }

    /// Stages all unstaged & untracked paths present in the latest MVCC status snapshot.
    pub fn stage_all_working_tree(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let paths: Vec<RepoRelPath> = tab
            .snapshot
            .working_tree
            .unstaged
            .iter()
            .chain(tab.snapshot.working_tree.conflicted.iter())
            .map(|d| d.new_path.clone())
            .collect();
        if !paths.is_empty() {
            self.stage_paths(paths);
        }
    }

    /// Unstages all staged paths present in the latest MVCC status snapshot.
    pub fn unstage_all_staged(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let paths: Vec<RepoRelPath> = tab
            .snapshot
            .working_tree
            .staged
            .iter()
            .map(|d| d.new_path.clone())
            .collect();
        if !paths.is_empty() {
            self.unstage_paths(paths);
        }
    }

    fn whole_file_index_op(&mut self, paths: Vec<RepoRelPath>, stage: bool) {
        if paths.is_empty() {
            return;
        }
        let (repo_id, repo_root, mvcc_store, internal_ops) = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            tab.is_mutating_index = true;
            (
                tab.repo_id,
                tab.repo_root.clone(),
                Arc::clone(&tab.mvcc_store),
                Arc::clone(&tab.internal_ops),
            )
        };
        spawn_whole_file_index_op(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            internal_ops,
            paths,
            stage,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Discards unstaged changes (`git checkout --` for tracked, `git clean -f -d --` for untracked).
    pub fn discard_worktree_deltas(&mut self, deltas: &[FileDelta]) {
        let mut tracked = Vec::new();
        let mut untracked = Vec::new();
        for d in deltas {
            if d.status == DeltaStatus::Untracked {
                untracked.push(d.new_path.clone());
            } else {
                tracked.push(d.new_path.clone());
            }
        }
        if tracked.is_empty() && untracked.is_empty() {
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((
                branvio_core::domain::OpKind::DiscardWorktreePatch,
                CompactString::const_new("Discard worktree changes"),
            )),
            move || async move {
                git_engine
                    .discard_worktree_paths(&repo_root, &tracked, &untracked)
                    .await
            },
        );
    }

    /// Applies a conflict resolution decision onto the active 4-buffer `ThreeWayMergeDocument`.
    pub fn resolve_conflict_region(&mut self, region_id: u32, resolution: ConflictResolution) {
        let diff_merge = Arc::clone(&self.host.diff_merge);
        let err_msg = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            let Some(session) = tab.conflict_session.as_mut() else {
                return;
            };
            diff_merge
                .resolve_region(&mut session.document, region_id, resolution)
                .err()
                .map(|e| e.to_string())
        };
        if let Some(message) = err_msg {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message,
            });
        }
    }

    /// Applies direct hand editing on `doc.output` during 3-way conflict resolution.
    pub fn edit_conflict_output(
        &mut self,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) {
        let diff_merge = Arc::clone(&self.host.diff_merge);
        let err_msg = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            let Some(session) = tab.conflict_session.as_mut() else {
                return;
            };
            diff_merge
                .apply_output_edit(&mut session.document, byte_range, replacement_utf8)
                .err()
                .map(|e| e.to_string())
        };
        if let Some(message) = err_msg {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message,
            });
        }
    }

    /// Exports, saves, and stages the currently resolved 3-way merge file.
    pub fn save_and_mark_conflict_resolved(&mut self, stage_after_save: bool) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(session) = tab.conflict_session.as_ref() else {
            return;
        };
        spawn_save_conflict_resolution(
            &self.host,
            tab.repo_id,
            tab.repo_root.clone(),
            Arc::clone(&tab.mvcc_store),
            Arc::clone(&tab.internal_ops),
            session.document.clone(),
            stage_after_save,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Resolves a non-UTF-8 conflicted file by accepting raw `Ours` or `Theirs` bytes verbatim
    /// without lossy UTF-8 transcoding, then optionally stages it via Tier 2.
    pub fn resolve_non_utf8_conflict_side(
        &mut self,
        path: RepoRelPath,
        side: RawConflictSide,
        stage_after_save: bool,
    ) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        spawn_resolve_raw_conflict_side(
            &self.host,
            tab.repo_id,
            tab.repo_root.clone(),
            Arc::clone(&tab.mvcc_store),
            Arc::clone(&tab.internal_ops),
            path,
            side,
            stage_after_save,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Commits staged changes using the composer fields and Tier 3 `git commit`.
    pub fn commit_staged(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let message = tab.full_commit_message();
        if message.trim().is_empty() {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message: "Commit subject cannot be empty".to_string(),
            });
            return;
        }
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let opts = make_commit_options(
            message,
            tab.commit_amend,
            tab.commit_gpg_sign,
            tab.commit_no_verify,
        );
        let undo_kind = if tab.commit_amend {
            branvio_core::domain::OpKind::Amend
        } else {
            branvio_core::domain::OpKind::Commit
        };
        let undo_summary = CompactString::from(format!(
            "{}: {}",
            if tab.commit_amend { "Amend" } else { "Commit" },
            tab.commit_subject.trim()
        ));
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((undo_kind, undo_summary)),
            move || async move { git_engine.commit(&repo_root, opts).await },
        );
    }

    /// Executes `git checkout` on `target_rev` (or creates + checks out `create_branch`).
    pub fn checkout_revision(&mut self, target_rev: String, create_branch: Option<String>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);
        let summary = CompactString::from(format!("Checkout {target_rev}"));

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((branvio_core::domain::OpKind::Checkout, summary)),
            move || async move {
                git_engine
                    .checkout_ref(&repo_root, &target_rev, create_branch.as_deref())
                    .await
            },
        );
    }

    /// Executes `git merge <target_rev>` via Tier 3.
    pub fn merge_revision(&mut self, target_rev: String, no_ff: bool) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);
        let opts = make_merge_options(&target_rev, no_ff);
        let summary = CompactString::from(format!("Merge {target_rev}"));

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((branvio_core::domain::OpKind::Merge, summary)),
            move || async move { git_engine.merge(&repo_root, opts).await },
        );
    }

    /// Executes `git rebase <onto_rev>` via Tier 3.
    pub fn rebase_onto(&mut self, onto_rev: String) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);
        let summary = CompactString::from(format!("Rebase onto {onto_rev}"));

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((branvio_core::domain::OpKind::Rebase, summary)),
            move || async move { git_engine.rebase(&repo_root, &onto_rev).await },
        );
    }

    /// Validates active `ActiveModal::InteractiveRebase` plan and triggers Tier 3 `git rebase -i`.
    pub fn execute_active_interactive_rebase(&mut self) {
        let payload_res = match &self.modal {
            ActiveModal::InteractiveRebase { planner, .. } => planner.to_payload(),
            _ => return,
        };
        let payload = match payload_res {
            Ok(p) => p,
            Err(err) => {
                if let ActiveModal::InteractiveRebase {
                    validation_error, ..
                } = &mut self.modal
                {
                    *validation_error = Some(err.to_string());
                }
                return;
            }
        };

        self.modal = ActiveModal::None;
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((
                branvio_core::domain::OpKind::InteractiveRebase,
                CompactString::const_new("Interactive Rebase"),
            )),
            move || async move { git_engine.interactive_rebase(&repo_root, payload).await },
        );
    }

    /// Executes `git cherry-pick <commits...>`.
    pub fn cherry_pick_commits(&mut self, commits: Vec<GitOid>) {
        if commits.is_empty() {
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);
        let summary = CompactString::from(format!("Cherry-pick {} commit(s)", commits.len()));

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((branvio_core::domain::OpKind::CherryPick, summary)),
            move || async move { git_engine.cherry_pick(&repo_root, &commits).await },
        );
    }

    /// Executes `git reset --soft|--mixed|--hard|--keep <target_rev>`.
    pub fn reset_to_revision(&mut self, target_rev: String, mode: ResetMode) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);
        let summary = CompactString::from(format!("Reset {mode:?} -> {target_rev}"));

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            Some((branvio_core::domain::OpKind::Reset { mode }, summary)),
            move || async move { git_engine.reset(&repo_root, &target_rev, mode).await },
        );
    }

    /// Executes `git fetch` across all remotes (`prune = true`).
    pub fn fetch_all(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            None,
            move || async move { git_engine.fetch(&repo_root, None, true).await },
        );
    }

    /// Executes `git pull`.
    pub fn pull_current(&mut self, rebase: bool) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            None,
            move || async move { git_engine.pull(&repo_root, rebase, !rebase).await },
        );
    }

    /// Executes a regular (non-forced) `git push` for the current HEAD branch (or falls back to
    /// `"HEAD"` if detached).
    pub fn push_current(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let branch = current_head_branch(tab).unwrap_or_else(|| "HEAD".to_string());
        let repo_id = tab.repo_id;
        self.spawn_push(repo_id, branch, PushForce::None);
    }

    /// Starts the force-push flow for the current branch.
    ///
    /// Reads `origin/<branch>` once and, when it arrives, opens [`ActiveModal::ConfirmForcePush`]
    /// holding that OID as the lease value. Nothing is pushed until
    /// [`Self::confirm_force_push`] is called.
    pub fn request_force_push(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(branch) = current_head_branch(tab) else {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message: "Check out a branch before force pushing (HEAD is detached).".to_string(),
            });
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let git_engine = Arc::clone(&self.host.git_engine);
        let tx = self.event_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        self.host.tokio_handle.spawn(async move {
            let result = git_engine
                .remote_tracking_oid(&repo_root, DEFAULT_PUSH_REMOTE, &branch)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(PipelineEvent::ForcePushLeaseCaptured {
                repo_id,
                branch: CompactString::from(branch),
                result,
            });
            wakeup.notify_repo_updated(repo_id, mvcc_store.committed_epoch());
        });
    }

    /// Confirms the open [`ActiveModal::ConfirmForcePush`] and pushes.
    ///
    /// [`ForcePushChoice::WithLease`] uses exactly the OID captured when the modal opened.
    /// Does nothing if no force-push confirmation is open.
    pub fn confirm_force_push(&mut self, choice: ForcePushChoice) {
        let ActiveModal::ConfirmForcePush {
            repo_id,
            branch,
            expected_remote_oid,
        } = std::mem::replace(&mut self.modal, ActiveModal::None)
        else {
            return;
        };
        let force = match choice {
            ForcePushChoice::WithLease => PushForce::WithLease {
                expected_remote_oid,
            },
            ForcePushChoice::Force => PushForce::Force,
        };
        self.spawn_push(repo_id, branch.to_string(), force);
    }

    fn spawn_push(&mut self, repo_id: RepoId, branch: String, force: PushForce) {
        let Some(tab) = self.tabs.iter().find(|t| t.repo_id == repo_id) else {
            return;
        };
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            None,
            move || async move {
                git_engine
                    .push(&repo_root, DEFAULT_PUSH_REMOTE, &branch, force, false)
                    .await
            },
        );
    }

    /// Creates a new linked worktree via Tier 3 `git worktree add`.
    pub fn create_worktree(
        &mut self,
        target_dir: HostPath,
        branch_or_oid: String,
        create_new_branch: bool,
    ) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let repo_id = tab.repo_id;
        let repo_root = tab.repo_root.clone();
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        let git_engine = Arc::clone(&self.host.git_engine);

        self.spawn_cli_command(
            repo_id,
            mvcc_store,
            internal_ops,
            None,
            move || async move {
                git_engine
                    .worktree_add(&repo_root, &target_dir, &branch_or_oid, create_new_branch)
                    .await
            },
        );
    }

    /// Starts or updates an active branch/commit drag-and-drop hover and requests Tier 2 merge prediction.
    pub fn update_drag_hover(
        &mut self,
        source_label: CompactString,
        source_oid: Option<GitOid>,
        target_label: Option<CompactString>,
        target_oid: Option<GitOid>,
    ) {
        let already_requested = self
            .drag_preview
            .as_ref()
            .is_some_and(|prev| prev.source_oid == source_oid && prev.target_oid == target_oid);

        let prediction = if already_requested {
            self.drag_preview
                .as_ref()
                .and_then(|p| p.prediction.clone())
        } else {
            None
        };

        self.drag_preview = Some(DragDropPreviewState {
            source_label,
            source_oid,
            target_label,
            target_oid,
            prediction,
        });

        if !already_requested
            && let (Some(src), Some(dst), Some(tab)) = (source_oid, target_oid, self.active_tab())
            && src != dst
        {
            spawn_drag_drop_prediction(
                &self.host,
                tab.repo_id,
                tab.repo_root.clone(),
                Arc::clone(&tab.mvcc_store),
                src,
                dst,
                self.event_tx.clone(),
                Arc::clone(&self.wakeup),
            );
        }
    }

    /// Completes a drop gesture by promoting `drag_preview` into [`ActiveModal::DropBranchAction`].
    pub fn finish_drag_drop_popover(&mut self) {
        if let Some(preview) = self.drag_preview.take()
            && preview.target_label.is_some()
        {
            self.modal = ActiveModal::DropBranchAction {
                preview,
                merge_no_ff: false,
            };
        }
    }

    /// Updates graph filter / pickaxe query on active tab and schedules background refresh.
    pub fn set_search_filter(&mut self, query: String, is_pickaxe: bool, is_regex: bool) {
        let Some(repo_id) = self.active_repo_id else {
            return;
        };
        if let Some(tab) = self.tab_mut(repo_id) {
            tab.search_query = query.clone();
            tab.search_is_pickaxe = is_pickaxe;
            tab.search_is_regex = is_regex;
            let trimmed = query.trim();
            if is_pickaxe {
                tab.filter_spec.message_query = None;
                tab.filter_spec.pickaxe_code_query =
                    (!trimmed.is_empty()).then(|| trimmed.to_string());
            } else {
                tab.filter_spec.pickaxe_code_query = None;
                tab.filter_spec.message_query =
                    (!trimmed.is_empty()).then(|| CompactString::from(trimmed));
            }
        }
        self.trigger_refresh(repo_id, ALL_DIRTY_SCOPES);
    }

    /// Builds Command Palette entries & fuzzy ranked matches from the current workspace tab state.
    pub fn search_command_palette(&self, query: &str) -> (Vec<PaletteEntry>, Vec<PaletteMatch>) {
        let (refs, wt_paths) = match self.active_tab() {
            Some(tab) => (
                tab.snapshot.refs.as_ref(),
                tab.worktrees
                    .iter()
                    .map(|w| w.path.as_path().display().to_string())
                    .collect(),
            ),
            None => (&[][..], Vec::new()),
        };
        let entries = build_palette_entries(refs, &wt_paths);
        let matches = search_palette_entries(query, &entries, 32);
        (entries, matches)
    }

    /// Executes a selected [`PaletteActionKind`] and closes the palette modal.
    pub fn execute_palette_action(&mut self, action: PaletteActionKind) {
        self.modal = ActiveModal::None;
        match action {
            PaletteActionKind::OpenRepositoryDialog => {
                self.modal = ActiveModal::OpenRepository {
                    path_draft: String::new(),
                    error: None,
                };
            }
            PaletteActionKind::OpenCreateBranchDialog => {
                let base_rev = self
                    .active_tab()
                    .map(|t| t.snapshot.head_label.to_string())
                    .unwrap_or_else(|| "HEAD".to_string());
                self.modal = ActiveModal::CreateBranch {
                    base_rev,
                    branch_name_draft: String::new(),
                    checkout_immediately: true,
                    error: None,
                };
            }
            PaletteActionKind::OpenCreateWorktreeDialog => {
                self.modal = ActiveModal::CreateWorktree {
                    target_dir_draft: String::new(),
                    branch_or_oid_draft: String::new(),
                    create_new_branch: true,
                    error: None,
                };
            }
            PaletteActionKind::StageAll => self.stage_all_working_tree(),
            PaletteActionKind::UnstageAll => self.unstage_all_staged(),
            PaletteActionKind::CommitStaged => self.commit_staged(),
            PaletteActionKind::FetchAll => self.fetch_all(),
            PaletteActionKind::PullCurrent { rebase } => self.pull_current(rebase),
            PaletteActionKind::PushCurrent => self.push_current(),
            PaletteActionKind::ForcePushCurrent => self.request_force_push(),
            PaletteActionKind::ToggleAiCloudUpload => {
                self.ai_privacy.allow_cloud_upload = !self.ai_privacy.allow_cloud_upload;
                let message = if self.ai_privacy.allow_cloud_upload {
                    "Allow sending diffs to cloud AI: on (secrets are masked before upload)"
                } else {
                    "Allow sending diffs to cloud AI: off"
                };
                self.status_banner = Some(StatusBanner {
                    is_error: false,
                    message: message.to_string(),
                });
            }
            PaletteActionKind::StashPush => {
                if let Some(tab) = self.active_tab() {
                    let repo_id = tab.repo_id;
                    let repo_root = tab.repo_root.clone();
                    let mvcc_store = Arc::clone(&tab.mvcc_store);
                    let internal_ops = Arc::clone(&tab.internal_ops);
                    let git_engine = Arc::clone(&self.host.git_engine);
                    self.spawn_cli_command(
                        repo_id,
                        mvcc_store,
                        internal_ops,
                        Some((
                            branvio_core::domain::OpKind::StashPush,
                            CompactString::const_new("Stash working tree"),
                        )),
                        move || async move { git_engine.stash_push(&repo_root, None, true).await },
                    );
                }
            }
            PaletteActionKind::StashPop => {
                if let Some(tab) = self.active_tab() {
                    let repo_id = tab.repo_id;
                    let repo_root = tab.repo_root.clone();
                    let mvcc_store = Arc::clone(&tab.mvcc_store);
                    let internal_ops = Arc::clone(&tab.internal_ops);
                    let git_engine = Arc::clone(&self.host.git_engine);
                    self.spawn_cli_command(
                        repo_id,
                        mvcc_store,
                        internal_ops,
                        Some((
                            branvio_core::domain::OpKind::StashPop,
                            CompactString::const_new("Pop stash@{0}"),
                        )),
                        move || async move { git_engine.stash_pop(&repo_root, 0, true).await },
                    );
                }
            }
            PaletteActionKind::OpenInteractiveRebase => {
                self.open_interactive_rebase_for_active_branch();
            }
            PaletteActionKind::SwitchDiffViewMode(mode) => self.diff_view_mode = mode,
            PaletteActionKind::ToggleFileListLayout => {
                self.file_list_layout = match self.file_list_layout {
                    FileListLayoutMode::FlatList => FileListLayoutMode::FolderTree,
                    FileListLayoutMode::FolderTree => FileListLayoutMode::FlatList,
                };
            }
            PaletteActionKind::ToggleThemeAppearance => {
                self.theme = BranvioTheme::for_appearance(self.theme.appearance.toggled());
            }
            PaletteActionKind::ToggleBottomDrawer => {
                self.bottom_drawer_open = !self.bottom_drawer_open;
            }
            PaletteActionKind::FocusSearchFilter => {}
            PaletteActionKind::UndoLastOperation => self.undo_last_operation(false),
            PaletteActionKind::RedoLastOperation => self.redo_last_operation(false),
            PaletteActionKind::CheckoutRevision(rev) => {
                self.checkout_revision(rev.to_string(), None);
            }
            PaletteActionKind::OpenWorktreeTab(path_str) => {
                if let Err(err) = self.open_repository_tab(Path::new(&path_str)) {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: err,
                    });
                }
            }
        }
    }

    /// Opens Interactive Rebase modal populated with recent linear commits from the active snapshot.
    pub fn open_interactive_rebase_for_active_branch(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let onto_rev = tab
            .snapshot
            .refs
            .iter()
            .find(|b| matches!(b.name.as_str(), "main" | "master" | "origin/main") && !b.is_head)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| CompactString::const_new("HEAD~3"));

        let slice_len = tab.snapshot.graph.commits.len().min(6);
        let planner = InteractiveRebasePlanner::from_newest_first_nodes(
            onto_rev,
            &tab.snapshot.graph.commits[..slice_len],
        );
        self.modal = ActiveModal::InteractiveRebase {
            planner,
            editing_message_row: None,
            message_draft: String::new(),
            validation_error: None,
        };
    }

    /// Opens Interactive Rebase modal with an explicit `onto_rev` and newest-first commit nodes.
    pub fn open_interactive_rebase_with_commits(
        &mut self,
        onto_rev: CompactString,
        commits_newest_first: &[branvio_core::domain::CommitNode],
    ) {
        let planner =
            InteractiveRebasePlanner::from_newest_first_nodes(onto_rev, commits_newest_first);
        self.modal = ActiveModal::InteractiveRebase {
            planner,
            editing_message_row: None,
            message_draft: String::new(),
            validation_error: None,
        };
    }

    /// Drains all completed background [`PipelineEvent`]s in FIFO order and updates tab mirrors.
    ///
    /// Returns the number of events processed in this batch.
    pub fn drain_events(&mut self) -> usize {
        let mut count = 0;
        let mut repos_to_refresh = HashSet::new();

        while let Ok(ev) = self.event_rx.try_recv() {
            count += 1;
            self.handle_pipeline_event(ev, &mut repos_to_refresh);
        }

        for repo_id in repos_to_refresh {
            self.trigger_refresh(repo_id, ALL_DIRTY_SCOPES);
        }
        count
    }

    fn handle_pipeline_event(&mut self, ev: PipelineEvent, repos_to_refresh: &mut HashSet<RepoId>) {
        match ev {
            PipelineEvent::RefreshCompleted {
                repo_id,
                epoch,
                summary,
                worktrees,
                submodules,
                pagination,
                revwalk_exhausted,
            } => {
                let mut reload_delta: Option<(HostPath, Arc<MvccRepoStateStore>, FileDelta)> = None;
                let mut watcher_setup = None;
                let has_watcher = self.host.repo_watcher.is_some();
                if let Some(tab) = self.tab_mut(repo_id) {
                    let latest = tab.mvcc_store.load_snapshot();
                    if epoch >= latest.epoch {
                        if tab.watch_handle.is_none() && has_watcher {
                            watcher_setup = Some((
                                summary.workdir_root.clone(),
                                summary.git_dir.clone(),
                                summary.common_git_dir.clone(),
                                Arc::clone(&tab.mvcc_store),
                                Arc::clone(&tab.internal_ops),
                            ));
                        }
                        tab.snapshot = latest;
                        tab.summary = Some(summary);
                        tab.worktrees = worktrees;
                        tab.submodules = submodules;
                        tab.pagination = pagination;
                        tab.revwalk_exhausted = revwalk_exhausted;
                        tab.is_loading_more = false;
                        tab.active_refresh_cancel = None;

                        if let Some(prev_delta) = tab.selected_delta.clone()
                            && prev_delta.area != DeltaArea::CommitDiff
                        {
                            let wt = &tab.snapshot.working_tree;
                            let pool: &[FileDelta] = match prev_delta.area {
                                DeltaArea::Staged => &wt.staged,
                                DeltaArea::Unstaged => &wt.unstaged,
                                DeltaArea::Conflicted => &wt.conflicted,
                                DeltaArea::CommitDiff => &[],
                            };
                            if let Some(updated) = pool
                                .iter()
                                .find(|d| d.new_path == prev_delta.new_path)
                                .cloned()
                            {
                                tab.selected_delta = Some(updated.clone());
                                tab.selected_diff_lines.clear();
                                if updated.area != DeltaArea::Conflicted {
                                    reload_delta = Some((
                                        tab.repo_root.clone(),
                                        Arc::clone(&tab.mvcc_store),
                                        updated,
                                    ));
                                }
                            } else {
                                tab.selected_delta = None;
                                tab.active_diff_patch = None;
                                tab.diff_notice = None;
                                tab.selected_diff_lines.clear();
                            }
                        }
                    }
                }
                if let Some((repo_root, mvcc_store, delta)) = reload_delta {
                    spawn_load_delta_diff(
                        &self.host,
                        repo_id,
                        repo_root,
                        mvcc_store,
                        delta,
                        self.diff_options.clone(),
                        self.event_tx.clone(),
                        Arc::clone(&self.wakeup),
                    );
                }
                if let Some((workdir_root, git_dir, common_git_dir, mvcc_store, internal_ops)) =
                    watcher_setup
                {
                    let handle = phase5_worker::start_repo_watcher(
                        &self.host,
                        repo_id,
                        &workdir_root,
                        &git_dir,
                        &common_git_dir,
                        mvcc_store,
                        internal_ops,
                        self.event_tx.clone(),
                        Arc::clone(&self.wakeup),
                    );
                    if let Some(tab) = self.tab_mut(repo_id) {
                        tab.watch_handle = handle;
                    }
                }
            }
            PipelineEvent::HistoryChunkAppended {
                repo_id,
                epoch,
                revwalk_exhausted,
            } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    let latest = tab.mvcc_store.load_snapshot();
                    if epoch >= latest.epoch {
                        tab.snapshot = latest;
                        tab.revwalk_exhausted = revwalk_exhausted;
                        tab.is_loading_more = false;
                    }
                }
            }
            PipelineEvent::RefreshFailed { repo_id, error } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.is_loading_more = false;
                    tab.active_refresh_cancel = None;
                }
                self.status_banner = Some(StatusBanner {
                    is_error: true,
                    message: error,
                });
            }
            PipelineEvent::CommitDetailsLoaded {
                repo_id,
                oid,
                details,
            } => {
                if let Some(tab) = self.tab_mut(repo_id)
                    && tab.selected_commit_oid == Some(oid)
                {
                    tab.selected_commit_details = Some(details);
                }
            }
            PipelineEvent::DeltaDiffLoaded {
                repo_id,
                delta,
                result,
            } => {
                if let Some(tab) = self.tab_mut(repo_id)
                    && tab
                        .selected_delta
                        .as_ref()
                        .is_some_and(|d| d.new_path == delta.new_path && d.area == delta.area)
                {
                    match result {
                        Ok(patch) => {
                            tab.active_diff_patch = Some(patch);
                            tab.diff_notice = None;
                        }
                        Err(notice) => {
                            tab.active_diff_patch = None;
                            tab.diff_notice = Some(notice);
                        }
                    }
                }
            }
            PipelineEvent::ConflictSessionLoaded { repo_id, result } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    match result {
                        Ok(session) => {
                            tab.conflict_session = Some(session);
                            tab.diff_notice = None;
                        }
                        Err(ConflictLoadError::UnsupportedEncoding {
                            path,
                            ours_label,
                            theirs_label,
                        }) => {
                            tab.conflict_session = None;
                            tab.diff_notice = Some(DiffPreviewNotice::UnsupportedEncoding {
                                path,
                                conflict_labels: Some((ours_label, theirs_label)),
                            });
                        }
                        Err(ConflictLoadError::Message(err)) => {
                            tab.conflict_session = None;
                            tab.diff_notice = Some(DiffPreviewNotice::Message(err));
                        }
                    }
                }
            }
            PipelineEvent::DragDropPredictionReady {
                source_oid,
                target_oid,
                prediction,
                ..
            } => {
                if let Some(ref mut preview) = self.drag_preview
                    && preview.source_oid == Some(source_oid)
                    && preview.target_oid == Some(target_oid)
                {
                    preview.prediction = Some(prediction.clone());
                }
                if let ActiveModal::DropBranchAction {
                    ref mut preview, ..
                } = self.modal
                    && preview.source_oid == Some(source_oid)
                    && preview.target_oid == Some(target_oid)
                {
                    preview.prediction = Some(prediction);
                }
            }
            PipelineEvent::CliStartedCancel { repo_id, cancel } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.active_cli_cancel = Some(cancel);
                }
            }
            PipelineEvent::CliEvent { repo_id, event } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    match event {
                        CliStreamEvent::Started { command_summary } => {
                            tab.cli_running = true;
                            tab.cli_transfer_progress = None;
                            tab.push_cli_log(false, format!("$ {command_summary}"));
                        }
                        CliStreamEvent::StdoutLine(line) => tab.push_cli_log(false, line),
                        CliStreamEvent::StderrLine(line) => tab.push_cli_log(true, line),
                        CliStreamEvent::TransferProgress { phase, percent } => {
                            tab.cli_transfer_progress = Some((phase, percent));
                        }
                        CliStreamEvent::HookStarted { hook_name } => {
                            tab.push_cli_log(false, format!("[hook] running {hook_name}..."));
                        }
                        CliStreamEvent::Finished { exit_code, success } => {
                            tab.cli_running = false;
                            tab.active_cli_cancel = None;
                            tab.cli_transfer_progress = None;
                            tab.push_cli_log(
                                !success,
                                format!("[exit] code={exit_code:?} success={success}"),
                            );
                            if success {
                                tab.commit_subject.clear();
                                tab.commit_body.clear();
                                tab.commit_amend = false;
                            } else {
                                self.status_banner = Some(StatusBanner {
                                    is_error: true,
                                    message: format!("Git command failed (exit={exit_code:?})"),
                                });
                            }
                            repos_to_refresh.insert(repo_id);
                        }
                    }
                }
            }
            PipelineEvent::IndexMutationFinished {
                repo_id,
                error,
                success_banner,
            } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.is_mutating_index = false;
                }
                if let Some(err) = error {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: err,
                    });
                } else if let Some(msg) = success_banner {
                    self.status_banner = Some(StatusBanner {
                        is_error: false,
                        message: msg,
                    });
                }
                repos_to_refresh.insert(repo_id);
            }
            PipelineEvent::ConflictSaved {
                repo_id,
                path,
                error,
            } => {
                if let Some(err) = error {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: err,
                    });
                } else {
                    if let Some(tab) = self.tab_mut(repo_id) {
                        tab.conflict_session = None;
                        tab.diff_notice = None;
                        tab.ai_conflict_advice.clear();
                    }
                    self.status_banner = Some(StatusBanner {
                        is_error: false,
                        message: format!("Resolved conflict in {}", path.as_str()),
                    });
                    repos_to_refresh.insert(repo_id);
                }
            }
            PipelineEvent::UndoRedoFinished {
                repo_id,
                is_redo,
                result,
            } => match result {
                Ok(outcome) => {
                    self.status_banner = Some(StatusBanner {
                        is_error: false,
                        message: outcome.status_message.to_string(),
                    });
                    repos_to_refresh.insert(repo_id);
                }
                Err(UndoRedoUiError::RemoteConfirmationRequired { remote_branches }) => {
                    self.modal = ActiveModal::ConfirmRewritePushedUndo {
                        is_redo,
                        remote_branches,
                    };
                }
                Err(UndoRedoUiError::Message(err)) => {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: err,
                    });
                }
            },
            PipelineEvent::WatcherTriggered { repo_id, .. } => {
                repos_to_refresh.insert(repo_id);
            }
            PipelineEvent::TerminalSessionReady {
                repo_id,
                session,
                title,
            } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.terminal_title = title;
                    tab.terminal_snapshot = session.snapshot_viewport().ok();
                    tab.terminal_session = Some(session);
                }
            }
            PipelineEvent::TerminalDamaged { repo_id } => {
                if let Some(tab) = self.tab_mut(repo_id)
                    && let Some(ref session) = tab.terminal_session
                    && let Ok(snap) = session.snapshot_viewport()
                {
                    if !snap.title.is_empty() {
                        tab.terminal_title = snap.title.clone();
                    }
                    tab.terminal_snapshot = Some(snap);
                }
            }
            PipelineEvent::PullRequestsLoaded {
                repo_id,
                remote_label,
                result,
            } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.pull_requests_loading = false;
                    tab.pull_requests_remote = Some(remote_label);
                    match result {
                        Ok(prs) => {
                            tab.pull_requests = prs;
                            tab.pull_requests_error = None;
                        }
                        Err(err) => {
                            tab.pull_requests_error = Some(err);
                        }
                    }
                }
            }
            PipelineEvent::PullRequestCreated { repo_id, result } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.pull_requests_loading = false;
                }
                match result {
                    Ok(pr) => {
                        let pr_num = pr.number;
                        if let Some(tab) = self.tab_mut(repo_id) {
                            tab.pull_requests.insert(0, pr);
                        }
                        self.modal = ActiveModal::None;
                        self.status_banner = Some(StatusBanner {
                            is_error: false,
                            message: format!("Created Pull Request #{pr_num}"),
                        });
                    }
                    Err(err) => {
                        if let ActiveModal::CreatePullRequest { ref mut error, .. } = self.modal {
                            *error = Some(err.clone());
                        }
                        self.status_banner = Some(StatusBanner {
                            is_error: true,
                            message: err,
                        });
                    }
                }
            }
            PipelineEvent::AiCommitSuggested { repo_id, result } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.ai_commit_busy = false;
                    match result {
                        Ok(suggestion) => {
                            tab.commit_subject = suggestion.subject;
                            tab.commit_body = suggestion.body.unwrap_or_default();
                        }
                        Err(err) => {
                            self.handle_ai_error(repo_id, AiFeature::SuggestCommitMessage, &err);
                        }
                    }
                }
            }
            PipelineEvent::AiConflictSuggested {
                repo_id,
                region_id,
                result,
            } => match result {
                Ok(advice) => {
                    if let Some(tab) = self.tab_mut(repo_id) {
                        tab.ai_conflict_advice.insert(region_id, advice);
                    }
                }
                Err(err) => {
                    self.handle_ai_error(repo_id, AiFeature::SuggestConflictResolution, &err);
                }
            },
            PipelineEvent::UndoCaptureFailed {
                repo_id,
                summary,
                reason,
                pending,
            } => {
                if let Some(tab) = self.tab_mut(repo_id) {
                    tab.cli_running = false;
                    tab.is_mutating_index = false;
                }
                if matches!(self.modal, ActiveModal::None) {
                    self.pending_unprotected = Some((repo_id, pending));
                    self.modal = ActiveModal::ConfirmContinueWithoutUndo {
                        repo_id,
                        summary,
                        reason,
                    };
                } else {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: format!(
                            "{summary} was not run: Branvio could not save an undo point ({reason})."
                        ),
                    });
                }
                repos_to_refresh.insert(repo_id);
            }
            PipelineEvent::ForcePushLeaseCaptured {
                repo_id,
                branch,
                result,
            } => match result {
                Ok(expected_remote_oid) => {
                    // The OID was read asynchronously: only open the confirmation if the user is
                    // still looking at the same repository, on the same branch, with no other
                    // dialog open. Otherwise the lease could target a branch the user no longer
                    // means to force push.
                    let still_current = self.active_repo_id == Some(repo_id)
                        && self
                            .active_tab()
                            .and_then(current_head_branch)
                            .is_some_and(|head| head == branch.as_str())
                        && matches!(self.modal, ActiveModal::None);
                    if still_current {
                        self.modal = ActiveModal::ConfirmForcePush {
                            repo_id,
                            branch,
                            expected_remote_oid,
                        };
                    } else {
                        self.status_banner = Some(StatusBanner {
                            is_error: true,
                            message: "Force push cancelled: the branch changed".to_string(),
                        });
                    }
                }
                Err(err) => {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: format!(
                            "Could not read {DEFAULT_PUSH_REMOTE}/{branch} to prepare a safe force push: {err}"
                        ),
                    });
                }
            },
        }
    }

    /// Surfaces an AI Copilot failure; cloud-consent refusals also open the consent prompt.
    fn handle_ai_error(&mut self, repo_id: RepoId, feature: AiFeature, err: &AiUiError) {
        self.status_banner = Some(StatusBanner {
            is_error: true,
            message: err.user_message(feature),
        });
        if let AiUiError::CloudUploadNotAllowed { provider } = err
            && matches!(self.modal, ActiveModal::None)
        {
            self.modal = ActiveModal::ConfirmAiCloudUpload {
                repo_id,
                provider: provider.clone(),
                feature,
            };
        }
    }

    /// Allows cloud AI uploads for one repository (from the consent prompt) and closes it.
    pub fn allow_cloud_ai_for_repository(&mut self, repo_id: RepoId) {
        if matches!(self.modal, ActiveModal::ConfirmAiCloudUpload { .. }) {
            self.modal = ActiveModal::None;
        }
        let Some(tab) = self.tabs.iter().find(|t| t.repo_id == repo_id) else {
            return;
        };
        let repo_root = tab.repo_root.clone();
        let name = tab.display_name.clone();
        self.ai_privacy.cloud_upload_repos.insert(repo_root);
        self.status_banner = Some(StatusBanner {
            is_error: false,
            message: format!(
                "Cloud AI allowed for {name} (secrets are masked before upload). Run the AI action again."
            ),
        });
    }

    /// Cancels the active Tier 3 CLI operation if one is running on the active tab.
    pub fn cancel_active_cli(&mut self) {
        if let Some(tab) = self.active_tab_mut()
            && let Some(cancel) = tab.active_cli_cancel.take()
        {
            cancel.cancel();
        }
    }

    /// Executes Time-Machine Undo for the most recent recorded operation.
    pub fn undo_last_operation(&mut self, allow_rewrite_pushed: bool) {
        self.trigger_undo_or_redo(false, allow_rewrite_pushed);
    }

    /// Executes Time-Machine Redo for the most recently undone operation.
    pub fn redo_last_operation(&mut self, allow_rewrite_pushed: bool) {
        self.trigger_undo_or_redo(true, allow_rewrite_pushed);
    }

    fn trigger_undo_or_redo(&mut self, is_redo: bool, allow_rewrite_pushed: bool) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        phase5_worker::spawn_undo_or_redo(
            &self.host,
            tab.repo_id,
            tab.repo_root.clone(),
            Arc::clone(&tab.mvcc_store),
            Arc::clone(&tab.internal_ops),
            is_redo,
            allow_rewrite_pushed,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Opens the bottom drawer on the integrated Terminal tab and lazily spawns an OS shell PTY.
    pub fn open_or_focus_terminal(&mut self) {
        self.bottom_drawer_open = true;
        self.bottom_drawer_tab = BottomDrawerTab::Terminal;
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.terminal_session.is_some() {
            return;
        }
        phase5_worker::spawn_open_terminal(
            &self.host,
            tab.repo_id,
            tab.repo_root.clone(),
            Arc::clone(&tab.mvcc_store),
            terminal_pty::PtySize {
                cols: 110,
                rows: 16,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Writes a shell command line followed by the Enter key (`\n`; `\r` on Windows ConPTY) to
    /// the active tab's PTY stdin.
    ///
    /// Destructive `git` commands (see [`destructive_guard`]) are held back and open
    /// [`ActiveModal::ConfirmDestructiveTerminalCommand`] instead; they are sent only after
    /// [`Self::confirm_destructive_terminal_command`].
    pub fn send_terminal_command_line(&mut self, line: &str) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let current_branch = current_head_branch(tab);
        if let Some(found) =
            destructive_guard::detect_destructive_git_command(line, current_branch.as_deref())
        {
            self.modal = ActiveModal::ConfirmDestructiveTerminalCommand {
                repo_id: tab.repo_id,
                command_line: line.to_string(),
                command: found.command,
                reason: found.reason,
            };
            return;
        }
        self.write_terminal_line(line);
    }

    /// Sends the command held by [`ActiveModal::ConfirmDestructiveTerminalCommand`] after the user
    /// confirmed it. Does nothing if that modal is not open; if its tab is no longer active the
    /// command is not sent and a banner explains why.
    pub fn confirm_destructive_terminal_command(&mut self) {
        let ActiveModal::ConfirmDestructiveTerminalCommand {
            repo_id,
            command_line,
            ..
        } = std::mem::replace(&mut self.modal, ActiveModal::None)
        else {
            return;
        };
        if self.active_repo_id == Some(repo_id) {
            self.write_terminal_line(&command_line);
        } else {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message: "Command not sent: the active repository changed before it was confirmed."
                    .to_string(),
            });
        }
    }

    /// Runs the mutation held by [`ActiveModal::ConfirmContinueWithoutUndo`] **without** an undo
    /// point, after the user explicitly chose to continue. Does nothing if that modal is not open.
    pub fn confirm_continue_without_undo(&mut self) {
        let ActiveModal::ConfirmContinueWithoutUndo { repo_id, .. } =
            std::mem::replace(&mut self.modal, ActiveModal::None)
        else {
            return;
        };
        let Some((pending_repo, pending)) = self.pending_unprotected.take() else {
            return;
        };
        let Some(tab) = self.tab_mut(repo_id).filter(|_| pending_repo == repo_id) else {
            self.status_banner = Some(StatusBanner {
                is_error: true,
                message: "Operation cancelled: its repository tab is no longer open.".to_string(),
            });
            return;
        };
        let mvcc_store = Arc::clone(&tab.mvcc_store);
        let internal_ops = Arc::clone(&tab.internal_ops);
        match pending {
            pipeline_worker::PendingUnprotectedMutation::Cli { launch } => {
                self.spawn_cli_command(repo_id, mvcc_store, internal_ops, None, launch);
            }
            pipeline_worker::PendingUnprotectedMutation::Patch {
                repo_root,
                patch,
                selections,
                direction,
            } => {
                tab.is_mutating_index = true;
                spawn_patch_mutation(
                    &self.host,
                    repo_id,
                    repo_root,
                    mvcc_store,
                    internal_ops,
                    *patch,
                    selections,
                    direction,
                    true,
                    self.event_tx.clone(),
                    Arc::clone(&self.wakeup),
                );
            }
        }
    }

    /// Cancels the mutation held by [`ActiveModal::ConfirmContinueWithoutUndo`] (the default).
    pub fn cancel_continue_without_undo(&mut self) {
        if matches!(self.modal, ActiveModal::ConfirmContinueWithoutUndo { .. }) {
            self.modal = ActiveModal::None;
        }
        if self.pending_unprotected.take().is_some() {
            self.status_banner = Some(StatusBanner {
                is_error: false,
                message: "Cancelled: nothing was changed.".to_string(),
            });
        }
    }

    fn write_terminal_line(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(TERMINAL_ENTER_KEY);
        self.send_terminal_bytes(bytes);
    }

    /// Writes raw UTF-8/VT bytes to the active tab's PTY stdin.
    pub fn send_terminal_bytes(&mut self, bytes: Vec<u8>) {
        let Some(session) = self.active_tab().and_then(|t| t.terminal_session.clone()) else {
            return;
        };
        self.host.tokio_handle.spawn(async move {
            let _ = session.write_bytes(&bytes).await;
        });
    }

    /// Closes the active tab's embedded PTY shell session cleanly.
    pub fn close_terminal_session(&mut self) {
        let session = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            tab.terminal_snapshot = None;
            tab.terminal_session.take()
        };
        if let Some(sess) = session {
            self.host.tokio_handle.spawn(async move {
                let _ = sess.close().await;
            });
        }
    }

    /// Fetches open Pull/Merge Requests from `origin` via `forge-client`.
    pub fn refresh_pull_requests(&mut self) {
        let (repo_id, repo_root, mvcc_store) = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            tab.pull_requests_loading = true;
            tab.pull_requests_error = None;
            (
                tab.repo_id,
                tab.repo_root.clone(),
                Arc::clone(&tab.mvcc_store),
            )
        };
        phase5_worker::spawn_load_pull_requests(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Opens the Create Pull Request modal pre-populated with current HEAD branch.
    pub fn open_create_pull_request_modal(&mut self) {
        let head_branch = self
            .active_tab()
            .map(|t| t.snapshot.head_label.clone())
            .unwrap_or_else(|| CompactString::const_new("main"));
        self.modal = ActiveModal::CreatePullRequest {
            head_branch,
            base_branch_draft: "main".to_string(),
            title_draft: String::new(),
            body_draft: String::new(),
            draft: false,
            error: None,
        };
    }

    /// Submits a Pull/Merge Request creation request asynchronously via `forge-client`.
    pub fn create_pull_request(&mut self, input: forge_client::CreatePullRequestInput) {
        let (repo_id, repo_root, mvcc_store) = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            tab.pull_requests_loading = true;
            (
                tab.repo_id,
                tab.repo_root.clone(),
                Arc::clone(&tab.mvcc_store),
            )
        };
        phase5_worker::spawn_create_pull_request(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            input,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Fetches and checks out Pull Request `#pr_number` head branch (`FETCH_HEAD` fallback).
    pub fn checkout_pull_request(&mut self, pr_number: u64) {
        let Some(pr) = self
            .active_tab()
            .and_then(|t| t.pull_requests.iter().find(|p| p.number == pr_number))
            .cloned()
        else {
            return;
        };
        self.checkout_revision(pr.head_branch.to_string(), None);
    }

    /// Invokes AI Copilot to draft a Conventional Commit subject + body from staged changes.
    pub fn generate_ai_commit_message(&mut self) {
        let (repo_id, repo_root, mvcc_store, recent) = {
            let Some(tab) = self.active_tab_mut() else {
                return;
            };
            tab.ai_commit_busy = true;
            let recent: Vec<String> = tab
                .snapshot
                .graph
                .commits
                .iter()
                .take(8)
                .map(|c| c.summary.to_string())
                .collect();
            (
                tab.repo_id,
                tab.repo_root.clone(),
                Arc::clone(&tab.mvcc_store),
                recent,
            )
        };
        let allow_cloud_upload = self.ai_privacy.allows_cloud_upload_for(&repo_root);
        phase5_worker::spawn_ai_commit_message(
            &self.host,
            repo_id,
            repo_root,
            mvcc_store,
            recent,
            allow_cloud_upload,
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Requests AI Copilot 3-way conflict resolution advice for `region_id` in the active conflict session.
    pub fn suggest_ai_conflict_resolution(&mut self, region_id: u32) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(ref session) = tab.conflict_session else {
            return;
        };
        let doc = &session.document;
        let Some(region) = doc.regions.iter().find(|r| r.region_id == region_id) else {
            return;
        };
        let base_lines = doc.extract_region_lines(region, false, true).join("");
        let ours_lines = doc.extract_region_lines(region, true, false).join("");
        let theirs_lines = doc.extract_region_lines(region, false, false).join("");
        let base_opt = (!base_lines.is_empty()).then_some(base_lines);

        phase5_worker::spawn_ai_conflict_advice(
            &self.host,
            tab.repo_id,
            Arc::clone(&tab.mvcc_store),
            doc.path.clone(),
            region_id,
            base_opt,
            ours_lines,
            theirs_lines,
            self.ai_privacy.allows_cloud_upload_for(&tab.repo_root),
            self.event_tx.clone(),
            Arc::clone(&self.wakeup),
        );
    }

    /// Applies an AI-generated resolution recommendation onto conflict region `region_id`.
    pub fn apply_ai_conflict_resolution(&mut self, region_id: u32) {
        let advice = self
            .active_tab()
            .and_then(|t| t.ai_conflict_advice.get(&region_id).cloned());
        let Some(advice) = advice else {
            return;
        };
        match advice.suggested_strategy {
            ConflictResolution::ManualEdit | ConflictResolution::LineLevelSelection(_) => {
                let diff_merge = Arc::clone(&self.host.diff_merge);
                let err = {
                    let Some(tab) = self.active_tab_mut() else {
                        return;
                    };
                    let Some(ref mut session) = tab.conflict_session else {
                        return;
                    };
                    let doc = &mut session.document;
                    if let Some(region) = doc.regions.iter().find(|r| r.region_id == region_id) {
                        let start = doc.output.line_to_byte(
                            (region.output_line_range.start as usize).min(doc.output.len_lines()),
                        );
                        let end = doc.output.line_to_byte(
                            (region.output_line_range.end as usize).min(doc.output.len_lines()),
                        );
                        let mut replacement = advice.proposed_code.clone();
                        if end < doc.output.len_bytes()
                            && !replacement.is_empty()
                            && !replacement.ends_with('\n')
                        {
                            replacement.push_str(doc.line_ending);
                        }
                        diff_merge
                            .apply_output_edit(doc, start..end, &replacement)
                            .err()
                            .map(|e| e.to_string())
                    } else {
                        None
                    }
                };
                if let Some(msg) = err {
                    self.status_banner = Some(StatusBanner {
                        is_error: true,
                        message: msg,
                    });
                }
            }
            strategy => self.resolve_conflict_region(region_id, strategy),
        }
    }

    fn spawn_cli_command<F, Fut>(
        &mut self,
        repo_id: RepoId,
        mvcc_store: Arc<MvccRepoStateStore>,
        internal_ops: Arc<InternalOpController>,
        undo_op: Option<(branvio_core::domain::OpKind, CompactString)>,
        launch: F,
    ) where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_engine::GitEngineResult<CliCommandHandle>>
            + Send
            + 'static,
    {
        let repo_root_for_undo = self.tab_mut(repo_id).map(|t| t.repo_root.clone());
        if let Some(tab) = self.tab_mut(repo_id) {
            if let Some(prev) = tab.active_cli_cancel.take() {
                prev.cancel();
            }
            tab.cli_running = true;
        }
        let tx = self.event_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        let rt_handle = self.host.tokio_handle.clone();
        let undo_engine = self.host.undo_engine.clone();
        let oplog_store = self.host.oplog_store.clone();

        self.host.tokio_handle.spawn(async move {
            let undo_ctx = if let (Some(engine), Some((kind, summary)), Some(root)) =
                (undo_engine, undo_op, repo_root_for_undo)
            {
                match engine
                    .capture_before_mutation(&root, kind, summary.clone())
                    .await
                {
                    Ok(op_id) => Some(UndoTxnContext {
                        engine,
                        store: oplog_store,
                        repo_root: root,
                        op_id,
                    }),
                    Err(err) => {
                        // Fail closed: never run the mutation silently without an undo point.
                        // Hand it back to the presenter, which asks the user first.
                        let launch: pipeline_worker::BoxedCliLaunch =
                            Box::new(move || Box::pin(launch()));
                        let _ = tx.send(PipelineEvent::UndoCaptureFailed {
                            repo_id,
                            summary: summary.to_string(),
                            reason: err.to_string(),
                            pending: pipeline_worker::PendingUnprotectedMutation::Cli { launch },
                        });
                        wakeup.notify_repo_updated(repo_id, mvcc_store.committed_epoch());
                        return;
                    }
                }
            } else {
                None
            };

            match launch().await {
                Ok(handle) => {
                    let cancel = pump_cli_handle(
                        &rt_handle,
                        repo_id,
                        Arc::clone(&mvcc_store),
                        internal_ops,
                        handle,
                        undo_ctx,
                        tx.clone(),
                        Arc::clone(&wakeup),
                    );
                    let _ = tx.send(PipelineEvent::CliStartedCancel { repo_id, cancel });
                    let epoch = mvcc_store.committed_epoch();
                    wakeup.notify_repo_updated(repo_id, epoch);
                }
                Err(err) => {
                    if let Some(ref ctx) = undo_ctx {
                        // Also deletes the entry's snapshot pin ref.
                        let _ = ctx
                            .engine
                            .discard_incomplete(&ctx.repo_root, ctx.op_id)
                            .await;
                    }
                    let _ = tx.send(PipelineEvent::CliEvent {
                        repo_id,
                        event: CliStreamEvent::StderrLine(err.to_string()),
                    });
                    let _ = tx.send(PipelineEvent::CliEvent {
                        repo_id,
                        event: CliStreamEvent::Finished {
                            exit_code: None,
                            success: false,
                        },
                    });
                    let epoch = mvcc_store.committed_epoch();
                    wakeup.notify_repo_updated(repo_id, epoch);
                }
            }
        });
    }

    /// Exposes active interactive rebase planner row manipulation for UI/tests.
    pub fn update_rebase_row_action(&mut self, row_index: usize, action: RebaseAction) {
        if let ActiveModal::InteractiveRebase {
            planner,
            validation_error,
            ..
        } = &mut self.modal
        {
            planner.set_action(row_index, action);
            *validation_error = planner.validate().err().map(|e| e.to_string());
        }
    }

    /// Asynchronously pumps incoming [`PipelineEvent`]s in strict FIFO order until `predicate`
    /// returns `true` or `timeout` elapses (primarily used by headless E2E tests).
    pub async fn wait_until<P>(
        &mut self,
        timeout: std::time::Duration,
        mut predicate: P,
    ) -> Result<(), String>
    where
        P: FnMut(&Self) -> bool,
    {
        self.drain_events();
        if predicate(self) {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timed out waiting for BranvioPresenter condition".to_string());
            }
            let remaining = deadline - now;
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    return Err("timed out waiting for BranvioPresenter condition".to_string());
                }
                ev_opt = self.event_rx.recv() => {
                    match ev_opt {
                        Some(first) => {
                            let mut repos_to_refresh = HashSet::new();
                            self.handle_pipeline_event(first, &mut repos_to_refresh);
                            while let Ok(ev) = self.event_rx.try_recv() {
                                self.handle_pipeline_event(ev, &mut repos_to_refresh);
                            }
                            for repo_id in repos_to_refresh {
                                self.trigger_refresh(repo_id, ALL_DIRTY_SCOPES);
                            }
                            if predicate(self) {
                                return Ok(());
                            }
                        }
                        None => return Err("presenter event channel closed".to_string()),
                    }
                }
            }
        }
    }
}

/// Maps [`RepoOperationState`] into a user-visible badge string when a merge/rebase is in progress.
pub const fn operation_state_banner(state: RepoOperationState) -> Option<&'static str> {
    match state {
        RepoOperationState::Clean => None,
        RepoOperationState::Merging => Some("MERGE IN PROGRESS"),
        RepoOperationState::Rebasing => Some("REBASE IN PROGRESS"),
        RepoOperationState::InteractiveRebasing => Some("INTERACTIVE REBASE"),
        RepoOperationState::CherryPicking => Some("CHERRY-PICK IN PROGRESS"),
        RepoOperationState::Reverting => Some("REVERT IN PROGRESS"),
        RepoOperationState::Bisecting => Some("BISECT IN PROGRESS"),
    }
}
