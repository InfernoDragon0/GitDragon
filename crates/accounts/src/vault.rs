//! Secrets in the OS keychain (service `gitdragon`, user = account id). Fails closed: when the
//! keychain is unavailable there is no plaintext fallback, only a clear error.

use crate::secret::Secret;

pub const SERVICE: &str = "gitdragon";

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error(
        "the system keychain is not available ({0}); GitDragon only stores tokens there, so \
         token sign-in is disabled. Use Git Credential Manager mode instead."
    )]
    Unavailable(String),
    #[error("keychain error: {0}")]
    Other(String),
}

pub trait Vault: Send + Sync {
    fn get(&self, account_id: &str) -> Result<Option<Secret>, VaultError>;
    fn set(&self, account_id: &str, secret: &Secret) -> Result<(), VaultError>;
    /// Deleting a missing entry is not an error.
    fn delete(&self, account_id: &str) -> Result<(), VaultError>;
}

/// The OS keychain via `keyring`.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringVault;

impl KeyringVault {
    fn entry(account_id: &str) -> Result<keyring::Entry, VaultError> {
        keyring::Entry::new(SERVICE, account_id).map_err(map_err)
    }
}

fn map_err(err: keyring::Error) -> VaultError {
    match err {
        keyring::Error::PlatformFailure(e) | keyring::Error::NoStorageAccess(e) => {
            VaultError::Unavailable(e.to_string())
        }
        other => VaultError::Other(other.to_string()),
    }
}

impl Vault for KeyringVault {
    fn get(&self, account_id: &str) -> Result<Option<Secret>, VaultError> {
        match Self::entry(account_id)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_err(err)),
        }
    }

    fn set(&self, account_id: &str, secret: &Secret) -> Result<(), VaultError> {
        Self::entry(account_id)?
            .set_password(secret.expose())
            .map_err(map_err)
    }

    fn delete(&self, account_id: &str) -> Result<(), VaultError> {
        match Self::entry(account_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_err(err)),
        }
    }
}

/// In-memory vault for tests (and a model of the contract).
#[derive(Debug, Default)]
pub struct MemoryVault {
    entries: std::sync::Mutex<std::collections::HashMap<String, Secret>>,
}

impl Vault for MemoryVault {
    fn get(&self, account_id: &str) -> Result<Option<Secret>, VaultError> {
        Ok(self.entries.lock().unwrap().get(account_id).cloned())
    }

    fn set(&self, account_id: &str, secret: &Secret) -> Result<(), VaultError> {
        self.entries
            .lock()
            .unwrap()
            .insert(account_id.to_string(), secret.clone());
        Ok(())
    }

    fn delete(&self, account_id: &str) -> Result<(), VaultError> {
        self.entries.lock().unwrap().remove(account_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real OS keychain with a throwaway entry; run by hand.
    #[test]
    #[ignore = "uses the real OS keychain"]
    fn keyring_round_trip() {
        let vault = KeyringVault;
        let id = format!("gitdragon-test-{}", crate::store::new_id());
        let secret = Secret::new("canary-token-123456");
        vault.set(&id, &secret).unwrap();
        assert_eq!(vault.get(&id).unwrap(), Some(secret));
        vault.delete(&id).unwrap();
        assert_eq!(vault.get(&id).unwrap(), None);
        vault.delete(&id).unwrap();
    }

    #[test]
    fn memory_vault_contract() {
        let vault = MemoryVault::default();
        assert_eq!(vault.get("x").unwrap(), None);
        vault.set("x", &Secret::new("v")).unwrap();
        assert_eq!(vault.get("x").unwrap().unwrap().expose(), "v");
        vault.delete("x").unwrap();
        vault.delete("x").unwrap();
        assert_eq!(vault.get("x").unwrap(), None);
    }
}
