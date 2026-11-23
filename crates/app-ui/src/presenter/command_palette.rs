//! `nucleo_matcher`-powered fuzzy Command Palette index and shortcut catalog.

use crate::ports::{DiffViewMode, PaletteCommandItem};
use branvio_core::domain::{RefBadge, RefKind};
use branvio_core::platform::ShortcutChord;
use compact_str::CompactString;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Concrete action triggered when executing an entry from the Command Palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteActionKind {
    /// Opens repository selector dialog.
    OpenRepositoryDialog,
    /// Opens Create Branch dialog targeting `HEAD`.
    OpenCreateBranchDialog,
    /// Opens Create Linked Worktree dialog.
    OpenCreateWorktreeDialog,
    /// Stages all unstaged & untracked changes.
    StageAll,
    /// Unstages all currently staged files.
    UnstageAll,
    /// Commits currently staged changes.
    CommitStaged,
    /// Fetches all remotes with `--prune`.
    FetchAll,
    /// Pulls current branch (`ff_only`).
    PullCurrent {
        /// Whether to use `--rebase` instead of fast-forward merge.
        rebase: bool,
    },
    /// Pushes current branch to `origin` (regular, non-forced push).
    PushCurrent,
    /// Opens the force-push confirmation for the current branch (captures the lease value).
    ForcePushCurrent,
    /// Toggles the global "Allow sending diffs to cloud AI" privacy setting.
    ToggleAiCloudUpload,
    /// Pushes dirty changes onto Git stash.
    StashPush,
    /// Pops top stash entry (`stash@{0}`).
    StashPop,
    /// Launches Interactive Rebase modal targeting a branch (`onto`).
    OpenInteractiveRebase,
    /// Toggles Diff view between Unified and Side-by-Side.
    SwitchDiffViewMode(DiffViewMode),
    /// Toggles File List layout between Flat List and Folder Tree.
    ToggleFileListLayout,
    /// Toggles Dark / Light theme appearance.
    ToggleThemeAppearance,
    /// Toggles Bottom CLI/Output Drawer visibility.
    ToggleBottomDrawer,
    /// Focuses Graph Search / Pickaxe filter input.
    FocusSearchFilter,
    /// Triggers Time-Machine Undo (via optional Phase 5 port).
    UndoLastOperation,
    /// Triggers Time-Machine Redo (via optional Phase 5 port).
    RedoLastOperation,
    /// Checks out a named local/remote branch or tag.
    CheckoutRevision(CompactString),
    /// Opens a linked worktree path as a workspace tab.
    OpenWorktreeTab(String),
}

/// Searchable entry combining metadata for UI rendering and strongly-typed execution action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    /// UI metadata (`id`, `category`, `label`, `shortcut_display`).
    pub item: PaletteCommandItem,
    /// Optional subtitle shown beneath/next to label (e.g. commit SHA or worktree path).
    pub subtitle: Option<String>,
    /// Strongly typed action dispatched upon activation.
    pub action: PaletteActionKind,
}

/// Ranked fuzzy match returned by [`search_palette_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteMatch {
    /// Index into the source `&[PaletteEntry]` slice.
    pub entry_index: usize,
    /// Fuzzy score (`u32::MAX` when query is empty).
    pub score: u32,
    /// Sorted UTF-32 character indices matched inside `item.label` / search string.
    pub matched_indices: Vec<u32>,
}

/// Formats a command chord cleanly using host-native modifier convention (`Cmd` vs `Ctrl`).
fn host_chord(shift: bool, alt: bool, key: &str) -> CompactString {
    ShortcutChord {
        primary: true,
        shift,
        alt,
        key: CompactString::from(key),
    }
    .format_for_host()
}

