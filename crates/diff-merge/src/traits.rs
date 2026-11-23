//! Core traits for `imara-diff`, `tree-sitter` syntax highlighting, synthetic line-level
//! patch generation, and 3-way conflict resolution.

use crate::merge_buffers::ThreeWayMergeDocument;
use branvio_core::domain::{
    ConflictRegion, ConflictResolution, EnclosingSymbol, FileDelta, FileDiffPatch,
};
use branvio_core::paths::RepoRelPath;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

/// Result alias for `diff-merge`.
pub type DiffMergeResult<T> = Result<T, DiffMergeError>;

/// Maximum per-blob byte size supported by [`DiffEngine`] and [`MergeConflictEngine`] (`64 MiB`),
/// preventing `u32` `ByteRange` saturation and protecting memory budgets.
pub const MAX_TEXT_DIFF_BYTES: usize = 64 * 1024 * 1024;

/// Maximum per-file byte size parsed by [`SyntaxHighlighter`] (`8 MiB`) before gracefully
/// skipping full-file `tree-sitter` AST construction.
pub const MAX_SYNTAX_HIGHLIGHT_BYTES: usize = 8 * 1024 * 1024;

/// Errors produced during diffing, AST highlighting, patch building, or 3-way merging.
#[derive(Debug, Error)]
pub enum DiffMergeError {
    /// Attempted to generate a line-level patch with empty or out-of-bounds selections.
    #[error("invalid patch selection for hunk {hunk_index}: {reason}")]
    InvalidSelection {
        /// Hunk index targeted.
        hunk_index: u32,
        /// Explanation of why the selection cannot produce a valid unified patch.
        reason: &'static str,
    },

    /// Binary blobs cannot undergo line-level text diffing or synthetic line patching.
    #[error("cannot compute line diff or line patch on binary file '{0}'")]
    BinaryFileUnsupported(String),

    /// Non-UTF-8 text blobs (e.g., Shift-JIS, Latin-1) are rejected explicitly rather than
    /// lossily rewritten with `U+FFFD` replacement characters.
    #[error("unsupported non-UTF-8 text encoding in '{path}'")]
    UnsupportedEncoding {
        /// Repository-relative file path whose bytes failed strict UTF-8 validation.
        path: String,
    },

    /// File change kind or tree entry mode cannot safely undergo synthetic line-level patching.
    #[error("line-level patch unsupported for '{path}': {reason}")]
    UnsupportedPatchOperation {
        /// Repository-relative file path.
        path: String,
        /// Explanation of why whole-file staging/unstaging must be used instead.
        reason: &'static str,
    },

    /// Input buffer exceeds safety limits for `u32` `ByteRange` addressing or interactive AST parsing.
    #[error(
        "input '{path}' ({byte_len} bytes) exceeds maximum diff/merge size ({max_bytes} bytes)"
    )]
    InputTooLarge {
        /// Repository-relative file path.
        path: String,
        /// Actual byte length encountered.
        byte_len: usize,
        /// Configured upper bound in bytes.
        max_bytes: usize,
    },

    /// Referenced conflict region does not exist in [`ThreeWayMergeDocument`].
    #[error("conflict region {0} out of range")]
    UnknownConflictRegion(u32),

    /// Attempted to export/mark resolved while unresolved conflict regions or raw markers remain.
    #[error("{remaining} conflict region(s) still unresolved")]
    UnresolvedConflictsRemaining {
        /// Count of unresolved conflict blocks.
        remaining: usize,
    },

    /// Language grammar or tree-sitter parser failure.
    #[error("syntax parser failure: {0}")]
    SyntaxParser(String),
}

/// Supported diff algorithms exposed by `imara-diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DiffAlgorithmKind {
    /// Histogram diff (produces cleaner, human-readable code hunks by preferring unique anchors).
    #[default]
    Histogram,
    /// Classic Myers O(ND) shortest-edit-script diff.
    Myers,
}

/// Options controlling diff generation and whitespace/EOL handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffOptions {
    /// Diff algorithm (`Histogram` default).
    pub algorithm: DiffAlgorithmKind,
    /// Number of unchanged context lines around edits (typically `3`).
    pub context_lines: u32,
    /// Ignore whitespace-only changes when computing diff hunks.
    pub ignore_whitespace: bool,
    /// Respect repository `core.autocrlf` normalization to prevent false full-file diffs.
    pub respect_autocrlf: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            algorithm: DiffAlgorithmKind::Histogram,
            context_lines: 3,
            ignore_whitespace: false,
            respect_autocrlf: true,
        }
    }
}

/// Direction of a synthetic unified patch produced by [`PatchBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatchDirection {
    /// Stage selected hunk/lines forward from Worktree into `.git/index`.
    StageToIndex,
    /// Unstage selected hunk/lines backward out of `.git/index` toward `HEAD`.
    UnstageFromIndex,
    /// Revert selected hunk/lines in the working directory back to `.git/index`.
    DiscardInWorktree,
}

/// Specifies discrete line indices inside a [`DiffHunk`] chosen by the user for partial staging/discarding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSelectionSpec {
    /// Target hunk index inside [`FileDiffPatch::hunks`].
    pub hunk_index: u32,
    /// Zero-based indices into `DiffHunk::lines` selecting `Addition`/`Deletion` rows to apply.
    /// Non-selected additions (when staging) are omitted; non-selected deletions are converted to context!
    pub selected_line_indices: BTreeSet<usize>,
}

