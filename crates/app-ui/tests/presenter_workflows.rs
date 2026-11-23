//! End-to-end integration tests for [`app_ui::BranvioPresenter`] against real temporary Git repositories,
//! including multi-repo tabs, chunked revwalk pagination, pickaxe filtering, AST-highlighted surgical
//! line patching, Tier 2 conflict prediction + 3-way merge resolution, and real binary `git rebase -i`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use app_ui::BranvioPresenter;
use app_ui::ports::{CoreAppHost, FileListLayoutMode};
use app_ui::presenter::command_palette::PaletteActionKind;
use app_ui::presenter::sidebar_tree::{BranchTreeNode, FileTreeRow, build_file_tree_rows};
use app_ui::presenter::{
    ActiveModal, AiFeature, AiUiError, BottomDrawerTab, DiffPreviewNotice, ForcePushChoice,
    RawConflictSide,
};
use app_ui::theme::ThemeAppearance;
use branvio_core::domain::{ConflictResolution, DiffLineOrigin, RepoOperationState};
use branvio_core::ids::{GitOid, RepoEpoch, RepoId};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::UiWakeupSink;
use branvio_core::platform::HydratedGitEnv;
use compact_str::CompactString;
use diff_merge::PatchDirection;
use git_engine::{BranvioGitEngine, HybridGitEngine, RebaseAction};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;

#[derive(Default)]
struct CountingWakeupSink {
    count: AtomicUsize,
}

impl UiWakeupSink for CountingWakeupSink {
    fn notify_repo_updated(&self, _repo_id: RepoId, _epoch: RepoEpoch) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

fn run_git(repo_dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Branvio E2E")
        .env("GIT_AUTHOR_EMAIL", "e2e@branvio.rs")
        .env("GIT_COMMITTER_NAME", "Branvio E2E")
        .env("GIT_COMMITTER_EMAIL", "e2e@branvio.rs")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git subprocess should execute");
    assert!(
        out.status.success(),
        "git {:?} failed: stdout={}, stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(default_branch: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    run_git(dir.path(), &["init", "-b", default_branch]);
    run_git(dir.path(), &["config", "user.name", "Branvio E2E"]);
    run_git(dir.path(), &["config", "user.email", "e2e@branvio.rs"]);
    run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
    dir
}

fn new_presenter(
    handle: &tokio::runtime::Handle,
    helper_exe: Option<PathBuf>,
) -> (BranvioPresenter, Arc<CountingWakeupSink>) {
    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let git_engine: Arc<dyn HybridGitEngine> =
        Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), helper_exe));
    let host = CoreAppHost::new(handle.clone(), git_env, git_engine, Vec::new());
    let wakeup = Arc::new(CountingWakeupSink::default());
    let presenter = BranvioPresenter::new(host, Arc::clone(&wakeup) as Arc<dyn UiWakeupSink>);
    (presenter, wakeup)
}

#[cfg(feature = "gpui-backend")]
#[test]
fn gpui_backend_exposes_expected_name_and_wakeup_sink() {
    use app_ui::UiRuntimeBackend;

    let backend = app_ui::GpuiRuntimeBackend::new();
    assert_eq!(backend.backend_name(), "PURE_RUST_GPUI (gpui + wgpu)");
    let sink = backend.create_wakeup_sink();
    sink.notify_repo_updated(RepoId(1), RepoEpoch(1));
}

#[tokio::test]
async fn multi_repo_tabs_mvcc_epochs_sidebar_and_worktrees() {
    let repo_a = init_repo("main");
    std::fs::write(
        repo_a.path().join(".gitmodules"),
        "[submodule \"deps/foo\"]\n\tpath = deps/foo\n\turl = https://example.com/foo.git\n\tbranch = main\n",
    )
    .unwrap();
    std::fs::write(repo_a.path().join("README.md"), "# Repo A\n").unwrap();
    run_git(repo_a.path(), &["add", "."]);
    run_git(repo_a.path(), &["commit", "-m", "init A"]);
    let head_hex = run_git(repo_a.path(), &["rev-parse", "HEAD"]);
    run_git(repo_a.path(), &["checkout", "-b", "feature/auth/oauth"]);
    run_git(repo_a.path(), &["checkout", "-b", "feature/auth/saml"]);
    run_git(repo_a.path(), &["checkout", "main"]);
    run_git(
        repo_a.path(),
        &["update-ref", "refs/remotes/origin/main", &head_hex],
    );

    let repo_b = init_repo("main");
    std::fs::write(repo_b.path().join("README.md"), "# Repo B\n").unwrap();
    run_git(repo_b.path(), &["add", "."]);
    run_git(repo_b.path(), &["commit", "-m", "init B"]);

    let wt_parent = TempDir::new().unwrap();
    let wt_path = wt_parent.path().join("wt-hotfix");

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, wakeup) = new_presenter(&rt, None);

    let id_a = presenter.open_repository_tab(repo_a.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.tabs
                .iter()
                .find(|t| t.repo_id == id_a)
                .is_some_and(|t| t.snapshot.epoch.0 >= 1 && !t.snapshot.graph.commits.is_empty())
        })
        .await
        .unwrap();

    assert!(wakeup.count.load(Ordering::Relaxed) >= 1);

    let (oid_a, sections_ok) = {
        let tab_a = presenter.active_tab().unwrap();
        assert_eq!(tab_a.submodules.len(), 1);
        assert_eq!(tab_a.submodules[0].path.as_str(), "deps/foo");
        let sections = tab_a.sidebar_sections();
        let ok = matches!(
            &sections.local_branches[0],
            BranchTreeNode::Folder { name, children, .. }
                if name == "feature" && matches!(&children[0], BranchTreeNode::Folder { name: sub, .. } if sub == "auth")
        ) && sections.remotes.len() == 1
            && sections.remotes[0].remote_name.as_str() == "origin";
        (tab_a.snapshot.head_oid.unwrap(), ok)
    };
    assert!(sections_ok);

    // Command palette fuzzy lookup across dynamic branches & actions
    let (entries, matches) = presenter.search_command_palette("oauth");
    assert!(!matches.is_empty());
    assert!(
        entries[matches[0].entry_index]
            .item
            .title
            .contains("feature/auth/oauth")
    );
    presenter.execute_palette_action(PaletteActionKind::ToggleThemeAppearance);
    assert_eq!(presenter.theme.appearance, ThemeAppearance::Light);
    presenter.execute_palette_action(PaletteActionKind::ToggleFileListLayout);
    assert_eq!(presenter.file_list_layout, FileListLayoutMode::FolderTree);

    // Flat vs FolderTree file rows helper on commit deltas
    presenter.select_commit_row(oid_a);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .and_then(|t| t.selected_commit_details.as_ref())
                .is_some()
        })
        .await
        .unwrap();
    let files = &presenter
        .active_tab()
        .unwrap()
        .selected_commit_details
        .as_ref()
        .unwrap()
        .changed_files;
    let tree_rows = build_file_tree_rows(files, FileListLayoutMode::FolderTree, &HashSet::new());
    assert!(
        tree_rows
            .iter()
            .any(|r| matches!(r, FileTreeRow::File { .. }))
    );

    // Create linked worktree & open second repository tab
    presenter.create_worktree(
        HostPath::from_path_buf(wt_path.clone()),
        "hotfix".into(),
        true,
    );
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.tabs
                .iter()
                .find(|t| t.repo_id == id_a)
                .is_some_and(|t| !t.cli_running && t.worktrees.len() >= 2)
        })
        .await
        .unwrap();

    let id_b = presenter.open_repository_tab(repo_b.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.tabs
                .iter()
                .find(|t| t.repo_id == id_b)
                .is_some_and(|t| t.snapshot.epoch.0 >= 1 && t.snapshot.head_oid.is_some())
        })
        .await
        .unwrap();
    assert_eq!(presenter.tabs.len(), 2);
    assert!(presenter.select_tab(id_a));
    assert!(presenter.close_tab(id_b));
    assert_eq!(presenter.tabs.len(), 1);
}

