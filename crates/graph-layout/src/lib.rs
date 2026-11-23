//! Metro-line DAG Commit Graph layout contracts, concrete layout engine,
//! deterministic color palette, and instant commit/graph filtering specifications.

pub mod engine;
pub mod filter;
pub mod palette;
pub mod traits;

pub use engine::{MetroLayoutEngine, MetroLayoutSession};
pub use filter::GraphFilterSpec;
pub use palette::{
    METRO_PALETTE, deterministic_anonymous_lane_color, deterministic_branch_color,
    is_primary_trunk_branch, palette_color,
};
pub use traits::{
    GraphLayoutEngine, IncrementalLayoutSession, LayoutError, LayoutResult, LayoutSessionConfig,
    ViewportRowRef,
};
