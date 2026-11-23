//! Embedded `redb` persistent storage for Branvio Time-Machine operation journal entries.

use crate::traits::{OpLogError, OpLogResult, OpLogStore};
use branvio_core::domain::{OpLogEntry, RepoStateSnapshot};
use branvio_core::paths::HostPath;
use compact_str::CompactString;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const ENTRIES_TABLE: TableDefinition<'_, [u8; 16], &[u8]> = TableDefinition::new("oplog_entries");
const SEQ_TABLE: TableDefinition<'_, u64, u64> = TableDefinition::new("oplog_seq");

/// Retention cap: at most this many journal entries are kept per repository; the oldest are
/// pruned in [`OpLogStore::record_begin`]. Bounds both the database and the number of pinned
/// worktree snapshot refs (`refs/branvio/undo/...`), which the undo engine garbage-collects
/// once their entry is gone.
pub const MAX_ENTRIES_PER_REPO: usize = 50;

/// Computes a deterministic 64-bit FNV-1a repository identifier from a canonicalized root path.
pub fn compute_repo_root_hash(repo_root: &HostPath) -> u64 {
    let canonical = std::fs::canonicalize(repo_root.as_path())
        .unwrap_or_else(|_| repo_root.as_path().to_path_buf());
    let lossy = canonical.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in lossy.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn entry_key(repo_root_hash: u64, op_id: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&repo_root_hash.to_be_bytes());
    key[8..].copy_from_slice(&op_id.to_be_bytes());
    key
}

fn repo_key_bounds(repo_root_hash: u64) -> ([u8; 16], [u8; 16]) {
    (
        entry_key(repo_root_hash, 0),
        entry_key(repo_root_hash, u64::MAX),
    )
}

fn default_oplog_db_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("branvio")
                .join("oplog.redb");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("branvio").join("oplog.redb");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            return PathBuf::from(xdg).join("branvio").join("oplog.redb");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("branvio")
                .join("oplog.redb");
        }
    }
    std::env::temp_dir().join("branvio").join("oplog.redb")
}

/// Embedded transactional `redb` store indexing per-repository operation history cleanly across restarts.
pub struct RedbOpLogStore {
    db: Database,
}

impl RedbOpLogStore {
    /// Opens or creates the `redb` operation log database at the OS default data directory.
    pub fn open_default() -> OpLogResult<Self> {
        Self::open_at(default_oplog_db_path())
    }

