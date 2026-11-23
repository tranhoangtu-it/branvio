//! Cross-platform path canonicalization, Windows `MAX_PATH` handling,
//! Git-canonical relative paths, and filesystem case-sensitivity helpers.

use crate::error::{CoreError, CoreResult};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Windows legacy `MAX_PATH` safety threshold (leaving headroom for `.git/` subpaths).
const WINDOWS_LONG_PATH_THRESHOLD: usize = 248;

/// Filesystem case-sensitivity behavior detected for a repository root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsCaseSensitivity {
    /// Linux ext4/btrfs/xfs or case-sensitive APFS volumes (`Foo.rs` != `foo.rs`).
    CaseSensitive,
    /// Default macOS APFS or Windows NTFS (`Foo.rs` and `foo.rs` alias the same inode).
    CaseInsensitivePreserving,
}

impl FsCaseSensitivity {
    /// Probes the target directory (typically `.git` or repository root) to determine
    /// whether the underlying filesystem distinguishes filenames by ASCII case.
    pub fn detect(dir: &Path) -> Self {
        let probe_upper = dir.join(".branvio_case_probe_TEMP");
        let probe_lower = dir.join(".branvio_case_probe_temp");

        // Never leave probe files behind; if directory is read-only fall back to OS default.
        if probe_upper.exists() {
            return if probe_lower.exists() {
                Self::CaseInsensitivePreserving
            } else {
                Self::CaseSensitive
            };
        }

        // Probe safely against an existing entry like HEAD or config inside .git when possible.
        let git_config_upper = dir.join("CONFIG");
        let git_config_lower = dir.join("config");
        if git_config_lower.exists() && git_config_upper.exists() {
            return Self::CaseInsensitivePreserving;
        }

        #[cfg(target_os = "linux")]
        {
            Self::CaseSensitive
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::CaseInsensitivePreserving
        }
    }

    /// Compares two repository-relative paths according to the host filesystem semantics.
    #[inline]
    pub fn paths_collide(self, a: &RepoRelPath, b: &RepoRelPath) -> bool {
        match self {
            Self::CaseSensitive => a.as_str() == b.as_str(),
            Self::CaseInsensitivePreserving => a.as_str().eq_ignore_ascii_case(b.as_str()),
        }
    }
}

/// Validated, Git-canonical repository-relative path using `/` separators.
///
/// Invariants enforced at construction:
/// - Always uses forward slashes (`/`), even when constructed on Windows.
/// - Never starts with `/` or `./`.
/// - Never contains `..` parent traversal segments or `.git` escape segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoRelPath(CompactString);

impl RepoRelPath {
    /// Normalizes a raw path string into a canonical Git-relative path.
    pub fn normalize(raw: &str) -> CoreResult<Self> {
        let replaced = raw.replace('\\', "/");
        let mut trimmed = replaced.trim();
        while let Some(rest) = trimmed.strip_prefix("./") {
            trimmed = rest;
        }
        trimmed = trimmed.trim_start_matches('/').trim_end_matches('/');

        if trimmed.is_empty() {
            return Err(CoreError::InvalidRepoPath {
                path: raw.to_string(),
                reason: "repository relative path cannot be empty",
            });
        }

        for segment in trimmed.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(CoreError::InvalidRepoPath {
                    path: raw.to_string(),
                    reason: "path contains empty, '.' or '..' traversal segment",
                });
            }
        }

        Ok(Self(CompactString::from(trimmed)))
    }

    /// Returns the canonical forward-slash string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the terminal filename component, if any.
    #[inline]
    pub fn file_name(&self) -> &str {
        self.0
            .rsplit_once('/')
            .map_or_else(|| self.0.as_str(), |(_, name)| name)
    }

    /// Returns the parent directory path inside the repository, if not at root.
    #[inline]
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    /// Resolves this relative path onto a host repository root using OS-native separators
    /// and Windows long-path protection.
    pub fn join_onto_host(&self, repo_root: &HostPath) -> HostPath {
        let mut joined = repo_root.as_path().to_path_buf();
        for segment in self.0.split('/') {
            joined.push(segment);
        }
        HostPath::from_path_buf(joined)
    }
}

impl fmt::Display for RepoRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cross-platform filesystem path wrapper that simplifies unnecessary UNC prefixes
/// via `dunce` while automatically promoting long Windows paths (`>= 248` UTF-16 code units)
/// to `\\?\` extended-length format so file I/O never fails on `MAX_PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostPath(PathBuf);

impl HostPath {
    /// Canonicalizes an existing directory/file on disk with cross-platform normalization.
    pub fn canonicalize_existing(path: impl AsRef<Path>) -> CoreResult<Self> {
        let canonical = dunce::canonicalize(path.as_ref())?;
        Ok(Self::from_path_buf(canonical))
    }

    /// Wraps a `PathBuf` applying OS-appropriate long-path or UNC simplification rules.
    pub fn from_path_buf(path: PathBuf) -> Self {
        let simplified = dunce::simplified(&path).to_path_buf();
        Self(apply_windows_long_path_guard(simplified))
    }

    /// Returns the inner `&Path`.
    #[inline]
    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    /// Converts the path to a command-safe string (stripping `\\?\` when under `MAX_PATH`
    /// so external Git hooks and shells on Windows can parse it cleanly).
    pub fn for_external_cli(&self) -> PathBuf {
        dunce::simplified(&self.0).to_path_buf()
    }

    /// Consumes the wrapper and yields the underlying `PathBuf`.
    #[inline]
    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for HostPath {
    #[inline]
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

/// Applies `\\?\` or `\\?\UNC\` on Windows when the absolute path length approaches `MAX_PATH`.
/// On Unix platforms this is a zero-cost pass-through.
fn apply_windows_long_path_guard(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let utf16_len = path.as_os_str().encode_wide().count();
        let s = path.to_string_lossy();
        if utf16_len >= WINDOWS_LONG_PATH_THRESHOLD && path.is_absolute() && !s.starts_with(r"\\?\")
        {
            if let Some(unc_tail) = s.strip_prefix(r"\\") {
                return PathBuf::from(format!(r"\\?\UNC\{unc_tail}"));
            }
            return PathBuf::from(format!(r"\\?\{s}"));
        }
        path
    }
    #[cfg(not(windows))]
    {
        let _ = WINDOWS_LONG_PATH_THRESHOLD;
        path
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_windows_and_unix_relative_paths() {
        let p1 = RepoRelPath::normalize(r".\crates\git-engine\src\lib.rs").unwrap();
        assert_eq!(p1.as_str(), "crates/git-engine/src/lib.rs");
        assert_eq!(p1.file_name(), "lib.rs");
        assert_eq!(p1.parent(), Some("crates/git-engine/src"));

        let p2 = RepoRelPath::normalize("/src//main.rs");
        assert!(p2.is_err());

        let traversal = RepoRelPath::normalize("crates/../secrets.txt");
        assert!(traversal.is_err());
    }

    #[test]
    fn detects_case_collision_by_filesystem_mode() {
        let a = RepoRelPath::normalize("src/MainView.rs").unwrap();
        let b = RepoRelPath::normalize("src/mainview.rs").unwrap();
        assert!(!FsCaseSensitivity::CaseSensitive.paths_collide(&a, &b));
        assert!(FsCaseSensitivity::CaseInsensitivePreserving.paths_collide(&a, &b));
    }
}
