//! Unified traits for GitHub, GitLab, Bitbucket, and Gitea/Self-hosted integration
//! plus OS-native Keyring token storage.

use async_trait::async_trait;
use branvio_core::ids::GitOid;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias for `forge-client`.
pub type ForgeResult<T> = Result<T, ForgeError>;

/// Errors raised during forge API calls or OS keyring access.
#[derive(Debug, Error)]
pub enum ForgeError {
    /// OS credential vault (`Keychain`, `Credential Manager`, `Secret Service`) error.
    #[error("secure token vault failure: {0}")]
    Keyring(String),

    /// HTTP transport error talking to forge API.
    #[error("HTTP request failure: {0}")]
    Http(#[from] reqwest::Error),

    /// Forge rejected request due to missing/expired OAuth token or PAT scope.
    #[error("authentication failed for {host}: {message}")]
    Unauthorized {
        /// Forge hostname.
        host: String,
        /// Server diagnostic message.
        message: String,
    },

    /// API rate limit exceeded.
    #[error("rate limit exceeded for {host}, retry after {retry_after_secs:?}s")]
    RateLimited {
        /// Forge hostname.
        host: String,
        /// Optional retry duration reported in headers.
        retry_after_secs: Option<u64>,
    },

    /// Unexpected API payload or unsupported remote URL format.
    #[error("forge protocol error: {0}")]
    Protocol(String),
}

/// Supported Git hosting platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForgeKind {
    /// GitHub / GitHub Enterprise.
    GitHub,
    /// GitLab.com / Self-managed GitLab.
    GitLab,
    /// Atlassian Bitbucket Cloud / Server.
    Bitbucket,
    /// Gitea / Forgejo self-hosted.
    Gitea,
}

/// Parsed repository identity extracted from a Git remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForgeRemoteTarget {
    /// Hosting platform kind.
    pub kind: ForgeKind,
    /// Base API origin (e.g. `https://api.github.com` or `https://git.company.internal`).
    pub api_base_url: String,
    /// Namespace / organization / owner slug.
    pub owner: CompactString,
    /// Repository slug.
    pub repo_name: CompactString,
}

/// Lifecycle state of a Pull Request / Merge Request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PullRequestState {
    /// Open and active.
    Open,
    /// Draft / Work-in-Progress.
    Draft,
    /// Merged into target branch.
    Merged,
    /// Closed without merging.
    Closed,
}

/// Aggregated CI/CD check status on the PR head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CiCheckStatus {
    /// No CI checks reported.
    None,
    /// Checks queued or running.
    Pending,
    /// All required checks succeeded.
    Passing,
    /// One or more checks failed or errored.
    Failing,
}

/// Single CI pipeline/check entry shown inside the PR inspector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCheckRun {
    /// Workflow or job name.
    pub name: CompactString,
    /// Status.
    pub status: CiCheckStatus,
    /// Optional external details URL.
    pub details_url: Option<String>,
}

/// Unified Pull Request / Merge Request representation across all forges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSummary {
    /// PR / MR number (`#42`, `!42`).
    pub number: u64,
    /// PR title.
    pub title: String,
    /// PR markdown body.
    pub body: String,
    /// Author login handle.
    pub author: CompactString,
    /// Current state.
    pub state: PullRequestState,
    /// Source branch name.
    pub head_branch: CompactString,
    /// Target base branch name.
    pub base_branch: CompactString,
    /// Head commit OID if provided by the API.
    pub head_oid: Option<GitOid>,
    /// Combined CI check status.
    pub ci_status: CiCheckStatus,
    /// Individual check runs.
    pub checks: Vec<CiCheckRun>,
    /// Web UI URL.
    pub web_url: String,
}

/// Parameters for opening a new Pull Request / Merge Request directly from Branvio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePullRequestInput {
    /// PR / MR title.
    pub title: String,
    /// Markdown description.
    pub body: String,
    /// Source branch already pushed to remote.
    pub head_branch: CompactString,
    /// Target base branch.
    pub base_branch: CompactString,
    /// Create as Draft PR/MR.
    pub draft: bool,
}

/// Secure cross-platform OAuth/PAT vault backed by the OS native keyring (`keyring` crate).
pub trait TokenVault: Send + Sync {
    /// Stores or rotates an API token for `(kind, host, account)` inside OS Keychain/Credential Manager.
    fn store_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
        token: &str,
    ) -> ForgeResult<()>;

    /// Retrieves a previously stored API token, if present.
    fn load_token(&self, kind: ForgeKind, host: &str, account: &str)
    -> ForgeResult<Option<String>>;

    /// Deletes a stored token from the OS keyring.
    fn delete_token(&self, kind: ForgeKind, host: &str, account: &str) -> ForgeResult<()>;
}

/// Unified hosting provider abstraction implemented identically by GitHub, GitLab, Bitbucket, and Gitea adapters.
#[async_trait]
pub trait ForgeProvider: Send + Sync {
    /// Provider kind handled by this implementation.
    fn kind(&self) -> ForgeKind;

    /// Lists Pull/Merge Requests filtered by state (`None` returns open + draft).
    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>>;

    /// Fetches detailed CI check runs for a specific PR/MR number.
    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>>;

    /// Creates a new Pull/Merge Request and returns its created summary.
    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary>;

    /// Resolves the canonical Git refspec to fetch and checkout a PR locally for review
    /// (e.g., `+refs/pull/<n>/head:refs/remotes/origin/pr/<n>` on GitHub or `merge-requests/<n>/head` on GitLab).
    fn local_checkout_refspec(&self, pr_number: u64) -> String;
}
