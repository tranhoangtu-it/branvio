//! Typed error hierarchy for Tier 1 (`gix`), Tier 2 (`git2`), and Tier 3 (`git` CLI) operations.

use branvio_core::CoreError;
use thiserror::Error;

/// Result alias for `git-engine` operations.
pub type GitEngineResult<T> = Result<T, GitEngineError>;

/// Errors returned by the 3-tier hybrid Git engine.
#[derive(Debug, Error)]
pub enum GitEngineError {
    /// Core identifier or path error.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// Repository could not be opened or is not a valid Git working tree / bare repository.
    #[error("failed to open Git repository at '{path}': {message}")]
    OpenRepoFailed {
        /// Filesystem path inspected.
        path: String,
        /// Underlying diagnostic reason.
        message: String,
    },

    /// Operation was cooperatively cancelled because a newer [`branvio_core::RepoEpoch`] superseded it.
    #[error("git operation cancelled")]
    Cancelled,

    /// Tier 1 (`gix`) read/traversal failure.
    #[error("gitoxide read failure in {operation}: {message}")]
    GixRead {
        /// Logical operation name (`revwalk`, `status`, `blame`, etc.).
        operation: &'static str,
        /// Diagnostic error detail.
        message: String,
    },

    /// `.git/index.lock` is currently held by another Git process (external CLI or concurrent op).
    /// Callers may perform bounded backoff retries or surface a lock warning to the user.
    #[error("repository index is locked at '{lock_path}'")]
    IndexLocked {
        /// Path to the active `.git/index.lock` file.
        lock_path: String,
    },

    /// Tier 2 (`git2`) in-memory index or merge simulation failure.
    #[error("in-memory index operation failed in {operation}: {message}")]
    IndexOp {
        /// Logical operation name.
        operation: &'static str,
        /// Diagnostic error detail.
        message: String,
    },

    /// Synthetic patch could not be applied cleanly to index or worktree.
    #[error("patch application rejected: {reason}")]
    PatchConflict {
        /// Line/hunk mismatch explanation.
        reason: String,
    },

    /// Requested blob exceeds the caller-specified `max_bytes` safety limit.
    #[error("blob {oid} ({size_bytes} bytes) exceeds maximum limit of {max_bytes} bytes")]
    BlobTooLarge {
        /// Object ID of the oversized blob.
        oid: branvio_core::ids::GitOid,
        /// Uncompressed blob size from the ODB header.
        size_bytes: u64,
        /// Maximum allowed byte limit passed to `read_blob`.
        max_bytes: usize,
    },

    /// Explicit local filesystem/temp-file I/O error (distinct from child process spawn errors).
    #[error("I/O error during {operation}: {source}")]
    Io {
        /// Logical step where the I/O failure occurred.
        operation: &'static str,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Tier 3 (`git` CLI) process could not be spawned (missing binary or invalid executable path).
    #[error("failed to spawn native binary '{program}': {source}")]
    SpawnFailed {
        /// Executable name or path attempted.
        program: String,
        /// Underlying OS spawn error.
        #[source]
        source: std::io::Error,
    },

    /// Native `git` CLI returned a non-zero exit code.
    #[error("git {subcommand} failed (exit code {exit_code:?}): {stderr_summary}")]
    CliFailed {
        /// Subcommand executed (e.g. `rebase`, `push`, `commit`).
        subcommand: String,
        /// Process exit code, or `None` if terminated by signal.
        exit_code: Option<i32>,
        /// Extracted error lines from stderr.
        stderr_summary: String,
    },

    /// Native `git` CLI requires authentication or passphrase input via AskPass/Credential Manager.
    #[error("authentication required for remote '{remote}': {prompt}")]
    AuthRequired {
        /// Remote name or URL.
        remote: String,
        /// Prompt string from Git/SSH.
        prompt: String,
    },

    /// A pre-commit, commit-msg, or pre-push Git hook aborted the operation.
    #[error("git hook '{hook_name}' aborted the operation (exit {exit_code:?})")]
    HookAborted {
        /// Hook script name (e.g., `pre-commit`).
        hook_name: String,
        /// Hook exit code.
        exit_code: Option<i32>,
        /// Hook output shown to the user.
        output: String,
    },
}
