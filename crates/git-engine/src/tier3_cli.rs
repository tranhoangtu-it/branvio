//! **Tier 3 (`tokio::process` native `git` CLI):** State-mutating, pickaxe search, and network operations.
//!
//! Preserves 100% compatibility with user `.gitconfig`, SSH Agents, GPG/SSH signing,
//! Git LFS smudge/clean filters, and Git hooks (`husky`, `pre-commit`, `commit-msg`, `pre-push`).

use crate::error::{GitEngineError, GitEngineResult};
use crate::traits::GitCliExecutor;
use crate::types::{
    CliCommandHandle, CliStreamEvent, CommitOptions, MergeOptions, PickaxeSearchRequest, PushForce,
    RebasePlanPayload, WorktreeInfo, build_safe_revision_args,
};
use async_trait::async_trait;
use branvio_core::domain::ResetMode;
use branvio_core::ids::GitOid;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::CancelToken;
use branvio_core::platform::HydratedGitEnv;
use branvio_core::platform::env_hydration::SubprocessEnvOptions;
use compact_str::CompactString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Tier 3 subprocess executor backed by `tokio::process` and native `git`.
#[derive(Debug, Clone)]
pub struct TokioGitCliExecutor {
    git_env: Arc<HydratedGitEnv>,
    self_helper_exe: Option<PathBuf>,
}

impl TokioGitCliExecutor {
    /// Creates a new [`TokioGitCliExecutor`] with hydrated OS Git/SSH/GPG environment.
    pub fn new(git_env: Arc<HydratedGitEnv>, self_helper_exe: Option<PathBuf>) -> Self {
        Self {
            git_env,
            self_helper_exe,
        }
    }

    fn git_program(&self) -> PathBuf {
        self.git_env
            .git_executable
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_git_name()))
    }

    fn spawn_git_child(
        &self,
        repo_root: &HostPath,
        args: &[String],
        env_opts: SubprocessEnvOptions<'_>,
    ) -> GitEngineResult<tokio::process::Child> {
        let program = self.git_program();
        let env_map = self.git_env.build_subprocess_env(env_opts);

        let mut cmd = Command::new(&program);
        cmd.current_dir(repo_root.for_external_cli())
            .args(args)
            .env_clear()
            .envs(env_map)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        configure_no_console_window(&mut cmd);

        cmd.spawn().map_err(|source| GitEngineError::SpawnFailed {
            program: program.display().to_string(),
            source,
        })
    }

    fn spawn_streaming(
        &self,
        repo_root: &HostPath,
        args: Vec<String>,
        mut env_opts: SubprocessEnvOptions<'_>,
        keep_alive_temp: Option<NamedTempFile>,
    ) -> GitEngineResult<CliCommandHandle> {
        // Force deterministic C/English diagnostics so progress & error parsers work regardless of OS locale.
        env_opts.force_c_locale = true;
        let command_summary = format!("git {}", args.join(" "));
        let child = self.spawn_git_child(repo_root, &args, env_opts)?;

        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let cancel = CancelToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            let _temp_guard = keep_alive_temp;
            try_emit_non_terminal(&tx, CliStreamEvent::Started { command_summary });

            let tx_watch = tx.clone();
            let outcome = run_supervised_child(
                child,
                Some(&cancel_clone),
                tx_watch.closed(),
                |stdout, stderr| {
                    let stdout_tx = tx.clone();
                    let stderr_tx = tx.clone();
                    async move {
                        let out_fut = async move {
                            if let Some(out) = stdout {
                                pump_stream_lines(out, false, stdout_tx).await;
                            }
                        };
                        let err_fut = async move {
                            if let Some(err) = stderr {
                                pump_stream_lines(err, true, stderr_tx).await;
                            }
                        };
                        tokio::join!(out_fut, err_fut);
                    }
                },
            )
            .await;

            let (exit_code, success) = match outcome {
                Ok(ChildSupervisionOutcome::Completed { status, .. }) => {
                    (status.code(), status.success())
                }
                Ok(ChildSupervisionOutcome::Cancelled) | Err(_) => (None, false),
            };
            let _ = tx.try_send(CliStreamEvent::Finished { exit_code, success });
        });

        Ok(CliCommandHandle { events: rx, cancel })
    }

    async fn run_buffered(
        &self,
        repo_root: &HostPath,
        args: Vec<String>,
        cancel: Option<&CancelToken>,
    ) -> GitEngineResult<Vec<u8>> {
        let env_opts = SubprocessEnvOptions {
            read_only_no_locks: true,
            force_c_locale: true,
            ..SubprocessEnvOptions::default()
        };
        let child = self.spawn_git_child(repo_root, &args, env_opts)?;

        let outcome = run_supervised_child(
            child,
            cancel,
            std::future::pending::<()>(),
            |mut stdout, mut stderr| async move {
                let out_fut = async {
                    let mut buf = Vec::new();
                    if let Some(ref mut s) = stdout {
                        let _ = s.read_to_end(&mut buf).await;
                    }
                    buf
                };
                let err_fut = async {
                    let mut buf = Vec::new();
                    if let Some(ref mut s) = stderr {
                        let _ = s.read_to_end(&mut buf).await;
                    }
                    buf
                };
                tokio::join!(out_fut, err_fut)
            },
        )
        .await
        .map_err(|source| GitEngineError::Io {
            operation: "wait_git_buffered",
            source,
        })?;

        match outcome {
            ChildSupervisionOutcome::Cancelled => Err(GitEngineError::Cancelled),
            ChildSupervisionOutcome::Completed {
                status,
                io_output: (out_bytes, err_bytes),
            } => {
                if !status.success() {
                    return Err(GitEngineError::CliFailed {
                        subcommand: args.first().cloned().unwrap_or_default(),
                        exit_code: status.code(),
                        stderr_summary: String::from_utf8_lossy(&err_bytes).trim().to_string(),
                    });
                }
                Ok(out_bytes)
            }
        }
    }
}

