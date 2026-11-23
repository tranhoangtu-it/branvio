//! Central domain data structures shared across all Branvio engine and UI crates.

pub mod commit;
pub mod conflict;
pub mod diff;
pub mod graph;
pub mod oplog;

pub use commit::{
    CommitDetails, CommitFlags, CommitNode, CommitSignatureState, CompactSignature, RefBadge,
    RefBadgeMap, RefKind,
};
pub use conflict::{ConflictLinePick, ConflictRegion, ConflictResolution, ConflictSide};
pub use diff::{
    ByteRange, ConflictSubkind, DeltaArea, DeltaStatus, DiffHunk, DiffLine, DiffLineOrigin,
    EnclosingSymbol, EolChange, FileDelta, FileDiffPatch, FileMode, InlineByteSpan, LfsPointerMeta,
    SymbolKind, SyntaxTokenKind, SyntaxTokenSpan,
};
pub use graph::{
    BranchLane, DeterministicColor, EdgeCurveKind, GraphEdge, GraphNodeKind, GraphRow,
    LanePriority, LaneSegment, UNRESOLVED_COMMIT_ROW, VirtualWipRow,
};
pub use oplog::{
    HeadTargetSnapshot, OpKind, OpLogEntry, RefMutationRecord, RemoteExposureState,
    RepoOperationState, RepoStateSnapshot, ResetMode,
};
