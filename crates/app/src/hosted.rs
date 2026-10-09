//! Author avatars from the repo's hosting service. Only the `GitHost` trait is used, so every
//! provider that implements `author_avatar` works and the ones that do not are skipped. The
//! account comes from the repo's remotes (origin first); with none that matches, hosted lookups
//! are silently off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use accounts::{Account, Accounts};
use git_core::parse::refs::Remote;
use gpui_kit::App;
use hosts::host::HostResult;
use hosts::{
    Feature, GitHost, HostError, HttpTransport, RateLimit, RepoId, Transport, account_for, connect,
};

use crate::repo_tab::RepoTab;

/// Below this many requests left the quota is kept for the pull request / issue panels.
const KEEP_QUOTA: u32 = 100;
/// Hosts (and the token they carry) are rebuilt after this, so a re-sign-in is picked up.
const HOST_TTL: Duration = Duration::from_secs(10 * 60);
/// Asking again after a hiccup (offline, rate limit, keychain busy).
const SOON: Duration = Duration::from_secs(60);
/// Asking again when waiting for the user helps more (commit not pushed, no access, no
/// account yet).
const LATER: Duration = Duration::from_secs(10 * 60);

/// What the host said about one author.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The avatar's public URL.
    Url(String),
    /// The host knows no avatar for this author.
    Miss,
    /// Could not be asked now; ask again after this long.
    Later(Duration),
}

/// The hosting side of one repo: where its remotes point and which accounts exist.
#[derive(Clone)]
pub struct HostedRepo {
    /// Remote URLs, origin first.
    remotes: Arc<[String]>,
    accounts: Arc<Accounts>,
}

impl HostedRepo {
    /// `None` when there are no accounts or the repo has no remote.
    pub fn of_tab(tab: &RepoTab, cx: &App) -> Option<Self> {
        let accounts = crate::accounts::accounts(cx)?;
        let remotes = remote_urls(&tab.remotes);
        (!remotes.is_empty()).then(|| Self {
            remotes: remotes.into(),
            accounts,
        })
    }

    /// The account and repo of the first remote some account is signed in for.
    fn target(&self) -> Option<(Account, RepoId)> {
        let accounts = self.accounts.list();
        self.remotes.iter().find_map(|url| {
            let repo = RepoId::parse(url)?;
            let account = account_for(&accounts, &repo)?.clone();
            Some((account, repo))
        })
    }

    /// Names the service (origin) the answers come from; for disk cache keys. Blocks briefly,
    /// call it off the UI thread.
    pub fn scope(&self) -> Option<String> {
        self.target().map(|(_, repo)| repo.web_base())
    }

    /// The avatar of `email`, who authored `commit`. Blocks on the keychain the first time an
    /// account is used, so call it off the UI thread (the network part is runtime-neutral).
    pub async fn avatar(&self, commit: &str, email: &str) -> Lookup {
        let Some((account, repo)) = self.target() else {
            return Lookup::Later(LATER);
        };
        let Some(host) = self.host(&account) else {
            return Lookup::Later(SOON);
        };
        if !host.supports(Feature::Avatars) {
            return Lookup::Later(LATER);
        }
        if quota_low(&host.rate_limit(), SystemTime::now()) {
            return Lookup::Later(SOON);
        }
        classify(host.author_avatar(&repo, commit, email).await)
    }

