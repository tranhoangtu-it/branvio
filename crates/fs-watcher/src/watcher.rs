//! Cross-platform `notify` watcher implementation with `.gitignore` filtering,
//! dual-stream debouncing (`.git/` metadata vs worktree), and event-storm backoff.

use crate::classifier::{GitInternalChangeKind, WatcherPathClass, classify_repo_event_path};
use crate::traits::{
    RepoWatchHandle, RepoWatcher, WatcherConfig, WatcherError, WatcherEvent, WatcherResult,
};
use branvio_core::ids::RepoId;
use branvio_core::paths::HostPath;
use branvio_core::pipeline::{DirtyScopeFlags, InternalOpController};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

struct NotifyWatchHandle {
    repo_id: RepoId,
    _watcher: RecommendedWatcher,
    _stop_tx: oneshot::Sender<()>,
}

impl RepoWatchHandle for NotifyWatchHandle {
    fn repo_id(&self) -> RepoId {
        self.repo_id
    }
}

/// Adaptive [`RepoWatcher`] powered by `notify` + `ignore` + Tokio dual-deadline debouncing.
pub struct NotifyRepoWatcher {
    tokio_handle: tokio::runtime::Handle,
}

impl NotifyRepoWatcher {
    /// Creates a [`NotifyRepoWatcher`] spawning its coalescing worker onto `tokio_handle`.
    pub fn new(tokio_handle: tokio::runtime::Handle) -> Self {
        Self { tokio_handle }
    }
}

impl RepoWatcher for NotifyRepoWatcher {
    fn watch_repository(
        &self,
        repo_id: RepoId,
        repo_root: &HostPath,
        git_dir: &HostPath,
        common_git_dir: &HostPath,
        op_controller: Arc<InternalOpController>,
        config: WatcherConfig,
    ) -> WatcherResult<(Box<dyn RepoWatchHandle>, mpsc::Receiver<WatcherEvent>)> {
        let canon_repo_root = std::fs::canonicalize(repo_root.as_path()).map_err(|e| {
            WatcherError::InvalidTarget(format!("{}: {e}", repo_root.as_path().display()))
        })?;
        let canon_git_dir = normalize_host_path(git_dir.as_path());
        let canon_common_git_dir = normalize_host_path(common_git_dir.as_path());

        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        })?;

        watcher.watch(&canon_repo_root, RecursiveMode::Recursive)?;
        if canon_git_dir.exists() && !canon_git_dir.starts_with(&canon_repo_root) {
            watcher.watch(&canon_git_dir, RecursiveMode::Recursive)?;
        }
        if canon_common_git_dir != canon_git_dir
            && canon_common_git_dir.exists()
            && !canon_common_git_dir.starts_with(&canon_repo_root)
        {
            watcher.watch(&canon_common_git_dir, RecursiveMode::Recursive)?;
        }

        let (out_tx, out_rx) = mpsc::channel(64);
        let (stop_tx, stop_rx) = oneshot::channel();

        let worker_ctx = WatcherWorkerContext {
            repo_id,
            canon_repo_root,
            canon_git_dir,
            canon_common_git_dir,
            op_controller,
            config,
            raw_rx,
            out_tx,
            stop_rx,
        };
        self.tokio_handle.spawn(run_debounced_watcher(worker_ctx));

        let handle = NotifyWatchHandle {
            repo_id,
            _watcher: watcher,
            _stop_tx: stop_tx,
        };
        Ok((Box::new(handle), out_rx))
    }
}

struct WatcherWorkerContext {
    repo_id: RepoId,
    canon_repo_root: PathBuf,
    canon_git_dir: PathBuf,
    canon_common_git_dir: PathBuf,
    op_controller: Arc<InternalOpController>,
    config: WatcherConfig,
    raw_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    out_tx: mpsc::Sender<WatcherEvent>,
    stop_rx: oneshot::Receiver<()>,
}

struct DebounceAccumulators {
    git_scopes: DirtyScopeFlags,
    git_deadline: Option<Instant>,
    worktree_dirty: bool,
    worktree_deadline: Option<Instant>,
    burst_count: usize,
    storm_active: bool,
    kernel_rescan: bool,
}