#[tokio::test]
async fn chunked_revwalk_pagination_and_pickaxe_filtering() {
    let repo = init_repo("main");
    for idx in 1..=7u32 {
        let filename = if idx == 5 { "needle.rs" } else { "history.txt" };
        let body = if idx == 5 {
            "pub const MAGIC_PICKAXE_TOKEN: u32 = 42;\n".to_string()
        } else {
            format!("commit step {idx}\n")
        };
        std::fs::write(repo.path().join(filename), body).unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", &format!("step {idx}")]);
    }

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _) = new_presenter(&rt, None);
    presenter.initial_chunk_size = 3;

    let id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.tabs
                .iter()
                .find(|t| t.repo_id == id)
                .is_some_and(|t| t.snapshot.epoch.0 >= 1 && t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();

    {
        let tab = presenter.active_tab().unwrap();
        assert_eq!(tab.snapshot.graph.commits.len(), 3);
        assert_eq!(tab.snapshot.graph.commit_rows.len(), 3);
        assert!(tab.snapshot.graph.has_more_history);
        assert!(!tab.revwalk_exhausted);
    }

    presenter.load_more_history(3);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| !t.is_loading_more && t.snapshot.graph.commits.len() == 6)
        })
        .await
        .unwrap();

    presenter.load_more_history(3);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| !t.is_loading_more && t.revwalk_exhausted)
        })
        .await
        .unwrap();

    let tab = presenter.active_tab().unwrap();
    assert_eq!(tab.snapshot.graph.commits.len(), 7);
    assert_eq!(tab.snapshot.graph.commit_rows.len(), 7);
    assert!(!tab.snapshot.graph.has_more_history);

    // Live Tier 3 Pickaxe search narrows commits exclusively to step 5
    presenter.set_search_filter("MAGIC_PICKAXE_TOKEN".to_string(), true, false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.graph.commits.len() == 1
            })
        })
        .await
        .unwrap();
    assert_eq!(
        presenter.active_tab().unwrap().snapshot.graph.commits[0]
            .summary
            .as_str(),
        "step 5"
    );

    // Reset search filter restores all 7 commits when initial chunk covers history
    presenter.initial_chunk_size = 50;
    presenter.set_search_filter(String::new(), false, false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.graph.commits.len() == 7
            })
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn surgical_line_patch_staging_unstaging_discard_and_commit_amend() {
    let repo = init_repo("main");
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn compute(x: i32) -> i32 {\n    let base = x + 1;\n    base\n}\n",
    )
    .unwrap();
    run_git(repo.path(), &["add", "lib.rs"]);
    run_git(repo.path(), &["commit", "-m", "init lib"]);

    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn compute(x: i32) -> i32 {\n    let step_one = 10;\n    let base = x + 1;\n    let step_two = 20;\n    base + step_one + step_two\n}\n",
    )
    .unwrap();

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _) = new_presenter(&rt, None);
    let _id = presenter.open_repository_tab(repo.path()).unwrap();

    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.unstaged.len() == 1)
        })
        .await
        .unwrap();

    let delta = presenter
        .active_tab()
        .unwrap()
        .snapshot
        .working_tree
        .unstaged[0]
        .clone();
    assert!(
        presenter
            .active_tab()
            .unwrap()
            .snapshot
            .graph
            .wip_row
            .is_some()
    );
    presenter.select_file_delta(delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .and_then(|t| t.active_diff_patch.as_ref())
                .is_some()
        })
        .await
        .unwrap();

    let step_one_line_idx = {
        let patch = presenter
            .active_tab()
            .unwrap()
            .active_diff_patch
            .as_ref()
            .unwrap();
        assert_eq!(patch.hunks.len(), 1);
        let hunk = &patch.hunks[0];
        assert!(hunk.enclosing_symbol.is_some());
        assert!(
            hunk.lines.iter().any(|l| !l.syntax_tokens.is_empty()),
            "tree-sitter syntax tokens should annotate Rust diff lines"
        );
        hunk.lines
            .iter()
            .position(|l| {
                l.origin == DiffLineOrigin::Addition
                    && patch.line_text(l).contains("let step_one = 10;")
            })
            .expect("step_one addition line must exist")
    };

    // Stage ONLY `let step_one = 10;` via synthetic index patch
    presenter.toggle_diff_line_selection(0, step_one_line_idx);
    presenter.apply_hunk_or_selected_lines(0, PatchDirection::StageToIndex);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none()
                    && t.snapshot.working_tree.staged.len() == 1
                    && t.snapshot.working_tree.unstaged.len() == 1
            })
        })
        .await
        .unwrap();

    let cached_diff = run_git(repo.path(), &["diff", "--cached"]);
    assert!(cached_diff.contains("let step_one = 10;"));
    assert!(!cached_diff.contains("let step_two = 20;"));

    // Select staged delta and unstage that hunk surgically
    let staged_delta = presenter.active_tab().unwrap().snapshot.working_tree.staged[0].clone();
    presenter.select_file_delta(staged_delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .and_then(|t| t.active_diff_patch.as_ref())
                .is_some_and(|patch| patch.delta.area == branvio_core::domain::DeltaArea::Staged)
        })
        .await
        .unwrap();
    presenter.apply_hunk_or_selected_lines(0, PatchDirection::UnstageFromIndex);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.working_tree.staged.is_empty()
            })
        })
        .await
        .unwrap();

    // Discard ONLY `let step_two = 20;` line in worktree while keeping `step_one`
    std::fs::write(
        repo.path().join("lib.rs"),
        "pub fn compute(x: i32) -> i32 {\n    let step_one = 10;\n    let base = x + 1;\n    let step_two = 20;\n    base\n}\n",
    )
    .unwrap();
    presenter.trigger_refresh(
        presenter.active_repo_id.unwrap(),
        branvio_core::pipeline::DirtyScopeFlags::WORKTREE,
    );
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();
    let unstaged_delta = presenter
        .active_tab()
        .unwrap()
        .snapshot
        .working_tree
        .unstaged[0]
        .clone();
    presenter.select_file_delta(unstaged_delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .and_then(|t| t.active_diff_patch.as_ref())
                .is_some()
        })
        .await
        .unwrap();
    let step_two_idx = {
        let patch = presenter
            .active_tab()
            .unwrap()
            .active_diff_patch
            .as_ref()
            .unwrap();
        patch.hunks[0]
            .lines
            .iter()
            .position(|l| {
                l.origin == DiffLineOrigin::Addition
                    && patch.line_text(l).contains("let step_two = 20;")
            })
            .unwrap()
    };
    presenter.toggle_diff_line_selection(0, step_two_idx);
    presenter.apply_hunk_or_selected_lines(0, PatchDirection::DiscardInWorktree);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| !t.is_mutating_index && t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();
    assert!(
        presenter.status_banner.as_ref().is_none_or(|b| !b.is_error),
        "discard patch failed: {:?}",
        presenter.status_banner
    );
    let on_disk = std::fs::read_to_string(repo.path().join("lib.rs")).unwrap();
    assert!(on_disk.contains("let step_one = 10;"));
    assert!(!on_disk.contains("let step_two = 20;"));

    // Stage remaining change & commit + amend via Tier 3
    presenter.stage_all_working_tree();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none()
                    && t.snapshot.working_tree.unstaged.is_empty()
                    && t.snapshot.working_tree.staged.len() == 1
            })
        })
        .await
        .unwrap();

    {
        let tab = presenter.active_tab_mut().unwrap();
        tab.commit_subject = "feat: add step_one".to_string();
        tab.commit_body = "clean commit body".to_string();
        assert!(tab.can_commit());
    }
    presenter.commit_staged();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.working_tree.total_dirty_count() == 0
                    && t.snapshot.graph.wip_row.is_none()
                    && t.snapshot.graph.commits.len() == 2
            })
        })
        .await
        .unwrap();
    assert_eq!(
        run_git(repo.path(), &["log", "-1", "--format=%B"]),
        "feat: add step_one\n\nclean commit body"
    );

    {
        let tab = presenter.active_tab_mut().unwrap();
        tab.commit_amend = true;
        tab.commit_subject = "feat: amended step_one".to_string();
        tab.commit_body.clear();
    }
    presenter.commit_staged();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.graph.commits[0].summary.as_str() == "feat: amended step_one"
            })
        })
        .await
        .unwrap();
    assert_eq!(
        presenter.active_tab().unwrap().snapshot.graph.commits.len(),
        2
    );
}

