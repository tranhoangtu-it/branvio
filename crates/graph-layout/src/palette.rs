//! Deterministic high-contrast Metro-line color palette and branch name hashing.

use branvio_core::domain::DeterministicColor;
use branvio_core::ids::GitOid;

/// 16 perceptual high-contrast sRGB Metro colors calibrated for readability on dark & light themes.
///
/// Slot `0` is reserved for primary trunk (`main` / `master`), slot `1` for `develop`,
/// slot `2` for `release/*`, slot `3` for `hotfix/*`, and slots `4..16` for arbitrary feature branches.
pub const METRO_PALETTE: [DeterministicColor; 16] = [
    DeterministicColor {
        r: 0x38,
        g: 0xBD,
        b: 0xF8,
        palette_slot: 0,
    }, // 0: Sky Cyan (main/master)
    DeterministicColor {
        r: 0xA8,
        g: 0x55,
        b: 0xF7,
        palette_slot: 1,
    }, // 1: Electric Violet (develop)
    DeterministicColor {
        r: 0x34,
        g: 0xD3,
        b: 0x99,
        palette_slot: 2,
    }, // 2: Emerald (release/*)
    DeterministicColor {
        r: 0xF8,
        g: 0x71,
        b: 0x71,
        palette_slot: 3,
    }, // 3: Coral Red (hotfix/*)
    DeterministicColor {
        r: 0xFB,
        g: 0xBF,
        b: 0x24,
        palette_slot: 4,
    }, // 4: Amber Gold
    DeterministicColor {
        r: 0xF4,
        g: 0x72,
        b: 0xB6,
        palette_slot: 5,
    }, // 5: Flamingo Pink
    DeterministicColor {
        r: 0x60,
        g: 0xA5,
        b: 0xFA,
        palette_slot: 6,
    }, // 6: Cobalt Blue
    DeterministicColor {
        r: 0x2D,
        g: 0xD4,
        b: 0xBF,
        palette_slot: 7,
    }, // 7: Teal Mint
    DeterministicColor {
        r: 0xFB,
        g: 0x92,
        b: 0x3C,
        palette_slot: 8,
    }, // 8: Tangerine
    DeterministicColor {
        r: 0xA3,
        g: 0xE6,
        b: 0x35,
        palette_slot: 9,
    }, // 9: Lime
    DeterministicColor {
        r: 0xC0,
        g: 0x84,
        b: 0xFC,
        palette_slot: 10,
    }, // 10: Lavender
    DeterministicColor {
        r: 0x22,
        g: 0xD3,
        b: 0xEE,
        palette_slot: 11,
    }, // 11: Aqua
    DeterministicColor {
        r: 0xE8,
        g: 0x79,
        b: 0xF9,
        palette_slot: 12,
    }, // 12: Magenta
    DeterministicColor {
        r: 0x81,
        g: 0x8C,
        b: 0xF8,
        palette_slot: 13,
    }, // 13: Indigo
    DeterministicColor {
        r: 0x4A,
        g: 0xDE,
        b: 0x80,
        palette_slot: 14,
    }, // 14: Spring Green
    DeterministicColor {
        r: 0xFD,
        g: 0xE0,
        b: 0x47,
        palette_slot: 15,
    }, // 15: Canary Yellow
];

/// Returns `true` if `branch_short_name` represents a repository's primary integration trunk.
pub fn is_primary_trunk_branch(branch_short_name: &str) -> bool {
    let bare = strip_remote_prefix(branch_short_name);
    matches!(bare, "main" | "master" | "trunk")
}

/// Computes a deterministic color for any branch name using canonical overrides for trunk/release/hotfix
/// and FNV-1a 64-bit hashing across the remaining 12 palette slots (`4..16`).
///
/// Crucially, `origin/feature/foo` hashes identically to `feature/foo` so local and remote
/// tracking branches visually unify onto a coherent metro color.
pub fn deterministic_branch_color(branch_name: &str) -> DeterministicColor {
    let bare = strip_remote_prefix(branch_name);
    if matches!(bare, "main" | "master" | "trunk") {
        return METRO_PALETTE[0];
    }
    if bare == "develop" || bare == "dev" {
        return METRO_PALETTE[1];
    }
    if bare.starts_with("release/") || bare.starts_with("release-") {
        return METRO_PALETTE[2];
    }
    if bare.starts_with("hotfix/") || bare.starts_with("hotfix-") {
        return METRO_PALETTE[3];
    }

    let hash = fnv1a_64(bare.as_bytes());
    let slot = 4 + ((hash as usize) % (METRO_PALETTE.len() - 4));
    METRO_PALETTE[slot]
}

/// Resolves a compact `u8` palette slot index back to a full [`DeterministicColor`].
#[inline]
pub fn palette_color(slot: u8) -> DeterministicColor {
    METRO_PALETTE[(slot as usize) % METRO_PALETTE.len()]
}

/// Computes a deterministic color for an anonymous secondary merge parent lane when no branch
/// reference points directly to that parent commit.
///
/// Uses pure FNV-1a hashing over `parent_oid.as_bytes()` mapped onto `METRO_PALETTE[4..16]`,
/// guaranteeing complete determinism without inspecting commit messages.
pub fn deterministic_anonymous_lane_color(parent_oid: GitOid) -> DeterministicColor {
    let hash = fnv1a_64(parent_oid.as_bytes());
    let slot = 4 + ((hash as usize) % (METRO_PALETTE.len() - 4));
    METRO_PALETTE[slot]
}

fn strip_remote_prefix(name: &str) -> &str {
    let no_refs = name
        .strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name);

    if name.starts_with("refs/remotes/") {
        no_refs
            .split_once('/')
            .map_or(no_refs, |(_, branch)| branch)
    } else if let Some(rest) = no_refs.strip_prefix("origin/") {
        rest
    } else {
        no_refs
    }
}

#[inline]
const fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(PRIME);
        i += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_and_tracking_branches_keep_stable_colors() {
        assert_eq!(deterministic_branch_color("main"), METRO_PALETTE[0]);
        assert_eq!(
            deterministic_branch_color("refs/remotes/origin/main"),
            METRO_PALETTE[0]
        );
        assert_eq!(deterministic_branch_color("develop"), METRO_PALETTE[1]);
        assert_eq!(deterministic_branch_color("release/v2.0"), METRO_PALETTE[2]);
        assert_eq!(
            deterministic_branch_color("hotfix/login-crash"),
            METRO_PALETTE[3]
        );

        let local = deterministic_branch_color("refs/heads/feat/gpui-graph");
        let remote = deterministic_branch_color("refs/remotes/origin/feat/gpui-graph");
        assert_eq!(local, remote);
    }

    #[test]
    fn anonymous_merge_lane_color_hashes_oid_into_feature_slots() {
        let sha1 = GitOid::from_sha1_bytes([0xAB; 20]);
        let sha256 = GitOid::from_sha256_bytes([0x42; 32]);

        let c1 = deterministic_anonymous_lane_color(sha1);
        let c1_repeat = deterministic_anonymous_lane_color(sha1);
        let c2 = deterministic_anonymous_lane_color(sha256);

        assert_eq!(c1, c1_repeat);
        assert!((4..16).contains(&c1.palette_slot));
        assert!((4..16).contains(&c2.palette_slot));
        assert_eq!(palette_color(c1.palette_slot), c1);
    }
}
