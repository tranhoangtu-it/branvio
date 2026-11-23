//! Snapshot pin lifecycle (`refs/branvio/undo/...`): pins are released when their journal entry
//! goes away, never overwrite each other across journal resets, and capture fails closed when
//! the staged state cannot be snapshotted. Also round-trips deletions and names with spaces.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::domain::{OpKind, ResetMode};
use branvio_core::paths::HostPath;
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use git_engine::{BranvioGitEngine, CliCommandHandle, CliStreamEvent, GitCliExecutor};
use oplog_undo::{
    BranvioUndoEngine, MAX_ENTRIES_PER_REPO, OpLogError, OpLogStore, RedbOpLogStore,
    UNDO_SNAPSHOT_REF_PREFIX, UndoRedoEngine, compute_repo_root_hash, snapshot_pin_ref,
};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn git_cmd(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "BranvioTester")
        .env("GIT_AUTHOR_EMAIL", "tester@branvio.rs")
        .env("GIT_COMMITTER_NAME", "BranvioTester")
        .env("GIT_COMMITTER_EMAIL", "tester@branvio.rs")
        .output()
        .expect("git invocation should succeed")
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = git_cmd(cwd, args);
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

struct Fixture {
    _repo_dir: TempDir,
    _db_dirs: Vec<TempDir>,
    root: HostPath,
    engine: Arc<BranvioGitEngine>,
    env: Arc<HydratedGitEnv>,
    store: Arc<RedbOpLogStore>,
    undo: BranvioUndoEngine,
}

impl Fixture {
    fn path(&self) -> &Path {
        self.root.as_path()
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.path().join(rel)).ok()
    }

    fn git(&self, args: &[&str]) -> String {
        run_git(self.path(), args)
    }

    fn pins(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/branvio/undo"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn entries(&self) -> Vec<branvio_core::domain::OpLogEntry> {
        self.store
            .list_recent(compute_repo_root_hash(&self.root), usize::MAX)
            .unwrap()
    }

    /// A second, empty journal on the same repository (simulates a deleted/reset database).
    fn fresh_journal(&mut self) -> (Arc<RedbOpLogStore>, BranvioUndoEngine) {
        let db_dir = TempDir::new().unwrap();
        let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
        self._db_dirs.push(db_dir);
        let undo =
            BranvioUndoEngine::new(store.clone(), self.engine.clone(), Arc::clone(&self.env));
        (store, undo)
    }

    async fn capture_reset_hard(&self) -> u64 {
        self.undo
            .capture_before_mutation(
                &self.root,
                OpKind::Reset {
                    mode: ResetMode::Hard,
                },
                CompactString::from("Reset --hard HEAD"),
            )
            .await
            .unwrap()
    }

    async fn reset_hard_head(&self) {
        let cli = self
            .engine
            .reset(&self.root, "HEAD", ResetMode::Hard)
            .await
            .unwrap();
        finish_cli(cli).await;
    }
}