#[async_trait]
impl GitCliExecutor for TokioGitCliExecutor {
    async fn commit(
        &self,
        repo_root: &HostPath,
        options: CommitOptions,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["commit".to_string()];
        if options.amend {
            args.push("--amend".to_string());
        }
        if options.no_verify {
            args.push("--no-verify".to_string());
        }
        if options.allow_empty {
            args.push("--allow-empty".to_string());
        }
        match (options.gpg_sign, options.signing_key_id) {
            (Some(true), Some(key)) => args.push(format!("-S{key}")),
            (Some(true), None) => args.push("-S".to_string()),
            (Some(false), _) => args.push("--no-gpg-sign".to_string()),
            (None, Some(key)) => args.push(format!("-S{key}")),
            (None, None) => {}
        }
        args.push("-m".to_string());
        args.push(options.message);

        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn checkout_ref(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        create_branch: Option<&str>,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["checkout".to_string()];
        if let Some(new_branch) = create_branch {
            if new_branch.is_empty() || new_branch.starts_with('-') {
                return Err(GitEngineError::CliFailed {
                    subcommand: "checkout".to_string(),
                    exit_code: None,
                    stderr_summary: format!(
                        "refusing invalid new branch name beginning with '-' or empty: '{new_branch}'"
                    ),
                });
            }
            args.push("-b".to_string());
            args.push(new_branch.to_string());
        }
        args.extend(build_safe_revision_args([target_rev]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn merge(
        &self,
        repo_root: &HostPath,
        options: MergeOptions,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["merge".to_string()];
        if options.no_ff {
            args.push("--no-ff".to_string());
        } else if options.ff_only {
            args.push("--ff-only".to_string());
        }
        if options.squash {
            args.push("--squash".to_string());
        }
        if let Some(msg) = options.message {
            args.push("-m".to_string());
            args.push(msg);
        }
        args.extend(build_safe_revision_args([options.target_rev.as_str()]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn rebase(
        &self,
        repo_root: &HostPath,
        onto_rev: &str,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["rebase".to_string()];
        args.extend(build_safe_revision_args([onto_rev]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn interactive_rebase(
        &self,
        repo_root: &HostPath,
        plan: RebasePlanPayload,
    ) -> GitEngineResult<CliCommandHandle> {
        let Some(helper_exe) = self.self_helper_exe.as_deref() else {
            return Err(GitEngineError::CliFailed {
                subcommand: "rebase -i".to_string(),
                exit_code: None,
                stderr_summary: "interactive rebase requires configured self_helper_exe"
                    .to_string(),
            });
        };

        let mut temp_plan = NamedTempFile::new().map_err(|source| GitEngineError::Io {
            operation: "create_rebase_plan_tempfile",
            source,
        })?;
        serde_json::to_writer(&mut temp_plan, &plan).map_err(|err| GitEngineError::Io {
            operation: "serialize_rebase_plan",
            source: std::io::Error::other(err),
        })?;

        let mut args = vec!["rebase".to_string(), "-i".to_string()];
        args.extend(build_safe_revision_args([plan.onto_rev.as_str()]));

        let plan_path = temp_plan.path().to_path_buf();
        let env_opts = SubprocessEnvOptions {
            self_helper_exe: Some(helper_exe),
            sequence_plan_path: Some(plan_path.as_path()),
            ..SubprocessEnvOptions::default()
        };

        self.spawn_streaming(repo_root, args, env_opts, Some(temp_plan))
    }

    async fn cherry_pick(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["cherry-pick".to_string()];
        let hexes: Vec<String> = commits.iter().map(GitOid::to_hex).collect();
        args.extend(build_safe_revision_args(hexes.iter().map(String::as_str)));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn revert(
        &self,
        repo_root: &HostPath,
        commits: &[GitOid],
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["revert".to_string(), "--no-edit".to_string()];
        let hexes: Vec<String> = commits.iter().map(GitOid::to_hex).collect();
        args.extend(build_safe_revision_args(hexes.iter().map(String::as_str)));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn reset(
        &self,
        repo_root: &HostPath,
        target_rev: &str,
        mode: ResetMode,
    ) -> GitEngineResult<CliCommandHandle> {
        let mode_flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
        };
        let mut args = vec!["reset".to_string(), mode_flag.to_string()];
        args.extend(build_safe_revision_args([target_rev]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn fetch(
        &self,
        repo_root: &HostPath,
        remote: Option<&str>,
        prune: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["fetch".to_string(), "--progress".to_string()];
        if prune {
            args.push("--prune".to_string());
        }
        match remote {
            Some(r) => args.extend(build_safe_revision_args([r])),
            None => args.push("--all".to_string()),
        }
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn pull(
        &self,
        repo_root: &HostPath,
        rebase: bool,
        ff_only: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["pull".to_string(), "--progress".to_string()];
        if rebase {
            args.push("--rebase".to_string());
        } else if ff_only {
            args.push("--ff-only".to_string());
        }
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn push(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
        force: PushForce,
        set_upstream: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        // Validate up front for EVERY force policy: an unvalidated branch can smuggle refspec
        // syntax (`+main` forces, `x:refs/heads/main` retargets, `:main` deletes).
        validate_push_remote(remote)?;
        validate_ref_component("push", "branch", branch)?;
        let mut args = vec!["push".to_string(), "--progress".to_string()];
        match force {
            PushForce::None => {}
            PushForce::WithLease {
                expected_remote_oid,
            } => {
                // Explicit expected value: never fall back to the (auto-fetched) tracking ref.
                // An empty expected value means "the remote branch must not exist".
                let expected = expected_remote_oid
                    .map(|oid| oid.to_hex())
                    .unwrap_or_default();
                args.push(format!("--force-with-lease=refs/heads/{branch}:{expected}"));
                args.push("--force-if-includes".to_string());
            }
            PushForce::Force => args.push("--force".to_string()),
        }
        if set_upstream {
            args.push("--set-upstream".to_string());
        }
        // Fully-qualified, explicit refspec so the (validated) name can never be reinterpreted
        // as `HEAD`, a tag, or another DWIM match.
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        args.extend(build_safe_revision_args([remote, refspec.as_str()]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn remote_tracking_oid(
        &self,
        repo_root: &HostPath,
        remote: &str,
        branch: &str,
    ) -> GitEngineResult<Option<GitOid>> {
        validate_ref_component("for-each-ref", "remote", remote)?;
        validate_ref_component("for-each-ref", "branch", branch)?;
        let full_ref = format!("refs/remotes/{remote}/{branch}");
        let mut args = vec![
            "for-each-ref".to_string(),
            "--format=%(refname)%00%(objectname)".to_string(),
        ];
        args.extend(build_safe_revision_args([full_ref.as_str()]));
        let bytes = self.run_buffered(repo_root, args, None).await?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            // `for-each-ref` patterns also match `<pattern>/...` children; require an exact name.
            if let Some((name, hex)) = line.split_once('\0')
                && name == full_ref
            {
                return GitOid::from_hex(hex.trim()).map(Some).map_err(|err| {
                    GitEngineError::CliFailed {
                        subcommand: "for-each-ref".to_string(),
                        exit_code: None,
                        stderr_summary: format!("invalid object id '{hex}' for {full_ref}: {err}"),
                    }
                });
            }
        }
        Ok(None)
    }

    async fn stash_push(
        &self,
        repo_root: &HostPath,
        message: Option<&str>,
        include_untracked: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["stash".to_string(), "push".to_string()];
        if include_untracked {
            args.push("--include-untracked".to_string());
        }
        if let Some(msg) = message {
            args.push("-m".to_string());
            args.push(msg.to_string());
        }
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn stash_pop(
        &self,
        repo_root: &HostPath,
        stash_index: usize,
        drop_after_apply: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let sub = if drop_after_apply { "pop" } else { "apply" };
        let stash_ref = format!("stash@{{{stash_index}}}");
        let mut args = vec!["stash".to_string(), sub.to_string()];
        args.extend(build_safe_revision_args([stash_ref.as_str()]));
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn worktree_list(&self, repo_root: &HostPath) -> GitEngineResult<Vec<WorktreeInfo>> {
        let args = vec![
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
            "-z".to_string(),
        ];
        let bytes = self.run_buffered(repo_root, args, None).await?;
        parse_worktree_porcelain_z(&bytes)
    }

    async fn worktree_add(
        &self,
        repo_root: &HostPath,
        target_dir: &HostPath,
        branch_or_oid: &str,
        create_new_branch: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let target_str = target_dir.for_external_cli().display().to_string();
        let mut args = vec!["worktree".to_string(), "add".to_string()];
        if create_new_branch {
            if branch_or_oid.is_empty() || branch_or_oid.starts_with('-') {
                return Err(GitEngineError::CliFailed {
                    subcommand: "worktree add".to_string(),
                    exit_code: None,
                    stderr_summary: format!(
                        "refusing invalid new branch name beginning with '-' or empty: '{branch_or_oid}'"
                    ),
                });
            }
            args.push("-b".to_string());
            args.push(branch_or_oid.to_string());
            args.push("--end-of-options".to_string());
            args.push(target_str);
        } else {
            args.push("--end-of-options".to_string());
            args.push(target_str);
            args.push(branch_or_oid.to_string());
        }
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn worktree_remove(
        &self,
        repo_root: &HostPath,
        worktree_path: &HostPath,
        force: bool,
    ) -> GitEngineResult<CliCommandHandle> {
        let mut args = vec!["worktree".to_string(), "remove".to_string()];
        if force {
            args.push("--force".to_string());
        }
        args.push("--end-of-options".to_string());
        args.push(worktree_path.for_external_cli().display().to_string());
        self.spawn_streaming(repo_root, args, SubprocessEnvOptions::default(), None)
    }

    async fn discard_worktree_paths(
        &self,
        repo_root: &HostPath,
        tracked_paths: &[RepoRelPath],
        untracked_paths: &[RepoRelPath],
    ) -> GitEngineResult<CliCommandHandle> {
        if tracked_paths.is_empty() && untracked_paths.is_empty() {
            return Err(GitEngineError::CliFailed {
                subcommand: "discard".to_string(),
                exit_code: None,
                stderr_summary: "no paths specified to discard".to_string(),
            });
        }

        let clean_args = if untracked_paths.is_empty() {
            None
        } else {
            let mut args = vec![
                "clean".to_string(),
                "-f".to_string(),
                "-d".to_string(),
                "--".to_string(),
            ];
            for path in untracked_paths {
                args.push(path.as_str().to_string());
            }
            Some(args)
        };

        let checkout_args = if tracked_paths.is_empty() {
            None
        } else {
            let mut args = vec!["checkout".to_string(), "--".to_string()];
            for path in tracked_paths {
                args.push(path.as_str().to_string());
            }
            Some(args)
        };

        match (clean_args, checkout_args) {
            (Some(clean), None) => {
                self.spawn_streaming(repo_root, clean, SubprocessEnvOptions::default(), None)
            }
            (None, Some(checkout)) => {
                self.spawn_streaming(repo_root, checkout, SubprocessEnvOptions::default(), None)
            }
            (Some(clean), Some(checkout)) => {
                self.run_buffered(repo_root, clean, None).await?;
                self.spawn_streaming(repo_root, checkout, SubprocessEnvOptions::default(), None)
            }
            (None, None) => Err(GitEngineError::CliFailed {
                subcommand: "discard".to_string(),
                exit_code: None,
                stderr_summary: "no paths specified to discard".to_string(),
            }),
        }
    }

    async fn search_commits_pickaxe(
        &self,
        repo_root: &HostPath,
        request: PickaxeSearchRequest,
        cancel: CancelToken,
    ) -> GitEngineResult<Vec<GitOid>> {
        let mut args = vec![
            "log".to_string(),
            "--format=%H".to_string(),
            format!("--max-count={}", request.limit.max(1)),
        ];
        if request.is_regex {
            args.push(format!("-G{}", request.query));
        } else {
            args.push(format!("-S{}", request.query));
        }

        if request.start_tips.is_empty() {
            args.push("--all".to_string());
        } else {
            let hexes: Vec<String> = request.start_tips.iter().map(GitOid::to_hex).collect();
            args.extend(build_safe_revision_args(hexes.iter().map(String::as_str)));
        }

        if !request.path_filters.is_empty() {
            args.push("--".to_string());
            for p in request.path_filters {
                args.push(p.as_str().to_string());
            }
        }

        let bytes = self.run_buffered(repo_root, args, Some(&cancel)).await?;
        let text = String::from_utf8_lossy(&bytes);
        let mut oids = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && let Ok(oid) = GitOid::from_hex(trimmed)
            {
                oids.push(oid);
            }
        }
        Ok(oids)
    }
}

/// Rejects remote/branch names that are not valid single ref names per
/// `git check-ref-format`, or that could break refspec/lease syntax, act as `for-each-ref`
/// globs, or be parsed as options, before they are embedded in an argument.
///
/// Beyond `check-ref-format` this also rejects a leading `-` (option) or `+` (forced refspec),
/// any whitespace, and the bare names `HEAD` / `@`.
fn validate_ref_component(subcommand: &str, what: &str, value: &str) -> GitEngineResult<()> {
    if !is_valid_ref_component(value) {
        return Err(GitEngineError::CliFailed {
            subcommand: subcommand.to_string(),
            exit_code: None,
            stderr_summary: format!("refusing invalid {what} name: '{value}'"),
        });
    }
    Ok(())
}

fn is_valid_ref_component(value: &str) -> bool {
    if value.is_empty()
        || value == "@"
        || value == "HEAD"
        || value.starts_with('-')
        || value.starts_with('+')
        || value.ends_with('.')
        || value.contains("..")
        || value.contains("@{")
        || value.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, ':' | '*' | '?' | '[' | '\\' | '^' | '~')
        })
    {
        return false;
    }
    // Leading/trailing `/` and `//` show up as empty components.
    value.split('/').all(|component| {
        !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
    })
}

/// Push targets may be a configured remote name or a URL/path, so only reject what could be
/// parsed as an option or corrupt the argument (empty, leading `-`, control characters).
fn validate_push_remote(remote: &str) -> GitEngineResult<()> {
    if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
        return Err(GitEngineError::CliFailed {
            subcommand: "push".to_string(),
            exit_code: None,
            stderr_summary: format!("refusing invalid remote: '{remote}'"),
        });
    }
    Ok(())
}

const STREAM_CHANNEL_CAPACITY: usize = 1024;
const RESERVED_TERMINAL_SLOTS: usize = 4;
const MAX_STREAM_LINE_BYTES: usize = 16 * 1024;

enum ChildSupervisionOutcome<T> {
    Completed {
        status: std::process::ExitStatus,
        io_output: T,
    },
    Cancelled,
}

async fn run_supervised_child<F, Fut, T>(
    mut child: tokio::process::Child,
    cancel: Option<&CancelToken>,
    peer_closed: impl std::future::Future<Output = ()>,
    pump_pipes: F,
) -> std::io::Result<ChildSupervisionOutcome<T>>
where
    F: FnOnce(Option<tokio::process::ChildStdout>, Option<tokio::process::ChildStderr>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if cancel.is_some_and(CancelToken::is_cancelled) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Ok(ChildSupervisionOutcome::Cancelled);
    }

    let pipe_fut = pump_pipes(stdout, stderr);
    tokio::pin!(pipe_fut);
    tokio::pin!(peer_closed);

    let mut io_output: Option<T> = None;
    let mut exit_status: Option<std::process::ExitStatus> = None;

    loop {
        if io_output.is_some() && exit_status.is_some() {
            break;
        }
        tokio::select! {
            biased;
            _ = async {
                match cancel {
                    Some(tok) => tok.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ChildSupervisionOutcome::Cancelled);
            }
            _ = &mut peer_closed => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ChildSupervisionOutcome::Cancelled);
            }
            out = &mut pipe_fut, if io_output.is_none() => {
                io_output = Some(out);
            }
            status_res = child.wait(), if exit_status.is_none() => {
                match status_res {
                    Ok(status) => exit_status = Some(status),
                    Err(err) => return Err(err),
                }
            }
        }
    }

    match (exit_status, io_output) {
        (Some(status), Some(output)) => Ok(ChildSupervisionOutcome::Completed {
            status,
            io_output: output,
        }),
        _ => Ok(ChildSupervisionOutcome::Cancelled),
    }
}

#[inline]
fn try_emit_non_terminal(tx: &mpsc::Sender<CliStreamEvent>, event: CliStreamEvent) {
    // Always reserve terminal slots so slow readers cannot block `Finished` or stall pipe pumping.
    if tx.capacity() > RESERVED_TERMINAL_SLOTS {
        let _ = tx.try_send(event);
    }
}

async fn pump_stream_lines<R: AsyncRead + Unpin>(
    reader: R,
    is_stderr: bool,
    tx: mpsc::Sender<CliStreamEvent>,
) {
    let mut buf_reader = BufReader::new(reader);
    let mut line_buf = Vec::with_capacity(256);

    loop {
        line_buf.clear();
        let Ok(n) = read_until_cr_or_lf_bounded(&mut buf_reader, &mut line_buf).await else {
            break;
        };
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf)
            .trim_end_matches(&['\r', '\n'][..])
            .to_string();
        if line.is_empty() {
            continue;
        }

        if is_stderr {
            if let Some((phase, percent)) = parse_git_progress_line(&line) {
                try_emit_non_terminal(&tx, CliStreamEvent::TransferProgress { phase, percent });
                continue;
            }
            if let Some(hook_name) = detect_hook_banner(&line) {
                try_emit_non_terminal(&tx, CliStreamEvent::HookStarted { hook_name });
            }
            try_emit_non_terminal(&tx, CliStreamEvent::StderrLine(line));
        } else {
            try_emit_non_terminal(&tx, CliStreamEvent::StdoutLine(line));
        }
    }
}

async fn read_until_cr_or_lf_bounded<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n' || b == b'\r') {
            let take = pos + 1;
            let copy_len = take.min(MAX_STREAM_LINE_BYTES.saturating_sub(buf.len()));
            buf.extend_from_slice(&available[..copy_len]);
            reader.consume(take);
            return Ok(total + take);
        }
        let len = available.len();
        let copy_len = len.min(MAX_STREAM_LINE_BYTES.saturating_sub(buf.len()));
        if copy_len > 0 {
            buf.extend_from_slice(&available[..copy_len]);
        }
        reader.consume(len);
        total += len;
    }
}

fn parse_git_progress_line(line: &str) -> Option<(CompactString, u8)> {
    const KNOWN_PHASES: &[&str] = &[
        "Enumerating objects",
        "Counting objects",
        "Compressing objects",
        "Writing objects",
        "Receiving objects",
        "Resolving deltas",
        "Updating files",
        "Checking out files",
        "Filtering content",
        "Unpacking objects",
        "Finding sources",
        "Checking connectivity",
    ];
    let trimmed = line.trim();
    let stripped = trimmed
        .strip_prefix("remote:")
        .map_or(trimmed, str::trim_start);
    let (phase_raw, rest) = stripped.split_once(':')?;
    let phase = phase_raw.trim();
    if !KNOWN_PHASES.contains(&phase) {
        return None;
    }
    let pct_pos = rest.find('%')?;
    let digits = rest[..pct_pos].trim().rsplit(' ').next()?;
    let percent = digits.parse::<u8>().ok()?;
    if percent > 100 {
        return None;
    }
    Some((CompactString::from(phase), percent))
}

fn detect_hook_banner(line: &str) -> Option<CompactString> {
    let lower = line.to_ascii_lowercase();
    let is_hook_banner = lower.starts_with("husky - ")
        || lower.starts_with("running ")
        || lower.contains(".git/hooks/")
        || lower.contains("hooks/");
    if !is_hook_banner {
        return None;
    }
    for hook in [
        "prepare-commit-msg",
        "pre-commit",
        "commit-msg",
        "pre-push",
        "post-checkout",
    ] {
        if lower.contains(hook) {
            return Some(CompactString::from(hook));
        }
    }
    None
}

/// Parses `git worktree list --porcelain -z` NUL-terminated records.
pub fn parse_worktree_porcelain_z(raw: &[u8]) -> GitEngineResult<Vec<WorktreeInfo>> {
    let mut results = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for field_bytes in raw.split(|&b| b == 0) {
        let field = String::from_utf8_lossy(field_bytes);
        if field.is_empty() {
            if let Some(wt) = current.take() {
                results.push(wt);
            }
            continue;
        }

        if let Some(path_str) = field.strip_prefix("worktree ") {
            if let Some(prev) = current.take() {
                results.push(prev);
            }
            let is_main = results.is_empty();
            current = Some(WorktreeInfo {
                path: HostPath::from_path_buf(PathBuf::from(path_str)),
                head_oid: None,
                branch: None,
                is_main,
                is_locked: false,
                is_prunable: false,
            });
        } else if let Some(ref mut wt) = current {
            if let Some(oid_str) = field.strip_prefix("HEAD ") {
                wt.head_oid = GitOid::from_hex(oid_str).ok();
            } else if let Some(branch_ref) = field.strip_prefix("branch ") {
                let short = branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref);
                wt.branch = Some(CompactString::from(short));
            } else if field == "bare" {
                wt.head_oid = None;
                wt.branch = None;
            } else if field == "detached" {
                wt.branch = None;
            } else if field == "locked" || field.starts_with("locked ") {
                wt.is_locked = true;
            } else if field == "prunable" || field.starts_with("prunable ") {
                wt.is_prunable = true;
            }
        }
    }

    if let Some(last) = current {
        results.push(last);
    }
    Ok(results)
}

#[inline]
const fn default_git_name() -> &'static str {
    #[cfg(windows)]
    {
        "git.exe"
    }
    #[cfg(not(windows))]
    {
        "git"
    }
}

#[cfg(windows)]
fn configure_no_console_window(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_console_window(_cmd: &mut Command) {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_progress_and_worktree_porcelain_z() {
        let progress = parse_git_progress_line("Receiving objects:  67% (67/100)");
        assert_eq!(
            progress,
            Some((CompactString::const_new("Receiving objects"), 67))
        );

        let remote_progress = parse_git_progress_line("remote: Compressing objects:  50% (5/10)");
        assert_eq!(
            remote_progress,
            Some((CompactString::const_new("Compressing objects"), 50))
        );

        // Error lines containing ':' and '%' MUST NOT be swallowed as transfer progress.
        assert_eq!(
            parse_git_progress_line("fatal: unable to access https://host/%20bad"),
            None
        );
        // Plain log lines mentioning hook names must not trigger HookStarted banners.
        assert_eq!(
            detect_hook_banner("fixed pre-push formatting in README"),
            None
        );
        assert_eq!(
            detect_hook_banner("husky - pre-commit hook started"),
            Some(CompactString::const_new("pre-commit"))
        );

        let raw = b"worktree /repo/main\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0worktree /repo/wt-feat\0HEAD 2222222222222222222222222222222222222222\0branch refs/heads/feat/x\0locked reason\0\0worktree /repo/wt-det\0HEAD 3333333333333333333333333333333333333333\0detached\0\0worktree /repo/bare.git\0bare\0\0";
        let list = parse_worktree_porcelain_z(raw).unwrap();
        assert_eq!(list.len(), 4);
        assert!(list[0].is_main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(!list[1].is_main);
        assert!(list[1].is_locked);
        assert_eq!(list[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(list[2].branch, None);
        assert!(list[2].head_oid.is_some());
        assert_eq!(list[3].head_oid, None);
        assert_eq!(list[3].branch, None);
    }

    #[test]
    fn ref_component_validation_follows_check_ref_format() {
        for ok in [
            "main",
            "feature/x",
            "release-1.2",
            "a.b",
            "user@host",
            "x_y/z-w",
        ] {
            assert!(is_valid_ref_component(ok), "should accept '{ok}'");
        }
        for bad in [
            "", "+main", "x:y", ":main", "HEAD", "@", "a.lock", "a/b.lock", "a@{1}", "@{-1}",
            ".hidden", "a/.b", "a//b", "/a", "a/", "a.", "a..b", "-x", "a b", "a\tb", "a\u{7f}",
            "a*", "a?", "a[", "a\\b", "a^", "a~1",
        ] {
            assert!(!is_valid_ref_component(bad), "should reject {bad:?}");
        }
    }
}
