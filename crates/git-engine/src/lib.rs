//! 3-Tier Hybrid Git Engine traits, concrete implementations, typed requests, and error definitions.
//!
//! - **Tier 1 (`gix` / `gitoxide`):** Zero-lock asynchronous reading, stateful topological revwalk, blame, refs, and working-tree status.
//! - **Tier 2 (`git2`):** In-memory `.git/index` patch manipulation and predictive conflict simulation inside `spawn_blocking`.
//! - **Tier 3 (`tokio::process` native `git`):** State-mutating, pickaxe search, and network commands preserving hooks, LFS, GPG/SSH signing, and `.gitconfig`.

pub mod engine;
pub mod error;
pub mod oid_conv;
pub mod tier1_gix;
pub mod tier2_git2;
pub mod tier3_cli;
pub mod traits;
pub mod types;

pub use engine::BranvioGitEngine;
pub use error::{GitEngineError, GitEngineResult};
pub use tier1_gix::{GixReadEngine, GixRevwalkSession};
pub use tier2_git2::Git2IndexEngine;
pub use tier3_cli::{TokioGitCliExecutor, parse_worktree_porcelain_z};
pub use traits::{GitCliExecutor, GitIndexEngine, GitReadEngine, HybridGitEngine, RevwalkSession};
pub use types::{
    BlameLine, BlameResult, CliCommandHandle, CliStreamEvent, CommitOptions, ConflictStageBlobs,
    MergeOptions, MergePrediction, PickaxeSearchRequest, PredictiveConflictEntry, PushForce,
    RebaseAction, RebasePlanEntry, RebasePlanPayload, RefsSnapshot, RepoSummary, RevwalkChunk,
    RevwalkSessionParams, WorktreeInfo, build_safe_revision_args,
};
