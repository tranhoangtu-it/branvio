//! Traits and debounced event stream contracts for repository watching.

use branvio_core::ids::RepoId;
use branvio_core::paths::HostPath;
use branvio_core::pipeline::{DirtyScopeFlags, InternalOpController};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

/// Result alias for `fs-watcher`.
pub type WatcherResult<T> = Result<T, WatcherError>;

/// Errors returned while starting or maintaining a filesystem watch handle.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// OS watcher (`inotify`, `FSEvents`, `ReadDirectoryChangesW`) initialization failed.
    #[error("filesystem watcher backend error: {0}")]
    Notify(#[from] notify::Error),

    /// Watch target path does not exist or is unreadable.
    #[error("invalid watch target '{0}'")]
    InvalidTarget(String),
}

/// Adaptive debounce and storm-protection parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// Fast debounce window for `.git/` metadata changes (`HEAD`, `index`, `refs`).
    pub git_internal_debounce: Duration,
    /// Baseline debounce window for working-tree file modifications.
    pub worktree_debounce: Duration,
    /// Expanded debounce window engaged automatically during bulk storms (`npm install` / `cargo build`).
    pub storm_backoff_debounce: Duration,
    /// Event burst count inside `worktree_debounce` that activates storm backoff mode.
    pub storm_threshold_events: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            git_internal_debounce: Duration::from_millis(35),
            worktree_debounce: Duration::from_millis(120),
            storm_backoff_debounce: Duration::from_millis(650),
            storm_threshold_events: 150,
        }
    }
}

/// Coalesced repository refresh signal emitted downstream to `git-engine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherEvent {
    /// Coalesced dirty flags indicating which repository layers need refreshing.
    Dirty {
        /// Repository tab ID.
        repo_id: RepoId,
        /// Combined scopes (`WORKTREE`, `INDEX`, `REFS_OR_HEAD`).
        scopes: DirtyScopeFlags,
        /// `true` if emitted after storm throttling cooled down.
        storm_coalesced: bool,
    },
    /// OS watcher overflowed its kernel ring buffer (e.g. `inotify` queue overflow);
    /// downstream pipeline must perform a full status + refs reconciliation.
    KernelRescanRequired {
        /// Repository tab ID.
        repo_id: RepoId,
    },
}

/// Active watch session handle (dropping it cleanly tears down OS watchers and background tasks).
pub trait RepoWatchHandle: Send + Sync {
    /// Repository ID bound to this watch handle.
    fn repo_id(&self) -> RepoId;
}

/// Starts dual-stream `.git/` + `.gitignore`-filtered working tree watchers.
pub trait RepoWatcher: Send + Sync {
    /// Spawns watchers on `repo_root` and `git_dir` (supporting external linked-worktree gitdirs),
    /// wiring suppression through `op_controller` and emitting coalesced events into `mpsc::Receiver`.
    fn watch_repository(
        &self,
        repo_id: RepoId,
        repo_root: &HostPath,
        git_dir: &HostPath,
        common_git_dir: &HostPath,
        op_controller: Arc<InternalOpController>,
        config: WatcherConfig,
    ) -> WatcherResult<(Box<dyn RepoWatchHandle>, mpsc::Receiver<WatcherEvent>)>;
}
