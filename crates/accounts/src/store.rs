//! `accounts.json`: account metadata only (ids, hosts, modes, usernames). Secrets are in the
//! keychain ([`crate::vault`]); this file never holds a token.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Account;

pub const FILE_NAME: &str = "accounts.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    accounts: Vec<Account>,
}

#[derive(Debug)]
pub struct AccountStore {
    path: PathBuf,
    accounts: Vec<Account>,
}

impl AccountStore {
    /// Loads `dir/accounts.json`; a missing or unreadable file is an empty list (logged).
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let accounts = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<File>(&bytes)
                .map(|f| f.accounts)
                .unwrap_or_else(|err| {
                    tracing::warn!(path = %path.display(), %err, "accounts.json unreadable");
                    Vec::new()
                }),
            Err(_) => Vec::new(),
        };
        Self { path, accounts }
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Adds or replaces (by id) and saves.
    pub fn upsert(&mut self, account: Account) -> std::io::Result<()> {
        match self.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(existing) => *existing = account,
            None => self.accounts.push(account),
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> std::io::Result<Option<Account>> {
        let Some(ix) = self.accounts.iter().position(|a| a.id == id) else {
            return Ok(None);
        };
        let removed = self.accounts.remove(ix);
        self.save()?;
        Ok(Some(removed))
    }

    /// Atomic write (temp file + rename).
    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(&File {
            accounts: self.accounts.clone(),
        })
        .map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// A fresh random account id (hex).
pub fn new_id() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthMode, HostKind};

    #[test]
    fn round_trip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AccountStore::load(dir.path());
        assert!(store.accounts().is_empty());
        let account = Account {
            id: new_id(),
            name: "me @ GitHub".into(),
            kind: HostKind::GitHub,
            base_url: "https://github.com".into(),
            auth: AuthMode::Pat,
            username: Some("me".into()),
        };
        store.upsert(account.clone()).unwrap();
        let again = AccountStore::load(dir.path());
        assert_eq!(again.accounts(), std::slice::from_ref(&account));
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("token") && !text.contains("secret"));

        let mut again = again;
        assert_eq!(again.remove(&account.id).unwrap(), Some(account));
        assert!(AccountStore::load(dir.path()).accounts().is_empty());
    }

    #[test]
    fn ids_are_unique_hex() {
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 24);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