#[tokio::test]
async fn drag_drop_predictive_conflicts_and_three_way_merge_resolution() {
    let repo = init_repo("main");
    std::fs::write(
        repo.path().join("config.txt"),
        "host=localhost\nport=8080\n",
    )
    .unwrap();
    run_git(repo.path(), &["add", "config.txt"]);
    run_git(repo.path(), &["commit", "-m", "base"]);

    run_git(repo.path(), &["checkout", "-b", "feature/tls"]);
    std::fs::write(
        repo.path().join("config.txt"),
        "host=localhost\nport=8443\n",
    )
    .unwrap();
    run_git(repo.path(), &["add", "config.txt"]);
    run_git(repo.path(), &["commit", "-m", "theirs"]);
    let theirs_oid = GitOid::from_hex(&run_git(repo.path(), &["rev-parse", "HEAD"])).unwrap();

    run_git(repo.path(), &["checkout", "main"]);
    std::fs::write(
        repo.path().join("config.txt"),
        "host=localhost\nport=9090\n",
    )
    .unwrap();
    run_git(repo.path(), &["add", "config.txt"]);
    run_git(repo.path(), &["commit", "-m", "ours"]);
    let ours_oid = GitOid::from_hex(&run_git(repo.path(), &["rev-parse", "HEAD"])).unwrap();

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _) = new_presenter(&rt, None);
    let _id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.epoch.0 >= 1 && t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();

    // Drag feature/tls onto main -> Tier 2 predicts conflict without mutating repo
    presenter.update_drag_hover(
        CompactString::const_new("feature/tls"),
        Some(theirs_oid),
        Some(CompactString::const_new("main")),
        Some(ours_oid),
    );
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.drag_preview
                .as_ref()
                .and_then(|d| d.prediction.as_ref())
                .is_some()
        })
        .await
        .unwrap();
    presenter.finish_drag_drop_popover();

    match &presenter.modal {
        ActiveModal::DropBranchAction { preview, .. } => {
            let pred = preview.prediction.as_ref().unwrap();
            assert!(!pred.will_succeed_cleanly);
            assert_eq!(pred.conflicts.len(), 1);
            assert_eq!(pred.conflicts[0].path.as_str(), "config.txt");
        }
        other => panic!("expected DropBranchAction modal, got {other:?}"),
    }

    // Execute real merge -> enters Merging state with conflicted config.txt
    presenter.modal = ActiveModal::None;
    presenter.merge_revision("feature/tls".to_string(), false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.operation_state == RepoOperationState::Merging
                    && !t.snapshot.working_tree.conflicted.is_empty()
            })
        })
        .await
        .unwrap();

    let conflict_delta = presenter
        .active_tab()
        .unwrap()
        .snapshot
        .working_tree
        .conflicted[0]
        .clone();
    presenter.select_file_delta(conflict_delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .and_then(|t| t.conflict_session.as_ref())
                .is_some()
        })
        .await
        .unwrap();

    {
        let doc = &presenter
            .active_tab()
            .unwrap()
            .conflict_session
            .as_ref()
            .unwrap()
            .document;
        assert_eq!(doc.regions.len(), 1);
        assert_eq!(doc.unresolved_count(), 1);
    }

    presenter.resolve_conflict_region(0, ConflictResolution::TakeBothOursFirst);
    assert!(
        presenter
            .active_tab()
            .unwrap()
            .conflict_session
            .as_ref()
            .unwrap()
            .document
            .is_fully_resolved()
    );

    presenter.save_and_mark_conflict_resolved(true);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.conflict_session.is_none()
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.working_tree.conflicted.is_empty()
                    && !t.snapshot.working_tree.staged.is_empty()
            })
        })
        .await
        .unwrap();

    let resolved_disk = std::fs::read_to_string(repo.path().join("config.txt")).unwrap();
    assert_eq!(resolved_disk, "host=localhost\nport=9090\nport=8443\n");

    let resolved_rel = RepoRelPath::normalize("config.txt").unwrap();
    let _ = resolved_rel;

    presenter.active_tab_mut().unwrap().commit_subject = "Merge feature/tls resolved".to_string();
    presenter.commit_staged();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.operation_state == RepoOperationState::Clean
            })
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn interactive_rebase_end_to_end_via_compiled_app_ui_helper() {
    let helper_exe = PathBuf::from(env!("CARGO_BIN_EXE_app-ui"));
    assert!(
        helper_exe.is_file(),
        "compiled app-ui binary missing at {}",
        helper_exe.display()
    );

    let repo = init_repo("main");
    std::fs::write(repo.path().join("app.txt"), "base\n").unwrap();
    run_git(repo.path(), &["add", "app.txt"]);
    run_git(repo.path(), &["commit", "-m", "chore: base"]);

    run_git(repo.path(), &["checkout", "-b", "feature/rebase-e2e"]);
    for (idx, msg) in [
        "feat: commit one",
        "feat: commit two",
        "temp: drop commit three",
        "feat: commit four",
    ]
    .iter()
    .enumerate()
    {
        let fname = format!("f{}.txt", idx + 1);
        std::fs::write(repo.path().join(&fname), format!("{fname} content\n")).unwrap();
        run_git(repo.path(), &["add", &fname]);
        run_git(repo.path(), &["commit", "-m", msg]);
    }

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _) = new_presenter(&rt, Some(helper_exe));
    let _id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.graph.commits.len() == 5
            })
        })
        .await
        .unwrap();

    // First 4 commits in newest-first order: [c4, c3, c2, c1]
    let top_four = presenter.active_tab().unwrap().snapshot.graph.commits[..4].to_vec();
    assert_eq!(top_four[0].summary.as_str(), "feat: commit four");
    assert_eq!(top_four[3].summary.as_str(), "feat: commit one");

    presenter.open_interactive_rebase_with_commits(CompactString::const_new("main"), &top_four);

    // Planner now holds oldest-first: [0: c1, 1: c2, 2: c3, 3: c4]
    if let ActiveModal::InteractiveRebase {
        ref mut planner, ..
    } = presenter.modal
    {
        // Reorder c4 above c3 -> [0: c1, 1: c2, 2: c4, 3: c3]
        assert!(planner.move_up(3));
        planner.set_action(0, RebaseAction::Pick);
        planner.set_action(1, RebaseAction::Squash);
        planner.set_message(1, Some("feat: melded c1 + c2 via squash\n".to_string()));
        planner.set_action(2, RebaseAction::Reword);
        planner.set_message(2, Some("feat: reworded c4 via done fallback\n".to_string()));
        planner.set_action(3, RebaseAction::Drop);
    } else {
        panic!("expected InteractiveRebase modal");
    }

    presenter.execute_active_interactive_rebase();
    presenter
        .wait_until(Duration::from_secs(15), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.operation_state == RepoOperationState::Clean
                    && t.snapshot.graph.commits.len() == 3
            })
        })
        .await
        .unwrap();

    assert!(repo.path().join("f1.txt").exists());
    assert!(repo.path().join("f2.txt").exists());
    assert!(
        !repo.path().join("f3.txt").exists(),
        "dropped commit c3 must remove f3.txt"
    );
    assert!(repo.path().join("f4.txt").exists());

    let subjects = run_git(repo.path(), &["log", "--format=%s", "main..HEAD"]);
    let lines: Vec<&str> = subjects.lines().collect();
    assert_eq!(
        lines,
        vec![
            "feat: reworded c4 via done fallback",
            "feat: melded c1 + c2 via squash",
        ]
    );
}

