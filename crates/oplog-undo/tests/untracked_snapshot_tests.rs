//! Regression tests: Undo of destructive operations must restore untracked (non-ignored) files,
//! not only tracked changes, and capturing the safety snapshot must not touch the real index.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::domain::{OpKind, ResetMode};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use git_engine::{BranvioGitEngine, CliCommandHandle, CliStreamEvent, GitCliExecutor};
use oplog_undo::{BranvioUndoEngine, RedbOpLogStore, UNDO_SNAPSHOT_REF_PREFIX, UndoRedoEngine};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "BranvioTester")
        .env("GIT_AUTHOR_EMAIL", "tester@branvio.rs")
        .env("GIT_COMMITTER_NAME", "BranvioTester")
        .env("GIT_COMMITTER_EMAIL", "tester@branvio.rs")
        .output()
        .expect("git invocation should succeed");
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
    _db_dir: TempDir,
    root: HostPath,
    engine: Arc<BranvioGitEngine>,
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

    fn index_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path().join(".git").join("index")).unwrap()
    }
}

/// Repo with committed `app.txt` + `.gitignore` (ignores `*.log`) on `main`, and a branch
/// `other` that tracks `notes.txt` (colliding with an untracked file created later).
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
    std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    run_git(root, &["add", "app.txt", ".gitignore"]);
    run_git(root, &["commit", "-m", "initial"]);

    run_git(root, &["checkout", "-b", "other"]);
    std::fs::write(root.join("notes.txt"), "theirs\n").unwrap();
    run_git(root, &["add", "notes.txt"]);
    run_git(root, &["commit", "-m", "other adds notes"]);
    run_git(root, &["checkout", "main"]);

    let db_dir = TempDir::new().unwrap();
    let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = Arc::new(BranvioGitEngine::new(Arc::clone(&env), None));
    let undo = BranvioUndoEngine::new(store, engine.clone(), env);
    Fixture {
        root: HostPath::canonicalize_existing(root).unwrap(),
        _repo_dir: repo_dir,
        _db_dir: db_dir,
        engine,
        undo,
    }
}

/// Tracked modification, staged new file, untracked files (top-level + nested), ignored file.
fn make_dirty(fx: &Fixture) {
    fx.write("app.txt", "v1\nprecious tracked edit\n");
    fx.write("staged.txt", "staged new file\n");
    fx.git(&["add", "staged.txt"]);
    fx.write("notes.txt", "mine: untracked notes\n");
    fx.write("scratch/deep.txt", "nested untracked\n");
    fx.write("build.log", "ignored output\n");
}

async fn finish_cli(mut handle: CliCommandHandle) {
    while let Some(ev) = next_cli_event(&mut handle.events).await {
        if let CliStreamEvent::Finished { success, .. } = ev {
            assert!(success, "cli operation should succeed");
            break;
        }
    }
}

fn assert_dirty_state_restored(fx: &Fixture) {
    assert_eq!(
        fx.read("app.txt").as_deref(),
        Some("v1\nprecious tracked edit\n")
    );
    assert_eq!(
        fx.read("notes.txt").as_deref(),
        Some("mine: untracked notes\n")
    );
    assert_eq!(
        fx.read("scratch/deep.txt").as_deref(),
        Some("nested untracked\n")
    );
    assert_eq!(fx.read("staged.txt").as_deref(), Some("staged new file\n"));
    assert_eq!(fx.read("build.log").as_deref(), Some("ignored output\n"));

    // Index state restored: staged file staged, tracked edit unstaged, untracked still untracked.
    let status = fx.git(&["status", "--porcelain", "--untracked-files=all"]);
    let lines: Vec<&str> = status.lines().collect();
    assert!(lines.contains(&"A  staged.txt"), "status: {status}");
    assert!(lines.contains(&" M app.txt"), "status: {status}");
    assert!(lines.contains(&"?? notes.txt"), "status: {status}");
    assert!(lines.contains(&"?? scratch/deep.txt"), "status: {status}");
    assert!(
        !status.contains("build.log"),
        "ignored file must stay ignored: {status}"
    );
}

