//! UI-agnostic sidebar tree builders: `/` nested branch folders, remote groups,
//! `.gitmodules` submodule parser, worktree summaries, and Flat/Tree file delta flattening.

use crate::ports::FileListLayoutMode;
use branvio_core::domain::{FileDelta, RefBadge, RefKind};
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// A node in the nested hierarchical branch tree (split on `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchTreeNode {
    /// Expandable folder node grouping branches with a common slash prefix (e.g. `feature/`).
    Folder {
        /// Short folder display segment (e.g. `feature`).
        name: CompactString,
        /// Unique folder prefix key preserved across refreshes for collapse state (`e.g. local:feature/auth`).
        key: CompactString,
        /// Nesting depth (`0` for root folders).
        depth: u16,
        /// True when collapsed by the user.
        collapsed: bool,
        /// Ordered child folders and branch leaves.
        children: Vec<BranchTreeNode>,
    },
    /// Concrete branch leaf backed by a [`RefBadge`].
    Branch {
        /// Leaf display name without parent folder prefix (e.g. `oauth`).
        leaf_name: CompactString,
        /// Nesting depth inside the folder tree.
        depth: u16,
        /// Underlying reference badge.
        badge: RefBadge,
    },
}

/// Flattened renderable row emitted from [`BranchTreeNode`] for virtualized/scrolled lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatSidebarBranchRow {
    /// Expandable folder header row.
    Folder {
        /// Folder segment display name.
        name: CompactString,
        /// Collapse key (`collapsed_folders` entry).
        key: CompactString,
        /// Indentation level.
        depth: u16,
        /// True if currently collapsed.
        collapsed: bool,
    },
    /// Concrete branch leaf row.
    Branch {
        /// Short leaf display name.
        leaf_name: CompactString,
        /// Indentation level.
        depth: u16,
        /// Backing ref badge.
        badge: RefBadge,
    },
}

/// Remote repository group containing its own `/` nested tracking branch tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGroupNode {
    /// Remote name (e.g. `origin`, `upstream`).
    pub remote_name: CompactString,
    /// Nested branch tree inside this remote.
    pub tree: Vec<BranchTreeNode>,
}

/// Parsed entry from `.gitmodules`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleSummary {
    /// Logical submodule section name (`[submodule "<name>"]`).
    pub name: CompactString,
    /// Repository-relative checkout path.
    pub path: RepoRelPath,
    /// Configured remote URL (`""` if omitted).
    pub url: String,
    /// Optional tracked branch configured in `.gitmodules`.
    pub branch: Option<CompactString>,
}

/// Partitioned sidebar sections built from [`RefBadge`] items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidebarRefSections {
    /// Hierarchical local branch tree (`refs/heads/...`).
    pub local_branches: Vec<BranchTreeNode>,
    /// Remote groups (`origin`, `upstream`, ...), each with a nested branch tree.
    pub remotes: Vec<RemoteGroupNode>,
    /// Chronologically/lexicographically ordered tag refs.
    pub tags: Vec<RefBadge>,
    /// Git stash entries (`stash@{0}`, ...).
    pub stashes: Vec<RefBadge>,
}

/// Flattened row for changed files in Working Tree or Commit Detail inspectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTreeRow {
    /// Expandable directory header row (only emitted in [`FileListLayoutMode::FolderTree`]).
    Directory {
        /// Folder segment display label.
        name: CompactString,
        /// Normalized folder path key (`"src/ui"`).
        dir_path: CompactString,
        /// Indentation depth (`0` for top-level directory).
        depth: u16,
        /// True when collapsed.
        collapsed: bool,
        /// Number of files contained recursively under this directory.
        file_count: usize,
    },
    /// Concrete changed file row.
    File {
        /// Indentation depth (`0` in Flat mode or root files in Tree mode).
        depth: u16,
        /// Display name (`"foo.rs"` in Tree mode, `"src/foo.rs"` in Flat mode).
        display_name: CompactString,
        /// Backing file delta.
        delta: FileDelta,
    },
}

