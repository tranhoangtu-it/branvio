//! **Tier 1 (`gix` / `gitoxide`):** Zero-lock asynchronous reading, stateful topological
//! revwalk sessions (`gix::traverse::commit::Topo`), blame, refs, commit details, and working-tree status.

use crate::error::{GitEngineError, GitEngineResult};
use crate::oid_conv::{git_oid_from_gix, git_oid_to_gix};
use crate::traits::{GitReadEngine, RevwalkSession};
use crate::types::{
    BlameLine, BlameResult, ConflictStageBlobs, RefsSnapshot, RepoSummary, RevwalkChunk,
    RevwalkSessionParams,
};
use async_trait::async_trait;
use branvio_core::domain::{
    CommitDetails, CommitFlags, CommitNode, CommitSignatureState, CompactSignature,
    ConflictSubkind, DeltaArea, DeltaStatus, EolChange, FileDelta, FileMode, LfsPointerMeta,
    RefBadge, RefBadgeMap, RefKind, RepoOperationState,
};
use branvio_core::ids::GitOid;
use branvio_core::paths::{FsCaseSensitivity, HostPath, RepoRelPath};
use branvio_core::pipeline::{CancelToken, WorkingTreeStatusView};
use bstr::{BStr, ByteSlice};
use compact_str::CompactString;
use gix::bstr::BString;
use gix::object::tree::diff::ChangeDetached;
use gix::prelude::FindExt;
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const TRUNK_BRANCH_CANDIDATES: &[&str] = &[
    "refs/heads/main",
    "refs/heads/master",
    "refs/heads/trunk",
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
    "refs/remotes/origin/trunk",
];

const MAX_LINE_STATS_BLOB_BYTES: u64 = 4 * 1024 * 1024;
const MAX_META_INSPECT_BLOB_BYTES: u64 = 32 * 1024 * 1024;

/// Tier 1 read engine backed by `gix`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GixReadEngine;

impl GixReadEngine {
    /// Creates a new [`GixReadEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

/// Object-database adapter that strips `parent <oid>` headers from shallow boundary commits
/// so `gix::traverse::commit::Topo` terminates cleanly at `.git/shallow` boundaries without
/// failing on un-fetched ancestor objects.
#[derive(Clone)]
struct ShallowAwareOdb {
    inner: gix::OdbHandle,
    shallow_oids: Arc<HashSet<gix::ObjectId>>,
}

impl gix::objs::Find for ShallowAwareOdb {
    fn try_find<'a>(
        &self,
        id: &gix::hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::Result<Option<gix::objs::Data<'a>>> {
        if !self.shallow_oids.is_empty() && self.shallow_oids.contains(&id.to_owned()) {
            let mut scratch = Vec::new();
            let Some(data) = self.inner.try_find(id, &mut scratch)? else {
                return Ok(None);
            };
            let kind = data.kind;
            let object_hash = data.object_hash;
            if kind == gix::objs::Kind::Commit {
                strip_commit_parent_headers_into(data.data, buffer);
            } else {
                buffer.clear();
                buffer.extend_from_slice(data.data);
            }
            return Ok(Some(gix::objs::Data {
                kind,
                object_hash,
                data: buffer.as_slice(),
            }));
        }
        self.inner.try_find(id, buffer)
    }
}

fn strip_commit_parent_headers_into(raw: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(raw.len());
    let mut pos = 0usize;
    while pos < raw.len() {
        let rel_end = raw[pos..].iter().position(|&b| b == b'\n');
        let line_end = match rel_end {
            Some(off) => pos + off + 1,
            None => raw.len(),
        };
        let line = &raw[pos..line_end];
        if line == b"\n" {
            out.extend_from_slice(&raw[pos..]);
            return;
        }
        if !line.starts_with(b"parent ") {
            out.extend_from_slice(line);
        }
        pos = line_end;
    }
}

type GixTopoWalker = gix::traverse::commit::Topo<ShallowAwareOdb, fn(&gix::hash::oid) -> bool>;

/// Stateful topological revwalk session backed by `gix::traverse::commit::Topo`.
pub struct GixRevwalkSession {
    objects: ShallowAwareOdb,
    topo: Option<GixTopoWalker>,
    head_oid: Option<GitOid>,
    primary_trunk_oids: HashSet<GitOid>,
    stash_oids: HashSet<GitOid>,
    path_filters: Vec<RepoRelPath>,
    exhausted: bool,
    decode_buf: Vec<u8>,
}

impl RevwalkSession for GixRevwalkSession {
    fn next_chunk(
        &mut self,
        max_commits: usize,
        cancel: &CancelToken,
    ) -> GitEngineResult<RevwalkChunk> {
        if self.exhausted || max_commits == 0 {
            return Ok(RevwalkChunk {
                commits: Vec::new(),
                exhausted: self.exhausted,
            });
        }

        let Some(topo) = self.topo.as_mut() else {
            self.exhausted = true;
            return Ok(RevwalkChunk {
                commits: Vec::new(),
                exhausted: true,
            });
        };

        let mut commits = Vec::with_capacity(max_commits.min(1024));
        while commits.len() < max_commits {
            if cancel.is_cancelled() {
                return Err(GitEngineError::Cancelled);
            }

            let Some(next_item) = topo.next() else {
                self.exhausted = true;
                break;
            };

            let info = next_item.map_err(|err| GitEngineError::GixRead {
                operation: "topo_revwalk",
                message: err.to_string(),
            })?;

            let oid = git_oid_from_gix(info.id)?;
            self.decode_buf.clear();
            let commit_ref = self
                .objects
                .find_commit(&info.id, &mut self.decode_buf)
                .map_err(|err| GitEngineError::GixRead {
                    operation: "find_commit",
                    message: err.to_string(),
                })?;

            let tree_oid = git_oid_from_gix(commit_ref.tree())?;
            let mut parents: SmallVec<[GitOid; 2]> = SmallVec::new();
            for p in commit_ref.parents() {
                parents.push(git_oid_from_gix(p)?);
            }

            if !self.path_filters.is_empty()
                && !commit_touches_any_path(
                    &self.objects,
                    &tree_oid,
                    parents.first().copied(),
                    &self.path_filters,
                )?
            {
                continue;
            }

            let author = to_compact_signature(commit_ref.author().map_err(|err| {
                GitEngineError::GixRead {
                    operation: "decode_author",
                    message: err.to_string(),
                }
            })?);
            let committer = to_compact_signature(commit_ref.committer().map_err(|err| {
                GitEngineError::GixRead {
                    operation: "decode_committer",
                    message: err.to_string(),
                }
            })?);

            let raw_msg = commit_ref.message.to_str_lossy();
            let (summary, has_extended_body) = split_subject_and_body_flag(raw_msg.as_ref());

            let signature_state = extract_signature_state(&commit_ref);
            let mut flags = CommitFlags::empty();
            if self.head_oid == Some(oid) {
                flags |= CommitFlags::IS_HEAD;
            }
            if parents.len() >= 2 {
                flags |= CommitFlags::IS_MERGE;
            }
            if parents.len() >= 3 {
                flags |= CommitFlags::IS_OCTOPUS;
            }
            if parents.is_empty() {
                flags |= CommitFlags::IS_ORPHAN_ROOT;
            }
            if self.primary_trunk_oids.contains(&oid) {
                flags |= CommitFlags::ON_PRIMARY_TRUNK;
            }
            if self.stash_oids.contains(&oid) {
                flags |= CommitFlags::IS_STASH;
            }

            commits.push(CommitNode {
                oid,
                parents,
                tree_oid,
                author,
                committer,
                summary,
                has_extended_body,
                signature_state,
                flags,
                generation: info.generation.unwrap_or(0),
            });
        }

        Ok(RevwalkChunk {
            commits,
            exhausted: self.exhausted,
        })
    }

