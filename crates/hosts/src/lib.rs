//! Hosting providers behind one [`GitHost`] trait (design "Hosting providers"): the UI asks
//! the trait, a host that lacks a feature answers [`HostError::Unsupported`] and the UI hides
//! it. Provider-specific code lives only in its own module ([`github`], [`gitlab`], [`azure`]).
//!
//! - [`remote`]: remote URL (https / ssh / scp) to a [`RepoId`].
//! - [`http`]: the transport (TLS always verified, credentials never cross origins).
//! - [`api`]: per-endpoint cache with TTL + ETag / Last-Modified, rate limits, error mapping.
//! - [`poll`]: refresh scheduling (focus, back-off, rate-limit windows).
//! - [`replay`]: recorded responses for tests.
//! - [`boards`]: the board model (GitHub Projects, GitLab issue boards, Azure Boards).

pub mod api;
pub mod azure;
pub mod boards;
pub mod error;
pub mod github;
pub mod gitlab;
pub mod host;
pub mod http;
pub mod model;
pub mod poll;
pub mod remote;
pub mod replay;

use std::sync::Arc;

use accounts::{Account, AuthMode, HostKind, Secret};

pub use api::{ApiClient, RateLimit};
pub use azure::AzureDevOps;
pub use boards::*;
pub use error::HostError;
pub use github::GitHub;
pub use gitlab::GitLab;
pub use host::GitHost;
pub use http::{HttpTransport, Transport};
pub use model::*;
pub use poll::{PollConfig, PollSchedule};
pub use remote::RepoId;

/// The provider for an account. `Generic` accounts have no API. OAuth tokens are not
/// refreshed here: the app refreshes them (see `accounts::oauth`) and hands the new one to
/// the host (`GitLab::set_token`).
pub fn connect(
    account: &Account,
    token: Option<Secret>,
    transport: Arc<dyn Transport>,
) -> Result<Arc<dyn GitHost>, HostError> {
    match account.kind {
        HostKind::GitHub => Ok(Arc::new(GitHub::new(&account.base_url, token, transport)?)),
        HostKind::GitLab => Ok(Arc::new(GitLab::new(
            &account.base_url,
            token,
            account.auth == AuthMode::OAuth,
            transport,
        )?)),
        // A GCM account has no token: calls answer 401 and surface as `Unauthorized`.
        HostKind::AzureDevOps => Ok(Arc::new(AzureDevOps::new(
            &account.base_url,
            token,
            account.auth == AuthMode::OAuth,
            transport,
        )?)),
        HostKind::Generic => Err(HostError::Unsupported(Feature::Api)),
    }
}

/// The account a remote belongs to (exact web origin, see `accounts::model`).
pub fn account_for<'a>(accounts: &'a [Account], repo: &RepoId) -> Option<&'a Account> {
    accounts::find_account(accounts, &repo.web_base())
}
