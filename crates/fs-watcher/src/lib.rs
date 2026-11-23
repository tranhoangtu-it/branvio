//! Dual-stream adaptive filesystem watcher separating `.git/` metadata updates
//! from `.gitignore`-filtered working-tree changes.

pub mod classifier;
pub mod traits;
pub mod watcher;

pub use classifier::{GitInternalChangeKind, WatcherPathClass, classify_repo_event_path};
pub use traits::{
    RepoWatchHandle, RepoWatcher, WatcherConfig, WatcherError, WatcherEvent, WatcherResult,
};
pub use watcher::NotifyRepoWatcher;
