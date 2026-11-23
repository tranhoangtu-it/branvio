#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::domain::{OpKind, RemoteExposureState, ResetMode};
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::CancelToken;
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use git_engine::{BranvioGitEngine, CommitOptions, GitCliExecutor, GitIndexEngine, GitReadEngine};
use oplog_undo::{BranvioUndoEngine, OpLogError, RedbOpLogStore, UndoRedoEngine};
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_test_repo() -> (TempDir, HostPath) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    run_git(root, &["init", "-b", "main"]);
    // Deterministic line endings: Windows runners ship `core.autocrlf=true` in system config.
    run_git(root, &["config", "core.autocrlf", "false"]);
    run_git(root, &["config", "user.name", "BranvioTester"]);
    run_git(root, &["config", "user.email", "tester@branvio.rs"]);
    std::fs::write(root.join("app.txt"), "v1\n").unwrap();
    run_git(root, &["add", "app.txt"]);
    run_git(root, &["commit", "-m", "chore: initial commit"]);
    let host_root = HostPath::canonicalize_existing(root).unwrap();
    (dir, host_root)
}

async fn finish_cli(mut handle: git_engine::CliCommandHandle) {
    while let Some(ev) = next_cli_event(&mut handle.events).await {
        if let git_engine::CliStreamEvent::Finished { success, .. } = ev {
            assert!(success, "cli operation should succeed");
            break;
        }
    }
}

#[tokio::test]
async fn undo_and_redo_commit_preserves_index_and_worktree() {
    let (_repo_dir, repo_root) = init_test_repo();
    let db_dir = TempDir::new().unwrap();
    let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = Arc::new(BranvioGitEngine::new(Arc::clone(&env), None));
    let undo_engine = BranvioUndoEngine::new(store, engine.clone(), env);

    let initial_head =
        GitOid::from_hex(&run_git(repo_root.as_path(), &["rev-parse", "HEAD"])).unwrap();

    std::fs::write(repo_root.as_path().join("app.txt"), "v2-staged\n").unwrap();
    engine
        .stage_files(&repo_root, &[RepoRelPath::normalize("app.txt").unwrap()])
        .await
        .unwrap();

    let op_id = undo_engine
        .capture_before_mutation(&repo_root, OpKind::Commit, CompactString::from("Commit v2"))
        .await
        .unwrap();

    let cli = engine
        .commit(
            &repo_root,
            CommitOptions {
                message: "feat: update app to v2".to_string(),
                amend: false,
                gpg_sign: Some(false),
                signing_key_id: None,
                no_verify: true,
                allow_empty: false,
            },
        )
        .await
        .unwrap();
    finish_cli(cli).await;
    undo_engine
        .finalize_after_mutation(&repo_root, op_id)
        .await
        .unwrap();

    let committed_head =
        GitOid::from_hex(&run_git(repo_root.as_path(), &["rev-parse", "HEAD"])).unwrap();
    assert_ne!(committed_head, initial_head);

    // Also add an unstaged edit on disk that must survive Undo Commit
    std::fs::write(repo_root.as_path().join("app.txt"), "v3-unstaged-edit\n").unwrap();

    let outcome = undo_engine.undo(&repo_root, false).await.unwrap();
    assert_eq!(outcome.restored_head_oid, Some(initial_head));
    let head_after_undo =
        GitOid::from_hex(&run_git(repo_root.as_path(), &["rev-parse", "HEAD"])).unwrap();
    assert_eq!(head_after_undo, initial_head);
    // Worktree edit was untouched while staged change was put back into index!
    assert_eq!(
        std::fs::read_to_string(repo_root.as_path().join("app.txt")).unwrap(),
        "v3-unstaged-edit\n"
    );
    let cached = run_git(repo_root.as_path(), &["diff", "--cached", "--name-only"]);
    assert_eq!(cached, "app.txt");

    // Redo restores the commit cleanly
    let redo_outcome = undo_engine.redo(&repo_root, false).await.unwrap();
    assert_eq!(redo_outcome.restored_head_oid, Some(committed_head));
    let head_after_redo =
        GitOid::from_hex(&run_git(repo_root.as_path(), &["rev-parse", "HEAD"])).unwrap();
    assert_eq!(head_after_redo, committed_head);
}

