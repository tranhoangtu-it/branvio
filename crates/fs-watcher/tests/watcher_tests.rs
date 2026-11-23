#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::ids::RepoId;
use branvio_core::paths::HostPath;
use branvio_core::pipeline::{DirtyScopeFlags, InternalOpController};
use fs_watcher::{NotifyRepoWatcher, RepoWatcher, WatcherConfig, WatcherEvent};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn init_git_repo(root: &std::path::Path) {
    let out = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    std::fs::write(root.join(".gitignore"), "*.ignored\nignored_dir/\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "hello\n").unwrap();
    let _ = Command::new("git")
        .args(["add", ".gitignore", "tracked.txt"])
        .current_dir(root)
        .output()
        .unwrap();
    let _ = Command::new("git")
        .args(["config", "user.name", "WatcherTest"])
        .current_dir(root)
        .output()
        .unwrap();
    let _ = Command::new("git")
        .args(["config", "user.email", "watch@branvio.rs"])
        .current_dir(root)
        .output()
        .unwrap();
    let _ = Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(root)
        .output()
        .unwrap();
}

#[tokio::test]
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
    while rx.try_recv().is_ok() {}

    // 1. Write ignored files -> MUST NOT emit any Dirty event
    std::fs::create_dir_all(root.join("ignored_dir")).unwrap();
    std::fs::write(root.join("ignored_dir").join("artifact.bin"), b"123").unwrap();
    std::fs::write(root.join("build.ignored"), b"ignored").unwrap();

    let no_event = tokio::time::timeout(Duration::from_millis(280), rx.recv()).await;
    assert!(
        no_event.is_err(),
        "gitignored writes must not emit watcher events, got {:?}",
        no_event
    );

    // 2. Write tracked worktree file -> MUST emit Dirty { WORKTREE }
    std::fs::write(root.join("tracked.txt"), "modified\n").unwrap();
    let mut saw_worktree = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
            && let WatcherEvent::Dirty { scopes, .. } = ev
            && scopes.union(DirtyScopeFlags::WORKTREE) == scopes
        {
            saw_worktree = true;
            break;
        }
    }
    assert!(saw_worktree, "tracked worktree edit must emit WORKTREE");

    // 3. Touch .git/refs/heads/feature -> MUST emit REFS_OR_HEAD rapidly
    let head_oid = std::fs::read_to_string(root.join(".git/refs/heads/main")).unwrap();
    std::fs::write(root.join(".git/refs/heads/feature"), head_oid).unwrap();
    let mut saw_refs = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
            && let WatcherEvent::Dirty { scopes, .. } = ev
            && scopes.has_refs_or_head()
        {
            saw_refs = true;
            break;
        }
    }
    assert!(saw_refs, ".git/refs update must emit REFS_OR_HEAD");
}

#[tokio::test]
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
    while rx.try_recv().is_ok() {}

    // Burst 20 un-ignored file creations back-to-back
    for i in 0..20 {
        std::fs::write(root.join(format!("burst_{i}.txt")), format!("line {i}\n")).unwrap();
    }

    let mut saw_storm = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
            && let WatcherEvent::Dirty {
                scopes,
                storm_coalesced,
                ..
            } = ev
            && scopes.union(DirtyScopeFlags::WORKTREE) == scopes
            && storm_coalesced
        {
            saw_storm = true;
            break;
        }
    }
    assert!(
        saw_storm,
        "burst exceeding threshold should coalesce with storm_coalesced = true"
    );
}