    #[inline]
    fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

#[async_trait]
impl GitReadEngine for GixReadEngine {
    async fn open_repo(&self, path: &Path) -> GitEngineResult<RepoSummary> {
        let target_path = path.to_path_buf();
        tokio::task::spawn_blocking(move || open_repo_blocking(&target_path))
            .await
            .map_err(|err| GitEngineError::GixRead {
                operation: "open_repo_join",
                message: err.to_string(),
            })?
    }

    async fn head_and_refs(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<RefsSnapshot> {
        let root_path = repo_root.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || head_and_refs_blocking(&root_path, &cancel))
            .await
            .map_err(|err| GitEngineError::GixRead {
                operation: "head_and_refs_join",
                message: err.to_string(),
            })?
    }

    async fn open_revwalk_session(
        &self,
        repo_root: &HostPath,
        params: RevwalkSessionParams,
        cancel: CancelToken,
    ) -> GitEngineResult<Box<dyn RevwalkSession>> {
        let root_path = repo_root.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || open_revwalk_blocking(&root_path, params, &cancel))
            .await
            .map_err(|err| GitEngineError::GixRead {
                operation: "open_revwalk_join",
                message: err.to_string(),
            })?
    }

    async fn load_commit_details(
        &self,
        repo_root: &HostPath,
        oid: GitOid,
        cancel: CancelToken,
    ) -> GitEngineResult<CommitDetails> {
        let root_path = repo_root.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || load_commit_details_blocking(&root_path, oid, &cancel))
            .await
            .map_err(|err| GitEngineError::GixRead {
                operation: "load_commit_details_join",
                message: err.to_string(),
            })?
    }

    async fn working_tree_status(
        &self,
        repo_root: &HostPath,
        cancel: CancelToken,
    ) -> GitEngineResult<WorkingTreeStatusView> {
        let root_path = repo_root.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || working_tree_status_blocking(&root_path, &cancel))
            .await
            .map_err(|err| GitEngineError::GixRead {
                operation: "working_tree_status_join",
                message: err.to_string(),
            })?
    }

    async fn read_blob(
        &self,
        repo_root: &HostPath,
        blob_oid: GitOid,
        max_bytes: Option<usize>,
    ) -> GitEngineResult<Vec<u8>> {
        let root_path = repo_root.as_path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_gix_repo(&root_path)?;
            let id = git_oid_to_gix(blob_oid)?;
            read_blob_from_gix_repo(&repo, id, max_bytes)
        })
        .await
        .map_err(|err| GitEngineError::GixRead {
            operation: "read_blob_join",
            message: err.to_string(),
        })?
    }

    async fn read_conflict_stages(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        max_bytes: usize,
    ) -> GitEngineResult<ConflictStageBlobs> {
        let root_path = repo_root.as_path().to_path_buf();
        let target_path = path.clone();
        tokio::task::spawn_blocking(move || {
            let repo = open_gix_repo(&root_path)?;
            let index = repo
                .index_or_empty()
                .map_err(|err| GitEngineError::GixRead {
                    operation: "read_conflict_stages_index",
                    message: err.to_string(),
                })?;

            let mut base = None;
            let mut ours = None;
            let mut theirs = None;

            for entry in index.entries() {
                let stage = entry.stage();
                if stage == gix::index::entry::Stage::Unconflicted {
                    continue;
                }
                let entry_rel = entry.path(&index);
                let exact_match = entry_rel.as_bytes() == target_path.as_str().as_bytes()
                    || try_normalize_path_bytes(entry_rel, "conflict_stage_match").as_ref()
                        == Some(&target_path);
                if !exact_match {
                    continue;
                }
                let data = read_blob_from_gix_repo(&repo, entry.id, Some(max_bytes))?;
                match stage {
                    gix::index::entry::Stage::Base => base = Some(data),
                    gix::index::entry::Stage::Ours => ours = Some(data),
                    gix::index::entry::Stage::Theirs => theirs = Some(data),
                    gix::index::entry::Stage::Unconflicted => {}
                }
            }

            Ok(ConflictStageBlobs { base, ours, theirs })
        })
        .await
        .map_err(|err| GitEngineError::GixRead {
            operation: "read_conflict_stages_join",
            message: err.to_string(),
        })?
    }

    async fn blame_file(
        &self,
        repo_root: &HostPath,
        path: &RepoRelPath,
        at_commit: Option<GitOid>,
        cancel: CancelToken,
    ) -> GitEngineResult<BlameResult> {
        let root_path = repo_root.as_path().to_path_buf();
        let rel_path = path.clone();
        tokio::task::spawn_blocking(move || {
            blame_file_blocking(&root_path, &rel_path, at_commit, &cancel)
        })
        .await
        .map_err(|err| GitEngineError::GixRead {
            operation: "blame_file_join",
            message: err.to_string(),
        })?
    }
}

fn read_blob_from_gix_repo(
    repo: &gix::Repository,
    id: gix::ObjectId,
    max_bytes: Option<usize>,
) -> GitEngineResult<Vec<u8>> {
    let header = repo
        .find_header(id)
        .map_err(|err| GitEngineError::GixRead {
            operation: "find_blob_header",
            message: err.to_string(),
        })?;
    if header.kind() != gix::objs::Kind::Blob {
        let oid = git_oid_from_gix(id)?;
        return Err(GitEngineError::GixRead {
            operation: "read_blob_kind",
            message: format!("object {oid} is not a blob"),
        });
    }
    if let Some(cap) = max_bytes
        && header.size() > cap as u64
    {
        let oid = git_oid_from_gix(id)?;
        return Err(GitEngineError::BlobTooLarge {
            oid,
            size_bytes: header.size(),
            max_bytes: cap,
        });
    }
    let mut blob = repo.find_blob(id).map_err(|err| GitEngineError::GixRead {
        operation: "find_blob",
        message: err.to_string(),
    })?;
    Ok(std::mem::take(&mut blob.data))
}

fn open_gix_repo(path: &Path) -> GitEngineResult<gix::Repository> {
    gix::open(path).map_err(|err| GitEngineError::OpenRepoFailed {
        path: path.display().to_string(),
        message: err.to_string(),
    })
}

fn open_repo_blocking(path: &Path) -> GitEngineResult<RepoSummary> {
    let repo = open_gix_repo(path)?;
    let git_dir = HostPath::from_path_buf(repo.git_dir().to_path_buf());
    let common_git_dir = HostPath::from_path_buf(repo.common_dir().to_path_buf());
    let workdir_root = match repo.workdir() {
        Some(wd) => HostPath::canonicalize_existing(wd)?,
        None => common_git_dir.clone(),
    };
    let case_sensitivity = FsCaseSensitivity::detect(repo.git_dir());
    let operation_state = detect_operation_state(repo.git_dir());
    let (head_label, head_oid) = resolve_head_label_and_oid(&repo)?;

    Ok(RepoSummary {
        workdir_root,
        git_dir,
        common_git_dir,
        is_bare: repo.is_bare(),
        case_sensitivity,
        operation_state,
        head_label,
        head_oid,
    })
}