/// Builds hierarchical Local Branches, grouped Remotes, Tags, and Stashes from ref badges.
pub fn build_sidebar_ref_sections(
    badges: &[RefBadge],
    collapsed_folders: &HashSet<CompactString>,
) -> SidebarRefSections {
    let mut local_entries: Vec<(&str, &RefBadge)> = Vec::new();
    let mut remote_buckets: BTreeMap<CompactString, Vec<(&str, &RefBadge)>> = BTreeMap::new();
    let mut tags = Vec::new();
    let mut stashes = Vec::new();

    for badge in badges {
        match &badge.kind {
            RefKind::LocalBranch => {
                local_entries.push((badge.name.as_str(), badge));
            }
            RefKind::RemoteBranch { remote } => {
                let raw = badge.name.as_str();
                let prefix = format!("{remote}/");
                let branch_part = raw.strip_prefix(&prefix).unwrap_or(raw);
                remote_buckets
                    .entry(remote.clone())
                    .or_default()
                    .push((branch_part, badge));
            }
            RefKind::Tag { .. } => tags.push(badge.clone()),
            RefKind::Stash => stashes.push(badge.clone()),
            RefKind::HeadDetached | RefKind::PullRequestRef { .. } => {}
        }
    }

    let local_branches = build_branch_tree_from_entries("local", &local_entries, collapsed_folders);
    let remotes = remote_buckets
        .into_iter()
        .map(|(remote_name, entries)| {
            let scope = format!("remote:{remote_name}");
            let tree = build_branch_tree_from_entries(&scope, &entries, collapsed_folders);
            RemoteGroupNode { remote_name, tree }
        })
        .collect();

    tags.sort_by(|a, b| a.name.cmp(&b.name));

    SidebarRefSections {
        local_branches,
        remotes,
        tags,
        stashes,
    }
}

/// Flattens a slice of [`BranchTreeNode`] respecting node collapse state.
pub fn flatten_branch_tree(nodes: &[BranchTreeNode]) -> Vec<FlatSidebarBranchRow> {
    let mut out = Vec::new();
    flatten_branch_nodes_into(nodes, &mut out);
    out
}

fn flatten_branch_nodes_into(nodes: &[BranchTreeNode], out: &mut Vec<FlatSidebarBranchRow>) {
    for node in nodes {
        match node {
            BranchTreeNode::Folder {
                name,
                key,
                depth,
                collapsed,
                children,
            } => {
                out.push(FlatSidebarBranchRow::Folder {
                    name: name.clone(),
                    key: key.clone(),
                    depth: *depth,
                    collapsed: *collapsed,
                });
                if !collapsed {
                    flatten_branch_nodes_into(children, out);
                }
            }
            BranchTreeNode::Branch {
                leaf_name,
                depth,
                badge,
            } => {
                out.push(FlatSidebarBranchRow::Branch {
                    leaf_name: leaf_name.clone(),
                    depth: *depth,
                    badge: badge.clone(),
                });
            }
        }
    }
}

#[derive(Default)]
struct MutableBranchDir<'a> {
    dirs: BTreeMap<CompactString, MutableBranchDir<'a>>,
    leaves: BTreeMap<CompactString, &'a RefBadge>,
}

fn build_branch_tree_from_entries(
    scope: &str,
    entries: &[(&str, &RefBadge)],
    collapsed_folders: &HashSet<CompactString>,
) -> Vec<BranchTreeNode> {
    let mut root = MutableBranchDir::default();
    for &(name, badge) in entries {
        let parts: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            root.leaves
                .insert(CompactString::from(badge.name.as_str()), badge);
            continue;
        }
        let mut cursor = &mut root;
        for dir_part in &parts[..parts.len() - 1] {
            cursor = cursor
                .dirs
                .entry(CompactString::from(*dir_part))
                .or_default();
        }
        if let Some(leaf_part) = parts.last() {
            cursor.leaves.insert(CompactString::from(*leaf_part), badge);
        }
    }
    freeze_branch_dir(scope, "", 0, &root, collapsed_folders)
}

fn freeze_branch_dir(
    scope: &str,
    prefix: &str,
    depth: u16,
    dir: &MutableBranchDir<'_>,
    collapsed_folders: &HashSet<CompactString>,
) -> Vec<BranchTreeNode> {
    let mut nodes = Vec::with_capacity(dir.dirs.len() + dir.leaves.len());
    for (name, child) in &dir.dirs {
        let next_prefix = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let key = CompactString::from(format!("{scope}:{next_prefix}"));
        let collapsed = collapsed_folders.contains(&key);
        let children = freeze_branch_dir(scope, &next_prefix, depth + 1, child, collapsed_folders);
        nodes.push(BranchTreeNode::Folder {
            name: name.clone(),
            key,
            depth,
            collapsed,
            children,
        });
    }
    for (leaf_name, badge) in &dir.leaves {
        nodes.push(BranchTreeNode::Branch {
            leaf_name: leaf_name.clone(),
            depth,
            badge: (*badge).clone(),
        });
    }
    nodes
}