#[tokio::test]
async fn non_utf8_diff_notice_whole_file_stage_and_raw_conflict_resolution() {
    let repo = init_repo("main");
    let raw_v1 = b"header\n\x82\xa0_shiftjis_v1\n";
    let raw_ours = b"header\n\x82\xa2_shiftjis_ours\n";
    let raw_theirs = b"header\n\x93\xfa_shiftjis_theirs\n";

    std::fs::write(repo.path().join("legacy.txt"), raw_v1).unwrap();
    run_git(repo.path(), &["add", "legacy.txt"]);
    run_git(repo.path(), &["commit", "-m", "init legacy"]);

    // Create diverged feature branch editing the same non-UTF-8 file
    run_git(repo.path(), &["checkout", "-b", "feature-legacy"]);
    std::fs::write(repo.path().join("legacy.txt"), raw_theirs).unwrap();
    run_git(repo.path(), &["add", "legacy.txt"]);
    run_git(repo.path(), &["commit", "-m", "feature raw bytes"]);

    run_git(repo.path(), &["checkout", "main"]);
    std::fs::write(repo.path().join("legacy.txt"), raw_ours).unwrap();
    run_git(repo.path(), &["add", "legacy.txt"]);
    run_git(repo.path(), &["commit", "-m", "main raw bytes"]);

    // Modify unstaged with invalid UTF-8 bytes -> must trigger UnsupportedEncoding diff notice
    let raw_unstaged = b"header\n\xff\xfe_raw_edit\n";
    std::fs::write(repo.path().join("legacy.txt"), raw_unstaged).unwrap();

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _) = new_presenter(&rt, None);
    let _repo_id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.unstaged.len() == 1)
        })
        .await
        .unwrap();

    let delta = presenter
        .active_tab()
        .unwrap()
        .snapshot
        .working_tree
        .unstaged[0]
        .clone();
    presenter.select_file_delta(delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                matches!(
                    t.diff_notice,
                    Some(DiffPreviewNotice::UnsupportedEncoding {
                        conflict_labels: None,
                        ..
                    })
                )
            })
        })
        .await
        .unwrap();

    // Whole-file stage preserves raw bytes verbatim without U+FFFD replacement
    let rel = RepoRelPath::normalize("legacy.txt").unwrap();
    presenter.stage_paths(vec![rel.clone()]);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.staged.len() == 1)
        })
        .await
        .unwrap();
    let staged_out = Command::new("git")
        .current_dir(repo.path())
        .args(["show", ":legacy.txt"])
        .output()
        .unwrap();
    assert_eq!(staged_out.stdout.as_slice(), raw_unstaged);

    // Trigger a merge conflict on the non-UTF-8 file
    run_git(repo.path(), &["reset", "--hard", "HEAD"]);
    let merge_status = Command::new("git")
        .current_dir(repo.path())
        .args(["merge", "feature-legacy"])
        .output()
        .unwrap();
    assert!(!merge_status.status.success());

    let repo_id = presenter.active_repo_id.unwrap();
    presenter.trigger_refresh(repo_id, branvio_core::pipeline::DirtyScopeFlags::WORKTREE);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.conflicted.len() == 1)
        })
        .await
        .unwrap();

    let conflict_delta = presenter
        .active_tab()
        .unwrap()
        .snapshot
        .working_tree
        .conflicted[0]
        .clone();
    presenter.select_file_delta(conflict_delta);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.conflict_session.is_none()
                    && matches!(
                        t.diff_notice,
                        Some(DiffPreviewNotice::UnsupportedEncoding {
                            conflict_labels: Some(_),
                            ..
                        })
                    )
            })
        })
        .await
        .unwrap();

    // Accept Theirs (raw bytes) and stage
    presenter.resolve_non_utf8_conflict_side(rel, RawConflictSide::Theirs, true);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none()
                    && t.diff_notice.is_none()
                    && t.snapshot.working_tree.conflicted.is_empty()
            })
        })
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(repo.path().join("legacy.txt")).unwrap(),
        raw_theirs
    );
}