fn resolve_head_label_and_oid(
    repo: &gix::Repository,
) -> GitEngineResult<(CompactString, Option<GitOid>)> {
    let mut head = repo.head().map_err(|err| GitEngineError::GixRead {
        operation: "read_head",
        message: err.to_string(),
    })?;
    let head_name = head
        .referent_name()
        .map(|n| CompactString::from(n.shorten().to_str_lossy().as_ref()));
    let head_oid = match head
        .try_peel_to_id()
        .map_err(|err| GitEngineError::GixRead {
            operation: "peel_head",
            message: err.to_string(),
        })? {
        Some(id) => {
            if repo
                .find_header(id.detach())
                .is_ok_and(|h| h.kind() == gix::objs::Kind::Commit)
            {
                Some(git_oid_from_gix(id.detach())?)
            } else {
                None
            }
        }
        None => None,
    };

    let label = match (head_name, head_oid) {
        (Some(name), _) => name,
        (None, Some(oid)) => CompactString::from(oid.short_hex(8)),
        (None, None) => CompactString::const_new("HEAD (unborn)"),
    };
    Ok((label, head_oid))
}

fn detect_operation_state(git_dir: &Path) -> RepoOperationState {
    if git_dir.join("rebase-merge").join("interactive").exists() {
        RepoOperationState::InteractiveRebasing
    } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        RepoOperationState::Rebasing
    } else if git_dir.join("MERGE_HEAD").exists() {
        RepoOperationState::Merging
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        RepoOperationState::CherryPicking
    } else if git_dir.join("REVERT_HEAD").exists() {
        RepoOperationState::Reverting
    } else if git_dir.join("BISECT_LOG").exists() {
        RepoOperationState::Bisecting
    } else {
        RepoOperationState::Clean
    }
}

fn head_and_refs_blocking(root_path: &Path, cancel: &CancelToken) -> GitEngineResult<RefsSnapshot> {
    let mut repo = open_gix_repo(root_path)?;
    repo.object_cache_size_if_unset(4 * 1024 * 1024);

    let (head_label, head_oid) = resolve_head_label_and_oid(&repo)?;
    let head_referent_full = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.as_bstr().to_str_lossy().into_owned());

    let mut badges = Vec::new();
    let mut by_oid: RefBadgeMap = RefBadgeMap::new();

    let platform = repo.references().map_err(|err| GitEngineError::GixRead {
        operation: "references_platform",
        message: err.to_string(),
    })?;
    let iter = platform.all().map_err(|err| GitEngineError::GixRead {
        operation: "references_all",
        message: err.to_string(),
    })?;

    for reference_res in iter {
        if cancel.is_cancelled() {
            return Err(GitEngineError::Cancelled);
        }
        let mut reference = match reference_res {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("skipping unreadable git ref: {err}");
                continue;
            }
        };
        let full_name_str = reference.name().as_bstr().to_str_lossy().into_owned();
        let full_ref = CompactString::from(&full_name_str);

        let is_head = head_referent_full
            .as_deref()
            .is_some_and(|h| h == full_name_str);

        let mut upstream_divergence = None;
        let (short_name, kind) = if full_name_str == "refs/stash" {
            (CompactString::const_new("stash"), RefKind::Stash)
        } else if full_name_str.starts_with("refs/pull/") {
            let Some(num) = full_name_str
                .split('/')
                .nth(2)
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            let short = full_name_str
                .strip_prefix("refs/")
                .unwrap_or(full_name_str.as_str());
            (
                CompactString::from(short),
                RefKind::PullRequestRef { number: num },
            )
        } else {
            let Some((category, short_bstr)) = reference.name().category_and_short_name() else {
                continue;
            };
            if matches!(category, gix::refs::Category::RemoteBranch)
                && short_bstr.ends_with(b"/HEAD")
                && reference.target().try_id().is_none()
            {
                continue;
            }
            let short_name = CompactString::from(short_bstr.to_str_lossy().as_ref());
            let kind = match category {
                gix::refs::Category::LocalBranch => {
                    if let Some(Ok(upstream_full)) =
                        reference.remote_tracking_ref_name(gix::remote::Direction::Fetch)
                        && let Ok(mut upstream_ref) = repo.find_reference(upstream_full.as_ref())
                        && let (Ok(local_id), Ok(remote_id)) =
                            (reference.peel_to_id(), upstream_ref.peel_to_id())
                    {
                        upstream_divergence = compute_ahead_behind(
                            &repo,
                            local_id.detach(),
                            remote_id.detach(),
                            cancel,
                        );
                    }
                    RefKind::LocalBranch
                }
                gix::refs::Category::RemoteBranch => {
                    let remote_name = short_name.split_once('/').map_or_else(
                        || CompactString::const_new("origin"),
                        |(r, _)| CompactString::from(r),
                    );
                    RefKind::RemoteBranch {
                        remote: remote_name,
                    }
                }
                gix::refs::Category::Tag => {
                    let is_annotated = reference
                        .try_id()
                        .and_then(|id| id.object().ok())
                        .is_some_and(|obj| obj.kind == gix::object::Kind::Tag);
                    RefKind::Tag { is_annotated }
                }
                _ => continue,
            };
            (short_name, kind)
        };

        let Ok(target_id) = reference.peel_to_id() else {
            continue;
        };
        let target_oid = git_oid_from_gix(target_id.detach())?;

        let badge = RefBadge {
            name: short_name,
            full_ref,
            kind,
            is_head,
            upstream_divergence,
        };
        by_oid.entry(target_oid).or_default().push(badge.clone());
        badges.push(badge);
    }

    if head_referent_full.is_none()
        && let Some(oid) = head_oid
    {
        let detached = RefBadge {
            name: CompactString::const_new("HEAD"),
            full_ref: CompactString::const_new("HEAD"),
            kind: RefKind::HeadDetached,
            is_head: true,
            upstream_divergence: None,
        };
        by_oid.entry(oid).or_default().push(detached.clone());
        badges.push(detached);
    }

    Ok(RefsSnapshot {
        head_label,
        head_oid,
        badges,
        by_oid,
    })
}

fn compute_ahead_behind(
    repo: &gix::Repository,
    local_id: gix::ObjectId,
    remote_id: gix::ObjectId,
    cancel: &CancelToken,
) -> Option<(u32, u32)> {
    if local_id == remote_id {
        return Some((0, 0));
    }
    let merge_base = repo
        .merge_base(local_id, remote_id)
        .ok()
        .flatten()?
        .detach();
    let count_to_base = |tip: gix::ObjectId| -> Option<u32> {
        if tip == merge_base {
            return Some(0);
        }
        let walk = repo.rev_walk([tip]).with_hidden([merge_base]).all().ok()?;
        let mut count = 0u32;
        for item in walk {
            if cancel.is_cancelled() {
                return None;
            }
            if item.is_ok() {
                count = count.saturating_add(1);
            }
            if count > 50_000 {
                break;
            }
        }
        Some(count)
    };
    let ahead = count_to_base(local_id)?;
    let behind = count_to_base(remote_id)?;
    Some((ahead, behind))
}

