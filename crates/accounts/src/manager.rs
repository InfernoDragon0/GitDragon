//! [`Accounts`]: the account list, its keychain secrets and the askpass answers, shared between
//! the UI and the askpass thread.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::askpass::{Ask, AskKind};
use crate::model::{Account, AuthMode, HostKind};
use crate::oauth::{OAuthError, OAuthTokens};
use crate::secret::Secret;
use crate::store::{AccountStore, new_id};
use crate::validate::{ValidateError, validate_oauth, validate_pat};
use crate::vault::{Vault, VaultError};

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Input(String),
    #[error(transparent)]
    Validate(#[from] ValidateError),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("could not save accounts.json: {0}")]
    Io(#[from] std::io::Error),
}

/// What the "add account" form submits.
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub kind: HostKind,
    pub base_url: String,
    pub auth: AuthMode,
    /// Required for Generic PAT accounts (no API to ask); otherwise from validation.
    pub username: Option<String>,
    /// PAT mode only.
    pub token: Option<Secret>,
}

pub struct Accounts {
    store: Mutex<AccountStore>,
    vault: Box<dyn Vault>,
    /// Bumped by every sign-in and sign-out, so holders of a token know to read it again.
    generation: AtomicU64,
}

impl Accounts {
    pub fn new(config_dir: &Path, vault: impl Vault + 'static) -> Arc<Self> {
        let accounts = Self {
            store: Mutex::new(AccountStore::load(config_dir)),
            vault: Box::new(vault),
            generation: AtomicU64::new(0),
        };
        accounts.merge_duplicates();
        Arc::new(accounts)
    }

    /// Older builds let the same sign-in be added twice: keep the first of each, carrying
    /// over a username it lacks, and drop the rest with their secrets.
    fn merge_duplicates(&self) {
        let mut store = self.store();
        let all = store.accounts().to_vec();
        let mut kept: Vec<Account> = Vec::new();
        for account in all {
            match kept.iter_mut().find(|k| same_sign_in(k, &account)) {
                Some(first) => {
                    if first.username.is_none() && account.username.is_some() {
                        first.username = account.username.clone();
                        first.name = account.name.clone();
                        _ = store.upsert(first.clone());
                    }
                    _ = self.vault.delete(&account.id);
                    if let Err(err) = store.remove(&account.id) {
                        tracing::warn!(%err, "could not drop a duplicate account");
                    }
                }
                None => kept.push(account),
            }
        }
    }