#[tokio::test]
async fn undo_hard_reset_restores_dirty_worktree_via_safety_stash() {
    let (_repo_dir, repo_root) = init_test_repo();
    let db_dir = TempDir::new().unwrap();
    let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = Arc::new(BranvioGitEngine::new(Arc::clone(&env), None));
    let undo_engine = BranvioUndoEngine::new(store, engine.clone(), env);

    // Dirty uncommitted worktree edit
    std::fs::write(repo_root.as_path().join("app.txt"), "precious-wip-edits\n").unwrap();

    let op_id = undo_engine
        .capture_before_mutation(
            &repo_root,
            OpKind::Reset {
                mode: ResetMode::Hard,
            },
            CompactString::from("Reset --hard HEAD"),
        )
        .await
        .unwrap();

    let cli = engine
        .reset(&repo_root, "HEAD", ResetMode::Hard)
        .await
        .unwrap();
    finish_cli(cli).await;
    undo_engine
        .finalize_after_mutation(&repo_root, op_id)
        .await
        .unwrap();

    // After hard reset, worktree is wiped back to v1
    assert_eq!(
        std::fs::read_to_string(repo_root.as_path().join("app.txt")).unwrap(),
        "v1\n"
    );
    // And refs/stash was NOT polluted
    let stash_list = run_git(repo_root.as_path(), &["stash", "list"]);
    assert!(stash_list.is_empty());

    // Time-machine Undo brings back the wiped uncommitted edit cleanly!
    undo_engine.undo(&repo_root, false).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(repo_root.as_path().join("app.txt")).unwrap(),
        "precious-wip-edits\n"
    );
}

#[tokio::test]
async fn external_cli_divergence_aborts_undo_and_pushed_history_requires_confirmation() {
    let bare_dir = TempDir::new().unwrap();
    run_git(bare_dir.path(), &["init", "--bare", "-b", "main"]);

    let (_repo_dir, repo_root) = init_test_repo();
    let db_dir = TempDir::new().unwrap();
    let store = Arc::new(RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = Arc::new(BranvioGitEngine::new(Arc::clone(&env), None));
    let undo_engine = BranvioUndoEngine::new(store, engine.clone(), env);

    run_git(
        repo_root.as_path(),
        &["remote", "add", "origin", bare_dir.path().to_str().unwrap()],
    );

    // Operation 1: local commit C2, then pushed to origin/main
    std::fs::write(repo_root.as_path().join("app.txt"), "v2-pushed\n").unwrap();
    engine
        .stage_files(&repo_root, &[RepoRelPath::normalize("app.txt").unwrap()])
        .await
        .unwrap();
    let op_id = undo_engine
        .capture_before_mutation(&repo_root, OpKind::Commit, CompactString::from("Commit v2"))
        .await
        .unwrap();
    finish_cli(
        engine
            .commit(
                &repo_root,
                CommitOptions {
                    message: "feat: v2 pushed".to_string(),
                    amend: false,
                    gpg_sign: Some(false),
                    signing_key_id: None,
                    no_verify: true,
                    allow_empty: false,
                },
            )
            .await
            .unwrap(),
    )
    .await;
    undo_engine
        .finalize_after_mutation(&repo_root, op_id)
        .await
        .unwrap();

    run_git(repo_root.as_path(), &["push", "-u", "origin", "main"]);

    // Inspect confirms pushed exposure
    let check = undo_engine.inspect_next_undo(&repo_root).await.unwrap();
    assert!(matches!(
        check.remote_exposure,
        RemoteExposureState::TouchesPushedHistory { .. }
    ));

    // Unconfirmed undo must refuse with RemoteExposureConfirmationRequired
    let unconfirmed = undo_engine.undo(&repo_root, false).await;
    assert!(matches!(
        unconfirmed,
        Err(OpLogError::RemoteExposureConfirmationRequired { .. })
    ));

    // External CLI commit outside Branvio moves HEAD away -> ExternalDivergence
    std::fs::write(repo_root.as_path().join("ext.txt"), "external\n").unwrap();
    run_git(repo_root.as_path(), &["add", "ext.txt"]);
    run_git(repo_root.as_path(), &["commit", "-m", "external commit"]);

    let diverged = undo_engine.undo(&repo_root, true).await;
    assert!(matches!(
        diverged,
        Err(OpLogError::ExternalDivergence { .. })
    ));

    // Verify ReadEngine still opens clean repository
    let summary = engine.open_repo(repo_root.as_path()).await.unwrap();
    let _ = engine
        .working_tree_status(&summary.workdir_root, CancelToken::new())
        .await
        .unwrap();
}

/// Receives the next CLI stream event, failing fast instead of hanging CI if a `git` subprocess stalls.
async fn next_cli_event(
    events: &mut tokio::sync::mpsc::Receiver<git_engine::CliStreamEvent>,
) -> Option<git_engine::CliStreamEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(60), events.recv())
        .await
        .expect("timed out after 60s waiting for the next git CLI stream event")
}
