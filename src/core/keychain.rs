//! The *storage* half of the SSH credential vault: the [`CredentialStore`]
//! trait, its OS-keychain backend and the in-memory test double.
//!
//! The naming half — [`CredentialKind`], [`CredentialRef`], [`endpoint_account`]
//! and the two service constants — lives one crate down in
//! `tty7_core::core::keychain` and is re-exported here, so every call site keeps
//! using `crate::core::keychain::…` for both halves.
//!
//! **Why the split.** `tty7-core` also builds the headless `tty7-server`, a
//! static binary meant to be small enough to push onto an arbitrary box. That
//! machine has no OS keychain and nothing in `tty7-core` ever reads a secret —
//! the daemon receives secrets already resolved by the GUI (see
//! `daemon::protocol`'s `NativeSshSpec`). Leaving `keyring` in the core manifest
//! made the server link `zbus` / `secret-service` and thirty-odd crates behind
//! them for code it can never call. So the store moved up here, where its callers
//! already were (`ui::ssh_prompt`, `ui::ssh_connect`, `ui::settings`, `ui::app`).
//!
//! Secrets are never logged. The typed helpers below deliberately keep secret
//! values out of `Debug`/log output.

pub use tty7_core::core::keychain::{
    CredentialKind, CredentialRef, SERVICE_KEY_PASSPHRASE, SERVICE_PASSWORD, endpoint_account,
    key_account_from_contents,
};

