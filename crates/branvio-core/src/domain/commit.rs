//! Compact commit, reference, and signature domain types.
//!
//! Designed for monorepo scale (>100,000 commits under 300 MB RAM) using
//! stack-allocated [`GitOid`], `SmallVec`, `CompactString`, and bitflags.

use crate::domain::diff::FileDelta;
use crate::ids::GitOid;
use bitflags::bitflags;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Compact commit author or committer identity with Unix timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactSignature {
    /// Display name (inline up to 24 bytes via `CompactString`).
    pub name: CompactString,
    /// Author or committer email address.
    pub email: CompactString,
    /// UTC timestamp in seconds since Unix epoch.
    pub timestamp_secs: i64,
    /// Original timezone offset in minutes relative to UTC.
    pub tz_offset_mins: i16,
}

/// Cryptographic signature verification status (GPG, SSH, or S/MIME).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitSignatureState {
    /// Commit does not contain a `gpgsig` header.
    Unsigned,
    /// Signature is present but verification has not yet run or was skipped.
    Unverified,
    /// Verified valid GPG signature.
    ValidGpg {
        /// Key ID or fingerprint reported by GnuPG.
        key_id: CompactString,
    },
    /// Verified valid SSH commit signature (`gpg.format = ssh`).
    ValidSsh {
        /// SSH public key fingerprint (e.g. `SHA256:...`).
        fingerprint: CompactString,
    },
    /// Signature verification failed or key is revoked/expired.
    Invalid {
        /// Reason returned by `git verify-commit` or GPG.
        reason: CompactString,
    },
    /// Signature exists, but the signing public key is absent from the local keyring/allowed_signers.
    UnknownKey {
        /// Key identifier extracted from the signature packet.
        key_id: CompactString,
    },
}

bitflags! {
    /// Dense metadata flags packed into a single byte per [`CommitNode`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct CommitFlags: u8 {
        /// Synthetic node representing uncommitted working-tree or index changes.
        const IS_VIRTUAL_WIP    = 1 << 0;
        /// Commit currently pointed to by `HEAD`.
        const IS_HEAD           = 1 << 1;
        /// Commit has 2 or more parents.
        const IS_MERGE          = 1 << 2;
        /// Commit has 3 or more parents (octopus merge).
        const IS_OCTOPUS        = 1 << 3;
        /// Root commit with 0 parents (including orphan branch roots).
        const IS_ORPHAN_ROOT    = 1 << 4;
        /// Commit node originates from `refs/stash` history.
        const IS_STASH          = 1 << 5;
        /// Commit lies on the first-parent chain of the primary trunk (`main`/`master`/`develop`).
        const ON_PRIMARY_TRUNK  = 1 << 6;
    }
}

/// Lightweight commit metadata kept resident in memory for graph traversal and virtualized rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitNode {
    /// Object ID of the commit (or [`GitOid::VIRTUAL_WIP`] for WIP row).
    pub oid: GitOid,
    /// Parent OIDs in Git parent order (inline up to 2 parents; spills cleanly for octopus merges).
    pub parents: SmallVec<[GitOid; 2]>,
    /// Root tree OID for this commit (or `VIRTUAL_WIP` if virtual).
    pub tree_oid: GitOid,
    /// Author identity and timestamp.
    pub author: CompactSignature,
    /// Committer identity and timestamp.
    pub committer: CompactSignature,
    /// First line (subject) of the commit message.
    pub summary: CompactString,
    /// Whether the commit has a multi-line body beyond `summary` (loaded lazily on selection).
    pub has_extended_body: bool,
    /// GPG/SSH commit verification state.
    pub signature_state: CommitSignatureState,
    /// Bitmask of structural commit properties.
    pub flags: CommitFlags,
    /// Topological commit-graph generation number (used for fast reachability pruning).
    pub generation: u32,
}

/// Full commit details fetched on demand when inspecting a commit in the Right Inspector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetails {
    /// Target commit OID.
    pub oid: GitOid,
    /// Complete commit message including subject and body.
    pub full_message: String,
    /// Parsed RFC-822 style trailers such as `Signed-off-by` or `Co-authored-by`.
    pub trailers: Vec<(CompactString, CompactString)>,
    /// Raw verification diagnostic output from GPG or `ssh-keygen`, if any.
    pub signature_diagnostic: Option<String>,
    /// Files changed relative to the first parent (or empty tree for root commits).
    pub changed_files: Vec<FileDelta>,
}

/// Classification of a Git reference attached to a commit node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefKind {
    /// Local branch under `refs/heads/*`.
    LocalBranch,
    /// Remote-tracking branch under `refs/remotes/<remote>/*`.
    RemoteBranch {
        /// Remote name (e.g., `origin`, `upstream`).
        remote: CompactString,
    },
    /// Lightweight or annotated tag under `refs/tags/*`.
    Tag {
        /// `true` if backed by a tag object with tagger/message.
        is_annotated: bool,
    },
    /// Stash entry under `refs/stash` or its reflog.
    Stash,
    /// Detached `HEAD` indicator.
    HeadDetached,
    /// Forge pull/merge request ref (e.g., `refs/pull/123/head`).
    PullRequestRef {
        /// Pull/Merge Request numeric ID.
        number: u64,
    },
}

/// UI badge representing a Git ref pointing at a specific commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefBadge {
    /// Short display name (e.g., `feature/auth`, `origin/main`, `v1.2.0`).
    pub name: CompactString,
    /// Canonical full ref name (e.g., `refs/heads/feature/auth`).
    pub full_ref: CompactString,
    /// Category of the reference.
    pub kind: RefKind,
    /// Whether `HEAD` currently resolves to this ref.
    pub is_head: bool,
    /// Ahead/behind counts relative to configured upstream `(ahead, behind)`.
    pub upstream_divergence: Option<(u32, u32)>,
}

/// Sparse lookup table mapping commit OIDs to their attached reference badges (`O(1)` lookup).
///
/// Stored out-of-line from [`crate::domain::GraphRow`] so rows without branches/tags pay zero bytes.
pub type RefBadgeMap = std::collections::HashMap<GitOid, SmallVec<[RefBadge; 2]>>;
