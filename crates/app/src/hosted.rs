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
    Feature, GitHost, HostError, HostVocab, HttpTransport, RateLimit, RepoId, Transport,
    WatchSignIn, account_for, accounts_for, connect,
};

use crate::repo_tab::RepoTab;

/// Below this many requests left the quota is kept for the pull request / issue panels.
const KEEP_QUOTA: u32 = 100;
/// Hosts (and the token they carry) are rebuilt after this; a sign-in rebuilds them at once.
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

/// A connection to the repo's hosting service, for the pull request and issue panels.
#[derive(Clone)]
pub struct Session {
    pub host: Arc<dyn GitHost>,
    pub repo: RepoId,
    /// The git remote the account matched (fetch and push go there).
    pub remote: String,
    /// Another account is signed in for this host: when this one's sign-in is rejected,
    /// resolving again picks that one.
    pub fallback: bool,
}

impl Session {
    /// The host rejected the sign-in and another account can be tried: resolve again.
    pub fn retry_with_other_account(&self, err: &HostError) -> bool {
        self.fallback && matches!(err, HostError::Unauthorized { .. })
    }
}

/// What [`HostedRepo::resolve`] found.
pub enum Resolution {
    Ready(Session),
    NoRemote,
    /// A hosted remote exists but no account is signed in for it.
    NoAccount(RepoId),
    /// The account is a plain git server: no API.
    NoApi(&'static str),
    Failed(String),
}

/// The hosting side of one repo: where its remotes point and which accounts exist.
#[derive(Clone)]
pub struct HostedRepo {
    /// `(name, fetch URL)` of the remotes, origin first.
    remotes: Arc<[(String, String)]>,
    accounts: Arc<Accounts>,
}

impl HostedRepo {
    /// `None` when there are no accounts or the repo has no remote.
    pub fn of_tab(tab: &RepoTab, cx: &App) -> Option<Self> {
        let accounts = crate::accounts::accounts(cx)?;
        let remotes = ordered_remotes(&tab.remotes);
        (!remotes.is_empty()).then(|| Self {
            remotes: remotes.into(),
            accounts,
        })
    }

    /// Finds the host for the first remote some account is signed in for. Blocks on the
    /// keychain: call it off the UI thread.
    pub fn resolve(&self) -> Resolution {
        let accounts = self.accounts.list();
        let mut first = None;
        for (name, url) in self.remotes.iter() {
            let Some(repo) = RepoId::parse(url) else {
                continue;
            };
            let Some((account, fallback)) = self.pick(&accounts, &repo) else {
                first.get_or_insert(repo);
                continue;
            };
            return match self.host_checked(account) {
                Ok(host) => Resolution::Ready(Session {
                    host,
                    repo,
                    remote: name.clone(),
                    fallback,
                }),
                Err(HostError::Unsupported(_)) => {
                    Resolution::NoApi(HostVocab::of(account.kind).service)
                }
                Err(err) => Resolution::Failed(err.to_string()),
            };
        }
        match first {
            Some(repo) => Resolution::NoAccount(repo),
            None => Resolution::NoRemote,
        }
    }

    /// The words of the repo's host (GitLab: "Merge requests"); from the matching account, else
    /// from the remote's host name. No blocking.
    pub fn vocab(&self) -> Option<&'static HostVocab> {
        let accounts = self.accounts.list();
        self.remotes.iter().find_map(|(_, url)| {
            let repo = RepoId::parse(url)?;
            let kind = account_for(&accounts, &repo).map_or_else(|| repo.kind(), |a| a.kind);
            Some(HostVocab::of(kind))
        })
    }

    /// The account and repo of the first remote some account is signed in for.
    fn target(&self) -> Option<(Account, RepoId)> {
        let accounts = self.accounts.list();
        self.remotes.iter().find_map(|(_, url)| {
            let repo = RepoId::parse(url)?;
            let (account, _) = self.pick(&accounts, &repo)?;
            Some((account.clone(), repo))
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

    /// [`Self::host_checked`] for the avatar lookups, which only log why there is none.
    fn host(&self, account: &Account) -> Option<Arc<dyn GitHost>> {
        self.host_checked(account)
            .inspect_err(|err| tracing::debug!(%err, "no host for avatars"))
            .ok()
    }

    /// The account for `repo`, and whether another one could stand in for it (see [`choose`]).
    fn pick<'a>(&self, accounts: &'a [Account], repo: &RepoId) -> Option<(&'a Account, bool)> {
        let all = accounts_for(accounts, repo);
        let generation = self.accounts.generation();
        let rejected = rejected().lock().unwrap_or_else(|p| p.into_inner());
        choose(&all, |a| rejected.get(&a.id) == Some(&generation))
    }

    /// One host per account, shared by all repos: they share the token and so the quota.
    fn host_checked(&self, account: &Account) -> Result<Arc<dyn GitHost>, HostError> {
        type Hosts = Option<HashMap<String, (Account, u64, Instant, Arc<dyn GitHost>)>>;
        static HOSTS: Mutex<Hosts> = Mutex::new(None);
        let generation = self.accounts.generation();
        let cached = |map: &Hosts| {
            map.as_ref()?
                .get(&account.id)
                .filter(|(a, made_in, built, _)| {
                    a == account && *made_in == generation && built.elapsed() < HOST_TTL
                })
                .map(|(.., host)| host.clone())
        };
        if let Some(host) = cached(&HOSTS.lock().unwrap_or_else(|p| p.into_inner())) {
            return Ok(host);
        }
        // Outside the lock: the keychain (and an OAuth refresh) can take a while. GCM accounts
        // have no secret here; public repos still answer anonymously.
        let token = self
            .accounts
            .git_token(account)
            .map_err(|err| HostError::Config(format!("could not read the saved sign-in: {err}")))?;
        let id = account.id.clone();
        let watched = WatchSignIn::new(transport(), move || {
            tracing::info!(account = %id, "sign-in rejected by the host");
            rejected()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone(), generation);
        });
        let host = connect(account, token, Arc::new(watched))?;
        HOSTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_default()
            .insert(
                account.id.clone(),
                (account.clone(), generation, Instant::now(), host.clone()),
            );
        Ok(host)
    }
}

/// Account id -> the [`Accounts::generation`] in which its host answered 401.
fn rejected() -> &'static Mutex<HashMap<String, u64>> {
    static REJECTED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    REJECTED.get_or_init(Default::default)
}