#[tokio::test]
async fn phase5_undo_redo_and_pushed_history_confirmation_workflow() {
    let bare = TempDir::new().unwrap();
    run_git(bare.path(), &["init", "--bare", "-b", "main"]);

    let repo = init_repo("main");
    std::fs::write(repo.path().join("notes.txt"), "v1\n").unwrap();
    run_git(repo.path(), &["add", "notes.txt"]);
    run_git(repo.path(), &["commit", "-m", "chore: initial"]);
    run_git(
        repo.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    run_git(repo.path(), &["push", "-u", "origin", "main"]);

    let db_dir = TempDir::new().unwrap();
    let rt = tokio::runtime::Handle::current();
    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let git_engine: Arc<dyn HybridGitEngine> =
        Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), None));
    let store: Arc<dyn oplog_undo::OpLogStore> = Arc::new(
        oplog_undo::RedbOpLogStore::open_at(db_dir.path().join("e2e_oplog.redb")).unwrap(),
    );
    let undo_engine: Arc<dyn oplog_undo::UndoRedoEngine> =
        Arc::new(oplog_undo::BranvioUndoEngine::new(
            Arc::clone(&store),
            Arc::clone(&git_engine),
            Arc::clone(&git_env),
        ));
    let host = CoreAppHost::new(rt.clone(), git_env, git_engine, Vec::new())
        .with_optional_services(Some(undo_engine), None, None)
        .with_phase5_runtime(Some(store), None, None, None, None);
    let wakeup = Arc::new(CountingWakeupSink::default());
    let mut presenter = BranvioPresenter::new(host, wakeup);

    presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none() && !t.snapshot.can_undo)
        })
        .await
        .unwrap();

    // Stage and commit v2 through presenter
    std::fs::write(repo.path().join("notes.txt"), "v2\n").unwrap();
    presenter.stage_paths(vec![RepoRelPath::normalize("notes.txt").unwrap()]);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.staged.len() == 1)
        })
        .await
        .unwrap();

    {
        let tab = presenter.active_tab_mut().unwrap();
        tab.commit_subject = "feat: add v2 notes".to_string();
    }
    let head_v1 = run_git(repo.path(), &["rev-parse", "HEAD"]);
    presenter.commit_staged();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                !t.cli_running
                    && t.active_refresh_cancel.is_none()
                    && t.snapshot.can_undo
                    && !t.snapshot.can_redo
            })
        })
        .await
        .unwrap();

    let head_v2 = run_git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(head_v1, head_v2);

    // Local Undo works immediately without confirmation modal
    presenter.undo_last_operation(false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && !t.snapshot.can_undo && t.snapshot.can_redo
            })
        })
        .await
        .unwrap();
    assert_eq!(run_git(repo.path(), &["rev-parse", "HEAD"]), head_v1);

    // Redo restores v2 commit cleanly
    presenter.redo_last_operation(false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.can_undo && !t.snapshot.can_redo
            })
        })
        .await
        .unwrap();
    assert_eq!(run_git(repo.path(), &["rev-parse", "HEAD"]), head_v2);

    // Now push v2 to bare remote -> Undo without confirmation must open ConfirmRewritePushedUndo modal!
    run_git(repo.path(), &["push", "origin", "main"]);
    presenter.undo_last_operation(false);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            matches!(
                p.modal,
                ActiveModal::ConfirmRewritePushedUndo { is_redo: false, .. }
            )
        })
        .await
        .unwrap();

    // Confirm override reverts HEAD cleanly back to v1
    presenter.modal = ActiveModal::None;
    presenter.undo_last_operation(true);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none() && t.snapshot.can_redo)
        })
        .await
        .unwrap();
    assert_eq!(run_git(repo.path(), &["rev-parse", "HEAD"]), head_v1);
}