    fn store(&self) -> MutexGuard<'_, AccountStore> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Changes whenever an account signs in (again) or out.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<Account> {
        self.store().accounts().to_vec()
    }

    /// The account's token from the keychain; registered for redaction once read.
    pub fn token(&self, id: &str) -> Result<Option<Secret>, VaultError> {
        let token = self.vault.get(id)?;
        if let Some(t) = &token {
            git_core::redact::register(t.expose());
        }
        Ok(token)
    }

    /// Validates (PAT), stores the secret in the keychain first (fails closed), then the
    /// metadata.
    pub async fn add(&self, new: NewAccount) -> Result<Account, AddError> {
        let base_url = normalise_base(&new.base_url)?;
        let mut username = new
            .username
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        // Typed usernames go into git URLs; Credential Manager's come back as emails too.
        let bad: &[char] = match new.auth {
            AuthMode::Pat => &['\n', '\0', ':', '@'],
            _ => &['\n', '\0'],
        };
        if username.as_deref().is_some_and(|u| u.contains(bad)) {
            return Err(AddError::Input(
                "usernames cannot contain ':', '@' or newlines".into(),
            ));
        }
        let token = match new.auth {
            AuthMode::Gcm => None,
            AuthMode::OAuth => {
                return Err(AddError::Input(
                    "use the browser sign-in for this mode".into(),
                ));
            }
            AuthMode::Pat => {
                let token = new
                    .token
                    .filter(|t| !t.expose().trim().is_empty())
                    .ok_or_else(|| AddError::Input("paste a token".into()))?;
                let token = Secret::new(token.expose().trim());
                if token.expose().contains(['\n', '\r', '\0', ' ']) {
                    return Err(AddError::Input(
                        "the token contains spaces or newlines".into(),
                    ));
                }
                git_core::redact::register(token.expose());
                if new.kind == HostKind::Generic {
                    if username.is_none() {
                        return Err(AddError::Input("enter the username for this server".into()));
                    }
                } else {
                    username = Some(validate_pat(new.kind, &base_url, &token).await?.username);
                }
                Some(token)
            }
        };
        let account = new_account(new.kind, base_url, new.auth, username);
        self.save_new(account, token.as_ref())
    }

    /// Stores an account signed in through the browser: the login comes from the host, the
    /// tokens go to the keychain as one secret.
    pub async fn add_oauth(
        &self,
        kind: HostKind,
        base_url: &str,
        tokens: OAuthTokens,
    ) -> Result<Account, AddError> {
        let base_url = normalise_base(base_url)?;
        tokens.register_redaction();
        let access = Secret::new(tokens.access_token.clone());
        let info = validate_oauth(kind, &base_url, &access).await?;
        let account = new_account(kind, base_url, AuthMode::OAuth, Some(info.username));
        self.save_new(account, Some(&tokens.to_secret()))
    }

    /// Saves a new account, or refreshes the one already holding this sign-in (same host,
    /// mode and login) instead of adding a duplicate.
    fn save_new(&self, mut account: Account, secret: Option<&Secret>) -> Result<Account, AddError> {
        let existing = self.list().into_iter().find(|a| same_sign_in(a, &account));
        if let Some(existing) = existing {
            account.id = existing.id;
            if account.username.is_none() {
                account.username = existing.username;
                account.name = existing.name;
            }
        }
        if let Some(secret) = secret {
            self.vault.set(&account.id, secret)?;
        }
        if let Err(err) = self.store().upsert(account.clone()) {
            // No orphaned secret without its account.
            _ = self.vault.delete(&account.id);
            return Err(err.into());
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(account)
    }

    /// Records the login Credential Manager signed in as (GCM accounts start without one).
    pub fn set_username(&self, id: &str, username: &str) -> Result<Option<Account>, AddError> {
        let username = username.trim();
        if username.is_empty() || username.contains(['\n', '\0']) {
            return Ok(None);
        }
        let mut store = self.store();
        let Some(mut account) = store.get(id).cloned() else {
            return Ok(None);
        };
        let host = url::Url::parse(&account.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        account.username = Some(username.to_string());
        account.name = format!("{username} @ {host}");
        store.upsert(account.clone())?;
        Ok(Some(account))
    }

    /// Signs out: deletes the keychain entry and the metadata, forgets the secret for
    /// redaction only after it can no longer be used.
    pub fn remove(&self, id: &str) -> Result<(), AddError> {
        let token = self.vault.get(id).ok().flatten();
        self.vault.delete(id)?;
        self.store().remove(id)?;
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(token) = token {
            git_core::redact::unregister(token.expose());
        }
        Ok(())
    }

    /// The token git should use for `account`: the PAT, or the OAuth access token (refreshed
    /// first when it is about to expire; blocks on the network, so only call it off the UI
    /// thread).
    pub fn git_token(&self, account: &Account) -> Result<Option<Secret>, VaultError> {
        let Some(secret) = self.token(&account.id)? else {
            return Ok(None);
        };
        if account.auth != AuthMode::OAuth {
            return Ok(Some(secret));
        }
        let Some(mut tokens) = OAuthTokens::from_secret(&secret) else {
            return Ok(None);
        };
        tokens.register_redaction();
        if tokens.needs_refresh() && account.kind == HostKind::GitLab {
            let refresh = tokens.refresh_token.clone().unwrap_or_default();
            match crate::net::block_on(crate::oauth::gitlab_refresh(&account.base_url, &refresh)) {
                Ok(fresh) => {
                    self.vault.set(&account.id, &fresh.to_secret())?;
                    tokens = fresh;
                }
                Err(err) => tracing::warn!(%err, "GitLab token refresh failed"),
            }
        }
        Ok(Some(Secret::new(tokens.access_token)))
    }

    /// The askpass answer: only token accounts, only the exact origin, and only when the
    /// prompt's username (if any) is the account's.
    pub fn answer(&self, ask: &Ask) -> Option<Secret> {
        let account = self
            .list()
            .into_iter()
            .filter(|a| a.auth.has_secret())
            .find(|a| a.matches(&ask.origin))?;
        // Azure DevOps remote URLs carry the organisation in the user slot
        // (`https://org@dev.azure.com/…`), not the login, and Azure ignores it.
        if let (Some(asked), Some(ours)) = (&ask.username, &account.username)
            && account.kind != HostKind::AzureDevOps
            && !asked.eq_ignore_ascii_case(ours)
        {
            return None;
        }
        match ask.kind {
            AskKind::Username => Some(Secret::new(git_username(&account))),
            AskKind::Password => match self.git_token(&account) {
                Ok(token) => token,
                Err(err) => {
                    tracing::warn!(%err, "askpass: keychain unavailable");
                    None
                }
            },
        }
    }
}

fn new_account(
    kind: HostKind,
    base_url: String,
    auth: AuthMode,
    username: Option<String>,
) -> Account {
    let host = url::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    Account {
        id: new_id(),
        name: match &username {
            Some(user) => format!("{user} @ {host}"),
            None => format!("{} ({host})", kind.label()),
        },
        kind,
        base_url,
        auth,
        username,
    }
}

/// The username git sends with the token. GitLab OAuth tokens must use `oauth2`.
fn git_username(account: &Account) -> String {
    match (account.auth, account.kind) {
        (AuthMode::OAuth, HostKind::GitLab) => "oauth2".into(),
        _ => account.username.clone().unwrap_or_else(|| "git".into()),
    }
}

/// `https://host[:port][/path]` without a trailing slash; https only (tokens never go over
/// plain http).
/// Same host (and path), same mode, same login. A Credential Manager link without a login yet
/// matches any login on that host.
fn same_sign_in(a: &Account, b: &Account) -> bool {
    if a.auth != b.auth || !a.base_url.eq_ignore_ascii_case(&b.base_url) {
        return false;
    }
    match (&a.username, &b.username) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        (None, None) => true,
        _ => a.auth == AuthMode::Gcm,
    }
}