/// Builds the full Command Palette catalog combining core static Git/UI commands and dynamic repo refs/worktrees.
pub fn build_palette_entries(refs: &[RefBadge], worktree_paths: &[String]) -> Vec<PaletteEntry> {
    let mut entries = vec![
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.commit"),
                category: CompactString::const_new("Git"),
                title: CompactString::const_new("Commit Staged Changes"),
                shortcut_hint: Some(host_chord(false, false, "Enter")),
            },
            subtitle: Some("Run Tier 3 `git commit` with hooks and signing".to_string()),
            action: PaletteActionKind::CommitStaged,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.stage_all"),
                category: CompactString::const_new("Git"),
                title: CompactString::const_new("Stage All Changes"),
                shortcut_hint: Some(host_chord(true, false, "A")),
            },
            subtitle: None,
            action: PaletteActionKind::StageAll,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.unstage_all"),
                category: CompactString::const_new("Git"),
                title: CompactString::const_new("Unstage All Changes"),
                shortcut_hint: Some(host_chord(true, false, "U")),
            },
            subtitle: None,
            action: PaletteActionKind::UnstageAll,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.fetch_all"),
                category: CompactString::const_new("Remote"),
                title: CompactString::const_new("Fetch All Remotes (Prune)"),
                shortcut_hint: Some(host_chord(true, false, "F")),
            },
            subtitle: None,
            action: PaletteActionKind::FetchAll,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.pull_ff"),
                category: CompactString::const_new("Remote"),
                title: CompactString::const_new("Pull (Fast-Forward)"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::PullCurrent { rebase: false },
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.pull_rebase"),
                category: CompactString::const_new("Remote"),
                title: CompactString::const_new("Pull (--rebase)"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::PullCurrent { rebase: true },
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.push"),
                category: CompactString::const_new("Remote"),
                title: CompactString::const_new("Push Current Branch"),
                shortcut_hint: Some(host_chord(true, false, "P")),
            },
            subtitle: None,
            action: PaletteActionKind::PushCurrent,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.force_push"),
                category: CompactString::const_new("Remote"),
                title: CompactString::const_new("Force Push Current Branch..."),
                shortcut_hint: None,
            },
            subtitle: Some("Asks for confirmation; defaults to a safe lease".to_string()),
            action: PaletteActionKind::ForcePushCurrent,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("ai.toggle_cloud_upload"),
                category: CompactString::const_new("Settings"),
                title: CompactString::const_new("Toggle: Allow sending diffs to cloud AI"),
                shortcut_hint: None,
            },
            subtitle: Some("Off by default; local Ollama never needs it".to_string()),
            action: PaletteActionKind::ToggleAiCloudUpload,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.branch_create"),
                category: CompactString::const_new("Branch"),
                title: CompactString::const_new("Create New Branch at HEAD..."),
                shortcut_hint: Some(host_chord(false, false, "B")),
            },
            subtitle: None,
            action: PaletteActionKind::OpenCreateBranchDialog,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.rebase_interactive"),
                category: CompactString::const_new("History"),
                title: CompactString::const_new("Start Interactive Rebase..."),
                shortcut_hint: None,
            },
            subtitle: Some("Reorder, reword, squash, fixup, or drop commits".to_string()),
            action: PaletteActionKind::OpenInteractiveRebase,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.stash_push"),
                category: CompactString::const_new("Stash"),
                title: CompactString::const_new("Stash Dirty Changes (Include Untracked)"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::StashPush,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.stash_pop"),
                category: CompactString::const_new("Stash"),
                title: CompactString::const_new("Pop Latest Stash (stash@{0})"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::StashPop,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("git.worktree_create"),
                category: CompactString::const_new("Worktree"),
                title: CompactString::const_new("Create Linked Worktree..."),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::OpenCreateWorktreeDialog,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("view.diff_unified"),
                category: CompactString::const_new("View"),
                title: CompactString::const_new("Diff View: Unified Patch"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::SwitchDiffViewMode(DiffViewMode::UnifiedDiff),
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("view.diff_split"),
                category: CompactString::const_new("View"),
                title: CompactString::const_new("Diff View: Side-by-Side Split"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::SwitchDiffViewMode(DiffViewMode::SideBySideDiff),
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("view.file_layout_toggle"),
                category: CompactString::const_new("View"),
                title: CompactString::const_new("Toggle File List Layout (Flat / Folder Tree)"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::ToggleFileListLayout,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("view.theme_toggle"),
                category: CompactString::const_new("View"),
                title: CompactString::const_new("Toggle Theme (Dark / Light)"),
                shortcut_hint: None,
            },
            subtitle: None,
            action: PaletteActionKind::ToggleThemeAppearance,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("view.drawer_toggle"),
                category: CompactString::const_new("View"),
                title: CompactString::const_new("Toggle CLI Output Drawer"),
                shortcut_hint: Some(host_chord(false, false, "`")),
            },
            subtitle: None,
            action: PaletteActionKind::ToggleBottomDrawer,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("nav.search_filter"),
                category: CompactString::const_new("Search"),
                title: CompactString::const_new("Focus Graph & Pickaxe Filter"),
                shortcut_hint: Some(host_chord(false, false, "F")),
            },
            subtitle: None,
            action: PaletteActionKind::FocusSearchFilter,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("repo.open"),
                category: CompactString::const_new("Workspace"),
                title: CompactString::const_new("Open Repository Tab..."),
                shortcut_hint: Some(host_chord(false, false, "O")),
            },
            subtitle: None,
            action: PaletteActionKind::OpenRepositoryDialog,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("oplog.undo"),
                category: CompactString::const_new("Undo"),
                title: CompactString::const_new("Undo Last Git Operation"),
                shortcut_hint: Some(host_chord(false, false, "Z")),
            },
            subtitle: Some("Time-Machine undo (requires oplog-undo provider)".to_string()),
            action: PaletteActionKind::UndoLastOperation,
        },
        PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::const_new("oplog.redo"),
                category: CompactString::const_new("Undo"),
                title: CompactString::const_new("Redo Undone Git Operation"),
                shortcut_hint: Some(host_chord(true, false, "Z")),
            },
            subtitle: None,
            action: PaletteActionKind::RedoLastOperation,
        },
    ];

    for badge in refs {
        let (category, verb) = match &badge.kind {
            RefKind::LocalBranch => ("Checkout", "Checkout branch"),
            RefKind::Tag { .. } => ("Tag", "Checkout tag"),
            _ => continue,
        };
        entries.push(PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::from(format!("checkout:{}", badge.full_ref)),
                category: CompactString::const_new(category),
                title: CompactString::from(format!("{verb} {}", badge.name)),
                shortcut_hint: None,
            },
            subtitle: Some(badge.full_ref.to_string()),
            action: PaletteActionKind::CheckoutRevision(badge.name.clone()),
        });
    }

    for wt_path in worktree_paths {
        entries.push(PaletteEntry {
            item: PaletteCommandItem {
                id: CompactString::from(format!("worktree:{wt_path}")),
                category: CompactString::const_new("Worktree"),
                title: CompactString::from(format!("Open worktree {wt_path}")),
                shortcut_hint: None,
            },
            subtitle: Some(wt_path.clone()),
            action: PaletteActionKind::OpenWorktreeTab(wt_path.clone()),
        });
    }

    entries
}