impl Default for DebounceAccumulators {
    fn default() -> Self {
        Self {
            git_scopes: DirtyScopeFlags::NONE,
            git_deadline: None,
            worktree_dirty: false,
            worktree_deadline: None,
            burst_count: 0,
            storm_active: false,
            kernel_rescan: false,
        }
    }
}

async fn run_debounced_watcher(mut ctx: WatcherWorkerContext) {
    let mut matcher = build_root_gitignore(&ctx.canon_repo_root, &ctx.canon_common_git_dir);
    let mut acc = DebounceAccumulators::default();

    loop {
        let next_wake = match (acc.git_deadline, acc.worktree_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        tokio::select! {
            _ = &mut ctx.stop_rx => break,
            maybe_raw = ctx.raw_rx.recv() => {
                match maybe_raw {
                    Some(Ok(event)) => {
                        ingest_notify_event(&ctx, &mut matcher, &mut acc, event);
                        // Coalesce immediately available queued events before re-arming timers.
                        while let Ok(Ok(extra)) = ctx.raw_rx.try_recv() {
                            ingest_notify_event(&ctx, &mut matcher, &mut acc, extra);
                        }
                        if acc.kernel_rescan {
                            acc.kernel_rescan = false;
                            acc.git_scopes = DirtyScopeFlags::NONE;
                            acc.git_deadline = None;
                            acc.worktree_dirty = false;
                            acc.worktree_deadline = None;
                            acc.burst_count = 0;
                            acc.storm_active = false;
                            let _ = ctx.op_controller.record_or_coalesce(
                                DirtyScopeFlags::WORKTREE
                                    .union(DirtyScopeFlags::INDEX)
                                    .union(DirtyScopeFlags::REFS_OR_HEAD),
                            );
                            if !ctx.op_controller.is_mutating()
                                && ctx.out_tx.send(WatcherEvent::KernelRescanRequired { repo_id: ctx.repo_id }).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Some(Err(err)) => {
                        tracing::debug!(repo_id = ?ctx.repo_id, error = %err, "notify backend warning");
                    }
                    None => break,
                }
            }
            _ = sleep_until_opt(next_wake), if next_wake.is_some() => {
                let now = Instant::now();
                let mut emit_scopes = DirtyScopeFlags::NONE;
                let mut storm_coalesced = false;

                if acc.git_deadline.is_some_and(|d| d <= now) {
                    acc.git_deadline = None;
                    emit_scopes = emit_scopes.union(acc.git_scopes);
                    acc.git_scopes = DirtyScopeFlags::NONE;
                }
                if acc.worktree_deadline.is_some_and(|d| d <= now) {
                    acc.worktree_deadline = None;
                    if acc.worktree_dirty {
                        emit_scopes = emit_scopes.union(DirtyScopeFlags::WORKTREE);
                        acc.worktree_dirty = false;
                    }
                    storm_coalesced = acc.storm_active;
                    acc.storm_active = false;
                    acc.burst_count = 0;
                }

                if emit_scopes.any()
                    && let Some(allowed) = ctx.op_controller.record_or_coalesce(emit_scopes)
                    && allowed.any()
                    && ctx
                        .out_tx
                        .send(WatcherEvent::Dirty {
                            repo_id: ctx.repo_id,
                            scopes: allowed,
                            storm_coalesced,
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn ingest_notify_event(
    ctx: &WatcherWorkerContext,
    matcher: &mut Gitignore,
    acc: &mut DebounceAccumulators,
    event: Event,
) {
    if event.need_rescan() {
        acc.kernel_rescan = true;
        return;
    }

    let now = Instant::now();
    for raw_path in &event.paths {
        let canon = normalize_host_path(raw_path);
        let mut class = classify_repo_event_path(&canon, &ctx.canon_repo_root, &ctx.canon_git_dir);
        if class == WatcherPathClass::IgnoreEphemeral
            && ctx.canon_common_git_dir != ctx.canon_git_dir
        {
            class =
                classify_repo_event_path(&canon, &ctx.canon_repo_root, &ctx.canon_common_git_dir);
        }

        match class {
            WatcherPathClass::IgnoreEphemeral
            | WatcherPathClass::GitInternal(GitInternalChangeKind::Objects) => {}
            WatcherPathClass::GitInternal(kind) => {
                if kind == GitInternalChangeKind::Config
                    && canon.file_name().is_some_and(|n| n == "exclude")
                {
                    *matcher =
                        build_root_gitignore(&ctx.canon_repo_root, &ctx.canon_common_git_dir);
                }
                let scope = match kind {
                    GitInternalChangeKind::Head
                    | GitInternalChangeKind::Refs
                    | GitInternalChangeKind::OperationState => DirtyScopeFlags::REFS_OR_HEAD,
                    GitInternalChangeKind::Index => DirtyScopeFlags::INDEX,
                    GitInternalChangeKind::Config => {
                        DirtyScopeFlags::WORKTREE.union(DirtyScopeFlags::REFS_OR_HEAD)
                    }
                    GitInternalChangeKind::Objects => DirtyScopeFlags::NONE,
                };
                if scope.any() {
                    acc.git_scopes = acc.git_scopes.union(scope);
                    acc.git_deadline = Some(now + ctx.config.git_internal_debounce);
                }
            }
            WatcherPathClass::WorktreeFile => {
                if canon.file_name().is_some_and(|n| n == ".gitignore") {
                    *matcher =
                        build_root_gitignore(&ctx.canon_repo_root, &ctx.canon_common_git_dir);
                }
                let is_dir = canon.is_dir();
                if is_path_ignored(matcher, &ctx.canon_repo_root, &canon, is_dir) {
                    continue;
                }
                acc.worktree_dirty = true;
                acc.burst_count = acc.burst_count.saturating_add(1);
                if acc.burst_count >= ctx.config.storm_threshold_events {
                    acc.storm_active = true;
                }
                let delay = if acc.storm_active {
                    ctx.config.storm_backoff_debounce
                } else {
                    ctx.config.worktree_debounce
                };
                acc.worktree_deadline = Some(now + delay);
            }
        }
    }
}

fn build_root_gitignore(repo_root: &Path, common_git_dir: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(repo_root);
    let root_ignore = repo_root.join(".gitignore");
    if root_ignore.is_file() {
        let _ = builder.add(&root_ignore);
    }
    let exclude = common_git_dir.join("info").join("exclude");
    if exclude.is_file() {
        let _ = builder.add(&exclude);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn is_path_ignored(
    root_matcher: &Gitignore,
    repo_root: &Path,
    canon_path: &Path,
    is_dir: bool,
) -> bool {
    let Ok(rel) = canon_path.strip_prefix(repo_root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    if root_matcher
        .matched_path_or_any_parents(rel, is_dir)
        .is_ignore()
    {
        return true;
    }

    // Check nested subdirectory `.gitignore` files from repository root downward.
    let mut cur_dir = repo_root.to_path_buf();
    let components: Vec<_> = rel.components().collect();
    if components.len() > 1 {
        for comp in components.iter().take(components.len() - 1) {
            cur_dir.push(comp.as_os_str());
            let sub_gi = cur_dir.join(".gitignore");
            if sub_gi.is_file() {
                let mut builder = GitignoreBuilder::new(&cur_dir);
                let _ = builder.add(&sub_gi);
                if let Ok(sub_matcher) = builder.build()
                    && let Ok(sub_rel) = canon_path.strip_prefix(&cur_dir)
                    && sub_matcher
                        .matched_path_or_any_parents(sub_rel, is_dir)
                        .is_ignore()
                {
                    return true;
                }
            }
        }
    }

    false
}

/// Canonicalizes existing prefixes even when the leaf path has just been deleted or renamed away,
/// ensuring macOS `/var` -> `/private/var` symlinks never break prefix stripping.
fn normalize_host_path(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon_parent) = std::fs::canonicalize(cur) {
            let mut out = canon_parent;
            for comp in tail.into_iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => break,
        }
    }
    let mut clean = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    clean
}