fn transport() -> Arc<dyn Transport> {
    static TRANSPORT: OnceLock<Arc<dyn Transport>> = OnceLock::new();
    TRANSPORT
        .get_or_init(|| Arc::new(HttpTransport::new()))
        .clone()
}

/// `(name, fetch URL)`, `origin` first, the rest in their order; remotes without a URL are
/// left out.
fn ordered_remotes(remotes: &[Remote]) -> Vec<(String, String)> {
    let url = |r: &Remote| {
        let url = r.fetch_url.clone().or_else(|| r.push_url.clone())?;
        Some((r.name.clone(), url))
    };
    let (origin, rest): (Vec<_>, Vec<_>) = remotes.iter().partition(|r| r.name == "origin");
    origin.into_iter().chain(rest).filter_map(url).collect()
}

/// Fetch URLs in [`ordered_remotes`] order.
#[cfg(test)]
fn remote_urls(remotes: &[Remote]) -> Vec<String> {
    ordered_remotes(remotes)
        .into_iter()
        .map(|(_, url)| url)
        .collect()
}

/// Blocked or nearly out: leave the rest of the quota to the panels.
/// Picks from `all` (best first): accounts holding a token come first; one whose sign-in the
/// host rejected is passed over until the accounts change (a sign-in), unless no other is left.
/// A tokenless account (Credential Manager) only stands in when no account holds a token: going
/// anonymous after a rejected sign-in would hide "sign in again" behind anonymous-API errors
/// (no GraphQL, 60 requests an hour).
fn choose<'a>(
    all: &[&'a Account],
    is_rejected: impl Fn(&Account) -> bool,
) -> Option<(&'a Account, bool)> {
    let with_token: Vec<&Account> = all
        .iter()
        .copied()
        .filter(|a| a.auth.has_secret())
        .collect();
    let pool = if with_token.is_empty() {
        all.to_vec()
    } else {
        with_token
    };
    let usable: Vec<&Account> = pool.iter().copied().filter(|a| !is_rejected(a)).collect();
    match usable.first() {
        Some(first) => Some((first, usable.len() > 1)),
        None => pool.first().map(|first| (*first, false)),
    }
}

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
    fn remote_names_travel_with_their_urls() {
        let remotes = [
            remote("fork", Some("https://github.com/me/r.git"), None),
            remote("origin", Some("git@github.com:o/r.git"), None),
        ];
        assert_eq!(
            ordered_remotes(&remotes),
            [
                ("origin".to_string(), "git@github.com:o/r.git".to_string()),
                (
                    "fork".to_string(),
                    "https://github.com/me/r.git".to_string()
                )
            ]
        );
    }

    #[test]
    fn a_rejected_token_account_never_falls_back_to_a_tokenless_one() {
        use accounts::{AuthMode, HostKind};
        let account = |id: &str, auth| Account {
            id: id.into(),
            name: id.into(),
            kind: HostKind::GitHub,
            base_url: "https://github.com".into(),
            auth,
            username: None,
        };
        let oauth = account("oauth", AuthMode::OAuth);
        let pat = account("pat", AuthMode::Pat);
        let gcm = account("gcm", AuthMode::Gcm);
        let id = |pick: Option<(&Account, bool)>| pick.map(|(a, more)| (a.id.clone(), more));
        let none = |_: &Account| false;
        let oauth_out = |a: &Account| a.id == "oauth";
        // Token first; the GCM account is no stand-in for it.
        assert_eq!(
            id(choose(&[&gcm, &oauth], none)),
            Some(("oauth".into(), false))
        );
        // Rejected: still the token account (its error says "sign in again"), not anonymous.
        assert_eq!(
            id(choose(&[&oauth, &gcm], oauth_out)),
            Some(("oauth".into(), false))
        );
        // Another token account does stand in.
        assert_eq!(
            id(choose(&[&oauth, &pat, &gcm], none)),
            Some(("oauth".into(), true))
        );
        assert_eq!(
            id(choose(&[&oauth, &pat, &gcm], oauth_out)),
            Some(("pat".into(), false))
        );
        // Only a GCM account: anonymous public reads are better than nothing.
        assert_eq!(id(choose(&[&gcm], none)), Some(("gcm".into(), false)));
        assert_eq!(id(choose(&[], none)), None);
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