fn open_revwalk_blocking(
    root_path: &Path,
    params: RevwalkSessionParams,
    cancel: &CancelToken,
) -> GitEngineResult<Box<dyn RevwalkSession>> {
    if cancel.is_cancelled() {
        return Err(GitEngineError::Cancelled);
    }

    let mut repo = open_gix_repo(root_path)?;
    repo.object_cache_size_if_unset(8 * 1024 * 1024);

    let (_, head_oid) = resolve_head_label_and_oid(&repo)?;

    let shallow_oids: Arc<HashSet<gix::ObjectId>> = match repo.shallow_commits() {
        Ok(Some(list)) => Arc::new(list.iter().copied().collect()),
        _ => Arc::new(HashSet::new()),
    };
    let shallow_odb = ShallowAwareOdb {
        inner: repo.objects.clone(),
        shallow_oids: Arc::clone(&shallow_oids),
    };

    let mut tips: Vec<gix::ObjectId> = Vec::new();
    if params.start_tips.is_empty() {
        if let Some(h_oid) = head_oid {
            tips.push(git_oid_to_gix(h_oid)?);
        }
        if let Ok(platform) = repo.references()
            && let Ok(iter) = platform.all()
        {
            for r_res in iter {
                let mut r = match r_res {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::debug!(
                            "skipping unreadable ref while collecting revwalk tips: {err}"
                        );
                        continue;
                    }
                };
                let Some((category, short_bstr)) = r.name().category_and_short_name() else {
                    continue;
                };
                let allowed = match category {
                    gix::refs::Category::LocalBranch | gix::refs::Category::Tag => true,
                    gix::refs::Category::RemoteBranch => {
                        !(short_bstr.ends_with(b"/HEAD") && r.target().try_id().is_none())
                    }
                    _ => false,
                };
                if !allowed {
                    continue;
                }
                if let Ok(commit) = r.peel_to_commit() {
                    tips.push(commit.id);
                }
            }
        }
    } else {
        for oid in &params.start_tips {
            let gid = git_oid_to_gix(*oid)?;
            let obj = repo
                .find_object(gid)
                .map_err(|err| GitEngineError::GixRead {
                    operation: "validate_start_tip",
                    message: err.to_string(),
                })?;
            let commit_obj = obj.peel_to_kind(gix::objs::Kind::Commit).map_err(|err| {
                GitEngineError::GixRead {
                    operation: "validate_start_tip",
                    message: err.to_string(),
                }
            })?;
            tips.push(commit_obj.id);
        }
    }

    // Deduplicate tips while preserving order.
    let mut seen_tips = HashSet::new();
    tips.retain(|id| seen_tips.insert(*id));

    let primary_trunk_oids = collect_primary_trunk_oids(&repo, &shallow_oids, cancel)?;
    let stash_oids = collect_stash_oids(&repo)?;

    if tips.is_empty() {
        return Ok(Box::new(GixRevwalkSession {
            objects: shallow_odb,
            topo: None,
            head_oid,
            primary_trunk_oids,
            stash_oids,
            path_filters: params.path_filters,
            exhausted: true,
            decode_buf: Vec::new(),
        }));
    }

    let parents_mode = if params.first_parent_only {
        gix::traverse::commit::Parents::First
    } else {
        gix::traverse::commit::Parents::All
    };

    // Commit-graph bypasses `ShallowAwareOdb`, so only attach it for non-shallow repositories.
    let commit_graph = if shallow_oids.is_empty() {
        repo.commit_graph_if_enabled().ok().flatten()
    } else {
        None
    };
    let builder = gix::traverse::commit::topo::Builder::from_iters(
        shallow_odb.clone(),
        tips,
        None::<Vec<gix::ObjectId>>,
    )
    .sorting(gix::traverse::commit::topo::Sorting::DateOrder)
    .parents(parents_mode)
    .with_commit_graph(commit_graph);

    let topo = builder.build().map_err(|err| GitEngineError::GixRead {
        operation: "build_topo_revwalk",
        message: err.to_string(),
    })?;

    Ok(Box::new(GixRevwalkSession {
        objects: shallow_odb,
        topo: Some(topo),
        head_oid,
        primary_trunk_oids,
        stash_oids,
        path_filters: params.path_filters,
        exhausted: false,
        decode_buf: Vec::with_capacity(4096),
    }))
}

fn collect_primary_trunk_oids(
    repo: &gix::Repository,
    shallow_oids: &HashSet<gix::ObjectId>,
    cancel: &CancelToken,
) -> GitEngineResult<HashSet<GitOid>> {
    let mut trunk_oids = HashSet::new();
    for candidate in TRUNK_BRANCH_CANDIDATES {
        let Ok(mut r) = repo.find_reference(*candidate) else {
            continue;
        };
        let Ok(commit) = r.peel_to_commit() else {
            continue;
        };
        if let Ok(walk) = repo.rev_walk([commit.id]).first_parent_only().all() {
            for item in walk {
                if cancel.is_cancelled() {
                    return Err(GitEngineError::Cancelled);
                }
                let Ok(info) = item else {
                    break;
                };
                trunk_oids.insert(git_oid_from_gix(info.id)?);
                if shallow_oids.contains(&info.id) {
                    break;
                }
            }
        }
        break;
    }
    Ok(trunk_oids)
}

fn collect_stash_oids(repo: &gix::Repository) -> GitEngineResult<HashSet<GitOid>> {
    let mut out = HashSet::new();
    if let Ok(mut stash_ref) = repo.find_reference("refs/stash") {
        if let Ok(id) = stash_ref.peel_to_id() {
            out.insert(git_oid_from_gix(id.detach())?);
        }
        let mut log_platform = stash_ref.log_iter();
        if let Ok(Some(log_iter)) = log_platform.all() {
            for entry in log_iter.flatten() {
                if let Ok(oid) = GitOid::from_hex(entry.new_oid.to_str_lossy().as_ref()) {
                    out.insert(oid);
                }
            }
        }
    }
    Ok(out)
}

fn commit_touches_any_path(
    objects: &ShallowAwareOdb,
    commit_tree_oid: &GitOid,
    first_parent_oid: Option<GitOid>,
    filters: &[RepoRelPath],
) -> GitEngineResult<bool> {
    let new_tree_id = git_oid_to_gix(*commit_tree_oid)?;
    let mut buf1 = Vec::new();
    let new_root_entries = {
        let new_tree =
            objects
                .find_tree(&new_tree_id, &mut buf1)
                .map_err(|err| GitEngineError::GixRead {
                    operation: "find_tree_filter",
                    message: err.to_string(),
                })?;
        collect_tree_entries(&new_tree)
    };

    let old_root_entries = match first_parent_oid {
        Some(p_oid) => {
            let pid = git_oid_to_gix(p_oid)?;
            let mut cbuf = Vec::new();
            let p_commit =
                objects
                    .find_commit(&pid, &mut cbuf)
                    .map_err(|err| GitEngineError::GixRead {
                        operation: "find_parent_commit_filter",
                        message: err.to_string(),
                    })?;
            let old_tid = p_commit.tree();
            let mut buf2 = Vec::new();
            objects
                .find_tree(&old_tid, &mut buf2)
                .ok()
                .map(|t| collect_tree_entries(&t))
        }
        None => None,
    };

    let mut scratch = Vec::new();
    for path in filters {
        let new_entry =
            lookup_tree_entry_id(objects, &new_root_entries, path.as_str(), &mut scratch);
        let old_entry = old_root_entries.as_ref().and_then(|old_entries| {
            lookup_tree_entry_id(objects, old_entries, path.as_str(), &mut scratch)
        });
        if new_entry != old_entry {
            return Ok(true);
        }
    }
    Ok(false)
}