#[tokio::test]
async fn phase5_watcher_pty_forge_and_ai_copilot_end_to_end() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Spin up lightweight mock HTTP server serving both Forge GitHub API (/api/v3/...) and Ollama (/api/chat)
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let path = raw
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path.contains("/pulls?") {
                    r#"[{"number":7,"title":"feat: phase5 PR","body":"body","state":"open","draft":false,"merged_at":null,"html_url":"http://local/pr/7","user":{"login":"dev"},"head":{"ref":"feat/p5","sha":"1111111111111111111111111111111111111111"},"base":{"ref":"main","sha":"2222222222222222222222222222222222222222"}}]"#
                } else if path.contains("/pulls/7") {
                    r#"{"number":7,"title":"feat: phase5 PR","body":"body","state":"open","draft":false,"merged_at":null,"html_url":"http://local/pr/7","user":{"login":"dev"},"head":{"ref":"feat/p5","sha":"1111111111111111111111111111111111111111"},"base":{"ref":"main","sha":"2222222222222222222222222222222222222222"}}"#
                } else if path.contains("/check-runs") {
                    r#"{"check_runs":[{"name":"build","status":"completed","conclusion":"success","html_url":null}]}"#
                } else {
                    r#"{"message":{"content":"{\"subject\":\"feat(ai): generated summary\",\"body\":\"Generated from staged diff.\"}"}}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let repo = init_repo("main");
    std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
    run_git(repo.path(), &["add", "main.rs"]);
    run_git(repo.path(), &["commit", "-m", "init"]);
    run_git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            &format!("http://127.0.0.1:{port}/acme/app.git"),
        ],
    );

    let rt = tokio::runtime::Handle::current();
    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let git_engine: Arc<dyn HybridGitEngine> =
        Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), None));
    let watcher: Arc<dyn fs_watcher::RepoWatcher> =
        Arc::new(fs_watcher::NotifyRepoWatcher::new(rt.clone()));
    let pty: Arc<dyn terminal_pty::TerminalBackend> =
        Arc::new(terminal_pty::PortablePtyBackend::new(Arc::clone(&git_env)));
    let forge: Arc<dyn forge_client::ForgeProvider> =
        Arc::new(forge_client::UnifiedForgeProvider::new(None).unwrap());
    let ai: Arc<dyn ai_copilot::AiCopilotProvider> =
        Arc::new(ai_copilot::HttpAiCopilotProvider::new(None).unwrap());
    let ai_cfg = ai_copilot::AiEndpointConfig {
        kind: ai_copilot::AiProviderKind::OllamaLocal,
        base_url: format!("http://127.0.0.1:{port}"),
        model_name: CompactString::const_new("qwen2.5-coder:14b"),
        custom_commit_style_hint: None,
        allow_cloud_upload: false,
    };

    let host = CoreAppHost::new(rt.clone(), git_env, git_engine, Vec::new())
        .with_optional_services(None, Some(forge), Some(ai))
        .with_phase5_runtime(None, Some(watcher), Some(pty), None, Some(ai_cfg));
    let mut presenter = BranvioPresenter::new(host, Arc::new(CountingWakeupSink::default()));

    presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none() && t.watch_handle.is_some())
        })
        .await
        .unwrap();

    // 1. Let fs-watcher settle then modify main.rs externally -> auto-refresh WITHOUT calling trigger_refresh!
    tokio::time::sleep(Duration::from_millis(250)).await;
    std::fs::write(
        repo.path().join("main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    presenter
        .wait_until(Duration::from_secs(5), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.unstaged.len() == 1)
        })
        .await
        .unwrap();

    // Stage change and generate AI commit message via mock Ollama server
    presenter.stage_paths(vec![RepoRelPath::normalize("main.rs").unwrap()]);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.snapshot.working_tree.staged.len() == 1)
        })
        .await
        .unwrap();

    presenter.generate_ai_commit_message();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| !t.ai_commit_busy && !t.commit_subject.is_empty())
        })
        .await
        .unwrap();
    assert_eq!(
        presenter.active_tab().unwrap().commit_subject,
        "feat(ai): generated summary"
    );

    // 2. Fetch Pull Requests via Forge mock server
    presenter.refresh_pull_requests();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| !t.pull_requests_loading && !t.pull_requests.is_empty())
        })
        .await
        .unwrap();
    assert_eq!(presenter.active_tab().unwrap().pull_requests[0].number, 7);

    // 3. Embedded PTY terminal opens, accepts shell command, and updates terminal_snapshot
    presenter.open_or_focus_terminal();
    assert_eq!(presenter.bottom_drawer_tab, BottomDrawerTab::Terminal);
    presenter
        .wait_until(Duration::from_secs(5), |p| {
            p.active_tab().is_some_and(|t| t.terminal_session.is_some())
        })
        .await
        .unwrap();
    presenter.send_terminal_command_line("printf 'PTY_%s_OK\\n' 'E2E'");
    presenter
        .wait_until(Duration::from_secs(5), |p| {
            p.active_tab().is_some_and(|t| {
                t.terminal_snapshot.as_ref().is_some_and(|snap| {
                    let s: String = snap.cells.iter().map(|c| c.ch).collect();
                    s.contains("PTY_E2E_OK")
                })
            })
        })
        .await
        .unwrap();
    presenter.close_terminal_session();
}