/// Parses `.gitmodules` INI-like content into structured [`SubmoduleSummary`] entries.
pub fn parse_gitmodules_content(content: &str) -> Vec<SubmoduleSummary> {
    let mut result = Vec::new();
    let mut current_name: Option<CompactString> = None;
    let mut current_path: Option<String> = None;
    let mut current_url: Option<String> = None;
    let mut current_branch: Option<CompactString> = None;

    let flush_current = |result: &mut Vec<SubmoduleSummary>,
                         name: &mut Option<CompactString>,
                         path: &mut Option<String>,
                         url: &mut Option<String>,
                         branch: &mut Option<CompactString>| {
        if let (Some(sub_name), Some(raw_path)) = (name.take(), path.take())
            && let Ok(rel_path) = RepoRelPath::normalize(&raw_path)
        {
            result.push(SubmoduleSummary {
                name: sub_name,
                path: rel_path,
                url: url.take().unwrap_or_default(),
                branch: branch.take(),
            });
        }
        *url = None;
        *branch = None;
    };

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush_current(
                &mut result,
                &mut current_name,
                &mut current_path,
                &mut current_url,
                &mut current_branch,
            );
            let inner = line[1..line.len() - 1].trim();
            if let Some(rest) = inner.strip_prefix("submodule") {
                let cleaned = rest.trim().trim_matches('"').trim();
                if !cleaned.is_empty() {
                    current_name = Some(CompactString::from(cleaned));
                }
            }
            continue;
        }
        if current_name.is_some()
            && let Some((raw_k, raw_v)) = line.split_once('=')
        {
            let key = raw_k.trim();
            let val = raw_v.trim();
            if key.eq_ignore_ascii_case("path") {
                current_path = Some(val.to_string());
            } else if key.eq_ignore_ascii_case("url") {
                current_url = Some(val.to_string());
            } else if key.eq_ignore_ascii_case("branch") {
                current_branch = Some(CompactString::from(val));
            }
        }
    }

    flush_current(
        &mut result,
        &mut current_name,
        &mut current_path,
        &mut current_url,
        &mut current_branch,
    );
    result
}

/// Reads and parses `<repo_root>/.gitmodules` if present on disk.
pub fn load_submodules_from_repo(repo_root: &Path) -> Vec<SubmoduleSummary> {
    let gitmodules_path = repo_root.join(".gitmodules");
    let Ok(content) = std::fs::read_to_string(gitmodules_path) else {
        return Vec::new();
    };
    parse_gitmodules_content(&content)
}

#[derive(Default)]
struct MutableFileDir<'a> {
    dirs: BTreeMap<CompactString, MutableFileDir<'a>>,
    files: Vec<(CompactString, &'a FileDelta)>,
}

impl MutableFileDir<'_> {
    fn total_files(&self) -> usize {
        self.files.len() + self.dirs.values().map(Self::total_files).sum::<usize>()
    }
}

/// Builds renderable file list rows respecting either [`FileListLayoutMode::FlatList`] or [`FileListLayoutMode::FolderTree`].
pub fn build_file_tree_rows(
    deltas: &[FileDelta],
    mode: FileListLayoutMode,
    collapsed_dirs: &HashSet<CompactString>,
) -> Vec<FileTreeRow> {
    if mode == FileListLayoutMode::FlatList {
        return deltas
            .iter()
            .map(|delta| FileTreeRow::File {
                depth: 0,
                display_name: CompactString::from(delta.new_path.as_str()),
                delta: delta.clone(),
            })
            .collect();
    }

    let mut root = MutableFileDir::default();
    for delta in deltas {
        let parts: Vec<&str> = delta
            .new_path
            .as_str()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let mut cursor = &mut root;
        for dir_seg in &parts[..parts.len() - 1] {
            cursor = cursor
                .dirs
                .entry(CompactString::from(*dir_seg))
                .or_default();
        }
        if let Some(filename) = parts.last() {
            cursor.files.push((CompactString::from(*filename), delta));
        }
    }

    let mut rows = Vec::with_capacity(deltas.len() + 8);
    flatten_file_dir("", 0, &root, collapsed_dirs, &mut rows);
    rows
}

