//! File delta, hunk, line-level diff, syntax token, and AST enclosing symbol types.

use crate::ids::GitOid;
use crate::paths::RepoRelPath;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::sync::Arc;

/// Git tree/index file mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileMode {
    /// Entry does not exist in this tree/stage.
    Absent,
    /// Normal non-executable file (`100644`).
    Regular644,
    /// Executable file (`100755`).
    Executable755,
    /// Symbolic link (`120000`).
    Symlink,
    /// Git submodule gitlink (`160000`).
    GitlinkSubmodule,
    /// Directory tree (`040000`).
    Tree,
}

/// Specific index stage combination for an unmerged conflicted entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictSubkind {
    /// Both sides modified the file (`AA`/`UU`).
    BothModified,
    /// Both sides added the file independently without a common ancestor (`AA`).
    BothAdded,
    /// Deleted by us (`HEAD`), modified by them.
    DeletedByUs,
    /// Modified by us, deleted by them.
    DeletedByThem,
    /// Added by us, absent in them.
    AddedByUs,
    /// Absent in us, added by them.
    AddedByThem,
    /// Deleted by both sides with stage conflict residue.
    BothDeleted,
}

/// Status of a file change within working tree, index, or commit diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeltaStatus {
    /// Newly created file.
    Added,
    /// Modified existing file.
    Modified,
    /// Deleted file.
    Deleted,
    /// Renamed file with similarity percentage (`0..=100`).
    Renamed {
        /// Git rename detection similarity score (`0..=100`).
        similarity_pct: u8,
    },
    /// Copied file with similarity percentage (`0..=100`).
    Copied {
        /// Git copy detection similarity score (`0..=100`).
        similarity_pct: u8,
    },
    /// File mode type transition (e.g., regular file <-> symlink or submodule).
    TypeChanged,
    /// Unmerged file requiring 3-way conflict resolution.
    Conflicted(ConflictSubkind),
    /// Untracked file not ignored by `.gitignore`.
    Untracked,
    /// Ignored file (only returned when explicitly requested).
    Ignored,
}

/// Metadata extracted from a Git LFS pointer blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsPointerMeta {
    /// SHA-256 content hash recorded in the LFS pointer (`oid sha256:...`).
    pub oid_sha256: CompactString,
    /// True binary payload size in bytes.
    pub byte_size: u64,
    /// Whether the actual LFS object is checked out in the working tree rather than just pointer text.
    pub materialized_in_worktree: bool,
}

/// Line-ending transition tracked explicitly to guard against `core.autocrlf` noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EolChange {
    /// Line endings are consistent or unchanged after filter rules.
    Unchanged,
    /// Transitioned from LF (`\n`) to CRLF (`\r\n`).
    LfToCrlf,
    /// Transitioned from CRLF (`\r\n`) to LF (`\n`).
    CrlfToLf,
    /// File contains mixed LF and CRLF line endings.
    MixedLineEndings,
}

/// Logical area where a delta belongs in the Staging / Commit Inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeltaArea {
    /// Working tree vs `.git/index` (unstaged or untracked).
    Unstaged,
    /// `.git/index` vs `HEAD` (staged for next commit).
    Staged,
    /// Unmerged index stages (1/2/3).
    Conflicted,
    /// Historical diff inside an existing commit.
    CommitDiff,
}

/// Summary descriptor for a single changed path in a status snapshot or commit diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelta {
    /// Previous path when renamed or copied; `None` otherwise.
    pub old_path: Option<RepoRelPath>,
    /// Current/destination path relative to repository root.
    pub new_path: RepoRelPath,
    /// Change kind.
    pub status: DeltaStatus,
    /// Staging or historical area.
    pub area: DeltaArea,
    /// File mode before change.
    pub old_mode: FileMode,
    /// File mode after change.
    pub new_mode: FileMode,
    /// Blob OID before change, if applicable.
    pub old_oid: Option<GitOid>,
    /// Blob OID after change, if applicable.
    pub new_oid: Option<GitOid>,
    /// True if binary heuristic or `.gitattributes` marks the blob as binary.
    pub is_binary: bool,
    /// Populated when the blob is tracked by Git LFS.
    pub lfs: Option<LfsPointerMeta>,
    /// Line-ending change metadata.
    pub eol_change: EolChange,
    /// Count of added lines (0 for binary or uncomputed).
    pub additions: u32,
    /// Count of deleted lines (0 for binary or uncomputed).
    pub deletions: u32,
}

/// Half-open UTF-8 byte range `[start, end)` inside [`FileDiffPatch::backing_text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteRange {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset.
    pub end: u32,
}

impl ByteRange {
    /// Slice length in bytes.
    #[inline]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` when `start == end`.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Line-local byte span `[start, end)` highlighting word-level character diffs inside a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InlineByteSpan {
    /// Inclusive line-relative start byte offset.
    pub start: u32,
    /// Exclusive line-relative end byte offset.
    pub end: u32,
}

/// Semantic token categories produced by `tree-sitter` syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SyntaxTokenKind {
    /// Language keyword (`fn`, `struct`, `if`, `async`, `return`).
    Keyword,
    /// Function or method identifier.
    Function,
    /// Type, struct, enum, class, or interface identifier.
    Type,
    /// Local variable or parameter.
    Variable,
    /// Struct field or object property.
    Property,
    /// String or char literal.
    String,
    /// Numeric literal.
    Number,
    /// Line or block comment.
    Comment,
    /// Arithmetic, logical, or comparison operator.
    Operator,
    /// Delimiter or punctuation.
    Punctuation,
    /// Macro invocation or definition.
    Macro,
    /// Attribute, decorator, or annotation.
    Attribute,
    /// Named constant or enum variant.
    Constant,
}