/// A backend failure while talking to the credential store. Intentionally never
/// carries a secret value — only a human-readable reason from the backend.
#[derive(Debug)]
pub enum CredentialError {
    /// The underlying store failed (keychain locked, access denied, IO error).
    Backend(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Backend(reason) => write!(f, "credential store error: {reason}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Result alias for credential-store operations.
pub type CredentialResult<T> = Result<T, CredentialError>;

/// A secret store keyed by `(service, account)`. Implementors talk to a real OS
/// keychain or an in-memory map.
///
/// Contract:
/// - `get` returns `Ok(None)` when the entry is absent (not an error).
/// - `delete` is idempotent: deleting an absent entry returns `Ok(())`.
/// - Implementors must never log secret values.
pub trait CredentialStore: Send + Sync {
    /// Fetch the secret for `(service, account)`, or `Ok(None)` if absent.
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>>;

    /// Store `secret` under `(service, account)`, overwriting any existing value.
    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()>;

    /// Remove the entry at `(service, account)`. Absent entry ⇒ `Ok(())`.
    fn delete(&self, service: &str, account: &str) -> CredentialResult<()>;

    // ── Typed endpoint/key helpers (default methods over get/set/delete) ──────

    /// The stored password for an endpoint, if any.
    fn password_for(&self, user: &str, host: &str, port: u16) -> CredentialResult<Option<String>> {
        self.get(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

    /// Store a password for an endpoint and return the [`CredentialRef`] naming it.
    fn set_password(
        &self,
        user: &str,
        host: &str,
        port: u16,
        secret: &str,
    ) -> CredentialResult<CredentialRef> {
        let account = endpoint_account(user, host, port);
        self.set(SERVICE_PASSWORD, &account, secret)?;
        Ok(CredentialRef {
            kind: CredentialKind::Password,
            account,
        })
    }

    /// Delete the stored password for an endpoint (idempotent).
    fn delete_password(&self, user: &str, host: &str, port: u16) -> CredentialResult<()> {
        self.delete(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

    /// The stored passphrase for a private key (keyed by its sha512-hex), if any.
    fn passphrase_for_key(&self, key_sha512_hex: &str) -> CredentialResult<Option<String>> {
        self.get(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    /// Store a passphrase for a private key and return the [`CredentialRef`].
    fn set_key_passphrase(
        &self,
        key_sha512_hex: &str,
        secret: &str,
    ) -> CredentialResult<CredentialRef> {
        self.set(SERVICE_KEY_PASSPHRASE, key_sha512_hex, secret)?;
        Ok(CredentialRef::key_passphrase(key_sha512_hex.to_string()))
    }

    // The three below are unused outside tests today. Unlike `tty7-core`, this is a
    // *binary* crate, where `pub` does not escape and `dead_code` therefore fires
    // on them; they are kept because the trait's five verbs (`password_*`,
    // `*_key_passphrase`, `*_ref`) only make sense as a set — a store you can
    // write a ref to but not read one back from is a trap for the next caller.
    /// Delete the stored passphrase for a private key (idempotent).
    #[allow(dead_code)]
    fn delete_key_passphrase(&self, key_sha512_hex: &str) -> CredentialResult<()> {
        self.delete(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    /// Resolve a [`CredentialRef`] to its secret, or `Ok(None)` if absent.
    #[allow(dead_code)]
    fn get_ref(&self, cref: &CredentialRef) -> CredentialResult<Option<String>> {
        self.get(cref.service(), &cref.account)
    }

    /// Delete the entry a [`CredentialRef`] names (idempotent).
    #[allow(dead_code)]
    fn delete_ref(&self, cref: &CredentialRef) -> CredentialResult<()> {
        self.delete(cref.service(), &cref.account)
    }
}

/// The production store backed by the OS keychain via the `keyring` crate.
///
/// `keyring` 4.x's default `v1` feature auto-selects the platform store on first
/// use, so this needs no per-platform wiring. A missing entry surfaces as
/// `Ok(None)`; every other failure becomes [`CredentialError::Backend`] with the
/// backend's message (never a secret).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsCredentialStore;

impl CredentialStore for OsCredentialStore {
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CredentialError::Backend(e.to_string())),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|e| CredentialError::Backend(e.to_string()))
    }

    fn delete(&self, service: &str, account: &str) -> CredentialResult<()> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CredentialError::Backend(e.to_string())),
        }
    }
}

/// An in-memory store for tests. Never touches the OS keychain.
///
/// `#[cfg(test)]` because this crate is a binary: a test-only type left in a
/// normal build is dead code here, where in `tty7-core` (a library) `pub` alone
/// kept the lint quiet.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    // Keyed by (service, account). Behind a Mutex so the store is `Sync` and can
    // be shared like the real one.
    entries: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

#[cfg(test)]
impl InMemoryCredentialStore {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries (test introspection).
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("credential store poisoned")
            .len()
    }

    /// Whether the store holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
impl CredentialStore for InMemoryCredentialStore {
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>> {
        let entries = self.entries.lock().expect("credential store poisoned");
        Ok(entries
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()> {
        let mut entries = self.entries.lock().expect("credential store poisoned");
        entries.insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> CredentialResult<()> {
        let mut entries = self.entries.lock().expect("credential store poisoned");
        entries.remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_store_get_set_delete() {
        let store = InMemoryCredentialStore::new();
        assert!(store.is_empty());

        // Absent → None (not an error).
        assert_eq!(store.password_for("deploy", "host", 22).unwrap(), None);

        // Set returns a ref that resolves back to the secret.
        let cref = store.set_password("deploy", "host", 22, "hunter2").unwrap();
        assert_eq!(cref, CredentialRef::password("deploy", "host", 22));
        assert_eq!(store.get_ref(&cref).unwrap().as_deref(), Some("hunter2"));
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("hunter2")
        );

        // Overwrite replaces in place (endpoint keying — one entry per endpoint).
        store.set_password("deploy", "host", 22, "newpass").unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("newpass")
        );

        // Delete is idempotent.
        store.delete_password("deploy", "host", 22).unwrap();
        assert_eq!(store.password_for("deploy", "host", 22).unwrap(), None);
        store.delete_password("deploy", "host", 22).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn key_passphrase_helpers_use_the_key_service() {
        let store = InMemoryCredentialStore::new();
        let key_id = key_account_from_contents(b"encrypted-key-bytes");
        assert_eq!(store.passphrase_for_key(&key_id).unwrap(), None);

        let cref = store.set_key_passphrase(&key_id, "s3cret").unwrap();
        assert_eq!(cref.kind, CredentialKind::KeyPassphrase);
        assert_eq!(cref.service(), "tty7-ssh-key");
        assert_eq!(
            store.passphrase_for_key(&key_id).unwrap().as_deref(),
            Some("s3cret")
        );

        // A password with the same account string does NOT collide (different service).
        store.set_password("deploy", "host", 22, "pw").unwrap();
        assert_eq!(store.len(), 2);

        store.delete_key_passphrase(&key_id).unwrap();
        assert_eq!(store.passphrase_for_key(&key_id).unwrap(), None);
    }
}