type TreeEntryList = Vec<(String, gix::objs::tree::EntryMode, gix::ObjectId)>;

fn collect_tree_entries(tree: &gix::objs::TreeRef<'_>) -> TreeEntryList {
    tree.entries
        .iter()
        .map(|e| {
            (
                e.filename.to_str_lossy().into_owned(),
                e.mode,
                e.oid.to_owned(),
            )
        })
        .collect()
}

fn lookup_tree_entry_id(
    objects: &ShallowAwareOdb,
    initial_entries: &TreeEntryList,
    rel_path: &str,
    scratch: &mut Vec<u8>,
) -> Option<(gix::objs::tree::EntryMode, gix::ObjectId)> {
    let mut segments = rel_path.split('/').peekable();
    let mut owned_entries: Option<TreeEntryList> = None;

    while let Some(seg) = segments.next() {
        let entries_ref = owned_entries.as_ref().unwrap_or(initial_entries);
        let matched = entries_ref
            .iter()
            .find(|(name, _, _)| name == seg)
            .map(|(_, mode, oid)| (*mode, *oid))?;
        if segments.peek().is_none() {
            return Some(matched);
        }
        if !matched.0.is_tree() {
            return None;
        }
        let subtree = objects.find_tree(&matched.1, scratch).ok()?;
        owned_entries = Some(collect_tree_entries(&subtree));
    }
    None
}

fn to_compact_signature(sig: gix::actor::SignatureRef<'_>) -> CompactSignature {
    let tz_offset_mins = sig.time().map(|t| (t.offset / 60) as i16).unwrap_or(0);
    CompactSignature {
        name: CompactString::from(sig.name.to_str_lossy().as_ref()),
        email: CompactString::from(sig.email.to_str_lossy().as_ref()),
        timestamp_secs: sig.seconds(),
        tz_offset_mins,
    }
}

fn split_subject_and_body_flag(raw: &str) -> (CompactString, bool) {
    let trimmed = raw.trim_end_matches(&['\n', '\r'][..]);
    match trimmed.split_once('\n') {
        Some((first, rest)) => {
            let subject = first.trim_end_matches('\r');
            let has_body = !rest.trim().is_empty();
            (CompactString::from(subject), has_body)
        }
        None => (CompactString::from(trimmed), false),
    }
}

fn extract_signature_state(commit: &gix::objs::CommitRef<'_>) -> CommitSignatureState {
    for (key, _) in &commit.extra_headers {
        if key.as_bytes() == b"gpgsig" || key.as_bytes() == b"gpgsig-sha256" {
            // Do not assert cryptographic validity until offline GPG/SSH verification is wired.
            return CommitSignatureState::Unverified;
        }
    }
    CommitSignatureState::Unsigned
}

fn load_commit_details_blocking(
    root_path: &Path,
    oid: GitOid,
    cancel: &CancelToken,
) -> GitEngineResult<CommitDetails> {
    if cancel.is_cancelled() {
        return Err(GitEngineError::Cancelled);
    }

    let mut repo = open_gix_repo(root_path)?;
    repo.object_cache_size_if_unset(4 * 1024 * 1024);

    let commit_id = git_oid_to_gix(oid)?;
    let commit = repo
        .find_commit(commit_id)
        .map_err(|err| GitEngineError::GixRead {
            operation: "find_commit_details",
            message: err.to_string(),
        })?;

    let full_message = commit.message_raw_sloppy().to_str_lossy().into_owned();
    let trailers = parse_trailers(&full_message);

    let commit_tree = commit.tree().map_err(|err| GitEngineError::GixRead {
        operation: "commit_tree",
        message: err.to_string(),
    })?;

    let is_shallow_boundary = repo
        .shallow_commits()
        .ok()
        .flatten()
        .is_some_and(|list| list.iter().any(|&sid| sid == commit_id));

    let parent_tree =
        if is_shallow_boundary {
            // Shallow graft commits have missing parent objects; diff against the empty tree like `git show`.
            None
        } else {
            match commit.parent_ids().next() {
                Some(parent_id) => {
                    let parent_commit =
                        parent_id.object().map_err(|err| GitEngineError::GixRead {
                            operation: "parent_commit",
                            message: err.to_string(),
                        })?;
                    Some(parent_commit.into_commit().tree().map_err(|err| {
                        GitEngineError::GixRead {
                            operation: "parent_tree",
                            message: err.to_string(),
                        }
                    })?)
                }
                None => None,
            }
        };

    let diff_opts = gix::diff::Options::default().with_rewrites(Some(Default::default()));
    let changes = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(diff_opts))
        .map_err(|err| GitEngineError::GixRead {
            operation: "diff_tree_to_tree",
            message: err.to_string(),
        })?;

    let mut changed_files = Vec::with_capacity(changes.len());
    for change in changes {
        if cancel.is_cancelled() {
            return Err(GitEngineError::Cancelled);
        }
        if let Some(delta) = convert_tree_change_to_delta(&repo, change)? {
            changed_files.push(delta);
        }
    }

    Ok(CommitDetails {
        oid,
        full_message,
        trailers,
        signature_diagnostic: None,
        changed_files,
    })
}

fn parse_trailers(message: &str) -> Vec<(CompactString, CompactString)> {
    let mut trailers = Vec::new();
    let trimmed = message.trim_end();
    let Some(last_para) = trimmed.rsplit("\n\n").next() else {
        return trailers;
    };
    for line in last_para.lines() {
        if let Some((key, val)) = line.split_once(": ") {
            let k = key.trim();
            if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                trailers.push((CompactString::from(k), CompactString::from(val.trim())));
            }
        }
    }
    trailers
}

