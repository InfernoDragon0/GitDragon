//! Account metadata (never secrets) and host matching.
//!
//! Matching is exact on scheme + host + port (design "Accounts & credentials", checklist P0):
//! `github.com` never matches `github.com.evil.io`, `api.github.com` or `http://github.com`.

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostKind {
    GitHub,
    GitLab,
    AzureDevOps,
    /// Any git server: git only, no API panels.
    Generic,
}

impl HostKind {
    pub const ALL: [HostKind; 4] = [
        HostKind::GitHub,
        HostKind::GitLab,
        HostKind::AzureDevOps,
        HostKind::Generic,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HostKind::GitHub => "GitHub",
            HostKind::GitLab => "GitLab",
            HostKind::AzureDevOps => "Azure DevOps",
            HostKind::Generic => "Other git server",
        }
    }

    /// The service a host name belongs to (`Generic` when unknown).
    pub fn of_host(host: &str) -> HostKind {
        match host.to_ascii_lowercase().as_str() {
            "github.com" => HostKind::GitHub,
            "gitlab.com" => HostKind::GitLab,
            "dev.azure.com" | "ssh.dev.azure.com" => HostKind::AzureDevOps,
            h if h.ends_with(".visualstudio.com") => HostKind::AzureDevOps,
            _ => HostKind::Generic,
        }
    }

    /// Default web base URL for the hosted service.
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            HostKind::GitHub => Some("https://github.com"),
            HostKind::GitLab => Some("https://gitlab.com"),
            HostKind::AzureDevOps => Some("https://dev.azure.com"),
            HostKind::Generic => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMode {
    /// Git Credential Manager (or any configured credential helper): git asks it itself.
    Gcm,
    /// Personal access token in the keychain, answered through askpass.
    Pat,
    /// OAuth access / refresh tokens in the keychain.
    OAuth,
}

impl AuthMode {
    pub fn label(self) -> &'static str {
        match self {
            AuthMode::Gcm => "Git Credential Manager",
            AuthMode::Pat => "Personal access token",
            AuthMode::OAuth => "Sign in",
        }
    }

    /// Does the keychain hold a secret for this mode?
    pub fn has_secret(self) -> bool {
        !matches!(self, AuthMode::Gcm)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Random id; also the keychain user name.
    pub id: String,
    /// Shown in the UI, e.g. `mushr @ GitHub`.
    pub name: String,
    pub kind: HostKind,
    /// Web base URL, e.g. `https://github.com` or `https://gitlab.example.com:8443`.
    pub base_url: String,
    pub auth: AuthMode,
    /// Login on the host (from validation), used as the git username.
    pub username: Option<String>,
}

/// Scheme + host + port of a URL, the unit credentials are bound to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl Origin {
    /// `None` for URLs without a host or an unknown default port. Hosts compare
    /// case-insensitively (the `url` crate lowercases them); a trailing dot is dropped.
    pub fn of(url: &str) -> Option<Self> {
        let url = Url::parse(url.trim()).ok()?;
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme: url.scheme().to_ascii_lowercase(),
            port: url.port_or_known_default()?,
            host,
        })
    }
}

impl Account {
    pub fn origin(&self) -> Option<Origin> {
        Origin::of(&self.base_url)
    }

    /// Azure DevOps also serves git from `<org>.visualstudio.com` (legacy) and
    /// `ssh.dev.azure.com`; only exact origins are matched, so legacy orgs need their own
    /// account with that base URL.
    pub fn matches(&self, origin: &Origin) -> bool {
        self.origin().as_ref() == Some(origin)
    }
}

/// The account for a remote / askpass URL: exact origin match, first wins.
pub fn find_account<'a>(accounts: &'a [Account], url: &str) -> Option<&'a Account> {
    let origin = Origin::of(url)?;
    accounts.iter().find(|a| a.matches(&origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(base: &str) -> Account {
        Account {
            id: "a1".into(),
            name: "test".into(),
            kind: HostKind::GitHub,
            base_url: base.into(),
            auth: AuthMode::Pat,
            username: Some("mushr".into()),
        }
    }

    #[test]
    fn exact_origin_matching() {
        let accounts = [account("https://github.com")];
        let hit = |url: &str| find_account(&accounts, url).is_some();
        assert!(hit("https://github.com/owner/repo.git"));
        assert!(hit("https://GitHub.com/owner/repo"));
        assert!(hit("https://github.com:443/x"));
        assert!(hit("https://user@github.com/x"));
        assert!(hit("https://github.com./x"));
        // Never by suffix, substring, other scheme or port.
        assert!(!hit("https://github.com.evil.io/x"));
        assert!(!hit("https://evilgithub.com/x"));
        assert!(!hit("https://api.github.com/x"));
        assert!(!hit("http://github.com/x"));
        assert!(!hit("https://github.com:8443/x"));
        assert!(!hit("git@github.com:owner/repo.git"));
        assert!(!hit("not a url"));
    }

    #[test]
    fn self_hosted_port() {
        let accounts = [account("https://gitlab.example.com:8443")];
        assert!(find_account(&accounts, "https://gitlab.example.com:8443/g/p.git").is_some());
        assert!(find_account(&accounts, "https://gitlab.example.com/g/p.git").is_none());
    }
}
