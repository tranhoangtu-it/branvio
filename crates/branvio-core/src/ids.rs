//! Compact stack-allocated identifiers for Git objects, repositories, and MVCC epochs.

use crate::error::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported Git object hash formats (SHA-1 legacy and SHA-256).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum ObjectFormat {
    /// Standard 20-byte (40 hex chars) SHA-1 object ID.
    Sha1 = 20,
    /// Modern 32-byte (64 hex chars) SHA-256 object ID.
    Sha256 = 32,
}

impl ObjectFormat {
    /// Returns the raw digest length in bytes (20 for SHA-1, 32 for SHA-256).
    #[inline]
    pub const fn byte_len(self) -> usize {
        self as usize
    }

    /// Returns the hexadecimal string length (40 for SHA-1, 64 for SHA-256).
    #[inline]
    pub const fn hex_len(self) -> usize {
        self.byte_len() * 2
    }
}

/// Stack-allocated Git Object ID supporting both SHA-1 and SHA-256, plus a sentinel
/// [`GitOid::VIRTUAL_WIP`] value for uncommitted working-tree nodes in the commit graph.
///
/// Size: 33 bytes (zero heap allocations, `Copy`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GitOid {
    bytes: [u8; 32],
    format: ObjectFormat,
}

impl GitOid {
    /// Sentinel OID representing the synthetic WIP (Work-In-Progress) node at the top of `HEAD`.
    pub const VIRTUAL_WIP: Self = Self {
        bytes: [0u8; 32],
        format: ObjectFormat::Sha1,
    };

    /// Sentinel OID representing a SHA-256 synthetic WIP node.
    pub const VIRTUAL_WIP_SHA256: Self = Self {
        bytes: [0u8; 32],
        format: ObjectFormat::Sha256,
    };

    /// Constructs a [`GitOid`] from raw 20-byte SHA-1 digest.
    #[inline]
    pub const fn from_sha1_bytes(digest: [u8; 20]) -> Self {
        let mut bytes = [0u8; 32];
        let mut i = 0;
        while i < 20 {
            bytes[i] = digest[i];
            i += 1;
        }
        Self {
            bytes,
            format: ObjectFormat::Sha1,
        }
    }

    /// Constructs a [`GitOid`] from raw 32-byte SHA-256 digest.
    #[inline]
    pub const fn from_sha256_bytes(digest: [u8; 32]) -> Self {
        Self {
            bytes: digest,
            format: ObjectFormat::Sha256,
        }
    }

    /// Parses a hexadecimal SHA-1 (40 chars) or SHA-256 (64 chars) OID string.
    pub fn from_hex(hex: &str) -> CoreResult<Self> {
        let trimmed = hex.trim();
        let format = match trimmed.len() {
            40 => ObjectFormat::Sha1,
            64 => ObjectFormat::Sha256,
            _ => {
                return Err(CoreError::InvalidOid {
                    input: trimmed.to_string(),
                    reason: "expected 40 (SHA-1) or 64 (SHA-256) hexadecimal characters",
                });
            }
        };

        let mut bytes = [0u8; 32];
        let raw = trimmed.as_bytes();
        for (idx, pair) in raw.chunks_exact(2).enumerate() {
            let hi = decode_hex_nibble(pair[0]).ok_or_else(|| CoreError::InvalidOid {
                input: trimmed.to_string(),
                reason: "contains non-hexadecimal character",
            })?;
            let lo = decode_hex_nibble(pair[1]).ok_or_else(|| CoreError::InvalidOid {
                input: trimmed.to_string(),
                reason: "contains non-hexadecimal character",
            })?;
            bytes[idx] = (hi << 4) | lo;
        }

        Ok(Self { bytes, format })
    }

    /// Returns the object hash format.
    #[inline]
    pub const fn format(self) -> ObjectFormat {
        self.format
    }

    /// Returns the active digest slice (20 bytes for SHA-1, 32 bytes for SHA-256).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.format.byte_len()]
    }

    /// Returns `true` if this OID represents the synthetic WIP graph node.
    #[inline]
    pub fn is_virtual_wip(&self) -> bool {
        self.bytes == [0u8; 32]
    }

    /// Formats the full lowercase hexadecimal representation.
    pub fn to_hex(&self) -> String {
        let slice = self.as_bytes();
        let mut out = String::with_capacity(slice.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for &b in slice {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    /// Formats an abbreviated hexadecimal prefix (typically 7..=12 characters).
    pub fn short_hex(&self, chars: usize) -> String {
        let full = self.to_hex();
        let take = chars.min(full.len());
        full[..take].to_string()
    }
}

#[inline]
const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_virtual_wip() {
            write!(f, "GitOid(VIRTUAL_WIP)")
        } else {
            write!(f, "GitOid({})", self.short_hex(10))
        }
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_virtual_wip() {
            f.write_str("WIP")
        } else {
            f.write_str(&self.to_hex())
        }
    }
}

/// Stable identifier for an open repository tab inside Branvio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoId(pub u64);

/// Monotonic generation counter for lock-free MVCC repository snapshots.
///
/// Every reactive state refresh increments [`RepoEpoch`]. Background tasks carrying
/// an epoch lower than or equal to the latest committed epoch are rejected atomically.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub struct RepoEpoch(pub u64);

impl RepoEpoch {
    /// Returns the next monotonically increasing epoch.
    #[inline]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha1_and_sha256_roundtrip() {
        let sha1_hex = "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";
        let oid = GitOid::from_hex(sha1_hex).unwrap();
        assert_eq!(oid.format(), ObjectFormat::Sha1);
        assert_eq!(oid.as_bytes().len(), 20);
        assert_eq!(oid.to_hex(), sha1_hex);
        assert_eq!(oid.short_hex(7), "a94a8fe");
        assert!(!oid.is_virtual_wip());

        let sha256_hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let oid256 = GitOid::from_hex(sha256_hex).unwrap();
        assert_eq!(oid256.format(), ObjectFormat::Sha256);
        assert_eq!(oid256.as_bytes().len(), 32);
        assert_eq!(oid256.to_hex(), sha256_hex);
    }

    #[test]
    fn rejects_invalid_oid_lengths_and_chars() {
        assert!(GitOid::from_hex("1234").is_err());
        let bad_char = "z94a8fe5ccb19ba61c4c0873d391e987982fbbd3";
        assert!(GitOid::from_hex(bad_char).is_err());
        assert!(GitOid::VIRTUAL_WIP.is_virtual_wip());
    }
}
