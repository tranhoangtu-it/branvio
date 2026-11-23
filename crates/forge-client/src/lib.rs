//! Unified Git hosting forge clients (GitHub, GitLab, Bitbucket, Gitea) and OS Keyring token vault.

pub mod providers;
pub mod remote_url;
pub mod traits;
pub mod vault;

pub use providers::{
    BitbucketForgeProvider, DEFAULT_TOKEN_ACCOUNT, GitHubForgeProvider, GitLabForgeProvider,
    GiteaForgeProvider, UnifiedForgeProvider,
};
pub use remote_url::parse_remote_url;
pub use traits::{
    CiCheckRun, CiCheckStatus, CreatePullRequestInput, ForgeError, ForgeKind, ForgeProvider,
    ForgeRemoteTarget, ForgeResult, PullRequestState, PullRequestSummary, TokenVault,
};
pub use vault::{InMemoryTokenVault, OsKeyringVault};