    /// One host per account, shared by all repos: they share the token and so the quota.
    fn host(&self, account: &Account) -> Option<Arc<dyn GitHost>> {
        type Hosts = Option<HashMap<String, (Account, Instant, Arc<dyn GitHost>)>>;
        static HOSTS: Mutex<Hosts> = Mutex::new(None);
        let cached = |map: &Hosts| {
            map.as_ref()?
                .get(&account.id)
                .filter(|(a, built, _)| a == account && built.elapsed() < HOST_TTL)
                .map(|(.., host)| host.clone())
        };
        if let Some(host) = cached(&HOSTS.lock().unwrap_or_else(|p| p.into_inner())) {
            return Some(host);
        }
        // Outside the lock: the keychain (and an OAuth refresh) can take a while. GCM accounts
        // have no secret here; public repos still answer anonymously.
        let token = match self.accounts.git_token(account) {
            Ok(token) => token,
            Err(err) => {
                tracing::debug!(%err, "no token for hosted avatars");
                return None;
            }
        };
        let host = connect(account, token, transport())
            .inspect_err(|err| tracing::debug!(%err, "no host for avatars"))
            .ok()?;
        HOSTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_default()
            .insert(
                account.id.clone(),
                (account.clone(), Instant::now(), host.clone()),
            );
        Some(host)
    }
}

fn transport() -> Arc<dyn Transport> {
    static TRANSPORT: OnceLock<Arc<dyn Transport>> = OnceLock::new();
    TRANSPORT
        .get_or_init(|| Arc::new(HttpTransport::new()))
        .clone()
}

/// Fetch URLs, `origin` first, the rest in their order.
fn remote_urls(remotes: &[Remote]) -> Vec<String> {
    let url = |r: &Remote| r.fetch_url.clone().or_else(|| r.push_url.clone());
    let (origin, rest): (Vec<_>, Vec<_>) = remotes.iter().partition(|r| r.name == "origin");
    origin.into_iter().chain(rest).filter_map(url).collect()
}

/// Blocked or nearly out: leave the rest of the quota to the panels.
fn quota_low(rate: &RateLimit, now: SystemTime) -> bool {
    rate.blocked(now).is_some() || rate.remaining.is_some_and(|n| n < KEEP_QUOTA)
}

fn classify(answer: HostResult<Option<String>>) -> Lookup {
    match answer {
        Ok(Some(url)) => Lookup::Url(url),
        Ok(None) => Lookup::Miss,
        // The commit is not pushed (or this account cannot see the repo): another commit of
        // the author may do later.
        Err(HostError::NotFound { .. }) => Lookup::Later(LATER),
        Err(HostError::Unsupported(_)) => Lookup::Later(LATER),
        Err(err) if err.is_transient() => Lookup::Later(SOON),
        Err(err) => {
            tracing::debug!(%err, "hosted avatar lookup refused");
            Lookup::Later(LATER)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, fetch: Option<&str>, push: Option<&str>) -> Remote {
        Remote {
            name: name.into(),
            fetch_url: fetch.map(Into::into),
            push_url: push.map(Into::into),
        }
    }

    #[test]
    fn origin_comes_first() {
        let remotes = [
            remote("fork", Some("https://github.com/me/r.git"), None),
            remote("origin", None, Some("git@github.com:o/r.git")),
            remote("empty", None, None),
        ];
        assert_eq!(
            remote_urls(&remotes),
            ["git@github.com:o/r.git", "https://github.com/me/r.git"]
        );
    }

    #[test]
    fn quota_is_kept_for_the_panels() {
        let now = SystemTime::now();
        let rate = |remaining, blocked_until| RateLimit {
            remaining,
            blocked_until,
            ..RateLimit::default()
        };
        assert!(!quota_low(&RateLimit::default(), now), "unknown: go ahead");
        assert!(!quota_low(&rate(Some(4000), None), now));
        assert!(quota_low(&rate(Some(99), None), now));
        assert!(quota_low(&rate(None, Some(now + SOON)), now));
        assert!(!quota_low(&rate(None, Some(now - SOON)), now));
    }

    #[test]
    fn answers() {
        let host = || "github.com".to_string();
        assert_eq!(classify(Ok(Some("u".into()))), Lookup::Url("u".into()));
        assert_eq!(classify(Ok(None)), Lookup::Miss);
        assert_eq!(
            classify(Err(HostError::NotFound { host: host() })),
            Lookup::Later(LATER)
        );
        assert_eq!(
            classify(Err(HostError::RateLimited {
                host: host(),
                until: None
            })),
            Lookup::Later(SOON)
        );
    }
}