/// Computes line and intra-line word diffs using `imara-diff`.
pub trait DiffEngine: Send + Sync {
    /// Computes a [`FileDiffPatch`] between raw old and new file byte buffers.
    fn diff_blobs(
        &self,
        delta: FileDelta,
        old_bytes: &[u8],
        new_bytes: &[u8],
        options: &DiffOptions,
    ) -> DiffMergeResult<FileDiffPatch>;
}

/// Enriches [`FileDiffPatch`] hunks with `tree-sitter` syntax token spans and enclosing AST symbol names.
///
/// Emitted [`SyntaxTokenSpan`](branvio_core::domain::SyntaxTokenSpan) coordinates are line-local UTF-8 byte
/// offsets (excluding trailing `\r`/`\n`). JavaScript extensions (`.js`, `.mjs`, `.cjs`, `.jsx`) are parsed
/// using the TypeScript / TSX superset grammar, and files larger than [`MAX_SYNTAX_HIGHLIGHT_BYTES`]
/// gracefully skip AST parsing while leaving diff hunks intact.
pub trait SyntaxHighlighter: Send + Sync {
    /// Annotates `DiffLine::syntax_tokens` on every line inside `patch` based on file extension.
    fn highlight_patch(
        &self,
        patch: &mut FileDiffPatch,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> DiffMergeResult<()>;

    /// Determines the innermost function, method, struct, class, or impl enclosing `new_start_line`.
    fn extract_enclosing_symbol(
        &self,
        path: &RepoRelPath,
        source: &[u8],
        target_line_1_based: u32,
    ) -> Option<EnclosingSymbol>;
}

/// Builds valid Git-compatible unified diff patches for whole hunks or discrete individual lines.
pub trait PatchBuilder: Send + Sync {
    /// Emits a unified patch bytes buffer containing a single complete [`DiffHunk`].
    fn build_hunk_patch(
        &self,
        patch: &FileDiffPatch,
        hunk_index: u32,
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>>;

    /// Emits a synthetic unified patch applying ONLY the discrete line indices selected by the user.
    ///
    /// Automatically recalculates `@@ -old_start,old_len +new_start,new_len @@` hunk headers,
    /// preserves unselected deletions as context lines (for forward staging), and drops unselected
    /// additions so `git2` / `git apply --cached` accepts the patch without offsets or rejections.
    fn build_line_selection_patch(
        &self,
        patch: &FileDiffPatch,
        selections: &[LineSelectionSpec],
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>>;
}

/// Input buffers and branch labels required to open a 3-way merge conflict session.
#[derive(Debug, Clone)]
pub struct ConflictedFileInputs<'a> {
    /// Repository-relative path of the conflicted file.
    pub path: RepoRelPath,
    /// Optional common ancestor (`stage 1`) bytes.
    pub base_bytes: Option<&'a [u8]>,
    /// Current `HEAD` (`stage 2`) bytes.
    pub ours_bytes: &'a [u8],
    /// Incoming branch (`stage 3`) bytes.
    pub theirs_bytes: &'a [u8],
    /// Optional working-tree file bytes containing raw conflict markers.
    pub worktree_with_markers: Option<&'a [u8]>,
    /// Display label for the `Ours` side (e.g. `"HEAD (main)"`).
    pub ours_label: &'a str,
    /// Display label for the `Theirs` side (e.g. `"feature/login"`).
    pub theirs_label: &'a str,
}

/// Engine managing 4-buffer `ropey::Rope` 3-way merge conflict sessions.
pub trait MergeConflictEngine: Send + Sync {
    /// Parses a conflicted file along with its 3 index stages (`Base`, `Ours`, `Theirs`) into
    /// a live [`ThreeWayMergeDocument`].
    fn open_conflicted_file(
        &self,
        inputs: ConflictedFileInputs<'_>,
    ) -> DiffMergeResult<ThreeWayMergeDocument>;

    /// Applies a region resolution (`TakeOurs`, `TakeTheirs`, `TakeBoth*`, `LineLevelSelection`, etc.),
    /// splices the resolved text into `doc.output`, and shifts subsequent [`ConflictRegion::output_line_range`]s.
    fn resolve_region(
        &self,
        doc: &mut ThreeWayMergeDocument,
        region_id: u32,
        resolution: ConflictResolution,
    ) -> DiffMergeResult<()>;

    /// Applies a direct user edit `[byte_start..byte_end)` on `doc.output`, automatically marking
    /// any affected [`ConflictRegion`] as [`ConflictResolution::ManualEdit`] and adjusting line offsets.
    fn edit_output_buffer(
        &mut self,
        doc: &mut ThreeWayMergeDocument,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) -> DiffMergeResult<()>;

    /// Exports the final merged file bytes (preserving original CRLF/LF convention) once all
    /// conflict regions are resolved (`require_all_resolved = true`).
    fn export_merged_bytes(
        &self,
        doc: &ThreeWayMergeDocument,
        require_all_resolved: bool,
    ) -> DiffMergeResult<Vec<u8>>;

    /// Re-scans `doc.output` for any hand-typed or residual conflict blocks (`<<<<<<< ... ======= ... >>>>>>>`)
    /// and stray conflict markers (`<<<<<<<`, `|||||||`, `=======` inside blocks, `>>>>>>>`).
    fn scan_remaining_markers(&self, doc: &ThreeWayMergeDocument) -> Vec<ConflictRegion>;
}