fn flatten_file_dir(
    prefix: &str,
    depth: u16,
    dir: &MutableFileDir<'_>,
    collapsed_dirs: &HashSet<CompactString>,
    out: &mut Vec<FileTreeRow>,
) {
    for (dir_name, child) in &dir.dirs {
        let next_prefix = if prefix.is_empty() {
            dir_name.to_string()
        } else {
            format!("{prefix}/{dir_name}")
        };
        let key = CompactString::from(next_prefix.as_str());
        let collapsed = collapsed_dirs.contains(&key);
        out.push(FileTreeRow::Directory {
            name: dir_name.clone(),
            dir_path: key,
            depth,
            collapsed,
            file_count: child.total_files(),
        });
        if !collapsed {
            flatten_file_dir(&next_prefix, depth + 1, child, collapsed_dirs, out);
        }
    }

    let mut sorted_files = dir.files.clone();
    sorted_files.sort_by(|a, b| a.0.cmp(&b.0));
    for (filename, delta) in sorted_files {
        out.push(FileTreeRow::File {
            depth,
            display_name: filename,
            delta: delta.clone(),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use branvio_core::domain::{DeltaArea, DeltaStatus, EolChange, FileMode};

    fn dummy_badge(name: &str, kind: RefKind, is_head: bool) -> RefBadge {
        let full_ref = match &kind {
            RefKind::LocalBranch => format!("refs/heads/{name}"),
            RefKind::RemoteBranch { .. } => format!("refs/remotes/{name}"),
            RefKind::Tag { .. } => format!("refs/tags/{name}"),
            RefKind::Stash => format!("refs/{name}"),
            RefKind::HeadDetached => "HEAD".to_string(),
            RefKind::PullRequestRef { number } => format!("refs/pull/{number}/head"),
        };
        RefBadge {
            name: CompactString::from(name),
            full_ref: CompactString::from(full_ref),
            kind,
            is_head,
            upstream_divergence: Some((1, 0)),
        }
    }

    fn dummy_delta(path: &str) -> FileDelta {
        FileDelta {
            old_path: None,
            new_path: RepoRelPath::normalize(path).unwrap(),
            status: DeltaStatus::Modified,
            area: DeltaArea::Unstaged,
            old_mode: FileMode::Regular644,
            new_mode: FileMode::Regular644,
            old_oid: None,
            new_oid: None,
            is_binary: false,
            lfs: None,
            eol_change: EolChange::Unchanged,
            additions: 1,
            deletions: 0,
        }
    }

    #[test]
    fn nests_slash_branches_and_groups_remotes() {
        let badges = vec![
            dummy_badge("main", RefKind::LocalBranch, true),
            dummy_badge("feature/auth/oauth", RefKind::LocalBranch, false),
            dummy_badge("feature/auth/saml", RefKind::LocalBranch, false),
            dummy_badge(
                "origin/main",
                RefKind::RemoteBranch {
                    remote: CompactString::const_new("origin"),
                },
                false,
            ),
            dummy_badge(
                "upstream/feature/perf",
                RefKind::RemoteBranch {
                    remote: CompactString::const_new("upstream"),
                },
                false,
            ),
            dummy_badge("v1.0.0", RefKind::Tag { is_annotated: true }, false),
        ];

        let mut collapsed = HashSet::new();
        collapsed.insert(CompactString::const_new("local:feature/auth"));

        let sections = build_sidebar_ref_sections(&badges, &collapsed);
        assert_eq!(sections.tags.len(), 1);
        assert_eq!(sections.remotes.len(), 2);
        assert_eq!(sections.remotes[0].remote_name.as_str(), "origin");
        assert_eq!(sections.remotes[1].remote_name.as_str(), "upstream");

        let flat_local = flatten_branch_tree(&sections.local_branches);
        // `feature` folder (open), `auth` folder (collapsed -> hides oauth/saml), and root leaf `main`.
        assert_eq!(flat_local.len(), 3);
        assert!(matches!(
            &flat_local[1],
            FlatSidebarBranchRow::Folder { key, collapsed: true, .. } if key == "local:feature/auth"
        ));
    }

    #[test]
    fn parses_gitmodules_entries() {
        let content = r#"
[submodule "vendor/libfoo"]
    path = vendor/libfoo
    url = https://github.com/example/libfoo.git
    branch = main
[submodule "docs-theme"]
    path = docs/theme
    url = git@github.com:example/theme.git
"#;
        let subs = parse_gitmodules_content(content);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name.as_str(), "vendor/libfoo");
        assert_eq!(subs[0].path.as_str(), "vendor/libfoo");
        assert_eq!(subs[0].branch.as_deref(), Some("main"));
        assert_eq!(subs[1].name.as_str(), "docs-theme");
        assert_eq!(subs[1].path.as_str(), "docs/theme");
        assert!(subs[1].branch.is_none());
    }

    #[test]
    fn builds_flat_and_folder_tree_file_lists() {
        let deltas = vec![
            dummy_delta("src/ui/mod.rs"),
            dummy_delta("src/lib.rs"),
            dummy_delta("Cargo.toml"),
        ];

        let flat = build_file_tree_rows(&deltas, FileListLayoutMode::FlatList, &HashSet::new());
        assert_eq!(flat.len(), 3);
        assert!(
            matches!(&flat[0], FileTreeRow::File { display_name, .. } if display_name == "src/ui/mod.rs")
        );

        let tree = build_file_tree_rows(&deltas, FileListLayoutMode::FolderTree, &HashSet::new());
        // `src` dir, `ui` dir, `mod.rs` file, `lib.rs` file, `Cargo.toml` file => 5 rows
        assert_eq!(tree.len(), 5);
        assert!(matches!(
            &tree[0],
            FileTreeRow::Directory { name, file_count: 2, depth: 0, .. } if name == "src"
        ));
        assert!(matches!(
            &tree[2],
            FileTreeRow::File { display_name, depth: 2, .. } if display_name == "mod.rs"
        ));
    }
}
