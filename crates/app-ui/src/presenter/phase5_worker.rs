//! Async Phase 5 background tasks: Time-Machine Undo/Redo, adaptive `fs-watcher` forwarding,
//! embedded `terminal-pty` lifecycle, `forge-client` PR queries, and `ai-copilot` inference.

use crate::ports::CoreAppHost;
use crate::presenter::pipeline_worker::{AiUiError, PipelineEvent, UndoRedoUiError};
use ai_copilot::{AiEndpointConfig, AiProviderKind};
use branvio_core::domain::{ConflictResolution, RemoteExposureState};
use branvio_core::ids::RepoId;
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::pipeline::{
    DirtyScopeFlags, InternalOpController, MvccRepoStateStore, UiWakeupSink,
};
use branvio_core::platform::SubprocessEnvOptions;
use compact_str::CompactString;
use forge_client::{CreatePullRequestInput, PullRequestState, parse_remote_url};
use fs_watcher::{RepoWatchHandle, WatcherConfig, WatcherEvent};
use oplog_undo::OpLogError;
use std::path::Path;
use std::sync::Arc;
use terminal_pty::{PtySize, ShellProfile, TerminalSession};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Spawns Time-Machine `undo()` or `redo()` asynchronously off the UI thread.
#[allow(clippy::too_many_arguments)]
pub fn spawn_undo_or_redo(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    is_redo: bool,
    allow_rewrite_pushed: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(undo_engine) = host.undo_engine.clone() else {
        let _ = event_tx.send(PipelineEvent::UndoRedoFinished {
            repo_id,
            is_redo,
            result: Err(UndoRedoUiError::Message(
                "Time-Machine Undo/Redo backend is not initialized".to_string(),
            )),
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
        return;
    };

    host.tokio_handle.spawn(async move {
        let _lease = internal_ops.acquire_lease();
        let res = if is_redo {
            undo_engine.redo(&repo_root, allow_rewrite_pushed).await
        } else {
            undo_engine.undo(&repo_root, allow_rewrite_pushed).await
        };
        let mapped = res.map_err(|err| match err {
            OpLogError::RemoteExposureConfirmationRequired { exposure } => {
                let remote_branches = match exposure {
                    RemoteExposureState::TouchesPushedHistory {
                        remote_branches, ..
                    } => remote_branches,
                    RemoteExposureState::LocalOnly => Vec::new(),
                };
                UndoRedoUiError::RemoteConfirmationRequired { remote_branches }
            }
            other => UndoRedoUiError::Message(other.to_string()),
        });
        let _ = event_tx.send(PipelineEvent::UndoRedoFinished {
            repo_id,
            is_redo,
            result: mapped,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Starts `fs-watcher` for `repo_id` and forwards debounced [`WatcherEvent`]s into [`PipelineEvent`].
#[allow(clippy::too_many_arguments)]
pub fn start_repo_watcher(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: &HostPath,
    git_dir: &HostPath,
    common_git_dir: &HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    internal_ops: Arc<InternalOpController>,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) -> Option<Box<dyn RepoWatchHandle>> {
    let watcher = host.repo_watcher.as_ref()?;
    let (handle, mut rx) = match watcher.watch_repository(
        repo_id,
        repo_root,
        git_dir,
        common_git_dir,
        internal_ops,
        WatcherConfig::default(),
    ) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(repo_id = ?repo_id, error = %err, "failed to start fs-watcher");
            return None;
        }
    };

    host.tokio_handle.spawn(async move {
        let full = DirtyScopeFlags::WORKTREE
            .union(DirtyScopeFlags::INDEX)
            .union(DirtyScopeFlags::REFS_OR_HEAD);
        while let Some(ev) = rx.recv().await {
            let (scopes, storm_coalesced) = match ev {
                WatcherEvent::Dirty {
                    scopes,
                    storm_coalesced,
                    ..
                } => (scopes, storm_coalesced),
                WatcherEvent::KernelRescanRequired { .. } => (full, true),
            };
            if event_tx
                .send(PipelineEvent::WatcherTriggered {
                    repo_id,
                    scopes,
                    storm_coalesced,
                })
                .is_err()
            {
                break;
            }
            let epoch = mvcc_store.committed_epoch();
            wakeup.notify_repo_updated(repo_id, epoch);
        }
    });

    Some(handle)
}

/// Spawns an embedded OS shell session in `repo_root` and wires PTY damage wakeups to the UI.
pub fn spawn_open_terminal(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    size: PtySize,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(backend) = host.terminal_backend.clone() else {
        return;
    };

    host.tokio_handle.spawn(async move {
        let profiles = backend.detect_available_shells(&repo_root);
        let profile = profiles.into_iter().next().unwrap_or(ShellProfile {
            label: CompactString::const_new("shell"),
            program: "/bin/sh".to_string(),
            args: Vec::new(),
            working_dir: repo_root,
        });
        let title = profile.label.clone();

        match backend.spawn_session(profile, size).await {
            Ok((boxed_session, mut damage_rx)) => {
                let session: Arc<dyn TerminalSession> = Arc::from(boxed_session);
                let _ = event_tx.send(PipelineEvent::TerminalSessionReady {
                    repo_id,
                    session,
                    title,
                });
                let epoch = mvcc_store.committed_epoch();
                wakeup.notify_repo_updated(repo_id, epoch);

                while damage_rx.recv().await.is_some() {
                    if event_tx
                        .send(PipelineEvent::TerminalDamaged { repo_id })
                        .is_err()
                    {
                        break;
                    }
                    let ep = mvcc_store.committed_epoch();
                    wakeup.notify_repo_updated(repo_id, ep);
                }
            }
            Err(err) => {
                tracing::warn!(repo_id = ?repo_id, error = %err, "failed to spawn terminal PTY");
            }
        }
    });
}

/// Fetches open Pull/Merge Requests and CI check runs for the repository's `origin` remote.
pub fn spawn_load_pull_requests(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(forge) = host.forge_provider.clone() else {
        let _ = event_tx.send(PipelineEvent::PullRequestsLoaded {
            repo_id,
            remote_label: "origin".to_string(),
            result: Err("Forge client is not configured".to_string()),
        });
        return;
    };
    let git_env = Arc::clone(&host.git_env);

    host.tokio_handle.spawn(async move {
        let res = async {
            let remote_url = read_origin_remote_url(&git_env, &repo_root).await?;
            let target = parse_remote_url(&remote_url, None).map_err(|e| e.to_string())?;
            let label = format!("{}/{}", target.owner, target.repo_name);
            let prs = forge
                .list_pull_requests(&target, Some(PullRequestState::Open))
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((label, prs))
        }
        .await;

        match res {
            Ok((remote_label, prs)) => {
                let _ = event_tx.send(PipelineEvent::PullRequestsLoaded {
                    repo_id,
                    remote_label,
                    result: Ok(prs),
                });
            }
            Err(err) => {
                let _ = event_tx.send(PipelineEvent::PullRequestsLoaded {
                    repo_id,
                    remote_label: "origin".to_string(),
                    result: Err(err),
                });
            }
        }
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Creates a Pull/Merge Request on the repository's `origin` remote.
pub fn spawn_create_pull_request(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    input: CreatePullRequestInput,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(forge) = host.forge_provider.clone() else {
        let _ = event_tx.send(PipelineEvent::PullRequestCreated {
            repo_id,
            result: Err("Forge client is not configured".to_string()),
        });
        return;
    };
    let git_env = Arc::clone(&host.git_env);

    host.tokio_handle.spawn(async move {
        let res = async {
            let remote_url = read_origin_remote_url(&git_env, &repo_root).await?;
            let target = parse_remote_url(&remote_url, None).map_err(|e| e.to_string())?;
            forge
                .create_pull_request(&target, input)
                .await
                .map_err(|e| e.to_string())
        }
        .await;

        let _ = event_tx.send(PipelineEvent::PullRequestCreated {
            repo_id,
            result: res,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Generates a Conventional Commit suggestion via `ai-copilot` from current staged diff (`git diff --cached`).
///
/// `allow_cloud_upload` is the user's consent for this repository (see
/// [`crate::presenter::AiPrivacySettings`]); without it off-device providers refuse the request
/// before any network I/O.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ai_commit_message(
    host: &CoreAppHost,
    repo_id: RepoId,
    repo_root: HostPath,
    mvcc_store: Arc<MvccRepoStateStore>,
    recent_subjects: Vec<String>,
    allow_cloud_upload: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(ai) = host.ai_provider.clone() else {
        let _ = event_tx.send(PipelineEvent::AiCommitSuggested {
            repo_id,
            result: Err(AiUiError::Message(
                "AI Copilot provider is not configured".to_string(),
            )),
        });
        return;
    };
    let config = resolve_ai_config(host.ai_config.as_ref(), allow_cloud_upload);
    let git_env = Arc::clone(&host.git_env);

    host.tokio_handle.spawn(async move {
        let res = async {
            let diff = read_staged_diff_excerpt(&git_env, &repo_root)
                .await
                .map_err(AiUiError::Message)?;
            if diff.trim().is_empty() {
                return Err(AiUiError::Message(
                    "Stage at least one change before generating an AI commit message".to_string(),
                ));
            }
            ai.generate_commit_message(&config, &diff, &recent_subjects)
                .await
                .map_err(AiUiError::from)
        }
        .await;
        let _ = event_tx.send(PipelineEvent::AiCommitSuggested {
            repo_id,
            result: res,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Requests AI 3-way conflict resolution advice for a single [`ConflictRegion`].
///
/// `allow_cloud_upload` carries the user's per-repository consent for off-device providers.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ai_conflict_advice(
    host: &CoreAppHost,
    repo_id: RepoId,
    mvcc_store: Arc<MvccRepoStateStore>,
    file_path: RepoRelPath,
    region_id: u32,
    base_snippet: Option<String>,
    ours_snippet: String,
    theirs_snippet: String,
    allow_cloud_upload: bool,
    event_tx: mpsc::UnboundedSender<PipelineEvent>,
    wakeup: Arc<dyn UiWakeupSink>,
) {
    let Some(ai) = host.ai_provider.clone() else {
        let _ = event_tx.send(PipelineEvent::AiConflictSuggested {
            repo_id,
            region_id,
            result: Err(AiUiError::Message(
                "AI Copilot provider is not configured".to_string(),
            )),
        });
        return;
    };
    let config = resolve_ai_config(host.ai_config.as_ref(), allow_cloud_upload);

    host.tokio_handle.spawn(async move {
        let res = ai
            .suggest_conflict_resolution(
                &config,
                &file_path,
                base_snippet.as_deref(),
                &ours_snippet,
                &theirs_snippet,
            )
            .await
            .map_err(AiUiError::from);
        let _ = event_tx.send(PipelineEvent::AiConflictSuggested {
            repo_id,
            region_id,
            result: res,
        });
        let epoch = mvcc_store.committed_epoch();
        wakeup.notify_repo_updated(repo_id, epoch);
    });
}

/// Formats short label for [`ConflictResolution`] recommendations surfaced from AI Copilot.
pub const fn conflict_strategy_label(strategy: &ConflictResolution) -> &'static str {
    match strategy {
        ConflictResolution::TakeOurs => "Take Ours",
        ConflictResolution::TakeTheirs => "Take Theirs",
        ConflictResolution::TakeBase => "Take Base",
        ConflictResolution::TakeBothOursFirst => "Take Both (Ours -> Theirs)",
        ConflictResolution::TakeBothTheirsFirst => "Take Both (Theirs -> Ours)",
        ConflictResolution::LineLevelSelection(_) => "Mixed Lines",
        ConflictResolution::ManualEdit => "Apply Proposed Edit",
        ConflictResolution::Unresolved => "Unresolved",
    }
}

/// Default Anthropic model used when `BRANVIO_AI_MODEL` is not set.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";

/// Builds the effective endpoint config for one AI request.
///
/// `allow_cloud_upload` is the user's consent for the target repository. It is OR-ed with an
/// embedder-supplied `explicit.allow_cloud_upload` so a host can pre-authorize. The
/// `BRANVIO_AI_*` environment variables only select provider/URL/model; they never grant consent.
pub(crate) fn resolve_ai_config(
    explicit: Option<&AiEndpointConfig>,
    allow_cloud_upload: bool,
) -> AiEndpointConfig {
    if let Some(cfg) = explicit {
        let mut cfg = cfg.clone();
        cfg.allow_cloud_upload |= allow_cloud_upload;
        return cfg;
    }
    let kind = match std::env::var("BRANVIO_AI_PROVIDER")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "openai" => AiProviderKind::OpenAiCompatible,
        "anthropic" | "claude" => AiProviderKind::AnthropicClaude,
        "gemini" => AiProviderKind::GoogleGemini,
        _ => AiProviderKind::OllamaLocal,
    };
    let (default_url, default_model) = match kind {
        AiProviderKind::OllamaLocal => ("http://127.0.0.1:11434", "qwen2.5-coder:14b"),
        AiProviderKind::OpenAiCompatible => ("https://api.openai.com", "gpt-4o-mini"),
        AiProviderKind::AnthropicClaude => ("https://api.anthropic.com", DEFAULT_ANTHROPIC_MODEL),
        AiProviderKind::GoogleGemini => (
            "https://generativelanguage.googleapis.com",
            "gemini-2.5-flash",
        ),
    };
    let base_url = std::env::var("BRANVIO_AI_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_url.to_string());
    let model_name = std::env::var("BRANVIO_AI_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(CompactString::from)
        .unwrap_or_else(|| CompactString::from(default_model));
    AiEndpointConfig {
        kind,
        base_url,
        model_name,
        custom_commit_style_hint: None,
        allow_cloud_upload,
    }
}

async fn read_origin_remote_url(
    git_env: &branvio_core::platform::HydratedGitEnv,
    repo_root: &HostPath,
) -> Result<String, String> {
    let git_bin = git_env
        .git_executable
        .as_deref()
        .unwrap_or_else(|| Path::new("git"));
    let envs = git_env.build_subprocess_env(SubprocessEnvOptions {
        read_only_no_locks: true,
        force_c_locale: true,
        ..SubprocessEnvOptions::default()
    });
    let out = Command::new(git_bin)
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_root.as_path())
        .envs(envs)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("No 'origin' remote configured in repository".to_string());
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        return Err("Remote 'origin' URL is empty".to_string());
    }
    Ok(url)
}

async fn read_staged_diff_excerpt(
    git_env: &branvio_core::platform::HydratedGitEnv,
    repo_root: &HostPath,
) -> Result<String, String> {
    let git_bin = git_env
        .git_executable
        .as_deref()
        .unwrap_or_else(|| Path::new("git"));
    let envs = git_env.build_subprocess_env(SubprocessEnvOptions {
        read_only_no_locks: true,
        force_c_locale: true,
        ..SubprocessEnvOptions::default()
    });
    let out = Command::new(git_bin)
        // `--no-ext-diff` / `--no-textconv`: never execute repo-configured diff drivers.
        .args([
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-U3",
        ])
        .current_dir(repo_root.as_path())
        .envs(envs)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(bound_staged_diff(&String::from_utf8_lossy(&out.stdout)))
}

/// Maximum number of bytes of (already sanitized) staged diff handed to the AI provider.
const STAGED_DIFF_EXCERPT_MAX_BYTES: usize = 12_000;

/// Sanitizes the **full** diff (drops sensitive files, masks secrets) and only then truncates it
/// on a char boundary, so a secret straddling the cut-off can never leak as an unmasked fragment.
fn bound_staged_diff(raw: &str) -> String {
    let sanitized = ai_copilot::redact::sanitize_diff(raw).text;
    let mut end = STAGED_DIFF_EXCERPT_MAX_BYTES.min(sanitized.len());
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized.get(..end).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_config(allow: bool) -> AiEndpointConfig {
        AiEndpointConfig {
            kind: AiProviderKind::AnthropicClaude,
            base_url: "https://api.anthropic.com".to_string(),
            model_name: CompactString::const_new(DEFAULT_ANTHROPIC_MODEL),
            custom_commit_style_hint: None,
            allow_cloud_upload: allow,
        }
    }

    #[test]
    fn explicit_config_keeps_consent_off_unless_user_allows() {
        let cfg = cloud_config(false);
        assert!(!resolve_ai_config(Some(&cfg), false).allow_cloud_upload);
        assert!(resolve_ai_config(Some(&cfg), true).allow_cloud_upload);
    }

    #[test]
    fn explicit_host_consent_is_respected() {
        let cfg = cloud_config(true);
        assert!(resolve_ai_config(Some(&cfg), false).allow_cloud_upload);
    }

    #[test]
    fn staged_diff_is_sanitized_before_truncation() {
        let token = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        // Place the token so that the 12k cut falls in the middle of it.
        let pad = "+".to_string() + &"a".repeat(STAGED_DIFF_EXCERPT_MAX_BYTES - 20) + "\n";
        let raw = format!("{pad}+{token}\n");
        let raw_cut = &raw[..STAGED_DIFF_EXCERPT_MAX_BYTES];
        assert!(
            raw_cut.contains("ghp_ABCDEFGH"),
            "test setup: cut must split token"
        );
        let out = bound_staged_diff(&raw);
        assert!(out.len() <= STAGED_DIFF_EXCERPT_MAX_BYTES);
        assert!(!out.contains("ghp_"), "token fragment leaked");
        assert!(!out.contains("ABCDEFGH"), "token fragment leaked");
    }

    #[test]
    fn staged_diff_truncates_on_char_boundary() {
        let raw = "é".repeat(STAGED_DIFF_EXCERPT_MAX_BYTES);
        let out = bound_staged_diff(&raw);
        assert!(out.len() <= STAGED_DIFF_EXCERPT_MAX_BYTES);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn default_anthropic_model_is_current() {
        assert_eq!(DEFAULT_ANTHROPIC_MODEL, "claude-sonnet-5-5");
    }
}