#[tokio::test]
async fn undo_hard_reset_restores_untracked_files_and_tracked_changes() {
    let fx = init_fixture();
    make_dirty(&fx);
    let main_head = fx.git(&["rev-parse", "HEAD"]);
    let index_before = fx.index_bytes();
    let status_before = fx.git(&["status", "--porcelain", "--untracked-files=all"]);

    let op_id = fx
        .undo
        .capture_before_mutation(
            &fx.root,
            OpKind::Reset {
                mode: ResetMode::Hard,
            },
            CompactString::from("Reset --hard other"),
        )
        .await
        .unwrap();

    // Snapshot capture must not touch the real index or worktree.
    assert_eq!(
        fx.index_bytes(),
        index_before,
        "real index modified by snapshot"
    );
    assert_eq!(
        fx.git(&["status", "--porcelain", "--untracked-files=all"]),
        status_before
    );

    // Snapshot is pinned under refs/branvio/undo/<op-id>-... and contains untracked, not ignored.
    let pins = fx.git(&["for-each-ref", "--format=%(refname)", "refs/branvio/undo"]);
    let pins: Vec<&str> = pins.lines().collect();
    assert_eq!(pins.len(), 1, "exactly one pin expected: {pins:?}");
    let pin = pins[0];
    assert!(
        pin.starts_with(&format!("{UNDO_SNAPSHOT_REF_PREFIX}{op_id}-")),
        "unexpected pin name {pin}"
    );
    let snap_hex = fx.git(&["rev-parse", "--verify", pin]);
    let files = fx.git(&["ls-tree", "-r", "--name-only", &snap_hex]);
    let files: Vec<&str> = files.lines().collect();
    for expected in [
        "app.txt",
        "notes.txt",
        "scratch/deep.txt",
        "staged.txt",
        ".gitignore",
    ] {
        assert!(
            files.contains(&expected),
            "snapshot missing {expected}: {files:?}"
        );
    }
    assert!(
        !files.contains(&"build.log"),
        "ignored file captured: {files:?}"
    );
    assert_eq!(fx.git(&["rev-parse", &format!("{snap_hex}^1")]), main_head);
    assert_eq!(
        fx.git(&["stash", "list"]),
        "",
        "refs/stash must not be polluted"
    );

    // Destructive op: reset --hard onto a commit that tracks `notes.txt` (clobbers untracked).
    let cli = fx
        .engine
        .reset(&fx.root, "other", ResetMode::Hard)
        .await
        .unwrap();
    finish_cli(cli).await;
    fx.undo
        .finalize_after_mutation(&fx.root, op_id)
        .await
        .unwrap();
    assert_eq!(fx.read("notes.txt").as_deref(), Some("theirs\n"));
    assert_eq!(fx.read("app.txt").as_deref(), Some("v1\n"));
    assert_eq!(fx.read("staged.txt"), None);

    let check = fx.undo.inspect_next_undo(&fx.root).await.unwrap();
    assert!(check.preserves_worktree);

    fx.undo.undo(&fx.root, false).await.unwrap();
    assert_eq!(fx.git(&["rev-parse", "HEAD"]), main_head);
    assert_dirty_state_restored(&fx);
}

#[tokio::test]
async fn undo_discard_restores_cleaned_untracked_file() {
    let fx = init_fixture();
    make_dirty(&fx);

    let op_id = fx
        .undo
        .capture_before_mutation(
            &fx.root,
            OpKind::DiscardWorktreePatch,
            CompactString::from("Discard changes"),
        )
        .await
        .unwrap();

    // Destructive op: discard tracked edit and `git clean` the untracked files.
    let cli = fx
        .engine
        .discard_worktree_paths(
            &fx.root,
            &[RepoRelPath::normalize("app.txt").unwrap()],
            &[
                RepoRelPath::normalize("notes.txt").unwrap(),
                RepoRelPath::normalize("scratch").unwrap(),
            ],
        )
        .await
        .unwrap();
    finish_cli(cli).await;
    fx.undo
        .finalize_after_mutation(&fx.root, op_id)
        .await
        .unwrap();
    assert_eq!(fx.read("notes.txt"), None);
    assert_eq!(fx.read("scratch/deep.txt"), None);
    assert_eq!(fx.read("app.txt").as_deref(), Some("v1\n"));

    fx.undo.undo(&fx.root, false).await.unwrap();
    assert_dirty_state_restored(&fx);
}

#[tokio::test]
async fn clean_worktree_creates_no_snapshot_ref() {
    let fx = init_fixture();
    fx.write("build.log", "ignored only\n");
    let op_id = fx
        .undo
        .capture_before_mutation(
            &fx.root,
            OpKind::Reset {
                mode: ResetMode::Hard,
            },
            CompactString::from("Reset --hard HEAD"),
        )
        .await
        .unwrap();
    let refs = fx.git(&["for-each-ref", "--format=%(refname)", "refs/branvio"]);
    assert!(
        refs.is_empty(),
        "unexpected snapshot refs for op {op_id}: {refs}"
    );
}

/// Receives the next CLI stream event, failing fast instead of hanging CI if a `git` subprocess stalls.
async fn next_cli_event(
    events: &mut tokio::sync::mpsc::Receiver<git_engine::CliStreamEvent>,
) -> Option<git_engine::CliStreamEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(60), events.recv())
        .await
        .expect("timed out after 60s waiting for the next git CLI stream event")
}
