//! Lossless conversions between [`branvio_core::ids::GitOid`], `gix::ObjectId`, and `git2::Oid`.

use crate::error::{GitEngineError, GitEngineResult};
use branvio_core::ids::{GitOid, ObjectFormat};

/// Converts a `gix_hash::ObjectId` (SHA-1 or SHA-256) into a stack-allocated [`GitOid`].
pub fn git_oid_from_gix(id: gix::ObjectId) -> GitEngineResult<GitOid> {
    match id {
        gix::ObjectId::Sha1(bytes) => Ok(GitOid::from_sha1_bytes(bytes)),
        gix::ObjectId::Sha256(bytes) => Ok(GitOid::from_sha256_bytes(bytes)),
        _ => Err(GitEngineError::GixRead {
            operation: "oid_conversion",
            message: "unsupported git object hash format".to_string(),
        }),
    }
}

/// Converts a [`GitOid`] into a `gix::ObjectId`.
pub fn git_oid_to_gix(oid: GitOid) -> GitEngineResult<gix::ObjectId> {
    if oid.is_virtual_wip() {
        return Err(GitEngineError::GixRead {
            operation: "oid_conversion",
            message: "cannot pass VIRTUAL_WIP sentinel OID to gitoxide".to_string(),
        });
    }
    match oid.format() {
        ObjectFormat::Sha1 => {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(oid.as_bytes());
            Ok(gix::ObjectId::Sha1(arr))
        }
        ObjectFormat::Sha256 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(oid.as_bytes());
            Ok(gix::ObjectId::Sha256(arr))
        }
    }
}

/// Converts a `git2::Oid` into a [`GitOid`].
pub fn git_oid_from_git2(id: git2::Oid) -> GitEngineResult<GitOid> {
    let raw = id.as_bytes();
    match raw.len() {
        20 => {
            let mut arr = [0u8; 20];
            arr.copy_from_slice(raw);
            Ok(GitOid::from_sha1_bytes(arr))
        }
        32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(raw);
            Ok(GitOid::from_sha256_bytes(arr))
        }
        other => Err(GitEngineError::IndexOp {
            operation: "oid_conversion",
            message: format!("unexpected git2::Oid digest length {other}"),
        }),
    }
}

/// Converts a [`GitOid`] into a `git2::Oid`.
pub fn git_oid_to_git2(oid: GitOid) -> GitEngineResult<git2::Oid> {
    if oid.is_virtual_wip() {
        return Err(GitEngineError::IndexOp {
            operation: "oid_conversion",
            message: "cannot pass VIRTUAL_WIP sentinel OID to git2".to_string(),
        });
    }
    git2::Oid::from_bytes(oid.as_bytes()).map_err(|err| GitEngineError::IndexOp {
        operation: "oid_conversion",
        message: err.to_string(),
    })
}
