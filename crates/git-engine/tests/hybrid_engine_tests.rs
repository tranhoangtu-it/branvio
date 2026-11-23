//! Integration tests for the 3-tier hybrid `git-engine` (Tier 1 `gix`, Tier 2 `git2`, Tier 3 CLI).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use branvio_core::domain::{
    CommitFlags, CommitSignatureState, ConflictSubkind, DeltaStatus, FileMode, RefKind,
    UNRESOLVED_COMMIT_ROW,
};
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::CancelToken;
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use git_engine::{
    BranvioGitEngine, CliStreamEvent, CommitOptions, GitCliExecutor, GitEngineError,
    GitIndexEngine, GitReadEngine, PickaxeSearchRequest, RebaseAction, RebasePlanEntry,
    RebasePlanPayload, RevwalkSessionParams,
};
use graph_layout::{GraphFilterSpec, GraphLayoutEngine, LayoutSessionConfig, MetroLayoutEngine};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn run_git(repo_dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Branvio Test")
        .env("GIT_AUTHOR_EMAIL", "test@branvio.rs")
        .env("GIT_COMMITTER_NAME", "Branvio Test")
        .env("GIT_COMMITTER_EMAIL", "test@branvio.rs")
        .env("GIT_TERMINAL_PROMPT", "0");

    for &(k, v) in extra_env {
        cmd.env(k, v);
    }

    let out = cmd.output().expect("git command should execute");
    assert!(
        out.status.success(),
        "git {:?} failed: stderr={}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_test_repo() -> (TempDir, HostPath, BranvioGitEngine) {
    let dir = TempDir::new().unwrap();
    run_git(dir.path(), &["init", "-b", "main"], &[]);
    run_git(dir.path(), &["config", "user.name", "Branvio Test"], &[]);
    run_git(
        dir.path(),
        &["config", "user.email", "test@branvio.rs"],
        &[],
    );
    run_git(dir.path(), &["config", "commit.gpgsign", "false"], &[]);

    let host_root = HostPath::canonicalize_existing(dir.path()).unwrap();
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let engine = BranvioGitEngine::new(env, None);
    (dir, host_root, engine)
}

#[tokio::test]
async fn revwalk_session_streams_chunks_and_enforces_topo_order_under_clock_skew() {
    let (dir, root, engine) = init_test_repo();

    // Parent commit has FUTURE timestamp (2025-01-01)
    std::fs::write(dir.path().join("app.txt"), "base\n").unwrap();
    run_git(dir.path(), &["add", "app.txt"], &[]);
    run_git(
        dir.path(),
        &["commit", "-m", "root in future clock"],
        &[
            ("GIT_AUTHOR_DATE", "2025-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2025-01-01T00:00:00Z"),
        ],
    );
    let parent_hex = run_git(dir.path(), &["rev-parse", "HEAD"], &[]);
    let parent_oid = GitOid::from_hex(&parent_hex).unwrap();

    // Child commit has PAST timestamp (2020-01-01 — severe 5-year backward clock skew!)
    std::fs::write(dir.path().join("app.txt"), "base\nchild\n").unwrap();
    run_git(dir.path(), &["add", "app.txt"], &[]);
    run_git(
        dir.path(),
        &["commit", "-m", "child with 5y backward clock skew"],
        &[
            ("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z"),
        ],
    );
    let child_hex = run_git(dir.path(), &["rev-parse", "HEAD"], &[]);
    let child_oid = GitOid::from_hex(&child_hex).unwrap();

    let cancel = CancelToken::new();
    let refs = engine.head_and_refs(&root, cancel.clone()).await.unwrap();
    assert_eq!(refs.head_label, "main");
    assert_eq!(refs.head_oid, Some(child_oid));

    let mut session = engine
        .open_revwalk_session(
            &root,
            RevwalkSessionParams {
                start_tips: Vec::new(),
                first_parent_only: false,
                path_filters: Vec::new(),
            },
            cancel.clone(),
        )
        .await
        .unwrap();

    let layout_engine = MetroLayoutEngine::new();
    let mut layout = layout_engine.start_session(
        LayoutSessionConfig::default(),
        refs.head_oid,
        GraphFilterSpec::default(),
    );

    // Stream 1 commit per chunk to prove stateful frontier + cross-chunk backpatching works cleanly.
    let mut collected_oids = Vec::new();
    while !session.is_exhausted() {
        let chunk = session.next_chunk(1, &cancel).unwrap();
        for c in &chunk.commits {
            collected_oids.push(c.oid);
        }
        layout
            .push_commits_chunk(&chunk.commits, &refs.by_oid)
            .unwrap();
    }

    assert_eq!(collected_oids, vec![child_oid, parent_oid]);
    assert_eq!(layout.commit_row_count(), 2);
    let rows = layout.commit_rows_slice(0..2);
    assert_eq!(rows[0].outgoing_edges[0].target_row, 1);
    assert_ne!(rows[0].outgoing_edges[0].target_row, UNRESOLVED_COMMIT_ROW);
}

#[tokio::test]
async fn tier1_working_tree_status_blame_and_commit_details() {
    let (dir, root, engine) = init_test_repo();

    std::fs::write(dir.path().join("lib.rs"), "line 1\nline 2\n").unwrap();
    run_git(dir.path(), &["add", "lib.rs"], &[]);
    run_git(
        dir.path(),
        &[
            "commit",
            "-m",
            "feat(core): add lib.rs\n\nDetailed body line.\n\nSigned-off-by: Alice <alice@branvio.rs>",
        ],
        &[],
    );
    let c1_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    std::fs::write(
        dir.path().join("lib.rs"),
        "line 1\nline 2\nline 3 from c2\n",
    )
    .unwrap();
    run_git(dir.path(), &["add", "lib.rs"], &[]);
    run_git(dir.path(), &["commit", "-m", "feat: extend lib.rs"], &[]);
    let c2_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    // Create staged, unstaged, and untracked changes.
    std::fs::write(dir.path().join("staged_new.txt"), "hello staged\n").unwrap();
    run_git(dir.path(), &["add", "staged_new.txt"], &[]);
    std::fs::write(
        dir.path().join("lib.rs"),
        "line 1\nline 2\nline 3 from c2\nunstaged edit\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("scratch.tmp"), "untracked").unwrap();

    let cancel = CancelToken::new();
    let summary = engine.open_repo(dir.path()).await.unwrap();
    assert!(!summary.is_bare);

    let status = engine
        .working_tree_status(&root, cancel.clone())
        .await
        .unwrap();
    assert!(
        status
            .staged
            .iter()
            .any(|d| d.new_path.as_str() == "staged_new.txt" && d.status == DeltaStatus::Added)
    );
    assert!(
        status
            .unstaged
            .iter()
            .any(|d| d.new_path.as_str() == "lib.rs" && d.status == DeltaStatus::Modified)
    );
    assert!(
        status
            .unstaged
            .iter()
            .any(|d| d.new_path.as_str() == "scratch.tmp" && d.status == DeltaStatus::Untracked)
    );

    let details = engine
        .load_commit_details(&root, c1_oid, cancel.clone())
        .await
        .unwrap();
    assert!(details.full_message.contains("Detailed body line."));
    assert_eq!(
        details.trailers,
        vec![(
            CompactString::const_new("Signed-off-by"),
            CompactString::const_new("Alice <alice@branvio.rs>")
        )]
    );
    assert_eq!(details.changed_files.len(), 1);
    assert_eq!(details.changed_files[0].new_path.as_str(), "lib.rs");

    let rel_lib = RepoRelPath::normalize("lib.rs").unwrap();
    let blame = engine
        .blame_file(&root, &rel_lib, Some(c2_oid), cancel)
        .await
        .unwrap();
    assert_eq!(blame.lines.len(), 3);
    assert_eq!(blame.lines[0].commit_oid, c1_oid);
    assert_eq!(blame.lines[2].commit_oid, c2_oid);
}

#[tokio::test]
async fn tier2_in_memory_line_patch_staging_and_predictive_merge_conflict() {
    let (dir, root, engine) = init_test_repo();

    let initial_text = (1..=12).map(|i| format!("line {i}\n")).collect::<String>();
    std::fs::write(dir.path().join("sample.txt"), &initial_text).unwrap();
    run_git(dir.path(), &["add", "sample.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "base"], &[]);
    let base_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    // Snapshot clean index tree OID.
    let clean_index_tree = engine.snapshot_index_tree_oid(&root).await.unwrap();

    // Modify line 2 AND line 11 in worktree, then apply synthetic patch staging ONLY line 2!
    let modified_worktree = initial_text
        .replace("line 2\n", "line 2 PATCHED\n")
        .replace("line 11\n", "line 11 UNSTAGED\n");
    std::fs::write(dir.path().join("sample.txt"), &modified_worktree).unwrap();

    let partial_patch = "\
diff --git a/sample.txt b/sample.txt
index 0000000..1111111 100644
--- a/sample.txt
+++ b/sample.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line 2 PATCHED
 line 3
";
    engine
        .apply_synthetic_patch_to_index(&root, partial_patch.as_bytes())
        .await
        .unwrap();

    let staged_diff = run_git(dir.path(), &["diff", "--cached"], &[]);
    assert!(staged_diff.contains("+line 2 PATCHED"));
    assert!(!staged_diff.contains("line 11 UNSTAGED"));

    let unstaged_diff = run_git(dir.path(), &["diff"], &[]);
    assert!(unstaged_diff.contains("+line 11 UNSTAGED"));
    assert!(!unstaged_diff.contains("line 2 PATCHED"));

    // Restore index to clean tree snapshot and reset worktree.
    engine
        .restore_index_from_tree_oid(&root, clean_index_tree)
        .await
        .unwrap();
    assert!(run_git(dir.path(), &["diff", "--cached"], &[]).is_empty());
    run_git(dir.path(), &["checkout", "--", "sample.txt"], &[]);

    // Predictive conflict test: branch A (main) and branch B edit line 1 differently.
    run_git(dir.path(), &["checkout", "-b", "feature/conflict"], &[]);
    std::fs::write(
        dir.path().join("sample.txt"),
        initial_text.replace("line 1\n", "line 1 THEIRS\n"),
    )
    .unwrap();
    run_git(dir.path(), &["commit", "-am", "theirs edit"], &[]);
    let theirs_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    run_git(dir.path(), &["checkout", "main"], &[]);
    std::fs::write(
        dir.path().join("sample.txt"),
        initial_text.replace("line 1\n", "line 1 OURS\n"),
    )
    .unwrap();
    run_git(dir.path(), &["commit", "-am", "ours edit"], &[]);
    let ours_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    let pred_conflict = engine
        .predict_merge_conflicts(&root, ours_oid, theirs_oid)
        .await
        .unwrap();
    assert!(!pred_conflict.will_succeed_cleanly);
    assert_eq!(pred_conflict.conflicts.len(), 1);
    assert_eq!(pred_conflict.conflicts[0].path.as_str(), "sample.txt");
    assert_eq!(
        pred_conflict.conflicts[0].kind,
        ConflictSubkind::BothModified
    );

    // Clean non-conflicting prediction against ancestor base -> fast-forward/clean.
    let pred_clean = engine
        .predict_merge_conflicts(&root, base_oid, theirs_oid)
        .await
        .unwrap();
    assert!(pred_clean.will_succeed_cleanly);
    assert!(pred_clean.is_fast_forward);
}

#[tokio::test]
async fn tier3_cli_commit_worktree_list_and_pickaxe_search() {
    let (dir, root, engine) = init_test_repo();

    std::fs::write(dir.path().join("mod.rs"), "pub fn alpha() {}\n").unwrap();
    run_git(dir.path(), &["add", "mod.rs"], &[]);
    let mut handle = engine
        .commit(
            &root,
            CommitOptions {
                message: "feat: initial alpha".to_string(),
                amend: false,
                gpg_sign: Some(false),
                signing_key_id: None,
                no_verify: true,
                allow_empty: false,
            },
        )
        .await
        .unwrap();

    let mut saw_finished_ok = false;
    while let Some(ev) = handle.events.recv().await {
        if let CliStreamEvent::Finished { success, .. } = ev {
            saw_finished_ok = success;
        }
    }
    assert!(saw_finished_ok);

    // Commit introducing unique pickaxe token.
    std::fs::write(
        dir.path().join("mod.rs"),
        "pub fn alpha() {}\npub fn BRANVIO_UNIQUE_SYMBOL_42() {}\n",
    )
    .unwrap();
    run_git(dir.path(), &["add", "mod.rs"], &[]);
    run_git(
        dir.path(),
        &["commit", "-m", "feat: add unique symbol"],
        &[],
    );
    let target_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    // Linked worktree round-trip.
    let wt_parent = TempDir::new().unwrap();
    let wt_path = HostPath::from_path_buf(wt_parent.path().join("linked-wt"));
    let mut wt_add = engine
        .worktree_add(&root, &wt_path, "wt-branch", true)
        .await
        .unwrap();
    while let Some(ev) = wt_add.events.recv().await {
        if let CliStreamEvent::Finished { success, .. } = ev {
            assert!(success);
        }
    }

    let worktrees = engine.worktree_list(&root).await.unwrap();
    assert_eq!(worktrees.len(), 2);
    assert!(worktrees[0].is_main);
    assert_eq!(worktrees[1].branch.as_deref(), Some("wt-branch"));

    let mut wt_rm = engine
        .worktree_remove(&root, &wt_path, false)
        .await
        .unwrap();
    while let Some(ev) = wt_rm.events.recv().await {
        if let CliStreamEvent::Finished { success, .. } = ev {
            assert!(success);
        }
    }

    // Pickaxe search via Tier 3.
    let matches = engine
        .search_commits_pickaxe(
            &root,
            PickaxeSearchRequest {
                query: "BRANVIO_UNIQUE_SYMBOL_42".to_string(),
                is_regex: false,
                limit: 10,
                start_tips: Vec::new(),
                path_filters: vec![RepoRelPath::normalize("mod.rs").unwrap()],
            },
            CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(matches, vec![target_oid]);
}

#[tokio::test]
async fn revwalk_handles_shallow_clone_cleanly() {
    let (src_dir, _, _) = init_test_repo();
    for step in 1..=4 {
        std::fs::write(src_dir.path().join("log.txt"), format!("step {step}\n")).unwrap();
        run_git(src_dir.path(), &["add", "log.txt"], &[]);
        run_git(
            src_dir.path(),
            &["commit", "-m", &format!("commit {step}")],
            &[],
        );
    }

    let shallow_parent = TempDir::new().unwrap();
    let shallow_path = shallow_parent.path().join("shallow-copy");
    let file_url = format!("file://{}", src_dir.path().display());
    run_git(
        shallow_parent.path(),
        &[
            "clone",
            "--depth",
            "2",
            &file_url,
            shallow_path.to_str().unwrap(),
        ],
        &[],
    );

    let shallow_root = HostPath::canonicalize_existing(&shallow_path).unwrap();
    let engine = BranvioGitEngine::new(Arc::new(HydratedGitEnv::from_current_process()), None);
    let cancel = CancelToken::new();

    let refs = engine
        .head_and_refs(&shallow_root, cancel.clone())
        .await
        .unwrap();
    let mut session = engine
        .open_revwalk_session(
            &shallow_root,
            RevwalkSessionParams {
                start_tips: Vec::new(),
                first_parent_only: false,
                path_filters: Vec::new(),
            },
            cancel.clone(),
        )
        .await
        .unwrap();

    let mut commits = Vec::new();
    while !session.is_exhausted() {
        let chunk = session.next_chunk(10, &cancel).unwrap();
        commits.extend(chunk.commits);
    }

    assert_eq!(commits.len(), 2);
    assert!(commits[1].parents.is_empty());
    assert!(commits[1].flags.contains(CommitFlags::IS_ORPHAN_ROOT));

    let layout_engine = MetroLayoutEngine::new();
    let mut layout = layout_engine.start_session(
        LayoutSessionConfig::default(),
        refs.head_oid,
        GraphFilterSpec::default(),
    );
    layout.push_commits_chunk(&commits, &refs.by_oid).unwrap();
    assert_eq!(layout.commit_row_count(), 2);
    let rows = layout.commit_rows_slice(0..2);
    assert_eq!(rows[0].outgoing_edges[0].target_row, 1);
    assert!(rows[1].outgoing_edges.is_empty());
    assert!(layout.active_frontier_lanes().is_empty());

    let boundary_details = engine
        .load_commit_details(&shallow_root, commits[1].oid, cancel)
        .await
        .unwrap();
    assert!(!boundary_details.changed_files.is_empty());
}

#[tokio::test]
async fn revwalk_ignores_non_commit_tags_notes_stash_and_preserves_feature_head_ref() {
    let (dir, root, engine) = init_test_repo();

    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    run_git(dir.path(), &["add", "a.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "initial"], &[]);
    let c1 = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    // Non-commit refs that previously broke or polluted revwalk tips:
    run_git(dir.path(), &["tag", "tree-light", "HEAD^{tree}"], &[]);
    run_git(
        dir.path(),
        &["tag", "-a", "tree-ann", "-m", "tree tag", "HEAD^{tree}"],
        &[],
    );
    let blob_hex = run_git(dir.path(), &["hash-object", "-w", "a.txt"], &[]);
    run_git(dir.path(), &["tag", "blob-tag", &blob_hex], &[]);
    run_git(
        dir.path(),
        &["notes", "add", "-m", "review note", "HEAD"],
        &[],
    );

    // Stash entry (`refs/stash`)
    std::fs::write(dir.path().join("a.txt"), "stashed\n").unwrap();
    run_git(dir.path(), &["stash", "push", "-m", "wip stash"], &[]);

    // Branch ending in `/HEAD` and PR refs (numeric & non-numeric).
    run_git(dir.path(), &["branch", "feature/HEAD"], &[]);
    run_git(
        dir.path(),
        &["update-ref", "refs/pull/42/head", &c1.to_hex()],
        &[],
    );
    run_git(
        dir.path(),
        &["update-ref", "refs/pull/not-a-number/head", &c1.to_hex()],
        &[],
    );

    let cancel = CancelToken::new();
    let refs = engine.head_and_refs(&root, cancel.clone()).await.unwrap();
    assert!(
        refs.badges
            .iter()
            .any(|b| b.kind == RefKind::LocalBranch && b.name == "feature/HEAD")
    );
    assert!(
        refs.badges
            .iter()
            .any(|b| b.kind == RefKind::PullRequestRef { number: 42 })
    );
    assert!(
        !refs
            .badges
            .iter()
            .any(|b| matches!(b.kind, RefKind::PullRequestRef { number: 0 }))
    );

    let mut session = engine
        .open_revwalk_session(
            &root,
            RevwalkSessionParams {
                start_tips: Vec::new(),
                first_parent_only: false,
                path_filters: Vec::new(),
            },
            cancel.clone(),
        )
        .await
        .unwrap();
    let chunk = session.next_chunk(20, &cancel).unwrap();
    assert_eq!(chunk.commits.len(), 1);
    assert_eq!(chunk.commits[0].oid, c1);
}

#[tokio::test]
async fn sha256_repo_and_forged_ssh_pgp_signatures_report_unverified() {
    let dir = TempDir::new().unwrap();
    run_git(
        dir.path(),
        &["init", "--object-format=sha256", "-b", "main"],
        &[],
    );
    run_git(dir.path(), &["config", "user.name", "Branvio Test"], &[]);
    run_git(
        dir.path(),
        &["config", "user.email", "test@branvio.rs"],
        &[],
    );
    run_git(dir.path(), &["config", "commit.gpgsign", "false"], &[]);

    std::fs::write(dir.path().join("lib.rs"), "fn one() {}\n").unwrap();
    run_git(dir.path(), &["add", "lib.rs"], &[]);
    run_git(dir.path(), &["commit", "-m", "base sha256"], &[]);
    let c1_hex = run_git(dir.path(), &["rev-parse", "HEAD"], &[]);
    let tree1_hex = run_git(dir.path(), &["rev-parse", "HEAD^{tree}"], &[]);

    // Craft commit with forged `gpgsig` SSH signature block + another with `gpgsig-sha256`.
    let craft_commit = |raw_obj: &str| -> String {
        let mut child = Command::new("git")
            .current_dir(dir.path())
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(raw_obj.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let c2_raw = [
        &format!("tree {tree1_hex}\n"),
        &format!("parent {c1_hex}\n"),
        "author Branvio Test <test@branvio.rs> 1700000100 +0000\n",
        "committer Branvio Test <test@branvio.rs> 1700000100 +0000\n",
        "gpgsig -----BEGIN SSH SIGNATURE-----\n",
        " U1NIU0lH\n",
        " -----END SSH SIGNATURE-----\n",
        "\n",
        "feat: fake ssh signed\n",
    ]
    .concat();
    let c2_hex = craft_commit(&c2_raw);

    let c3_raw = [
        &format!("tree {tree1_hex}\n"),
        &format!("parent {c2_hex}\n"),
        "author Branvio Test <test@branvio.rs> 1700000200 +0000\n",
        "committer Branvio Test <test@branvio.rs> 1700000200 +0000\n",
        "gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n",
        " iQEzBAABCAAd\n",
        " -----END PGP SIGNATURE-----\n",
        "\n",
        "feat: fake pgp sha256 signed\n",
    ]
    .concat();
    let c3_hex = craft_commit(&c3_raw);
    run_git(dir.path(), &["update-ref", "refs/heads/main", &c3_hex], &[]);

    let root = HostPath::canonicalize_existing(dir.path()).unwrap();
    let engine = BranvioGitEngine::new(Arc::new(HydratedGitEnv::from_current_process()), None);
    let cancel = CancelToken::new();

    let summary = engine.open_repo(dir.path()).await.unwrap();
    assert_eq!(summary.head_oid.unwrap().to_hex().len(), 64);

    let mut session = engine
        .open_revwalk_session(
            &root,
            RevwalkSessionParams {
                start_tips: Vec::new(),
                first_parent_only: false,
                path_filters: Vec::new(),
            },
            cancel.clone(),
        )
        .await
        .unwrap();
    let chunk = session.next_chunk(10, &cancel).unwrap();
    assert_eq!(chunk.commits.len(), 3);
    // Neither forged SSH nor `gpgsig-sha256` may ever be reported as ValidSsh/ValidGpg.
    assert_eq!(
        chunk.commits[0].signature_state,
        CommitSignatureState::Unverified
    );
    assert_eq!(
        chunk.commits[1].signature_state,
        CommitSignatureState::Unverified
    );
    assert_eq!(
        chunk.commits[2].signature_state,
        CommitSignatureState::Unsigned
    );

    let details = engine
        .load_commit_details(&root, GitOid::from_hex(&c3_hex).unwrap(), cancel)
        .await
        .unwrap();
    assert!(details.full_message.contains("fake pgp sha256 signed"));
}

#[tokio::test]
async fn tier3_option_injection_and_missing_helper_rebase_are_blocked() {
    let (dir, root, engine) = init_test_repo();

    std::fs::write(dir.path().join("init.txt"), "init\n").unwrap();
    run_git(dir.path(), &["add", "init.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "initial"], &[]);
    let c1 = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    // Option injection via `worktree_add`: `--detach` must be treated as a ref, not a flag!
    let wt_parent = TempDir::new().unwrap();
    let wt_path = HostPath::from_path_buf(wt_parent.path().join("wt-inject"));
    let mut h_detach = engine
        .worktree_add(&root, &wt_path, "--detach", false)
        .await
        .unwrap();
    let mut finish_ok = None;
    let mut stderr_buf = String::new();
    while let Some(ev) = h_detach.events.recv().await {
        match ev {
            CliStreamEvent::StderrLine(line) => {
                stderr_buf.push_str(&line);
                stderr_buf.push('\n');
            }
            CliStreamEvent::Finished { success, .. } => finish_ok = Some(success),
            _ => {}
        }
    }
    assert_eq!(finish_ok, Some(false));
    assert!(!wt_path.as_path().exists());
    assert!(!stderr_buf.contains("unknown option"));

    // `-x` with `create_new_branch = true` is rejected up front.
    let err = match engine.worktree_add(&root, &wt_path, "-x", true).await {
        Ok(_) => panic!("expected invalid new branch name beginning with '-' to be rejected"),
        Err(e) => e,
    };
    assert!(matches!(err, GitEngineError::CliFailed { .. }));

    // `interactive_rebase` with `self_helper_exe = None` must fail rather than launching user's editor.
    let plan = RebasePlanPayload {
        onto_rev: CompactString::const_new("HEAD"),
        entries: vec![RebasePlanEntry {
            oid: c1,
            action: RebaseAction::Pick,
            updated_message: None,
        }],
    };
    let rebase_err = match engine.interactive_rebase(&root, plan).await {
        Ok(_) => panic!("expected interactive_rebase without helper to return Err"),
        Err(e) => e,
    };
    assert!(matches!(rebase_err, GitEngineError::CliFailed { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn tier3_cancel_buffered_pickaxe_drop_handle_and_slow_receiver() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, root, _) = init_test_repo();
    std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
    run_git(dir.path(), &["add", "f.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "init"], &[]);

    let pid_file = dir.path().join("child.pid");
    let sleep_script = dir.path().join("fake_git_sleep.sh");
    std::fs::write(
        &sleep_script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&sleep_script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&sleep_script, perms).unwrap();

    let mut sleep_env = HydratedGitEnv::from_current_process();
    sleep_env.git_executable = Some(sleep_script);
    let sleep_engine = BranvioGitEngine::new(Arc::new(sleep_env), None);

    // 1) Cancelling buffered pickaxe kills child immediately rather than blocking on EOF.
    let cancel = CancelToken::new();
    let cancel_trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel_trigger.cancel();
    });
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        sleep_engine.search_commits_pickaxe(
            &root,
            PickaxeSearchRequest {
                query: "x".to_string(),
                is_regex: false,
                limit: 10,
                start_tips: Vec::new(),
                path_filters: Vec::new(),
            },
            cancel,
        ),
    )
    .await
    .expect("cancelled buffered command should return promptly");
    assert!(matches!(res, Err(GitEngineError::Cancelled)));

    // 2) Dropping CliCommandHandle kills the running child process via Drop + closed channel.
    let _ = std::fs::remove_file(&pid_file);
    let handle = sleep_engine.rebase(&root, "main").await.unwrap();
    let mut recorded_pid = None;
    for _ in 0..40 {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Some(line) = text.lines().next()
            && !line.trim().is_empty()
        {
            recorded_pid = Some(line.trim().to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pid_str = recorded_pid.expect("child process should write pid file");
    drop(handle);

    let mut child_dead = false;
    for _ in 0..40 {
        let alive = Command::new("kill")
            .args(["-0", &pid_str])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            child_dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        child_dead,
        "dropping CliCommandHandle must kill child PID {pid_str}"
    );

    // 3) Slow/unread receiver must NEVER stall a noisy child (>1500 stderr lines) or lose `Finished`.
    let flood_script = dir.path().join("fake_git_flood.sh");
    std::fs::write(
        &flood_script,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 1500 ]; do\n  echo \"line $i\" >&2\n  i=$((i + 1))\ndone\nexit 0\n",
    )
    .unwrap();
    let mut f_perms = std::fs::metadata(&flood_script).unwrap().permissions();
    f_perms.set_mode(0o755);
    std::fs::set_permissions(&flood_script, f_perms).unwrap();

    let mut flood_env = HydratedGitEnv::from_current_process();
    flood_env.git_executable = Some(flood_script);
    let flood_engine = BranvioGitEngine::new(Arc::new(flood_env), None);

    let mut flood_handle = flood_engine.rebase(&root, "main").await.unwrap();
    // Deliberately pause reading so kernel pipe + mpsc channel fill completely before child finishes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut saw_finished = None;
    let drain = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(ev) = flood_handle.events.recv().await {
            if let CliStreamEvent::Finished { success, exit_code } = ev {
                saw_finished = Some((success, exit_code));
            }
        }
    })
    .await;
    assert!(
        drain.is_ok(),
        "draining noisy command should complete quickly"
    );
    assert_eq!(saw_finished, Some((true, Some(0))));
}

#[cfg(unix)]
#[tokio::test]
async fn tier1_and_tier2_edge_cases_blob_cap_renames_typechange_broken_symlink_target_patch_and_locks()
 {
    let (dir, root, engine) = init_test_repo();

    // Commit 1: multi-line file `orig.txt`, binary/oversized `big.bin`, and `target_a.txt`/`target_b.txt`.
    let twenty_lines: String = (1..=20)
        .map(|i| format!("line {i} shared content\n"))
        .collect();
    std::fs::write(dir.path().join("orig.txt"), &twenty_lines).unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![b'A'; 1024]).unwrap();
    std::fs::write(
        dir.path().join("kind_item.txt"),
        "regular file content here\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("target_a.txt"), "a1\na2\na3\n").unwrap();
    std::fs::write(dir.path().join("target_b.txt"), "b1\nb2\nb3\n").unwrap();
    run_git(dir.path(), &["add", "."], &[]);
    run_git(dir.path(), &["commit", "-m", "initial files"], &[]);

    let big_blob_hex = run_git(dir.path(), &["rev-parse", "HEAD:big.bin"], &[]);
    let big_blob_oid = GitOid::from_hex(&big_blob_hex).unwrap();

    // Check `read_blob` rejects oversized blobs via header check (`BlobTooLarge`).
    let blob_err = engine
        .read_blob(&root, big_blob_oid, Some(64))
        .await
        .unwrap_err();
    assert!(matches!(
        blob_err,
        GitEngineError::BlobTooLarge {
            size_bytes: 1024,
            max_bytes: 64,
            ..
        }
    ));

    // Check `open_repo` rejects non-repo subdirectories instead of discovering ancestor repo.
    std::fs::create_dir_all(dir.path().join("plain_subdir")).unwrap();
    assert!(matches!(
        engine.open_repo(&dir.path().join("plain_subdir")).await,
        Err(GitEngineError::OpenRepoFailed { .. })
    ));

    // Commit 2: rename `orig.txt` -> `renamed.txt` with small edit + replace `kind_item.txt` blob with symlink.
    std::fs::remove_file(dir.path().join("orig.txt")).unwrap();
    std::fs::write(
        dir.path().join("renamed.txt"),
        twenty_lines.replace("line 10 shared content", "line 10 EDITED content"),
    )
    .unwrap();
    std::fs::remove_file(dir.path().join("kind_item.txt")).unwrap();
    std::os::unix::fs::symlink(
        "completely_different_link_target",
        dir.path().join("kind_item.txt"),
    )
    .unwrap();
    run_git(dir.path(), &["add", "-A"], &[]);
    run_git(dir.path(), &["commit", "-m", "rename and typechange"], &[]);
    let c2_oid = GitOid::from_hex(&run_git(dir.path(), &["rev-parse", "HEAD"], &[])).unwrap();

    let cancel = CancelToken::new();
    let details = engine
        .load_commit_details(&root, c2_oid, cancel.clone())
        .await
        .unwrap();
    let rename_delta = details
        .changed_files
        .iter()
        .find(|d| d.new_path.as_str() == "renamed.txt")
        .expect("renamed.txt should be detected as Rename");
    assert!(matches!(
        rename_delta.status,
        DeltaStatus::Renamed { similarity_pct } if similarity_pct >= 50
    ));
    assert!(rename_delta.additions > 0 && rename_delta.deletions > 0);
    assert_eq!(
        rename_delta.old_path.as_ref().map(RepoRelPath::as_str),
        Some("orig.txt")
    );

    let type_delta = details
        .changed_files
        .iter()
        .find(|d| d.new_path.as_str() == "kind_item.txt")
        .expect("kind_item.txt should be present");
    assert_eq!(type_delta.status, DeltaStatus::TypeChanged);
    assert_eq!(type_delta.old_mode, FileMode::Regular644);
    assert_eq!(type_delta.new_mode, FileMode::Symlink);

    // Stage a broken (dangling) symlink -> MUST stage as Added Symlink, NOT Removed!
    std::os::unix::fs::symlink(
        "nonexistent_dangling_target",
        dir.path().join("broken_link"),
    )
    .unwrap();
    engine
        .stage_files(&root, &[RepoRelPath::normalize("broken_link").unwrap()])
        .await
        .unwrap();
    let st = engine
        .working_tree_status(&root, cancel.clone())
        .await
        .unwrap();
    let staged_link = st
        .staged
        .iter()
        .find(|d| d.new_path.as_str() == "broken_link")
        .expect("broken symlink should be staged");
    assert_eq!(staged_link.status, DeltaStatus::Added);
    assert_eq!(staged_link.new_mode, FileMode::Symlink);
    engine
        .unstage_files(&root, &[RepoRelPath::normalize("broken_link").unwrap()])
        .await
        .unwrap();
    std::fs::remove_file(dir.path().join("broken_link")).unwrap();

    // `apply_reverse_patch_to_worktree` given a 2-file patch must modify ONLY `target_a.txt`!
    std::fs::write(dir.path().join("target_a.txt"), "a1\na2 MOD\na3\n").unwrap();
    std::fs::write(dir.path().join("target_b.txt"), "b1\nb2 KEEP\nb3\n").unwrap();
    let patch_out = Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "-R"])
        .output()
        .unwrap();
    assert!(patch_out.status.success());

    engine
        .apply_reverse_patch_to_worktree(
            &root,
            &RepoRelPath::normalize("target_a.txt").unwrap(),
            &patch_out.stdout,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("target_a.txt")).unwrap(),
        "a1\na2\na3\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("target_b.txt")).unwrap(),
        "b1\nb2 KEEP\nb3\n"
    );

    // `.git/index.lock`: Tier 1 status must succeed and never touch locks; Tier 2 returns IndexLocked!
    let lock_path = dir.path().join(".git/index.lock");
    std::fs::write(&lock_path, "locked").unwrap();
    let status_while_locked = engine.working_tree_status(&root, cancel).await;
    assert!(status_while_locked.is_ok());

    let stage_locked_err = engine
        .stage_files(&root, &[RepoRelPath::normalize("target_b.txt").unwrap()])
        .await
        .unwrap_err();
    assert!(matches!(
        stage_locked_err,
        GitEngineError::IndexLocked { .. }
    ));
    std::fs::remove_file(&lock_path).unwrap();
}

#[tokio::test]
async fn tier1_reads_conflict_stages_tier3_discards_paths_and_detects_exec_bit() {
    let (dir, root, engine) = init_test_repo();
    let script_path = dir.path().join("run.sh");
    std::fs::write(&script_path, "#!/bin/sh\necho base\n").unwrap();
    std::fs::write(dir.path().join("shared.txt"), "base line\n").unwrap();
    run_git(dir.path(), &["add", "."], &[]);
    run_git(dir.path(), &["commit", "-m", "base"], &[]);

    let cancel = CancelToken::new();

    // Verify chmod +x executable bit detection on Unix (`new_mode == Executable755`).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        let st = engine
            .working_tree_status(&root, cancel.clone())
            .await
            .unwrap();
        let delta = st
            .unstaged
            .iter()
            .find(|d| d.new_path.as_str() == "run.sh")
            .expect("chmod +x script should show in unstaged");
        assert_eq!(delta.old_mode, FileMode::Regular644);
        assert_eq!(delta.new_mode, FileMode::Executable755);
    }

    // Create an untracked file and modify run.sh, then discard both via Tier 3.
    std::fs::write(&script_path, "#!/bin/sh\necho dirty\n").unwrap();
    std::fs::write(dir.path().join("temp_untracked.log"), "noise\n").unwrap();
    let tracked = [RepoRelPath::normalize("run.sh").unwrap()];
    let untracked = [RepoRelPath::normalize("temp_untracked.log").unwrap()];
    let mut handle = engine
        .discard_worktree_paths(&root, &tracked, &untracked)
        .await
        .unwrap();
    let mut succeeded = None;
    while let Some(ev) = handle.events.recv().await {
        if let CliStreamEvent::Finished { exit_code, success } = ev {
            succeeded = Some((exit_code, success));
        }
    }
    assert_eq!(succeeded, Some((Some(0), true)));
    assert_eq!(
        std::fs::read_to_string(&script_path).unwrap(),
        "#!/bin/sh\necho base\n"
    );
    assert!(!dir.path().join("temp_untracked.log").exists());

    // Create conflicting branch and run merge to populate index stages 1, 2, 3.
    run_git(dir.path(), &["checkout", "-b", "topic"], &[]);
    std::fs::write(dir.path().join("shared.txt"), "theirs line\n").unwrap();
    run_git(dir.path(), &["add", "shared.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "theirs"], &[]);

    run_git(dir.path(), &["checkout", "main"], &[]);
    std::fs::write(dir.path().join("shared.txt"), "ours line\n").unwrap();
    run_git(dir.path(), &["add", "shared.txt"], &[]);
    run_git(dir.path(), &["commit", "-m", "ours"], &[]);

    let merge_out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "topic"])
        .output()
        .unwrap();
    assert!(!merge_out.status.success(), "expected merge conflict");

    let stages = engine
        .read_conflict_stages(
            &root,
            &RepoRelPath::normalize("shared.txt").unwrap(),
            1024 * 1024,
        )
        .await
        .unwrap();
    assert_eq!(stages.base.as_deref(), Some(b"base line\n".as_slice()));
    assert_eq!(stages.ours.as_deref(), Some(b"ours line\n".as_slice()));
    assert_eq!(stages.theirs.as_deref(), Some(b"theirs line\n".as_slice()));
}
