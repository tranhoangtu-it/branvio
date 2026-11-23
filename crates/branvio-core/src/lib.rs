//! UI-agnostic core domain types, cross-platform environment abstractions,
//! and lock-free MVCC reactive pipeline for Branvio.

pub mod domain;
pub mod error;
pub mod ids;
pub mod paths;
pub mod pipeline;
pub mod platform;

pub use error::{CoreError, CoreResult};
pub use ids::{GitOid, ObjectFormat, RepoEpoch, RepoId};
pub use paths::{FsCaseSensitivity, HostPath, RepoRelPath};