    /// Opens or creates a `redb` operation log database at an explicit filesystem path.
    pub fn open_at(path: impl AsRef<Path>) -> OpLogResult<Self> {
        let path_ref = path.as_ref();
        if let Some(parent) = path_ref.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| OpLogError::Storage(e.to_string()))?;
        }
        let db = Database::create(path_ref).map_err(|e| OpLogError::Storage(e.to_string()))?;
        let wtx = db
            .begin_write()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        {
            let _ = wtx
                .open_table(ENTRIES_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            let _ = wtx
                .open_table(SEQ_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        Ok(Self { db })
    }

    /// Returns `(can_undo, can_redo)` availability flags for `repo_root_hash` in O(entries).
    pub fn availability(&self, repo_root_hash: u64) -> OpLogResult<(bool, bool)> {
        let entries = self.list_recent(repo_root_hash, 256)?;
        let can_undo = entries.iter().any(|e| e.after.is_some() && !e.undone);
        let can_redo = entries.iter().any(|e| e.after.is_some() && e.undone);
        Ok((can_undo, can_redo))
    }
}

impl OpLogStore for RedbOpLogStore {
    fn record_begin(&self, mut entry: OpLogEntry) -> OpLogResult<u64> {
        let repo_hash = entry.repo_root_hash;
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        let next_id = {
            let mut seq_tbl = wtx
                .open_table(SEQ_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            let prev = seq_tbl
                .get(repo_hash)
                .map_err(|e| OpLogError::Storage(e.to_string()))?
                .map_or(0, |v| v.value());
            let next_id = prev.saturating_add(1);
            seq_tbl
                .insert(repo_hash, next_id)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;

            let mut entries_tbl = wtx
                .open_table(ENTRIES_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;

            // New forward mutation invalidates any previously undone redo tail for this repo.
            let (lo, hi) = repo_key_bounds(repo_hash);
            let mut keys_to_prune = Vec::new();
            for row in entries_tbl
                .range(lo..=hi)
                .map_err(|e| OpLogError::Storage(e.to_string()))?
            {
                let (k, v) = row.map_err(|e| OpLogError::Storage(e.to_string()))?;
                if let Ok(existing) = serde_json::from_slice::<OpLogEntry>(v.value())
                    && existing.undone
                {
                    keys_to_prune.push(k.value());
                }
            }
            for k in keys_to_prune {
                entries_tbl
                    .remove(k)
                    .map_err(|e| OpLogError::Storage(e.to_string()))?;
            }

            entry.op_id = next_id;
            entry.undone = false;
            let bytes =
                serde_json::to_vec(&entry).map_err(|e| OpLogError::Storage(e.to_string()))?;
            entries_tbl
                .insert(entry_key(repo_hash, next_id), bytes.as_slice())
                .map_err(|e| OpLogError::Storage(e.to_string()))?;

            // Retention: drop the oldest entries beyond the per-repository cap.
            let mut all_keys = Vec::new();
            for row in entries_tbl
                .range(lo..=hi)
                .map_err(|e| OpLogError::Storage(e.to_string()))?
            {
                let (k, _v) = row.map_err(|e| OpLogError::Storage(e.to_string()))?;
                all_keys.push(k.value());
            }
            let excess = all_keys.len().saturating_sub(MAX_ENTRIES_PER_REPO);
            for k in all_keys.into_iter().take(excess) {
                entries_tbl
                    .remove(k)
                    .map_err(|e| OpLogError::Storage(e.to_string()))?;
            }
            next_id
        };
        wtx.commit()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        Ok(next_id)
    }

    fn record_commit(
        &self,
        repo_root_hash: u64,
        op_id: u64,
        after: RepoStateSnapshot,
        reflog_selector: Option<CompactString>,
    ) -> OpLogResult<()> {
        let key = entry_key(repo_root_hash, op_id);
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        {
            let mut tbl = wtx
                .open_table(ENTRIES_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            let existing_bytes = tbl
                .get(key)
                .map_err(|e| OpLogError::Storage(e.to_string()))?
                .map(|g| g.value().to_vec())
                .ok_or_else(|| OpLogError::Storage(format!("missing oplog entry {op_id}")))?;
            let mut entry: OpLogEntry = serde_json::from_slice(&existing_bytes)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            entry.after = Some(after);
            entry.reflog_selector = reflog_selector;
            entry.undone = false;
            let encoded =
                serde_json::to_vec(&entry).map_err(|e| OpLogError::Storage(e.to_string()))?;
            tbl.insert(key, encoded.as_slice())
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        Ok(())
    }

    fn discard_incomplete(&self, repo_root_hash: u64, op_id: u64) -> OpLogResult<()> {
        let key = entry_key(repo_root_hash, op_id);
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        {
            let mut tbl = wtx
                .open_table(ENTRIES_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            tbl.remove(key)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        Ok(())
    }

    fn list_recent(&self, repo_root_hash: u64, limit: usize) -> OpLogResult<Vec<OpLogEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        let tbl = rtx
            .open_table(ENTRIES_TABLE)
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        let (lo, hi) = repo_key_bounds(repo_root_hash);
        let mut out = Vec::with_capacity(limit.min(128));
        for row in tbl
            .range(lo..=hi)
            .map_err(|e| OpLogError::Storage(e.to_string()))?
            .rev()
        {
            if out.len() >= limit {
                break;
            }
            let (_k, v) = row.map_err(|e| OpLogError::Storage(e.to_string()))?;
            let entry: OpLogEntry = serde_json::from_slice(v.value())
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            out.push(entry);
        }
        Ok(out)
    }

    fn set_undone_state(&self, repo_root_hash: u64, op_id: u64, undone: bool) -> OpLogResult<()> {
        let key = entry_key(repo_root_hash, op_id);
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        {
            let mut tbl = wtx
                .open_table(ENTRIES_TABLE)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            let existing_bytes = tbl
                .get(key)
                .map_err(|e| OpLogError::Storage(e.to_string()))?
                .map(|g| g.value().to_vec())
                .ok_or_else(|| OpLogError::Storage(format!("missing oplog entry {op_id}")))?;
            let mut entry: OpLogEntry = serde_json::from_slice(&existing_bytes)
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
            entry.undone = undone;
            let encoded =
                serde_json::to_vec(&entry).map_err(|e| OpLogError::Storage(e.to_string()))?;
            tbl.insert(key, encoded.as_slice())
                .map_err(|e| OpLogError::Storage(e.to_string()))?;
        }
        wtx.commit()
            .map_err(|e| OpLogError::Storage(e.to_string()))?;
        Ok(())
    }
}