/// Fuzzy-matches and sorts `entries` against `query` using `nucleo_matcher`.
pub fn search_palette_entries(
    query: &str,
    entries: &[PaletteEntry],
    limit: usize,
) -> Vec<PaletteMatch> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return entries
            .iter()
            .enumerate()
            .take(limit)
            .map(|(entry_index, _)| PaletteMatch {
                entry_index,
                score: u32::MAX,
                matched_indices: Vec::new(),
            })
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(trimmed, CaseMatching::Ignore, Normalization::Smart);
    let mut char_buf = Vec::new();
    let mut indices = Vec::new();
    let mut scored = Vec::new();

    for (entry_index, entry) in entries.iter().enumerate() {
        indices.clear();
        let searchable = format!("{}: {}", entry.item.category, entry.item.title);
        let haystack = Utf32Str::new(&searchable, &mut char_buf);
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut indices) {
            let prefix_len = entry.item.category.len() as u32 + 2;
            let label_indices = indices
                .iter()
                .filter_map(|&idx| idx.checked_sub(prefix_len))
                .collect();
            scored.push(PaletteMatch {
                entry_index,
                score,
                matched_indices: label_indices,
            });
        }
    }

    scored.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            entries[a.entry_index]
                .item
                .title
                .cmp(&entries[b.entry_index].item.title)
        })
    });
    scored.truncate(limit);
    scored
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ranks_fuzzy_matches_with_nucleo_matcher_and_indices() {
        let badges = vec![
            RefBadge {
                name: CompactString::const_new("feature/gpui-palette"),
                full_ref: CompactString::const_new("refs/heads/feature/gpui-palette"),
                kind: RefKind::LocalBranch,
                is_head: false,
                upstream_divergence: None,
            },
            RefBadge {
                name: CompactString::const_new("main"),
                full_ref: CompactString::const_new("refs/heads/main"),
                kind: RefKind::LocalBranch,
                is_head: true,
                upstream_divergence: None,
            },
        ];

        let entries = build_palette_entries(&badges, &["/tmp/wt-hotfix".to_string()]);
        let results = search_palette_entries("gpui", &entries, 10);
        assert!(!results.is_empty());
        assert_eq!(
            entries[results[0].entry_index].item.title.as_str(),
            "Checkout branch feature/gpui-palette"
        );
        assert!(!results[0].matched_indices.is_empty());

        let rebase_results = search_palette_entries("rebs", &entries, 5);
        assert!(rebase_results.iter().any(|m| {
            matches!(
                entries[m.entry_index].action,
                PaletteActionKind::OpenInteractiveRebase
            )
        }));
    }
}