/// Waits for the active tab's in-flight CLI command (push) to finish.
async fn wait_cli_idle(presenter: &mut BranvioPresenter) {
    presenter
        .wait_until(Duration::from_secs(20), |p| {
            p.active_tab().is_some_and(|t| !t.cli_running)
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn force_push_lease_uses_oid_captured_at_confirmation_and_plain_push_never_forces() {
    let bare = TempDir::new().unwrap();
    run_git(bare.path(), &["init", "--bare", "-b", "main"]);
    let bare_str = bare.path().to_str().unwrap();

    let repo = init_repo("main");
    std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    run_git(repo.path(), &["add", "a.txt"]);
    run_git(repo.path(), &["commit", "-m", "A"]);
    run_git(repo.path(), &["remote", "add", "origin", bare_str]);
    run_git(repo.path(), &["push", "-u", "origin", "main"]);
    let oid_a = run_git(repo.path(), &["rev-parse", "HEAD"]);

    // Rewrite local history so `main` diverges from `origin/main`.
    run_git(repo.path(), &["commit", "--amend", "-m", "A rewritten"]);
    let oid_b = run_git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(oid_a, oid_b);

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _wakeup) = new_presenter(&rt, None);
    presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.refs.iter().any(|b| b.is_head)
            })
        })
        .await
        .unwrap();

    // 1. Plain push is a regular push: the diverged branch is rejected, the remote is untouched.
    presenter.push_current();
    wait_cli_idle(&mut presenter).await;
    assert_eq!(run_git(bare.path(), &["rev-parse", "main"]), oid_a);

    // 2. Opening the force-push confirmation captures origin/main (= A) as the lease value.
    presenter.request_force_push();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            matches!(p.modal, ActiveModal::ConfirmForcePush { .. })
        })
        .await
        .unwrap();
    match &presenter.modal {
        ActiveModal::ConfirmForcePush {
            branch,
            expected_remote_oid,
            ..
        } => {
            assert_eq!(branch.as_str(), "main");
            assert_eq!(
                *expected_remote_oid,
                Some(GitOid::from_hex(&oid_a).unwrap())
            );
        }
        other => panic!("unexpected modal {other:?}"),
    }
    assert_eq!(
        app_ui::presenter::force_push_title("main"),
        "Force push main?"
    );

    // 3. While the dialog is open a teammate pushes C and a background fetch advances origin/main.
    let other_parent = TempDir::new().unwrap();
    run_git(other_parent.path(), &["clone", bare_str, "other"]);
    let other = other_parent.path().join("other");
    run_git(&other, &["config", "commit.gpgsign", "false"]);
    std::fs::write(other.join("c.txt"), "c\n").unwrap();
    run_git(&other, &["add", "c.txt"]);
    run_git(&other, &["commit", "-m", "C from teammate"]);
    run_git(&other, &["push", "origin", "main"]);
    let oid_c = run_git(&other, &["rev-parse", "HEAD"]);
    run_git(repo.path(), &["fetch", "origin"]);
    assert_eq!(run_git(repo.path(), &["rev-parse", "origin/main"]), oid_c);

    // 4. "Force push safely" must use the lease captured in step 2 (A), not the fetched C,
    //    so the teammate's commit survives.
    presenter.confirm_force_push(ForcePushChoice::WithLease);
    assert!(matches!(presenter.modal, ActiveModal::None));
    wait_cli_idle(&mut presenter).await;
    assert_eq!(run_git(bare.path(), &["rev-parse", "main"]), oid_c);

    // 5. Re-opening the dialog captures the new remote state; the safe force push now succeeds.
    presenter.request_force_push();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            matches!(
                p.modal,
                ActiveModal::ConfirmForcePush { expected_remote_oid: Some(oid), .. }
                    if oid.to_hex() == oid_c
            )
        })
        .await
        .unwrap();
    presenter.confirm_force_push(ForcePushChoice::WithLease);
    wait_cli_idle(&mut presenter).await;
    assert_eq!(run_git(bare.path(), &["rev-parse", "main"]), oid_b);

    // 6. Confirming with no dialog open is a no-op.
    presenter.confirm_force_push(ForcePushChoice::Force);
    assert!(!presenter.active_tab().unwrap().cli_running);

    // 7. If another dialog opened (or the tab/branch changed) before the lease OID arrived, the
    //    force-push confirmation is not opened; a banner explains the cancellation instead.
    presenter.request_force_push();
    presenter.modal = ActiveModal::ConfirmDestructiveTerminalCommand {
        repo_id: presenter.active_repo_id.unwrap(),
        command_line: "x".to_string(),
        command: "x".to_string(),
        reason: "x".to_string(),
    };
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.status_banner
                .as_ref()
                .is_some_and(|b| b.message.contains("Force push cancelled"))
        })
        .await
        .unwrap();
    assert!(matches!(
        presenter.modal,
        ActiveModal::ConfirmDestructiveTerminalCommand { .. }
    ));
}

#[tokio::test]
async fn cloud_ai_refusal_maps_to_actionable_message_and_per_repo_consent() {
    let repo = init_repo("main");
    std::fs::write(repo.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
    run_git(repo.path(), &["add", "lib.rs"]);
    run_git(repo.path(), &["commit", "-m", "init"]);
    std::fs::write(repo.path().join("lib.rs"), "pub fn a() { let _x = 1; }\n").unwrap();
    run_git(repo.path(), &["add", "lib.rs"]);

    let rt = tokio::runtime::Handle::current();
    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let git_engine: Arc<dyn HybridGitEngine> =
        Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), None));
    let ai: Arc<dyn ai_copilot::AiCopilotProvider> =
        Arc::new(ai_copilot::HttpAiCopilotProvider::new(None).unwrap());
    // Off-device endpoint (never contacted: consent is checked before any network I/O).
    let ai_cfg = ai_copilot::AiEndpointConfig {
        kind: ai_copilot::AiProviderKind::AnthropicClaude,
        base_url: "https://ai-endpoint.invalid".to_string(),
        model_name: CompactString::const_new("claude-sonnet-5-5"),
        custom_commit_style_hint: None,
        allow_cloud_upload: false,
    };
    let host = CoreAppHost::new(rt.clone(), git_env, git_engine, Vec::new())
        .with_optional_services(None, None, Some(ai))
        .with_phase5_runtime(None, None, None, None, Some(ai_cfg));
    let mut presenter = BranvioPresenter::new(host, Arc::new(CountingWakeupSink::default()));
    // Isolate from a developer's BRANVIO_AI_ALLOW_CLOUD_UPLOAD environment.
    presenter.ai_privacy = app_ui::presenter::AiPrivacySettings::default();

    let repo_id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();

    presenter.generate_ai_commit_message();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| !t.ai_commit_busy)
        })
        .await
        .unwrap();

    let banner = presenter.status_banner.clone().unwrap();
    assert!(banner.is_error);
    assert!(
        banner
            .message
            .starts_with("Suggest message would send your diff to Anthropic."),
        "{}",
        banner.message
    );
    assert!(banner.message.contains("Allow sending diffs to cloud AI"));
    assert!(banner.message.contains("local Ollama model"));
    assert!(presenter.active_tab().unwrap().commit_subject.is_empty());
    match &presenter.modal {
        ActiveModal::ConfirmAiCloudUpload {
            repo_id: modal_repo,
            provider,
            feature,
        } => {
            assert_eq!(*modal_repo, repo_id);
            assert_eq!(provider, "Anthropic");
            assert_eq!(*feature, AiFeature::SuggestCommitMessage);
        }
        other => panic!("unexpected modal {other:?}"),
    }

    // "Allow for this repository" grants consent for this repo only.
    let repo_root = presenter.active_tab().unwrap().repo_root.clone();
    assert!(!presenter.ai_privacy.allows_cloud_upload_for(&repo_root));
    presenter.allow_cloud_ai_for_repository(repo_id);
    assert!(matches!(presenter.modal, ActiveModal::None));
    assert!(presenter.ai_privacy.allows_cloud_upload_for(&repo_root));
    assert!(!presenter.ai_privacy.allow_cloud_upload);

    // Global toggle via the command palette.
    presenter.execute_palette_action(PaletteActionKind::ToggleAiCloudUpload);
    assert!(presenter.ai_privacy.allow_cloud_upload);

    // Sensitive files are reported without leaking content.
    let sensitive = AiUiError::SensitivePathExcluded {
        path: ".env".to_string(),
    };
    assert_eq!(
        sensitive.user_message(AiFeature::SuggestConflictResolution),
        "Skipped AI help for .env because it may contain secrets."
    );
}

