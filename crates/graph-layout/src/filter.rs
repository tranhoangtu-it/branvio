//! Graph filtering criteria for instant search across commits, authors, branches, files, and code diffs.

use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// Query parameters applied to the Commit Graph layout and revwalk pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GraphFilterSpec {
    /// Restrict graph to specific branch or tag patterns (empty means all refs).
    pub branch_patterns: Vec<CompactString>,
    /// Case-insensitive substring matches on author name or email.
    pub author_queries: Vec<CompactString>,
    /// Restrict commits to those touching specific repository paths.
    pub file_paths: Vec<RepoRelPath>,
    /// Hex SHA prefix search (e.g., `"a94a8fe"`).
    pub sha_prefix: Option<CompactString>,
    /// Substring / phrase search across commit summary and body.
    pub message_query: Option<CompactString>,
    /// Git pickaxe code change query (`-S <snippet>`).
    pub pickaxe_code_query: Option<String>,
    /// Hide merge commits with 2+ parents.
    pub hide_merges: bool,
    /// Follow only first parent along active tips.
    pub first_parent_only: bool,
    /// Include or hide the synthetic WIP working-tree node above `HEAD`.
    pub show_wip_node: bool,
}

impl GraphFilterSpec {
    /// Returns `true` when no filtering constraints narrow the full repository graph.
    #[inline]
    pub fn is_unfiltered(&self) -> bool {
        self.branch_patterns.is_empty()
            && self.author_queries.is_empty()
            && self.file_paths.is_empty()
            && self.sha_prefix.is_none()
            && self.message_query.is_none()
            && self.pickaxe_code_query.is_none()
            && !self.hide_merges
            && !self.first_parent_only
    }
}