fn init_fixture() -> Fixture {
    let repo_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    run_git(root, &["init", "-b", "main"]);
    // Deterministic line endings: Windows runners ship `core.autocrlf=true` in system config.
    run_git(root, &["config", "core.autocrlf", "false"]);
    run_git(root, &["config", "user.name", "BranvioTester"]);
    run_git(root, &["config", "user.email", "tester@branvio.rs"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("app.txt"), "v1\n").unwrap();
    std::fs::write(root.join("gone.txt"), "to be deleted\n").unwrap();
    std::fs::create_dir_all(root.join("dir with space")).unwrap();
    std::fs::write(root.join("dir with space").join("file a.txt"), "orig\n").unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-m", "initial"]);

    let db_dir = TempDir::new().unwrap();
    let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = Arc::new(BranvioGitEngine::new(Arc::clone(&env), None));
    let undo = BranvioUndoEngine::new(store.clone(), engine.clone(), Arc::clone(&env));
    Fixture {
        root: HostPath::canonicalize_existing(root).unwrap(),
        _repo_dir: repo_dir,
        _db_dirs: vec![db_dir],
        engine,
        env,
        store,
        undo,
    }
}

async fn finish_cli(mut handle: CliCommandHandle) {
    while let Some(ev) = next_cli_event(&mut handle.events).await {
        if let CliStreamEvent::Finished { success, .. } = ev {
            assert!(success, "cli operation should succeed");
            break;
        }
    }
}

/// Parses `status --porcelain -z` into entries such as `" D gone.txt"`.
fn status_entries(fx: &Fixture) -> Vec<String> {
    let out = git_cmd(
        fx.path(),
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    );
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn pin_is_deleted_when_incomplete_entry_is_discarded() {
    let fx = init_fixture();
    fx.write("app.txt", "dirty\n");
    let op_id = fx.capture_reset_hard().await;
    let entry = fx.entries().into_iter().find(|e| e.op_id == op_id).unwrap();
    let pin = snapshot_pin_ref(&entry).expect("dirty worktree must be snapshotted");
    assert_eq!(fx.pins(), vec![pin]);

    fx.undo.discard_incomplete(&fx.root, op_id).await.unwrap();
    assert!(fx.entries().is_empty());
    assert!(
        fx.pins().is_empty(),
        "pin must be released: {:?}",
        fx.pins()
    );
}

#[tokio::test]
async fn pin_of_dropped_redo_tail_is_released() {
    let fx = init_fixture();
    fx.write("app.txt", "dirty one\n");
    let first = fx.capture_reset_hard().await;
    fx.reset_hard_head().await;
    fx.undo
        .finalize_after_mutation(&fx.root, first)
        .await
        .unwrap();
    fx.undo.undo(&fx.root, false).await.unwrap();
    assert_eq!(fx.read("app.txt").as_deref(), Some("dirty one\n"));
    // Undone entry is still redoable, so its pin stays.
    assert_eq!(fx.pins().len(), 1);

    // A new forward mutation drops the redo tail, which must release the old pin.
    fx.write("app.txt", "dirty two\n");
    let second = fx.capture_reset_hard().await;
    let entries = fx.entries();
    assert_eq!(entries.len(), 1, "redo tail must be dropped");
    assert_eq!(entries[0].op_id, second);
    assert_eq!(fx.pins(), vec![snapshot_pin_ref(&entries[0]).unwrap()]);
}

#[tokio::test]
async fn retention_cap_bounds_entries_and_pins() {
    let fx = init_fixture();
    let total = MAX_ENTRIES_PER_REPO + 5;
    for i in 0..total {
        fx.write("counter.txt", &format!("{i}\n"));
        let op_id = fx
            .undo
            .capture_before_mutation(
                &fx.root,
                OpKind::DiscardWorktreePatch,
                CompactString::from(format!("op {i}")),
            )
            .await
            .unwrap();
        fx.undo
            .finalize_after_mutation(&fx.root, op_id)
            .await
            .unwrap();
    }
    let entries = fx.entries();
    assert_eq!(entries.len(), MAX_ENTRIES_PER_REPO);
    let mut expected: Vec<String> = entries.iter().filter_map(snapshot_pin_ref).collect();
    expected.sort();
    let mut pins = fx.pins();
    pins.sort();
    assert_eq!(pins.len(), MAX_ENTRIES_PER_REPO);
    assert_eq!(pins, expected);
}

#[tokio::test]
async fn fresh_journal_never_reuses_or_overwrites_an_existing_pin() {
    let mut fx = init_fixture();
    fx.write("app.txt", "first journal\n");
    let op_a = fx.capture_reset_hard().await;
    let entry_a = fx.entries().into_iter().find(|e| e.op_id == op_a).unwrap();
    let pin_a = snapshot_pin_ref(&entry_a).unwrap();
    let snap_a = entry_a.before.worktree_safety_stash_oid.unwrap().to_hex();

    // Same repository, empty journal: op ids restart at 1.
    let (store_b, undo_b) = fx.fresh_journal();
    fx.write("app.txt", "second journal\n");
    let op_b = undo_b
        .capture_before_mutation(
            &fx.root,
            OpKind::Reset {
                mode: ResetMode::Hard,
            },
            CompactString::from("Reset --hard HEAD"),
        )
        .await
        .unwrap();
    assert_eq!(op_a, op_b, "op ids restart with a fresh journal");
    let entry_b = store_b
        .list_recent(compute_repo_root_hash(&fx.root), 8)
        .unwrap()
        .into_iter()
        .find(|e| e.op_id == op_b)
        .unwrap();
    let pin_b = snapshot_pin_ref(&entry_b).unwrap();
    let snap_b = entry_b.before.worktree_safety_stash_oid.unwrap().to_hex();
    assert_ne!(pin_a, pin_b);
    assert_ne!(snap_a, snap_b);
    assert!(pin_b.starts_with(&format!("{UNDO_SNAPSHOT_REF_PREFIX}{op_b}-")));
    assert_eq!(fx.git(&["rev-parse", "--verify", &pin_b]), snap_b);
}

#[tokio::test]
async fn capture_fails_closed_on_unmerged_index() {
    let fx = init_fixture();
    fx.git(&["checkout", "-b", "theirs"]);
    fx.write("app.txt", "theirs\n");
    fx.git(&["commit", "-am", "theirs"]);
    fx.git(&["checkout", "main"]);
    fx.write("app.txt", "ours\n");
    fx.git(&["commit", "-am", "ours"]);
    let merge = git_cmd(fx.path(), &["merge", "theirs"]);
    assert!(!merge.status.success(), "merge should conflict");

    let res = fx
        .undo
        .capture_before_mutation(
            &fx.root,
            OpKind::Reset {
                mode: ResetMode::Hard,
            },
            CompactString::from("Reset --hard HEAD"),
        )
        .await;
    assert!(
        matches!(res, Err(OpLogError::SnapshotIncomplete { .. })),
        "expected SnapshotIncomplete, got {res:?}"
    );
    assert!(fx.entries().is_empty(), "no undo point may be recorded");
    assert!(fx.pins().is_empty());
}

#[tokio::test]
async fn deleted_tracked_files_and_names_with_spaces_round_trip() {
    let fx = init_fixture();
    std::fs::remove_file(fx.path().join("gone.txt")).unwrap();
    fx.write("dir with space/file a.txt", "edited\n");
    fx.write("my notes.txt", "untracked with space\n");
    fx.write("staged space.txt", "staged\n");
    fx.git(&["add", "staged space.txt"]);
    let mut status_before = status_entries(&fx);
    status_before.sort();

    let op_id = fx.capture_reset_hard().await;
    fx.reset_hard_head().await;
    fx.undo
        .finalize_after_mutation(&fx.root, op_id)
        .await
        .unwrap();
    assert_eq!(fx.read("gone.txt").as_deref(), Some("to be deleted\n"));
    assert_eq!(
        fx.read("dir with space/file a.txt").as_deref(),
        Some("orig\n")
    );
    assert_eq!(fx.read("staged space.txt"), None);

    fx.undo.undo(&fx.root, false).await.unwrap();
    assert_eq!(fx.read("gone.txt"), None, "deletion must be restored");
    assert_eq!(
        fx.read("dir with space/file a.txt").as_deref(),
        Some("edited\n")
    );
    assert_eq!(
        fx.read("my notes.txt").as_deref(),
        Some("untracked with space\n")
    );
    assert_eq!(fx.read("staged space.txt").as_deref(), Some("staged\n"));

    let mut status_after = status_entries(&fx);
    status_after.sort();
    assert_eq!(status_after, status_before);
    for expected in [
        " D gone.txt",
        " M dir with space/file a.txt",
        "?? my notes.txt",
        "A  staged space.txt",
    ] {
        assert!(
            status_after.iter().any(|s| s == expected),
            "missing {expected:?} in {status_after:?}"
        );
    }
}

/// Receives the next CLI stream event, failing fast instead of hanging CI if a `git` subprocess stalls.
async fn next_cli_event(
    events: &mut tokio::sync::mpsc::Receiver<git_engine::CliStreamEvent>,
) -> Option<git_engine::CliStreamEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(60), events.recv())
        .await
        .expect("timed out after 60s waiting for the next git CLI stream event")
}