#[tokio::test]
async fn destructive_terminal_commands_require_confirmation() {
    let repo = init_repo("main");
    std::fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    run_git(repo.path(), &["add", "a.txt"]);
    run_git(repo.path(), &["commit", "-m", "init"]);

    let rt = tokio::runtime::Handle::current();
    let (mut presenter, _wakeup) = new_presenter(&rt, None);
    let repo_id = presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab().is_some_and(|t| {
                t.active_refresh_cancel.is_none() && t.snapshot.refs.iter().any(|b| b.is_head)
            })
        })
        .await
        .unwrap();

    presenter.send_terminal_command_line("git status");
    assert!(matches!(presenter.modal, ActiveModal::None));

    presenter.send_terminal_command_line("git reset --hard HEAD~1");
    match &presenter.modal {
        ActiveModal::ConfirmDestructiveTerminalCommand {
            repo_id: modal_repo,
            command_line,
            ..
        } => {
            assert_eq!(*modal_repo, repo_id);
            assert_eq!(command_line, "git reset --hard HEAD~1");
        }
        other => panic!("unexpected modal {other:?}"),
    }
    presenter.confirm_destructive_terminal_command();
    assert!(matches!(presenter.modal, ActiveModal::None));

    // Confirming after the active repository changed does not send and explains why.
    presenter.send_terminal_command_line("git clean -fd");
    assert!(matches!(
        presenter.modal,
        ActiveModal::ConfirmDestructiveTerminalCommand { .. }
    ));
    presenter.active_repo_id = None;
    presenter.confirm_destructive_terminal_command();
    assert!(matches!(presenter.modal, ActiveModal::None));
    let banner = presenter.status_banner.clone().unwrap();
    assert!(
        banner.message.contains("Command not sent"),
        "{}",
        banner.message
    );
    presenter.active_repo_id = Some(repo_id);

    // Wrapped / quoted bypasses are caught too.
    presenter.send_terminal_command_line("sh -c 'git reset --hard'");
    assert!(matches!(
        presenter.modal,
        ActiveModal::ConfirmDestructiveTerminalCommand { .. }
    ));
    presenter.modal = ActiveModal::None;

    // Deleting the checked-out trunk is flagged even with the "safe" `-d`.
    presenter.send_terminal_command_line("git branch -d main");
    assert!(matches!(
        &presenter.modal,
        ActiveModal::ConfirmDestructiveTerminalCommand { reason, .. } if reason.contains("primary branch")
    ));
}

#[tokio::test]
async fn failed_undo_capture_holds_back_destructive_reset_until_confirmed() {
    let repo = init_repo("main");
    std::fs::write(repo.path().join("a.txt"), "base\n").unwrap();
    run_git(repo.path(), &["add", "a.txt"]);
    run_git(repo.path(), &["commit", "-m", "base"]);
    run_git(repo.path(), &["checkout", "-b", "other"]);
    std::fs::write(repo.path().join("a.txt"), "theirs\n").unwrap();
    run_git(repo.path(), &["commit", "-am", "theirs"]);
    run_git(repo.path(), &["checkout", "main"]);
    std::fs::write(repo.path().join("a.txt"), "ours\n").unwrap();
    run_git(repo.path(), &["commit", "-am", "ours"]);
    // Conflicting merge leaves an unmerged index, which cannot be snapshotted as a tree.
    let merge = Command::new("git")
        .args(["merge", "other"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let conflicted = std::fs::read_to_string(repo.path().join("a.txt")).unwrap();
    assert!(conflicted.contains("<<<<<<<"));

    let db_dir = TempDir::new().unwrap();
    let rt = tokio::runtime::Handle::current();
    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let git_engine: Arc<dyn HybridGitEngine> =
        Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), None));
    let store: Arc<dyn oplog_undo::OpLogStore> =
        Arc::new(oplog_undo::RedbOpLogStore::open_at(db_dir.path().join("oplog.redb")).unwrap());
    let undo_engine: Arc<dyn oplog_undo::UndoRedoEngine> =
        Arc::new(oplog_undo::BranvioUndoEngine::new(
            Arc::clone(&store),
            Arc::clone(&git_engine),
            Arc::clone(&git_env),
        ));
    let host = CoreAppHost::new(rt.clone(), git_env, git_engine, Vec::new())
        .with_optional_services(Some(undo_engine), None, None)
        .with_phase5_runtime(Some(store), None, None, None, None);
    let mut presenter = BranvioPresenter::new(host, Arc::new(CountingWakeupSink::default()));
    presenter.open_repository_tab(repo.path()).unwrap();
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            p.active_tab()
                .is_some_and(|t| t.active_refresh_cancel.is_none())
        })
        .await
        .unwrap();

    // 1. The reset is held back behind a confirmation; nothing changes on disk.
    presenter.reset_to_revision("HEAD".to_string(), branvio_core::domain::ResetMode::Hard);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            matches!(p.modal, ActiveModal::ConfirmContinueWithoutUndo { .. })
        })
        .await
        .unwrap();
    match &presenter.modal {
        ActiveModal::ConfirmContinueWithoutUndo { reason, .. } => assert!(!reason.is_empty()),
        other => panic!("unexpected modal {other:?}"),
    }
    assert!(!presenter.active_tab().unwrap().cli_running);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        conflicted
    );

    // 2. Cancel (the default) drops the operation.
    presenter.cancel_continue_without_undo();
    assert!(matches!(presenter.modal, ActiveModal::None));
    presenter.confirm_continue_without_undo(); // no-op without the modal
    assert!(!presenter.active_tab().unwrap().cli_running);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        conflicted
    );

    // 3. Explicitly continuing runs the reset without an undo point.
    presenter.reset_to_revision("HEAD".to_string(), branvio_core::domain::ResetMode::Hard);
    presenter
        .wait_until(Duration::from_secs(10), |p| {
            matches!(p.modal, ActiveModal::ConfirmContinueWithoutUndo { .. })
        })
        .await
        .unwrap();
    presenter.confirm_continue_without_undo();
    assert!(matches!(presenter.modal, ActiveModal::None));
    wait_cli_idle(&mut presenter).await;
    assert_eq!(
        std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
        "ours\n"
    );
}
