//! API-key storage in the OS credential store — macOS Keychain, Windows Credential
//! Manager, or the Linux Secret Service — via the cross-platform `keyring` crate.
//! Keys never touch the SQLite DB or any config file. Each provider's key is stored
//! under service "callimachus", account = the provider name.

use anyhow::{Context, Result};

const SERVICE: &str = "callimachus";

fn entry(provider: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, provider).context("opening credential-store entry")
}

/// Store (or replace) the API key for a provider.
pub fn set_key(provider: &str, key: &str) -> Result<()> {
    entry(provider)?.set_password(key).context("writing key")
}

/// Fetch a provider's API key, or None if not set.
pub fn get_key(provider: &str) -> Result<Option<String>> {
    match entry(provider)?.get_password() {
        Ok(k) => Ok(Some(k)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context("reading key")),
    }
}

/// Remove a provider's API key (no error if it was absent).
pub fn delete_key(provider: &str) -> Result<()> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context("deleting key")),
    }
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
