#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::ids::RepoId;
use branvio_core::paths::HostPath;
use branvio_core::pipeline::{DirtyScopeFlags, InternalOpController};
use fs_watcher::{NotifyRepoWatcher, RepoWatcher, WatcherConfig, WatcherEvent};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Upper bound for any single expected watcher event; generous for slow CI runners, but a broken
/// watcher fails the test with a clear message instead of hanging the job.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

fn git(root: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_git_repo(root: &std::path::Path) {
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "user.name", "WatcherTest"]);
    git(root, &["config", "user.email", "watch@branvio.rs"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join(".gitignore"), "*.ignored\nignored_dir/\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "hello\n").unwrap();
    git(root, &["add", ".gitignore", "tracked.txt"]);
    git(root, &["commit", "-m", "init"]);
}

/// Waits (bounded by [`EVENT_TIMEOUT`]) for the first `Dirty` event satisfying `pred`, skipping
/// unrelated events. Panics with `what` on timeout or if the watcher channel closes.
async fn expect_dirty(
    rx: &mut mpsc::Receiver<WatcherEvent>,
    what: &str,
    pred: impl Fn(DirtyScopeFlags, bool) -> bool,
) {
    let wait = async {
        loop {
            match rx.recv().await {
                Some(WatcherEvent::Dirty {
                    scopes,
                    storm_coalesced,
                    ..
                }) if pred(scopes, storm_coalesced) => return,
                Some(_) => {}
                None => panic!("watcher channel closed while waiting for {what}"),
            }
        }
    };
    if tokio::time::timeout(EVENT_TIMEOUT, wait).await.is_err() {
        panic!("timed out after {EVENT_TIMEOUT:?} waiting for {what}");
    }
}

/// Drains startup noise: waits until the watcher has been quiet for `quiet`, bounded overall.
async fn settle(rx: &mut mpsc::Receiver<WatcherEvent>, quiet: Duration) {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    while tokio::time::timeout(quiet, rx.recv()).await.is_ok() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "watcher never went quiet after registration (event feedback loop?)"
        );
    }
}

// Multi-threaded so the test's own timeouts keep firing even if a watcher worker misbehaves and
// monopolizes an executor thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watches_worktree_ignores_gitignored_and_separates_git_internal_refs() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    init_git_repo(root);

    let watcher = NotifyRepoWatcher::new(tokio::runtime::Handle::current());
    let repo_id = RepoId(42);
    let root_hp = HostPath::canonicalize_existing(root).unwrap();
    let git_hp = HostPath::canonicalize_existing(root.join(".git")).unwrap();
    let op_ctl = Arc::new(InternalOpController::default());

    let cfg = WatcherConfig {
        git_internal_debounce: Duration::from_millis(30),
        worktree_debounce: Duration::from_millis(80),
        storm_backoff_debounce: Duration::from_millis(250),
        storm_threshold_events: 8,
    };

    let (_handle, mut rx) = watcher
        .watch_repository(
            repo_id,
            &root_hp,
            &git_hp,
            &git_hp,
            Arc::clone(&op_ctl),
            cfg,
        )
        .unwrap();

    // Let OS FSEvents / inotify settle after initial registration
    tokio::time::sleep(Duration::from_millis(200)).await;
    settle(&mut rx, Duration::from_millis(300)).await;

    // 1. Pure reads (inotify IN_OPEN / IN_CLOSE_NOWRITE) and writes to ignored files -> MUST NOT
    //    emit any Dirty event. Reading `.gitignore` / `.git/info/exclude` previously re-triggered a
    //    matcher rebuild that re-opened them: an endless feedback loop on Linux.
    let _ = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    let _ = std::fs::read_to_string(root.join("tracked.txt")).unwrap();
    let _ = std::fs::read_to_string(root.join(".git/HEAD")).unwrap();
    std::fs::create_dir_all(root.join("ignored_dir")).unwrap();
    std::fs::write(root.join("ignored_dir").join("artifact.bin"), b"123").unwrap();
    std::fs::write(root.join("build.ignored"), b"ignored").unwrap();

    let no_event = tokio::time::timeout(Duration::from_millis(280), rx.recv()).await;
    assert!(
        no_event.is_err(),
        "gitignored writes must not emit watcher events, got {no_event:?}"
    );

    // 2. Write tracked worktree file -> MUST emit Dirty { WORKTREE }
    std::fs::write(root.join("tracked.txt"), "modified\n").unwrap();
    expect_dirty(
        &mut rx,
        "WORKTREE dirty event for tracked edit",
        |scopes, _| scopes.union(DirtyScopeFlags::WORKTREE) == scopes,
    )
    .await;

    // 3. Touch .git/refs/heads/feature -> MUST emit REFS_OR_HEAD rapidly
    let head_oid = std::fs::read_to_string(root.join(".git/refs/heads/main")).unwrap();
    std::fs::write(root.join(".git/refs/heads/feature"), head_oid).unwrap();
    expect_dirty(
        &mut rx,
        "REFS_OR_HEAD dirty event for .git/refs update",
        |scopes, _| scopes.has_refs_or_head(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storm_burst_coalesces_into_storm_flagged_dirty_event() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    init_git_repo(root);

    let watcher = NotifyRepoWatcher::new(tokio::runtime::Handle::current());
    let root_hp = HostPath::canonicalize_existing(root).unwrap();
    let git_hp = HostPath::canonicalize_existing(root.join(".git")).unwrap();
    let cfg = WatcherConfig {
        git_internal_debounce: Duration::from_millis(25),
        worktree_debounce: Duration::from_millis(70),
        storm_backoff_debounce: Duration::from_millis(160),
        storm_threshold_events: 5,
    };

    let (_handle, mut rx) = watcher
        .watch_repository(
            RepoId(7),
            &root_hp,
            &git_hp,
            &git_hp,
            Arc::new(InternalOpController::default()),
            cfg,
        )
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    settle(&mut rx, Duration::from_millis(300)).await;

    // Burst 20 un-ignored file creations back-to-back
    for i in 0..20 {
        std::fs::write(root.join(format!("burst_{i}.txt")), format!("line {i}\n")).unwrap();
    }

    expect_dirty(
        &mut rx,
        "storm-coalesced WORKTREE dirty event after a 20-file burst",
        |scopes, storm_coalesced| {
            scopes.union(DirtyScopeFlags::WORKTREE) == scopes && storm_coalesced
        },
    )
    .await;
}