fn convert_tree_change_to_delta(
    repo: &gix::Repository,
    change: ChangeDetached,
) -> GitEngineResult<Option<FileDelta>> {
    match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return Ok(None);
            }
            let Some(new_path) = try_normalize_path_bytes(&location, "commit_addition") else {
                return Ok(None);
            };
            let new_mode = map_tree_entry_mode(entry_mode);
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id), true);
            let additions = if !is_binary
                && matches!(new_mode, FileMode::Regular644 | FileMode::Executable755)
            {
                read_small_text_blob(repo, id).map_or(0, |b| count_text_lines(&b))
            } else {
                0
            };
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: DeltaStatus::Added,
                area: DeltaArea::CommitDiff,
                old_mode: FileMode::Absent,
                new_mode,
                old_oid: None,
                new_oid: Some(git_oid_from_gix(id)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions,
                deletions: 0,
            }))
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return Ok(None);
            }
            let Some(new_path) = try_normalize_path_bytes(&location, "commit_deletion") else {
                return Ok(None);
            };
            let old_mode = map_tree_entry_mode(entry_mode);
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id), false);
            let deletions = if !is_binary
                && matches!(old_mode, FileMode::Regular644 | FileMode::Executable755)
            {
                read_small_text_blob(repo, id).map_or(0, |b| count_text_lines(&b))
            } else {
                0
            };
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: DeltaStatus::Deleted,
                area: DeltaArea::CommitDiff,
                old_mode,
                new_mode: FileMode::Absent,
                old_oid: Some(git_oid_from_gix(id)?),
                new_oid: None,
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions,
            }))
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            if entry_mode.is_tree() || previous_entry_mode.is_tree() {
                return Ok(None);
            }
            let Some(new_path) = try_normalize_path_bytes(&location, "commit_modification") else {
                return Ok(None);
            };
            let old_mode = map_tree_entry_mode(previous_entry_mode);
            let new_mode = map_tree_entry_mode(entry_mode);
            let status = if is_type_change(old_mode, new_mode) {
                DeltaStatus::TypeChanged
            } else {
                DeltaStatus::Modified
            };
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id), true);
            let (additions, deletions) = if status == DeltaStatus::Modified
                && previous_id != id
                && !is_binary
                && matches!(old_mode, FileMode::Regular644 | FileMode::Executable755)
                && matches!(new_mode, FileMode::Regular644 | FileMode::Executable755)
            {
                compute_blob_line_stats(repo, previous_id, id)
            } else {
                (0, 0)
            };
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status,
                area: DeltaArea::CommitDiff,
                old_mode,
                new_mode,
                old_oid: Some(git_oid_from_gix(previous_id)?),
                new_oid: Some(git_oid_from_gix(id)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions,
                deletions,
            }))
        }
        ChangeDetached::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            diff,
            entry_mode,
            id,
            location,
            copy,
            ..
        } => {
            let Some(old_path) = try_normalize_path_bytes(&source_location, "commit_rewrite_src")
            else {
                return Ok(None);
            };
            let Some(new_path) = try_normalize_path_bytes(&location, "commit_rewrite_dst") else {
                return Ok(None);
            };
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id), true);
            let (similarity_pct, additions, deletions) = match diff {
                Some(stats) => (
                    (stats.similarity * 100.0).round().clamp(0.0, 100.0) as u8,
                    stats.insertions,
                    stats.removals,
                ),
                None => (100, 0, 0),
            };
            let status = if copy {
                DeltaStatus::Copied { similarity_pct }
            } else {
                DeltaStatus::Renamed { similarity_pct }
            };
            Ok(Some(FileDelta {
                old_path: Some(old_path),
                new_path,
                status,
                area: DeltaArea::CommitDiff,
                old_mode: map_tree_entry_mode(source_entry_mode),
                new_mode: map_tree_entry_mode(entry_mode),
                old_oid: Some(git_oid_from_gix(source_id)?),
                new_oid: Some(git_oid_from_gix(id)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions,
                deletions,
            }))
        }
    }
}

fn try_normalize_path_bytes(raw: impl AsRef<[u8]>, context: &'static str) -> Option<RepoRelPath> {
    let utf8_lossy = BStr::new(raw.as_ref()).to_str_lossy();
    match RepoRelPath::normalize(utf8_lossy.as_ref()) {
        Ok(rel) => Some(rel),
        Err(err) => {
            tracing::warn!("skipping unnormalizable path in {context}: {err}");
            None
        }
    }
}

#[inline]
fn is_type_change(old_mode: FileMode, new_mode: FileMode) -> bool {
    let is_regular = |m: FileMode| matches!(m, FileMode::Regular644 | FileMode::Executable755);
    is_regular(old_mode) != is_regular(new_mode) || (!is_regular(old_mode) && old_mode != new_mode)
}

fn count_text_lines(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let total = if bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    };
    u32::try_from(total).unwrap_or(u32::MAX)
}

fn read_small_text_blob(repo: &gix::Repository, oid: gix::ObjectId) -> Option<Vec<u8>> {
    let header = repo.find_header(oid).ok()?;
    if header.kind() != gix::objs::Kind::Blob || header.size() > MAX_LINE_STATS_BLOB_BYTES {
        return None;
    }
    let mut blob = repo.find_blob(oid).ok()?;
    if blob.data[..blob.data.len().min(8192)].contains(&0) {
        return None;
    }
    Some(std::mem::take(&mut blob.data))
}

fn compute_blob_line_stats(
    repo: &gix::Repository,
    old_id: gix::ObjectId,
    new_id: gix::ObjectId,
) -> (u32, u32) {
    let (Some(old_bytes), Some(new_bytes)) = (
        read_small_text_blob(repo, old_id),
        read_small_text_blob(repo, new_id),
    ) else {
        return (0, 0);
    };
    let input = gix::diff::blob::InternedInput::new(old_bytes.as_slice(), new_bytes.as_slice());
    let diff = gix::diff::blob::Diff::compute(gix::diff::blob::Algorithm::Histogram, &input);
    (diff.count_additions(), diff.count_removals())
}

fn working_tree_status_blocking(
    root_path: &Path,
    cancel: &CancelToken,
) -> GitEngineResult<WorkingTreeStatusView> {
    if cancel.is_cancelled() {
        return Err(GitEngineError::Cancelled);
    }

    let repo = open_gix_repo(root_path)?;
    let index = repo
        .index_or_empty()
        .map_err(|err| GitEngineError::GixRead {
            operation: "index_or_empty",
            message: err.to_string(),
        })?;

    // Collect unmerged stage (1, 2, 3) entries directly from the index.
    let mut conflict_stages: BTreeMap<String, ([bool; 3], Option<gix::ObjectId>)> = BTreeMap::new();
    for entry in index.entries() {
        let stage = entry.stage();
        if stage != gix::index::entry::Stage::Unconflicted {
            let rel = entry.path(&index).to_str_lossy().into_owned();
            let slot = conflict_stages.entry(rel).or_insert(([false; 3], None));
            match stage {
                gix::index::entry::Stage::Base => slot.0[0] = true,
                gix::index::entry::Stage::Ours => {
                    slot.0[1] = true;
                    slot.1 = Some(entry.id);
                }
                gix::index::entry::Stage::Theirs => {
                    slot.0[2] = true;
                    if slot.1.is_none() {
                        slot.1 = Some(entry.id);
                    }
                }
                gix::index::entry::Stage::Unconflicted => {}
            }
        }
    }

    let mut conflicted = Vec::with_capacity(conflict_stages.len());
    let mut conflicted_paths: HashSet<String> = HashSet::with_capacity(conflict_stages.len());
    for (path_str, (stages, sample_oid)) in conflict_stages {
        let Some(rel) = try_normalize_path_bytes(path_str.as_bytes(), "working_tree_conflicted")
        else {
            continue;
        };
        conflicted_paths.insert(rel.as_str().to_string());
        let subkind = match stages {
            [true, true, true] => ConflictSubkind::BothModified,
            [false, true, true] => ConflictSubkind::BothAdded,
            [true, false, true] => ConflictSubkind::DeletedByUs,
            [true, true, false] => ConflictSubkind::DeletedByThem,
            [false, true, false] => ConflictSubkind::AddedByUs,
            [false, false, true] => ConflictSubkind::AddedByThem,
            _ => ConflictSubkind::BothDeleted,
        };
        let oid = sample_oid.map(git_oid_from_gix).transpose()?;
        conflicted.push(FileDelta {
            old_path: None,
            new_path: rel,
            status: DeltaStatus::Conflicted(subkind),
            area: DeltaArea::Conflicted,
            old_mode: FileMode::Regular644,
            new_mode: FileMode::Regular644,
            old_oid: oid,
            new_oid: oid,
            is_binary: false,
            lfs: None,
            eol_change: EolChange::Unchanged,
            additions: 0,
            deletions: 0,
        });
    }

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();

    let status_platform = repo
        .status(gix::progress::Discard)
        .map_err(|err| GitEngineError::GixRead {
            operation: "status_platform",
            message: err.to_string(),
        })?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .should_interrupt_owned(cancel.clone_arc());

    let iter = status_platform
        .into_iter(Vec::<BString>::new())
        .map_err(|err| GitEngineError::GixRead {
            operation: "status_iter",
            message: err.to_string(),
        })?;

    for item_res in iter {
        if cancel.is_cancelled() {
            return Err(GitEngineError::Cancelled);
        }
        let item = item_res.map_err(|err| GitEngineError::GixRead {
            operation: "status_item",
            message: err.to_string(),
        })?;

        match item {
            gix::status::Item::TreeIndex(change) => {
                if let Some(delta) = convert_tree_index_change(&repo, change)?
                    && !conflicted_paths.contains(delta.new_path.as_str())
                {
                    staged.push(delta);
                }
            }
            gix::status::Item::IndexWorktree(iw_item) => {
                if let Some(delta) = convert_index_worktree_item(&repo, iw_item)?
                    && !conflicted_paths.contains(delta.new_path.as_str())
                {
                    unstaged.push(delta);
                }
            }
        }
    }

    staged.sort_by(|a, b| a.new_path.cmp(&b.new_path));
    unstaged.sort_by(|a, b| a.new_path.cmp(&b.new_path));
    conflicted.sort_by(|a, b| a.new_path.cmp(&b.new_path));

    Ok(WorkingTreeStatusView {
        unstaged: unstaged.into(),
        staged: staged.into(),
        conflicted: conflicted.into(),
    })
}

