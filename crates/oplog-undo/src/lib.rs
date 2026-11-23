//! Time-Machine transactional Undo/Redo operation journal and safety verification crate.

pub mod engine;
pub mod store;
pub mod traits;

pub use engine::{BranvioUndoEngine, UNDO_SNAPSHOT_REF_PREFIX, snapshot_pin_ref};
pub use store::{MAX_ENTRIES_PER_REPO, RedbOpLogStore, compute_repo_root_hash};
pub use traits::{
    OpLogError, OpLogResult, OpLogStore, PushSafetyCheck, UndoOutcome, UndoRedoEngine,
};