/// Line-local UTF-8 byte span tagged with a `tree-sitter` syntax category.
///
/// `start` and `end` are UTF-8 byte offsets relative to the line slice (excluding trailing `\r`/`\n`),
/// matching `str` slicing indices rather than UTF-16 or grapheme column counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyntaxTokenSpan {
    /// Inclusive line-relative UTF-8 byte offset.
    pub start: u32,
    /// Exclusive line-relative UTF-8 byte offset.
    pub end: u32,
    /// Syntax token category mapped by the active theme.
    pub kind: SyntaxTokenKind,
}

/// Kind of enclosing AST declaration shown in a diff hunk header (`@@ ... @@ fn foo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Associated method inside `impl` or class.
    Method,
    /// Struct declaration.
    Struct,
    /// Enum declaration.
    Enum,
    /// Class declaration.
    Class,
    /// Interface declaration.
    Interface,
    /// Trait declaration.
    Trait,
    /// Impl block.
    Impl,
    /// Module or namespace block.
    Module,
    /// Top-level constant or static item.
    Constant,
}

/// AST symbol enclosing a [`DiffHunk`], extracted via `tree-sitter` for exact hunk context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnclosingSymbol {
    /// Declaration kind.
    pub kind: SymbolKind,
    /// Compact signature or header text (e.g., `pub fn apply_patch(&mut self)`).
    pub signature: CompactString,
    /// 1-based line number where the enclosing symbol begins in the new (or old) file.
    pub start_line: u32,
}

/// Line origin inside a unified or side-by-side diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiffLineOrigin {
    /// Unchanged context line present on both sides.
    Context,
    /// Line added in the new side (`+`).
    Addition,
    /// Line removed from the old side (`-`).
    Deletion,
    /// Marker indicating old file lacked trailing newline (`\ No newline at end of file`).
    EofNoNewlineOld,
    /// Marker indicating new file lacked trailing newline (`\ No newline at end of file`).
    EofNoNewlineNew,
}

/// Single line inside a [`DiffHunk`], referencing zero-copy text inside [`FileDiffPatch::backing_text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// Line role (`Context`, `Addition`, `Deletion`, or EOF marker).
    pub origin: DiffLineOrigin,
    /// 1-based line number on the old side (`None` for additions).
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side (`None` for deletions).
    pub new_lineno: Option<u32>,
    /// UTF-8 byte range into `FileDiffPatch::backing_text` excluding trailing `\n` (new side for `Context`).
    pub content_range: ByteRange,
    /// UTF-8 byte range into `FileDiffPatch::backing_text` for the old side of this line.
    ///
    /// Equal to `content_range` for additions, deletions, EOF markers, and byte-identical context lines;
    /// differs on `Context` lines when `ignore_whitespace` or `respect_autocrlf` matched distinct raw bytes.
    pub old_content_range: ByteRange,
    /// Whether the source line ended with a newline (`\n` or `\r\n`).
    pub has_trailing_newline: bool,
    /// Word-level intra-line diff highlights.
    pub word_highlights: SmallVec<[InlineByteSpan; 4]>,
    /// `tree-sitter` syntax highlight spans relative to this line's UTF-8 byte slice.
    pub syntax_tokens: SmallVec<[SyntaxTokenSpan; 8]>,
}

/// Contiguous diff hunk with header coordinates, AST symbol context, and line entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Zero-based index of this hunk within [`FileDiffPatch::hunks`].
    pub hunk_index: u32,
    /// 1-based starting line in the old file (0 when adding into an empty file).
    pub old_start: u32,
    /// Line count on the old side covered by this hunk.
    pub old_lines: u32,
    /// 1-based starting line in the new file (0 when deleting a file to empty).
    pub new_start: u32,
    /// Line count on the new side covered by this hunk.
    pub new_lines: u32,
    /// Standard `@@ -a,b +c,d @@` hunk header text.
    pub header_raw: CompactString,
    /// Enclosing function/struct/class parsed from the file AST, if available.
    pub enclosing_symbol: Option<EnclosingSymbol>,
    /// Ordered diff lines in this hunk.
    pub lines: Vec<DiffLine>,
    /// Stable 64-bit hash over the hunk's normalized lines for optimistic patch conflict detection.
    pub content_fingerprint: u64,
}

/// Complete diff payload for a single file, sharing one contiguous immutable string buffer
/// so thousands of diff lines require only tiny [`ByteRange`] indices instead of per-line allocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffPatch {
    /// Descriptor of the file change.
    pub delta: FileDelta,
    /// Shared contiguous backing text referenced by every [`DiffLine::content_range`].
    pub backing_text: Arc<str>,
    /// Parsed hunks with optional word-diff and `tree-sitter` tokens.
    pub hunks: Vec<DiffHunk>,
}

impl FileDiffPatch {
    /// Resolves the zero-copy string slice for a [`DiffLine`] (new side on `Context`).
    #[inline]
    pub fn line_text(&self, line: &DiffLine) -> &str {
        self.slice_range(line.content_range)
    }

    /// Resolves the zero-copy string slice for the old side of a [`DiffLine`].
    ///
    /// Returns the old side's exact bytes on `Context` lines even when `ignore_whitespace` or
    /// `respect_autocrlf` matched lines that differed in whitespace or CRLF/LF.
    #[inline]
    pub fn old_line_text(&self, line: &DiffLine) -> &str {
        self.slice_range(line.old_content_range)
    }

    #[inline]
    fn slice_range(&self, range: ByteRange) -> &str {
        let start = range.start as usize;
        let end = (range.end as usize).min(self.backing_text.len());
        if start <= end {
            &self.backing_text[start..end]
        } else {
            ""
        }
    }
}