fn convert_tree_index_change(
    repo: &gix::Repository,
    change: gix::diff::index::Change,
) -> GitEngineResult<Option<FileDelta>> {
    use gix::diff::index::ChangeRef;
    match change {
        ChangeRef::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            let Some(new_path) = try_normalize_path_bytes(location.as_ref(), "tree_index_addition")
            else {
                return Ok(None);
            };
            let id_owned = id.into_owned();
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id_owned), true);
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: DeltaStatus::Added,
                area: DeltaArea::Staged,
                old_mode: FileMode::Absent,
                new_mode: map_index_mode(entry_mode),
                old_oid: None,
                new_oid: Some(git_oid_from_gix(id_owned)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
        ChangeRef::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => {
            let Some(new_path) = try_normalize_path_bytes(location.as_ref(), "tree_index_deletion")
            else {
                return Ok(None);
            };
            let id_owned = id.into_owned();
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id_owned), false);
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: DeltaStatus::Deleted,
                area: DeltaArea::Staged,
                old_mode: map_index_mode(entry_mode),
                new_mode: FileMode::Absent,
                old_oid: Some(git_oid_from_gix(id_owned)?),
                new_oid: None,
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
            ..
        } => {
            let Some(new_path) =
                try_normalize_path_bytes(location.as_ref(), "tree_index_modification")
            else {
                return Ok(None);
            };
            let id_owned = id.into_owned();
            let prev_owned = previous_id.into_owned();
            let old_mode = map_index_mode(previous_entry_mode);
            let new_mode = map_index_mode(entry_mode);
            let status = if is_type_change(old_mode, new_mode) {
                DeltaStatus::TypeChanged
            } else {
                DeltaStatus::Modified
            };
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id_owned), true);
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status,
                area: DeltaArea::Staged,
                old_mode,
                new_mode,
                old_oid: Some(git_oid_from_gix(prev_owned)?),
                new_oid: Some(git_oid_from_gix(id_owned)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
        ChangeRef::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            location,
            entry_mode,
            id,
            copy,
            ..
        } => {
            let Some(old_path) =
                try_normalize_path_bytes(source_location.as_ref(), "tree_index_rewrite_src")
            else {
                return Ok(None);
            };
            let Some(new_path) =
                try_normalize_path_bytes(location.as_ref(), "tree_index_rewrite_dst")
            else {
                return Ok(None);
            };
            let id_owned = id.into_owned();
            let prev_owned = source_id.into_owned();
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(id_owned), true);
            Ok(Some(FileDelta {
                old_path: Some(old_path),
                new_path,
                status: if copy {
                    DeltaStatus::Copied { similarity_pct: 90 }
                } else {
                    DeltaStatus::Renamed { similarity_pct: 90 }
                },
                area: DeltaArea::Staged,
                old_mode: map_index_mode(source_entry_mode),
                new_mode: map_index_mode(entry_mode),
                old_oid: Some(git_oid_from_gix(prev_owned)?),
                new_oid: Some(git_oid_from_gix(id_owned)?),
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
    }
}

fn convert_index_worktree_item(
    repo: &gix::Repository,
    item: gix::status::index_worktree::Item,
) -> GitEngineResult<Option<FileDelta>> {
    use gix::status::index_worktree::Item;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};

    match item {
        Item::Modification {
            entry,
            rela_path,
            status,
            ..
        } => {
            let Some(new_path) =
                try_normalize_path_bytes(rela_path.as_slice(), "worktree_modification")
            else {
                return Ok(None);
            };
            let old_oid = git_oid_from_gix(entry.id)?;
            let old_mode = map_index_mode(entry.mode);
            let (delta_status, new_mode, area) = match status {
                EntryStatus::Conflict { .. } => (
                    DeltaStatus::Conflicted(ConflictSubkind::BothModified),
                    old_mode,
                    DeltaArea::Conflicted,
                ),
                EntryStatus::Change(Change::Removed) => {
                    (DeltaStatus::Deleted, FileMode::Absent, DeltaArea::Unstaged)
                }
                EntryStatus::Change(Change::Type { worktree_mode }) => (
                    DeltaStatus::TypeChanged,
                    map_index_mode(worktree_mode),
                    DeltaArea::Unstaged,
                ),
                EntryStatus::Change(Change::Modification {
                    executable_bit_changed,
                    ..
                }) => {
                    let computed_new_mode = if executable_bit_changed {
                        match old_mode {
                            FileMode::Regular644 => FileMode::Executable755,
                            FileMode::Executable755 => FileMode::Regular644,
                            other => other,
                        }
                    } else {
                        old_mode
                    };
                    (
                        DeltaStatus::Modified,
                        computed_new_mode,
                        DeltaArea::Unstaged,
                    )
                }
                EntryStatus::Change(Change::SubmoduleModification(_)) => {
                    (DeltaStatus::Modified, old_mode, DeltaArea::Unstaged)
                }
                EntryStatus::IntentToAdd => (DeltaStatus::Added, old_mode, DeltaArea::Unstaged),
                EntryStatus::NeedsUpdate(_) => return Ok(None),
            };
            let (is_binary, lfs) = inspect_blob_meta(repo, Some(entry.id), false);
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: delta_status,
                area,
                old_mode,
                new_mode,
                old_oid: Some(old_oid),
                new_oid: None,
                is_binary,
                lfs,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
        Item::DirectoryContents { entry, .. } => {
            if !matches!(entry.status, gix::dir::entry::Status::Untracked) {
                return Ok(None);
            }
            let Some(new_path) =
                try_normalize_path_bytes(entry.rela_path.as_slice(), "worktree_untracked")
            else {
                return Ok(None);
            };
            Ok(Some(FileDelta {
                old_path: None,
                new_path,
                status: DeltaStatus::Untracked,
                area: DeltaArea::Unstaged,
                old_mode: FileMode::Absent,
                new_mode: FileMode::Regular644,
                old_oid: None,
                new_oid: None,
                is_binary: false,
                lfs: None,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
        Item::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => {
            let Some(old_path) =
                try_normalize_path_bytes(source.rela_path(), "worktree_rewrite_src")
            else {
                return Ok(None);
            };
            let Some(new_path) = try_normalize_path_bytes(
                dirwalk_entry.rela_path.as_slice(),
                "worktree_rewrite_dst",
            ) else {
                return Ok(None);
            };
            Ok(Some(FileDelta {
                old_path: Some(old_path),
                new_path,
                status: if copy {
                    DeltaStatus::Copied { similarity_pct: 90 }
                } else {
                    DeltaStatus::Renamed { similarity_pct: 90 }
                },
                area: DeltaArea::Unstaged,
                old_mode: FileMode::Regular644,
                new_mode: FileMode::Regular644,
                old_oid: None,
                new_oid: None,
                is_binary: false,
                lfs: None,
                eol_change: EolChange::Unchanged,
                additions: 0,
                deletions: 0,
            }))
        }
    }
}

fn inspect_blob_meta(
    repo: &gix::Repository,
    oid: Option<gix::ObjectId>,
    materialized: bool,
) -> (bool, Option<LfsPointerMeta>) {
    let Some(id) = oid else {
        return (false, None);
    };
    let Ok(header) = repo.find_header(id) else {
        return (false, None);
    };
    if header.kind() != gix::objs::Kind::Blob {
        return (false, None);
    }
    if header.size() > MAX_META_INSPECT_BLOB_BYTES {
        return (true, None);
    }
    let Ok(blob) = repo.find_blob(id) else {
        return (false, None);
    };
    let sample = &blob.data[..blob.data.len().min(4096)];
    if let Some(lfs) = parse_lfs_pointer(sample, materialized) {
        return (true, Some(lfs));
    }
    let is_binary = sample.contains(&0);
    (is_binary, None)
}

fn parse_lfs_pointer(bytes: &[u8], materialized: bool) -> Option<LfsPointerMeta> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1") {
        return None;
    }
    let mut sha = None;
    let mut size = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("oid sha256:") {
            sha = Some(CompactString::from(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("size ") {
            size = rest.trim().parse::<u64>().ok();
        }
    }
    Some(LfsPointerMeta {
        oid_sha256: sha?,
        byte_size: size?,
        materialized_in_worktree: materialized,
    })
}

fn map_tree_entry_mode(mode: gix::object::tree::EntryMode) -> FileMode {
    use gix::object::tree::EntryKind;
    match mode.kind() {
        EntryKind::Tree => FileMode::Tree,
        EntryKind::Blob => FileMode::Regular644,
        EntryKind::BlobExecutable => FileMode::Executable755,
        EntryKind::Link => FileMode::Symlink,
        EntryKind::Commit => FileMode::GitlinkSubmodule,
    }
}

fn map_index_mode(mode: gix::index::entry::Mode) -> FileMode {
    use gix::index::entry::Mode;
    if mode.contains(Mode::DIR) {
        FileMode::Tree
    } else if mode.contains(Mode::SYMLINK) {
        FileMode::Symlink
    } else if mode.contains(Mode::COMMIT) {
        FileMode::GitlinkSubmodule
    } else if mode.contains(Mode::FILE_EXECUTABLE) {
        FileMode::Executable755
    } else if mode.contains(Mode::FILE) {
        FileMode::Regular644
    } else {
        FileMode::Absent
    }
}

fn blame_file_blocking(
    root_path: &Path,
    path: &RepoRelPath,
    at_commit: Option<GitOid>,
    cancel: &CancelToken,
) -> GitEngineResult<BlameResult> {
    if cancel.is_cancelled() {
        return Err(GitEngineError::Cancelled);
    }

    let mut repo = open_gix_repo(root_path)?;
    repo.object_cache_size_if_unset(4 * 1024 * 1024);

    let suspect_id = match at_commit {
        Some(oid) => git_oid_to_gix(oid)?,
        None => {
            let mut head = repo.head().map_err(|err| GitEngineError::GixRead {
                operation: "blame_head",
                message: err.to_string(),
            })?;
            let Some(id) = head
                .try_peel_to_id()
                .map_err(|err| GitEngineError::GixRead {
                    operation: "blame_peel_head",
                    message: err.to_string(),
                })?
            else {
                return Ok(BlameResult {
                    path: path.clone(),
                    base_oid: None,
                    lines: Vec::new(),
                });
            };
            id.detach()
        }
    };

    let outcome = repo
        .blame_file(BStr::new(path.as_str()), suspect_id, Default::default())
        .map_err(|err| GitEngineError::GixRead {
            operation: "blame_file",
            message: err.to_string(),
        })?;

    let mut author_cache: std::collections::HashMap<gix::ObjectId, (CompactString, i64)> =
        std::collections::HashMap::new();
    let mut lines = Vec::new();

    for entry in outcome.entries {
        if cancel.is_cancelled() {
            return Err(GitEngineError::Cancelled);
        }
        let (author_name, author_time_secs) =
            if let Some(cached) = author_cache.get(&entry.commit_id) {
                cached.clone()
            } else {
                let meta = if let Ok(commit) = repo.find_commit(entry.commit_id) {
                    commit
                        .author()
                        .map(|sig| {
                            (
                                CompactString::from(sig.name.to_str_lossy().as_ref()),
                                sig.seconds(),
                            )
                        })
                        .unwrap_or_else(|_| (CompactString::const_new("Unknown"), 0))
                } else {
                    (CompactString::const_new("Unknown"), 0)
                };
                author_cache.insert(entry.commit_id, meta.clone());
                meta
            };

        let commit_oid = git_oid_from_gix(entry.commit_id)?;
        let orig_path = entry
            .source_file_name
            .as_ref()
            .and_then(|src| try_normalize_path_bytes(src.as_slice(), "blame_source_path"))
            .unwrap_or_else(|| path.clone());

        let count = entry.len.get();
        for offset in 0..count {
            lines.push(BlameLine {
                line_number: entry.start_in_blamed_file + offset + 1,
                orig_line_number: entry.start_in_source_file + offset + 1,
                commit_oid,
                author_name: author_name.clone(),
                author_time_secs,
                orig_path: orig_path.clone(),
            });
        }
    }

    Ok(BlameResult {
        path: path.clone(),
        base_oid: Some(git_oid_from_gix(suspect_id)?),
        lines,
    })
}
