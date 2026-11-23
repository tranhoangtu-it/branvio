//! Cross-platform OAuth/PAT secure storage backed by the OS native keyring (`keyring`).

use crate::traits::{ForgeError, ForgeKind, ForgeResult, TokenVault};
use std::collections::HashMap;
use std::sync::RwLock;

fn service_key(kind: ForgeKind, host: &str) -> String {
    let kind_slug = match kind {
        ForgeKind::GitHub => "github",
        ForgeKind::GitLab => "gitlab",
        ForgeKind::Bitbucket => "bitbucket",
        ForgeKind::Gitea => "gitea",
    };
    let clean_host = host.trim().to_ascii_lowercase();
    format!("branvio.{kind_slug}.{clean_host}")
}

/// Production [`TokenVault`] storing tokens inside macOS Keychain, Windows Credential Manager,
/// or Linux Secret Service via `keyring`.
#[derive(Debug, Default)]
pub struct OsKeyringVault;

impl OsKeyringVault {
    /// Creates an OS keyring-backed token vault.
    pub fn new() -> Self {
        Self
    }
}

impl TokenVault for OsKeyringVault {
    fn store_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
        token: &str,
    ) -> ForgeResult<()> {
        let entry = keyring::Entry::new(&service_key(kind, host), account)
            .map_err(|e| ForgeError::Keyring(e.to_string()))?;
        entry
            .set_password(token)
            .map_err(|e| ForgeError::Keyring(e.to_string()))
    }

    fn load_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
    ) -> ForgeResult<Option<String>> {
        let entry = keyring::Entry::new(&service_key(kind, host), account)
            .map_err(|e| ForgeError::Keyring(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(ForgeError::Keyring(err.to_string())),
        }
    }

    fn delete_token(&self, kind: ForgeKind, host: &str, account: &str) -> ForgeResult<()> {
        let entry = keyring::Entry::new(&service_key(kind, host), account)
            .map_err(|e| ForgeError::Keyring(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(ForgeError::Keyring(err.to_string())),
        }
    }
}

/// Thread-safe in-memory [`TokenVault`] useful for unit/integration tests and ephemeral sessions.
///
/// `Debug` is implemented by hand so tokens are never printed (only the entry count).
#[derive(Default)]
pub struct InMemoryTokenVault {
    entries: RwLock<HashMap<(ForgeKind, String, String), String>>,
}

impl std::fmt::Debug for InMemoryTokenVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map(|g| g.len()).ok();
        f.debug_struct("InMemoryTokenVault")
            .field("entries", &count)
            .finish_non_exhaustive()
    }
}

impl InMemoryTokenVault {
    /// Creates an empty in-memory token vault.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenVault for InMemoryTokenVault {
    fn store_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
        token: &str,
    ) -> ForgeResult<()> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| ForgeError::Keyring("in-memory vault lock poisoned".to_string()))?;
        guard.insert(
            (kind, host.trim().to_ascii_lowercase(), account.to_string()),
            token.to_string(),
        );
        Ok(())
    }

    fn load_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
    ) -> ForgeResult<Option<String>> {
        let guard = self
            .entries
            .read()
            .map_err(|_| ForgeError::Keyring("in-memory vault lock poisoned".to_string()))?;
        Ok(guard
            .get(&(kind, host.trim().to_ascii_lowercase(), account.to_string()))
            .cloned())
    }

    fn delete_token(&self, kind: ForgeKind, host: &str, account: &str) -> ForgeResult<()> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| ForgeError::Keyring("in-memory vault lock poisoned".to_string()))?;
        guard.remove(&(kind, host.trim().to_ascii_lowercase(), account.to_string()));
        Ok(())
    }
}
