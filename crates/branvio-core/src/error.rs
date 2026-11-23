//! Core error types shared across Branvio crates.

use thiserror::Error;

/// Result alias using [`CoreError`].
pub type CoreResult<T> = Result<T, CoreError>;

/// Errors arising from identifier parsing, path normalization, or platform hydration.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Failed to parse a hexadecimal Git object identifier.
    #[error("invalid Git OID '{input}': {reason}")]
    InvalidOid {
        /// Original raw input string.
        input: String,
        /// Explanation of why parsing failed.
        reason: &'static str,
    },

    /// Repository-relative path failed security or normalization invariants.
    #[error("invalid repository path '{path}': {reason}")]
    InvalidRepoPath {
        /// Original path string.
        path: String,
        /// Explanation of the validation failure.
        reason: &'static str,
    },

    /// Filesystem or I/O error encountered during platform path or environment setup.
    #[error("platform I/O error: {0}")]
    Io(#[from] std::io::Error),
}
