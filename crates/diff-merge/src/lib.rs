//! Diff computation (`imara-diff`), AST syntax highlighting & enclosing symbol extraction
//! (`tree-sitter`), synthetic line-level patch generation, and 4-buffer `ropey::Rope` 3-way merge
//! conflict resolution.

pub mod conflict_engine;
pub mod diff_engine;
pub mod merge_buffers;
pub mod patch_builder;
pub mod syntax;
pub mod traits;

pub use conflict_engine::RopeConflictEngine;
pub use diff_engine::ImaraDiffEngine;
pub use merge_buffers::ThreeWayMergeDocument;
pub use patch_builder::SyntheticPatchBuilder;
pub use syntax::TreeSitterHighlighter;
pub use traits::{
    ConflictedFileInputs, DiffAlgorithmKind, DiffEngine, DiffMergeError, DiffMergeResult,
    DiffOptions, LineSelectionSpec, MergeConflictEngine, PatchBuilder, PatchDirection,
    SyntaxHighlighter,
};

use branvio_core::domain::{
    ConflictRegion, ConflictResolution, EnclosingSymbol, FileDelta, FileDiffPatch,
};
use branvio_core::paths::RepoRelPath;

/// Unified facade composing [`ImaraDiffEngine`], [`TreeSitterHighlighter`],
/// [`SyntheticPatchBuilder`], and [`RopeConflictEngine`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BranvioDiffMergeEngine {
    diff: ImaraDiffEngine,
    syntax: TreeSitterHighlighter,
    patch: SyntheticPatchBuilder,
    conflict: RopeConflictEngine,
}

impl BranvioDiffMergeEngine {
    /// Creates a new [`BranvioDiffMergeEngine`].
    #[inline]
    pub const fn new() -> Self {
        Self {
            diff: ImaraDiffEngine::new(),
            syntax: TreeSitterHighlighter::new(),
            patch: SyntheticPatchBuilder::new(),
            conflict: RopeConflictEngine::new(),
        }
    }
}

impl DiffEngine for BranvioDiffMergeEngine {
    fn diff_blobs(
        &self,
        delta: FileDelta,
        old_bytes: &[u8],
        new_bytes: &[u8],
        options: &DiffOptions,
    ) -> DiffMergeResult<FileDiffPatch> {
        self.diff.diff_blobs(delta, old_bytes, new_bytes, options)
    }
}

impl SyntaxHighlighter for BranvioDiffMergeEngine {
    fn highlight_patch(
        &self,
        patch: &mut FileDiffPatch,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> DiffMergeResult<()> {
        self.syntax.highlight_patch(patch, old_source, new_source)
    }

    fn extract_enclosing_symbol(
        &self,
        path: &RepoRelPath,
        source: &[u8],
        target_line_1_based: u32,
    ) -> Option<EnclosingSymbol> {
        self.syntax
            .extract_enclosing_symbol(path, source, target_line_1_based)
    }
}

impl PatchBuilder for BranvioDiffMergeEngine {
    fn build_hunk_patch(
        &self,
        patch: &FileDiffPatch,
        hunk_index: u32,
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>> {
        self.patch.build_hunk_patch(patch, hunk_index, direction)
    }

    fn build_line_selection_patch(
        &self,
        patch: &FileDiffPatch,
        selections: &[LineSelectionSpec],
        direction: PatchDirection,
    ) -> DiffMergeResult<Vec<u8>> {
        self.patch
            .build_line_selection_patch(patch, selections, direction)
    }
}

impl MergeConflictEngine for BranvioDiffMergeEngine {
    fn open_conflicted_file(
        &self,
        inputs: ConflictedFileInputs<'_>,
    ) -> DiffMergeResult<ThreeWayMergeDocument> {
        self.conflict.open_conflicted_file(inputs)
    }

    fn resolve_region(
        &self,
        doc: &mut ThreeWayMergeDocument,
        region_id: u32,
        resolution: ConflictResolution,
    ) -> DiffMergeResult<()> {
        self.conflict.resolve_region(doc, region_id, resolution)
    }

    fn edit_output_buffer(
        &mut self,
        doc: &mut ThreeWayMergeDocument,
        byte_range: std::ops::Range<usize>,
        replacement_utf8: &str,
    ) -> DiffMergeResult<()> {
        self.conflict
            .edit_output_buffer(doc, byte_range, replacement_utf8)
    }

    fn export_merged_bytes(
        &self,
        doc: &ThreeWayMergeDocument,
        require_all_resolved: bool,
    ) -> DiffMergeResult<Vec<u8>> {
        self.conflict.export_merged_bytes(doc, require_all_resolved)
    }

    fn scan_remaining_markers(&self, doc: &ThreeWayMergeDocument) -> Vec<ConflictRegion> {
        self.conflict.scan_remaining_markers(doc)
    }
}
