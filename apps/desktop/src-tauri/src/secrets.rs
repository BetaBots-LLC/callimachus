//! API-key storage in the OS credential store — macOS Keychain, Windows Credential
//! Manager, or the Linux Secret Service — via the cross-platform `keyring` crate.
//! Keys never touch the SQLite DB or any config file. Each provider's key is stored
//! under service "callimachus", account = the provider name.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const SERVICE: &str = "callimachus";

/// In-memory cache of resolved keys (provider -> Some(key) | None), so we hit the OS
/// keychain at most ONCE per provider per process. Without this, has_key/pick_synth
/// poll the keychain on every UI query — which on macOS re-prompts for access every
/// time (especially in dev, where each rebuild is a "new" app). Writes keep it in sync.
static CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn entry(provider: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, provider).context("opening credential-store entry")
}

/// Store (or replace) the API key for a provider.
pub fn set_key(provider: &str, key: &str) -> Result<()> {
    entry(provider)?.set_password(key).context("writing key")?;
    if let Ok(mut c) = CACHE.lock() {
        c.insert(provider.to_string(), Some(key.to_string()));
    }
    Ok(())
}

/// Fetch a provider's API key, or None if not set. Cached in memory after the first read.
pub fn get_key(provider: &str) -> Result<Option<String>> {
    if let Ok(c) = CACHE.lock() {
        if let Some(cached) = c.get(provider) {
            return Ok(cached.clone());
        }
    }
    let val = match entry(provider)?.get_password() {
        Ok(k) => Some(k),
        Err(keyring::Error::NoEntry) => None,
        // Access denied / cancelled / locked keychain (NOT "no entry"): treat as "no key this
        // session" and CACHE it, so we respect the user's Deny and never re-prompt in a loop.
        // Previously this returned an error without caching, so every has_key re-read the
        // keychain and macOS re-prompted on each click. Restart the app to retry after a deny.
        Err(_) => None,
    };
    if let Ok(mut c) = CACHE.lock() {
        c.insert(provider.to_string(), val.clone());
    }
    Ok(val)
}

/// Remove a provider's API key (no error if it was absent).
pub fn delete_key(provider: &str) -> Result<()> {
    let res = match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context("deleting key")),
    };
    if let Ok(mut c) = CACHE.lock() {
        c.insert(provider.to_string(), None);
    }
    res
}

/// Whether a key exists for a provider (without returning it).
pub fn has_key(provider: &str) -> bool {
    matches!(get_key(provider), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real credential-store roundtrip (writes to the OS keychain). Ignored by
    /// default: `cargo test -- --ignored keychain_roundtrip`
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let provider = "callimachus_test_provider";
        set_key(provider, "sk-test-123").unwrap();
        assert_eq!(get_key(provider).unwrap().as_deref(), Some("sk-test-123"));
        assert!(has_key(provider));
        delete_key(provider).unwrap();
        assert_eq!(get_key(provider).unwrap(), None);
    }
}