fn normalise_base(base: &str) -> Result<String, AddError> {
    let url =
        url::Url::parse(base.trim()).map_err(|_| AddError::Input(format!("not a URL: {base}")))?;
    if url.scheme() != "https" {
        return Err(AddError::Input(
            "the server URL must start with https://".into(),
        ));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(AddError::Input(
            "the server URL must be https://host[/path] without a user".into(),
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askpass::parse_prompt;
    use crate::vault::MemoryVault;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        async_io::block_on(f)
    }

    fn generic(token: &str) -> NewAccount {
        NewAccount {
            kind: HostKind::Generic,
            base_url: "https://git.example.com/".into(),
            auth: AuthMode::Pat,
            username: Some("me".into()),
            token: Some(Secret::new(token)),
        }
    }

    #[test]
    fn sign_in_and_out_change_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let start = accounts.generation();
        let account = block(accounts.add(generic("first-token-0123"))).unwrap();
        let added = accounts.generation();
        assert_ne!(added, start);
        // Signing in again as the same login keeps the account but is still a change.
        block(accounts.add(generic("second-token-0123"))).unwrap();
        assert_ne!(accounts.generation(), added);
        let before = accounts.generation();
        accounts.remove(&account.id).unwrap();
        assert_ne!(accounts.generation(), before);
    }

    #[test]
    fn add_answer_and_remove_a_token_account() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let canary = "CANARY-token-0123456789";
        let account = block(accounts.add(generic(canary))).unwrap();
        assert_eq!(account.base_url, "https://git.example.com");
        assert_eq!(account.username.as_deref(), Some("me"));

        // Metadata on disk, secret nowhere in it.
        let json = std::fs::read_to_string(dir.path().join(crate::store::FILE_NAME)).unwrap();
        assert!(!json.contains(canary));
        // And never printed once known.
        assert!(!git_core::redact::redact(&format!("error {canary}")).contains(canary));

        let ask = |p: &str| {
            accounts
                .answer(&parse_prompt(p).unwrap())
                .map(|s| s.expose().to_string())
        };
        assert_eq!(
            ask("Username for 'https://git.example.com': ").as_deref(),
            Some("me")
        );
        assert_eq!(
            ask("Password for 'https://me@git.example.com': ").as_deref(),
            Some(canary)
        );
        assert_eq!(ask("Password for 'https://other@git.example.com': "), None);
        assert_eq!(
            ask("Password for 'https://git.example.com.evil.io': "),
            None
        );
        assert_eq!(ask("Password for 'https://git.example.com:8443': "), None);

        accounts.remove(&account.id).unwrap();
        assert!(accounts.list().is_empty());
        assert_eq!(ask("Password for 'https://git.example.com': "), None);
    }

    /// Azure remotes put the organisation where git expects a user; the token still answers.
    #[test]
    fn azure_org_in_the_user_slot() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let account = Account {
            id: "az".into(),
            name: "me @ dev.azure.com".into(),
            kind: HostKind::AzureDevOps,
            base_url: "https://dev.azure.com/infernodragon0".into(),
            auth: AuthMode::Pat,
            username: Some("me@example.com".into()),
        };
        accounts
            .save_new(account, Some(&Secret::new("azure-pat-0123456789")))
            .unwrap();
        let ask = parse_prompt("Password for 'https://infernodragon0@dev.azure.com': ").unwrap();
        assert_eq!(
            accounts
                .answer(&ask)
                .map(|s| s.expose().to_string())
                .as_deref(),
            Some("azure-pat-0123456789")
        );
        // Still exact on the host.
        let other = parse_prompt("Password for 'https://infernodragon0@vs-ssh.visualstudio.com': ")
            .unwrap();
        assert!(accounts.answer(&other).is_none());
    }

    #[test]
    fn gcm_accounts_never_answer_askpass() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        block(accounts.add(NewAccount {
            kind: HostKind::GitHub,
            base_url: "https://github.com".into(),
            auth: AuthMode::Gcm,
            username: None,
            token: None,
        }))
        .unwrap();
        let ask = parse_prompt("Password for 'https://github.com': ").unwrap();
        assert!(accounts.answer(&ask).is_none());
    }

    #[test]
    fn no_duplicate_sign_ins() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let gcm = |user: Option<&str>| NewAccount {
            kind: HostKind::GitHub,
            base_url: "https://github.com/".into(),
            auth: AuthMode::Gcm,
            username: user.map(str::to_owned),
            token: None,
        };
        let first = block(accounts.add(gcm(None))).unwrap();
        let again = block(accounts.add(gcm(Some("Me@example.com")))).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(again.username.as_deref(), Some("Me@example.com"));
        block(accounts.add(gcm(Some("me@example.com")))).unwrap();
        block(accounts.add(gcm(None))).unwrap();
        assert_eq!(accounts.list().len(), 1);
        assert_eq!(
            accounts.list()[0].username.as_deref(),
            Some("me@example.com")
        );
        // A second login on the same host is its own sign-in.
        block(accounts.add(gcm(Some("work")))).unwrap();
        assert_eq!(accounts.list().len(), 2);
        // Re-adding a token replaces it instead of piling up.
        block(accounts.add(generic("tok-1111111111"))).unwrap();
        let tok = block(accounts.add(generic("tok-2222222222"))).unwrap();
        assert_eq!(accounts.list().len(), 3);
        assert_eq!(
            accounts.token(&tok.id).unwrap().unwrap().expose(),
            "tok-2222222222"
        );
    }

    #[test]
    fn merges_duplicates_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AccountStore::load(dir.path());
        for (id, user) in [("a", None), ("b", Some("me")), ("c", None)] {
            store
                .upsert(Account {
                    id: id.into(),
                    name: id.into(),
                    kind: HostKind::GitHub,
                    base_url: "https://github.com".into(),
                    auth: AuthMode::Gcm,
                    username: user.map(str::to_owned),
                })
                .unwrap();
        }
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let list = accounts.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a");
        assert_eq!(list[0].username.as_deref(), Some("me"));
    }

    #[test]
    fn input_checks() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(dir.path(), MemoryVault::default());
        let mut http = generic("tok-1234567890");
        http.base_url = "http://git.example.com".into();
        assert!(matches!(block(accounts.add(http)), Err(AddError::Input(_))));
        let mut with_user = generic("tok-1234567890");
        with_user.base_url = "https://me:pw@git.example.com".into();
        assert!(block(accounts.add(with_user)).is_err());
        let mut no_user = generic("tok-1234567890");
        no_user.username = None;
        assert!(block(accounts.add(no_user)).is_err());
        assert!(block(accounts.add(generic("two words"))).is_err());
        assert!(block(accounts.add(generic("  "))).is_err());
        assert!(accounts.list().is_empty());
    }
}
