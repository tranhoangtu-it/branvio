//! Branvio binary entry point handling both internal Git CLI helper invocations
//! (`--branvio-git-sequence-editor`, `--branvio-git-editor`, `--branvio-git-askpass`)
//! and normal native GPUI desktop launch.

#![deny(unsafe_code)]

use ai_copilot::{AiCopilotProvider, HttpAiCopilotProvider};
use app_ui::cli_helper::try_handle_internal_git_helper;
use app_ui::ports::CoreAppHost;
use app_ui::presenter::pipeline_worker::make_empty_repo_snapshot;
use branvio_core::ids::RepoId;
use branvio_core::pipeline::MvccRepoStateStore;
use branvio_core::platform::HydratedGitEnv;
use forge_client::{ForgeProvider, OsKeyringVault, TokenVault, UnifiedForgeProvider};
use fs_watcher::{NotifyRepoWatcher, RepoWatcher};
use git_engine::{BranvioGitEngine, GitReadEngine, HybridGitEngine};
use oplog_undo::{BranvioUndoEngine, OpLogStore, RedbOpLogStore, UndoRedoEngine};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use terminal_pty::{PortablePtyBackend, TerminalBackend};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let args_os: Vec<OsString> = std::env::args_os().collect();

    // Fast path for Git subprocess callbacks (`< 2ms` startup, zero GUI initialization).
    match try_handle_internal_git_helper(&args_os) {
        Ok(Some(code)) => return ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Ok(None) => {}
        Err(err) => {
            eprintln!("branvio git helper error: {err}");
            return ExitCode::from(1);
        }
    }

    init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("branvio-tokio-worker")
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("failed to initialize Tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    let git_env = Arc::new(HydratedGitEnv::from_current_process());
    let self_exe = std::env::current_exe().ok();
    let git_engine = Arc::new(BranvioGitEngine::new(Arc::clone(&git_env), self_exe));
    let hybrid_git: Arc<dyn HybridGitEngine> = Arc::clone(&git_engine) as Arc<dyn HybridGitEngine>;

    let candidate_path = args_os
        .get(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    let mut initial_repos = Vec::new();
    if let Some(ref path) = candidate_path
        && let Ok(summary) = runtime.block_on(git_engine.open_repo(path))
    {
        let repo_id = RepoId(1);
        let initial_snap = make_empty_repo_snapshot(repo_id, summary.workdir_root.clone());
        let store = Arc::new(MvccRepoStateStore::new(initial_snap));
        initial_repos.push((repo_id, summary.workdir_root, store));
    }

    let oplog_store: Option<Arc<dyn OpLogStore>> = match RedbOpLogStore::open_default() {
        Ok(store) => Some(Arc::new(store) as Arc<dyn OpLogStore>),
        Err(err) => {
            tracing::warn!("oplog database unavailable, Undo/Redo disabled: {err}");
            None
        }
    };
    let undo_engine: Option<Arc<dyn UndoRedoEngine>> = oplog_store.as_ref().map(|store| {
        Arc::new(BranvioUndoEngine::new(
            Arc::clone(store),
            Arc::clone(&hybrid_git),
            Arc::clone(&git_env),
        )) as Arc<dyn UndoRedoEngine>
    });
    let repo_watcher: Option<Arc<dyn RepoWatcher>> =
        Some(Arc::new(NotifyRepoWatcher::new(runtime.handle().clone())));
    let terminal_backend: Option<Arc<dyn TerminalBackend>> =
        Some(Arc::new(PortablePtyBackend::new(Arc::clone(&git_env))));
    let token_vault: Option<Arc<dyn TokenVault>> = Some(Arc::new(OsKeyringVault::new()));
    let forge_provider: Option<Arc<dyn ForgeProvider>> =
        UnifiedForgeProvider::new(token_vault.clone())
            .ok()
            .map(|p| Arc::new(p) as Arc<dyn ForgeProvider>);
    let ai_provider: Option<Arc<dyn AiCopilotProvider>> = HttpAiCopilotProvider::new(None)
        .ok()
        .map(|p| Arc::new(p) as Arc<dyn AiCopilotProvider>);

    let host = CoreAppHost::new(runtime.handle().clone(), git_env, hybrid_git, initial_repos)
        .with_optional_services(undo_engine, forge_provider, ai_provider)
        .with_phase5_runtime(
            oplog_store,
            repo_watcher,
            terminal_backend,
            token_vault,
            None,
        );

    launch_ui_backend(host)
}

#[cfg(feature = "gpui-backend")]
fn launch_ui_backend(host: CoreAppHost) -> ExitCode {
    use app_ui::ports::UiRuntimeBackend;

    let backend: Box<dyn UiRuntimeBackend> = Box::new(app_ui::GpuiRuntimeBackend::new());
    tracing::info!("launching UI backend: {}", backend.backend_name());
    if let Err(err) = backend.run_event_loop(host) {
        eprintln!("branvio UI error: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(not(feature = "gpui-backend"))]
fn launch_ui_backend(_host: CoreAppHost) -> ExitCode {
    eprintln!("branvio built in headless mode (`gpui-backend` feature disabled)");
    ExitCode::SUCCESS
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wgpu=warn,gpui=warn,naga=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
