//! Integration tests for force-push safety (`PushForce`) against local bare `file://` remotes.
//!
//! The key regression: an explicit-value lease captured when the user confirmed the force push
//! must still reject the push after a teammate pushed and background auto-fetch advanced the
//! remote-tracking ref (which a bare `--force-with-lease` would silently accept).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use branvio_core::ids::GitOid;
use branvio_core::paths::HostPath;
use branvio_core::platform::HydratedGitEnv;
use git_engine::{BranvioGitEngine, CliCommandHandle, CliStreamEvent, GitCliExecutor, PushForce};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Branvio Test")
        .env("GIT_AUTHOR_EMAIL", "test@branvio.rs")
        .env("GIT_COMMITTER_NAME", "Branvio Test")
        .env("GIT_COMMITTER_EMAIL", "test@branvio.rs")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git command should execute");
    assert!(
        out.status.success(),
        "git {:?} failed: stderr={}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn configure_identity(dir: &Path) {
    run_git(dir, &["config", "user.name", "Branvio Test"]);
    run_git(dir, &["config", "user.email", "test@branvio.rs"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

fn commit_file(dir: &Path, name: &str, content: &str, msg: &str) -> GitOid {
    std::fs::write(dir.join(name), content).unwrap();
    run_git(dir, &["add", name]);
    run_git(dir, &["commit", "-m", msg]);
    GitOid::from_hex(&run_git(dir, &["rev-parse", "HEAD"])).unwrap()
}

struct PushFixture {
    _bare: TempDir,
    bare_url: String,
    mine_dir: TempDir,
    mine_root: HostPath,
    engine: BranvioGitEngine,
}

impl PushFixture {
    fn mine(&self) -> &Path {
        self.mine_dir.path()
    }

    fn remote_main_oid(&self) -> GitOid {
        let out = run_git(
            self.mine(),
            &["ls-remote", &self.bare_url, "refs/heads/main"],
        );
        GitOid::from_hex(out.split_whitespace().next().unwrap()).unwrap()
    }

    async fn tracking(&self, branch: &str) -> Option<GitOid> {
        self.engine
            .remote_tracking_oid(&self.mine_root, "origin", branch)
            .await
            .unwrap()
    }

    async fn push(&self, branch: &str, force: PushForce, set_upstream: bool) -> (bool, String) {
        let handle = self
            .engine
            .push(&self.mine_root, "origin", branch, force, set_upstream)
            .await
            .unwrap();
        drain(handle).await
    }
}

/// Bare `file://` remote with `main` pushed (and tracked) from a local clone.
fn init_push_fixture() -> PushFixture {
    let bare = TempDir::new().unwrap();
    run_git(bare.path(), &["init", "--bare", "-b", "main"]);
    let bare_url = format!("file://{}", bare.path().display());

    let mine_dir = TempDir::new().unwrap();
    run_git(mine_dir.path(), &["init", "-b", "main"]);
    // Deterministic line endings: Windows runners ship `core.autocrlf=true` in system config.
    run_git(mine_dir.path(), &["config", "core.autocrlf", "false"]);
    configure_identity(mine_dir.path());
    run_git(mine_dir.path(), &["remote", "add", "origin", &bare_url]);
    commit_file(mine_dir.path(), "base.txt", "base\n", "base");
    commit_file(mine_dir.path(), "second.txt", "second\n", "second");
    run_git(mine_dir.path(), &["push", "-u", "origin", "main"]);

    let mine_root = HostPath::canonicalize_existing(mine_dir.path()).unwrap();
    let env = Arc::new(HydratedGitEnv::from_current_process());
    PushFixture {
        _bare: bare,
        bare_url,
        mine_dir,
        mine_root,
        engine: BranvioGitEngine::new(env, None),
    }
}

/// Simulates a teammate pushing a new commit to `main` from a separate clone.
fn teammate_pushes(bare_url: &str) -> GitOid {
    let other = TempDir::new().unwrap();
    run_git(
        other.path(),
        &[
            "clone",
            "-c",
            "core.autocrlf=false",
            "-b",
            "main",
            bare_url,
            "theirs",
        ],
    );
    let theirs = other.path().join("theirs");
    configure_identity(&theirs);
    let oid = commit_file(&theirs, "teammate.txt", "precious\n", "teammate work");
    run_git(&theirs, &["push", "origin", "main"]);
    oid
}

/// Rewrites local `main` so that pushing it is a non-fast-forward update.
fn rewrite_local_history(dir: &Path) -> GitOid {
    run_git(dir, &["reset", "--hard", "HEAD~1"]);
    commit_file(dir, "rewritten.txt", "rewritten\n", "rewritten history")
}

async fn drain(mut handle: CliCommandHandle) -> (bool, String) {
    let mut ok = false;
    let mut stderr = String::new();
    while let Some(ev) = next_cli_event(&mut handle.events).await {
        match ev {
            CliStreamEvent::StderrLine(line) => {
                stderr.push_str(&line);
                stderr.push('\n');
            }
            CliStreamEvent::Finished { success, .. } => ok = success,
            _ => {}
        }
    }
    (ok, stderr)
}

#[tokio::test]
async fn with_lease_succeeds_when_remote_unchanged() {
    let fx = init_push_fixture();

    // Confirm dialog opens: capture the remote-tracking OID the user is looking at.
    let expected = fx.tracking("main").await;
    assert_eq!(expected, Some(fx.remote_main_oid()));
    assert_eq!(fx.tracking("does-not-exist").await, None);

    let rewritten = rewrite_local_history(fx.mine());
    let (ok, stderr) = fx
        .push(
            "main",
            PushForce::WithLease {
                expected_remote_oid: expected,
            },
            false,
        )
        .await;
    assert!(ok, "lease push should succeed: {stderr}");
    assert_eq!(fx.remote_main_oid(), rewritten);
}

#[tokio::test]
async fn with_lease_is_rejected_after_teammate_push_even_if_auto_fetch_ran() {
    let fx = init_push_fixture();

    // User opens the force-push confirmation: OID captured now.
    let expected = fx.tracking("main").await;
    assert!(expected.is_some());
    rewrite_local_history(fx.mine());

    // Meanwhile a teammate pushes, and background auto-fetch advances `origin/main`.
    let teammate_oid = teammate_pushes(&fx.bare_url);
    run_git(fx.mine(), &["fetch", "origin"]);
    assert_eq!(fx.tracking("main").await, Some(teammate_oid));

    let (ok, stderr) = fx
        .push(
            "main",
            PushForce::WithLease {
                expected_remote_oid: expected,
            },
            false,
        )
        .await;
    assert!(!ok, "stale lease must be rejected: {stderr}");
    assert!(
        stderr.contains("stale info") || stderr.contains("rejected"),
        "unexpected stderr: {stderr}"
    );
    // Teammate's commit is still on the remote.
    assert_eq!(fx.remote_main_oid(), teammate_oid);
}

#[tokio::test]
async fn with_lease_none_requires_remote_branch_to_be_absent() {
    let fx = init_push_fixture();
    run_git(fx.mine(), &["checkout", "-b", "feature"]);
    commit_file(fx.mine(), "feature.txt", "f\n", "feature");

    // Remote branch absent when the dialog opened and still absent: push creates it.
    let lease_absent = PushForce::WithLease {
        expected_remote_oid: None,
    };
    let (ok, stderr) = fx.push("feature", lease_absent, true).await;
    assert!(ok, "creating absent branch should succeed: {stderr}");

    // Now it exists: the same "must not exist" lease is rejected.
    commit_file(fx.mine(), "feature2.txt", "f2\n", "feature 2");
    let (ok, stderr) = fx.push("feature", lease_absent, false).await;
    assert!(
        !ok,
        "lease expecting absent branch must reject existing branch: {stderr}"
    );
}

#[tokio::test]
async fn force_overwrites_and_plain_push_rejects_non_fast_forward() {
    let fx = init_push_fixture();
    let teammate_oid = teammate_pushes(&fx.bare_url);
    run_git(fx.mine(), &["fetch", "origin"]);
    let diverged = commit_file(fx.mine(), "mine.txt", "mine\n", "diverged local work");

    // `PushForce::None` rejects the non-fast-forward update.
    let (ok, _) = fx.push("main", PushForce::None, false).await;
    assert!(!ok, "plain push must reject non-fast-forward");
    assert_eq!(fx.remote_main_oid(), teammate_oid);

    // `PushForce::Force` overwrites unconditionally.
    let (ok, stderr) = fx.push("main", PushForce::Force, false).await;
    assert!(ok, "force push should succeed: {stderr}");
    assert_eq!(fx.remote_main_oid(), diverged);
}

#[tokio::test]
async fn rejects_option_like_or_glob_names() {
    let fx = init_push_fixture();
    for bad in ["--all", "ma*", "a:b", ""] {
        assert!(
            fx.engine
                .remote_tracking_oid(&fx.mine_root, "origin", bad)
                .await
                .is_err(),
            "tracking lookup should reject '{bad}'"
        );
    }
    let lease = PushForce::WithLease {
        expected_remote_oid: None,
    };
    assert!(
        fx.engine
            .push(&fx.mine_root, "origin", "a:b", lease, false)
            .await
            .is_err()
    );
}

/// Refspec-smuggling branch names must be refused for EVERY force policy, before anything
/// is spawned, so the remote is never touched.
#[tokio::test]
async fn refspec_like_branch_names_are_rejected_for_all_force_policies() {
    let fx = init_push_fixture();
    let before = fx.remote_main_oid();
    // Diverge so that any accidental forced push would be visible on the remote.
    rewrite_local_history(fx.mine());

    let policies = [
        PushForce::None,
        PushForce::Force,
        PushForce::WithLease {
            expected_remote_oid: Some(before),
        },
        PushForce::WithLease {
            expected_remote_oid: None,
        },
    ];
    let bad_names = [
        "+main",
        "x:y",
        "main:refs/heads/main",
        ":main",
        "HEAD",
        "@",
        "a.lock",
        "a@{1}",
        "@{-1}",
        ".hidden",
        "a//b",
        "a/.b",
        "main.",
        "-main",
        "ma\u{7f}in",
    ];
    for force in policies {
        for bad in bad_names {
            let res = fx
                .engine
                .push(&fx.mine_root, "origin", bad, force, false)
                .await;
            assert!(
                res.is_err(),
                "push of {bad:?} with {force:?} must be refused"
            );
        }
        assert!(
            fx.engine
                .push(&fx.mine_root, "--mirror", "main", force, false)
                .await
                .is_err(),
            "option-like remote must be refused for {force:?}"
        );
    }
    assert_eq!(fx.remote_main_oid(), before, "nothing may have been pushed");
    let heads = run_git(fx.mine(), &["ls-remote", "--heads", &fx.bare_url]);
    assert_eq!(
        heads.lines().count(),
        1,
        "unexpected remote branches: {heads}"
    );
}

/// A legitimate explicit-value lease must still work when the local repo keeps no reflogs
/// (`core.logAllRefUpdates=false`, tracking reflog deleted), i.e. `--force-if-includes`
/// must not turn into a spurious rejection.
#[tokio::test]
async fn with_lease_succeeds_without_any_reflog() {
    let fx = init_push_fixture();
    run_git(fx.mine(), &["config", "core.logAllRefUpdates", "false"]);
    let logs = fx.mine().join(".git").join("logs");
    if logs.exists() {
        std::fs::remove_dir_all(&logs).unwrap();
    }

    let expected = fx.tracking("main").await;
    assert_eq!(expected, Some(fx.remote_main_oid()));
    let rewritten = rewrite_local_history(fx.mine());
    assert!(
        !fx.mine()
            .join(".git/logs/refs/remotes/origin/main")
            .exists(),
        "tracking reflog should be absent for this test"
    );

    let (ok, stderr) = fx
        .push(
            "main",
            PushForce::WithLease {
                expected_remote_oid: expected,
            },
            false,
        )
        .await;
    assert!(ok, "lease push without reflog should succeed: {stderr}");
    assert_eq!(fx.remote_main_oid(), rewritten);
}

/// Receives the next CLI stream event, failing fast instead of hanging CI if a `git` subprocess stalls.
async fn next_cli_event(
    events: &mut tokio::sync::mpsc::Receiver<git_engine::CliStreamEvent>,
) -> Option<git_engine::CliStreamEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(60), events.recv())
        .await
        .expect("timed out after 60s waiting for the next git CLI stream event")
}
