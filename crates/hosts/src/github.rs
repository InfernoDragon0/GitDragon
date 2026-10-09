//! GitHub and GitHub Enterprise Server over REST v3. github.com's API lives at
//! `api.github.com`; GHES at `<base>/api/v3`.

use std::sync::Arc;
use std::time::Duration;

use accounts::{HostKind, Secret};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::api::{ApiClient, RateLimit};
use crate::error::HostError;
use crate::host::{GitHost, HostResult};
use crate::http::{Auth, Method, Transport};
use crate::model::*;
use crate::remote::RepoId;

const API_VERSION: &str = "2022-11-28";

pub struct GitHub {
    api: ApiClient,
    /// Web base, e.g. `https://github.com`.
    web: Url,
}

impl GitHub {
    /// `web_base` is the account's base URL; HTTPS only.
    pub fn new(
        web_base: &str,
        token: Option<Secret>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, HostError> {
        let web = Url::parse(web_base.trim().trim_end_matches('/'))
            .map_err(|_| HostError::Config(format!("bad GitHub URL: {web_base}")))?;
        if web.scheme() != "https" || web.host_str().is_none() {
            return Err(HostError::Config(format!(
                "GitHub needs an https:// URL, got {web_base}"
            )));
        }
        let api = ApiClient::new(
            transport,
            api_base(&web),
            vec![
                ("accept".into(), "application/vnd.github+json".into()),
                ("x-github-api-version".into(), API_VERSION.into()),
            ],
        );
        let github = Self { api, web };
        github.set_token(token);
        Ok(github)
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.api = self.api.with_ttl(ttl);
        self
    }

    /// Swaps the token (re-sign-in); clears the cache.
    pub fn set_token(&self, token: Option<Secret>) {
        self.api
            .set_auth(token.filter(|t| !t.is_empty()).map(Auth::Bearer));
    }

    fn repo_url(&self, repo: &RepoId, rest: &[&str], query: &[(&str, &str)]) -> Url {
        let mut segments = vec!["repos", repo.owner.as_str(), repo.repo.as_str()];
        segments.extend_from_slice(rest);
        self.api.url(&segments, query)
    }

    fn web(&self, segments: &[&str]) -> String {
        let mut url = self.web.clone();
        url.path_segments_mut()
            .expect("https base")
            .pop_if_empty()
            .extend(segments);
        url.to_string()
    }

    fn repo_id(&self, owner: &str, name: &str) -> RepoId {
        RepoId {
            host: self.web.host_str().unwrap_or_default().to_ascii_lowercase(),
            web_port: self.web.port(),
            owner: owner.to_string(),
            project: None,
            repo: name.to_string(),
        }
    }

    async fn page<T, R>(
        &self,
        url: Url,
        page: u32,
        map: impl Fn(R) -> Option<T>,
    ) -> HostResult<Page<T>>
    where
        R: for<'de> Deserialize<'de>,
    {
        let fetched = self.api.get(url).await?;
        let raw: Vec<R> = self.api.decode(&fetched.body)?;
        Ok(Page {
            items: raw.into_iter().filter_map(map).collect(),
            next: fetched.next_page.then_some(page + 1),
        })
    }
}

fn api_base(web: &Url) -> Url {
    if web.host_str() == Some("github.com") {
        Url::parse("https://api.github.com/").expect("static URL")
    } else {
        let mut url = web.clone();
        url.path_segments_mut()
            .expect("https base")
            .pop_if_empty()
            .extend(["api", "v3"]);
        url
    }
}

fn state_param(state: StateFilter) -> &'static str {
    match state {
        StateFilter::Open => "open",
        StateFilter::Closed => "closed",
        StateFilter::All => "all",
    }
}

#[async_trait]
impl GitHost for GitHub {
    fn kind(&self) -> HostKind {
        HostKind::GitHub
    }

    fn supports(&self, feature: Feature) -> bool {
        matches!(
            feature,
            Feature::Api
                | Feature::Whoami
                | Feature::Repos
                | Feature::PullRequests
                | Feature::CreatePullRequest
                | Feature::Issues
                | Feature::CreateIssue
                | Feature::IssueComments
                | Feature::CiStatus
                | Feature::Avatars
        )
    }

    fn web_url(&self, repo: &RepoId, target: &WebTarget) -> Option<String> {
        let (o, r) = (repo.owner.as_str(), repo.repo.as_str());
        let n;
        let compare;
        let segments: Vec<&str> = match target {
            WebTarget::Repo => vec![o, r],
            WebTarget::Commit(sha) => vec![o, r, "commit", sha],
            WebTarget::Branch(branch) => vec![o, r, "tree", branch],
            WebTarget::PullRequest(number) => {
                n = number.to_string();
                vec![o, r, "pull", &n]
            }
            WebTarget::PullRequests => vec![o, r, "pulls"],
            WebTarget::Issue(number) => {
                n = number.to_string();
                vec![o, r, "issues", &n]
            }
            WebTarget::Issues => vec![o, r, "issues"],
            WebTarget::NewPullRequest { source, target } => {
                compare = format!("{target}...{source}");
                vec![o, r, "compare", &compare]
            }
        };
        let url = self.web(&segments);
        Some(match target {
            WebTarget::NewPullRequest { .. } => format!("{url}?expand=1"),
            _ => url,
        })
    }

    fn rate_limit(&self) -> RateLimit {
        self.api.rate_limit()
    }

    fn invalidate(&self, repo: Option<&RepoId>) {
        match repo {
            Some(repo) => self.api.invalidate(&["repos", &repo.owner, &repo.repo]),
            None => self.api.clear_cache(),
        }
    }

    async fn whoami(&self) -> HostResult<Person> {
        let fetched = self.api.get(self.api.url(&["user"], &[])).await?;
        let user: GhUser = self.api.decode(&fetched.body)?;
        Ok(user.into())
    }

    async fn repos(&self, page: u32) -> HostResult<Page<RepoSummary>> {
        let page_s = page.to_string();
        let url = self.api.url(
            &["user", "repos"],
            &[
                ("sort", "updated"),
                ("per_page", "50"),
                ("page", &page_s),
                ("affiliation", "owner,collaborator,organization_member"),
            ],
        );
        self.page(url, page, |r: GhRepo| {
            Some(RepoSummary {
                id: self.repo_id(&r.owner.login, &r.name),
                description: r.description,
                private: r.private,
                fork: r.fork,
                default_branch: r.default_branch,
                web_url: r.html_url,
                clone_url: r.clone_url,
                ssh_url: r.ssh_url,
                updated_at: r.updated_at,
            })
        })
        .await
    }

    async fn pull_requests(
        &self,
        repo: &RepoId,
        query: &ListQuery,
    ) -> HostResult<Page<PullRequest>> {
        let (page, per_page) = (query.page.to_string(), query.per_page.to_string());
        let url = self.repo_url(
            repo,
            &["pulls"],
            &[
                ("state", state_param(query.state)),
                ("sort", "updated"),
                ("direction", "desc"),
                ("per_page", &per_page),
                ("page", &page),
            ],
        );
        self.page(url, query.page, |p: GhPull| Some(p.into())).await
    }

    async fn pull_request(&self, repo: &RepoId, number: u64) -> HostResult<PullRequest> {
        let n = number.to_string();
        let fetched = self
            .api
            .get(self.repo_url(repo, &["pulls", &n], &[]))
            .await?;
        Ok(self.api.decode::<GhPull>(&fetched.body)?.into())
    }

    async fn create_pull_request(
        &self,
        repo: &RepoId,
        new: &NewPullRequest,
    ) -> HostResult<PullRequest> {
        let body = json!({
            "title": new.title,
            "body": new.body,
            "head": new.source_branch,
            "base": new.target_branch,
            "draft": new.draft,
        });
        let url = self.repo_url(repo, &["pulls"], &[]);
        let answer = self.api.send_json(Method::Post, url, &body).await?;
        self.invalidate(Some(repo));
        Ok(self.api.decode::<GhPull>(&answer)?.into())
    }

    async fn issues(&self, repo: &RepoId, query: &ListQuery) -> HostResult<Page<Issue>> {
        let (page, per_page) = (query.page.to_string(), query.per_page.to_string());
        let url = self.repo_url(
            repo,
            &["issues"],
            &[
                ("state", state_param(query.state)),
                ("sort", "updated"),
                ("direction", "desc"),
                ("per_page", &per_page),
                ("page", &page),
            ],
        );
        // GitHub lists pull requests as issues too; those carry `pull_request`.
        self.page(url, query.page, |i: GhIssue| {
            i.pull_request.is_none().then(|| i.into())
        })
        .await
    }

    async fn create_issue(&self, repo: &RepoId, new: &NewIssue) -> HostResult<Issue> {
        let body = json!({ "title": new.title, "body": new.body, "labels": new.labels });
        let url = self.repo_url(repo, &["issues"], &[]);
        let answer = self.api.send_json(Method::Post, url, &body).await?;
        self.invalidate(Some(repo));
        Ok(self.api.decode::<GhIssue>(&answer)?.into())
    }

    async fn comment_on_issue(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
    ) -> HostResult<Comment> {
        let n = number.to_string();
        let url = self.repo_url(repo, &["issues", &n, "comments"], &[]);
        let answer = self
            .api
            .send_json(Method::Post, url, &json!({ "body": body }))
            .await?;
        self.invalidate(Some(repo));
        let c: GhComment = self.api.decode(&answer)?;
        Ok(Comment {
            id: c.id,
            author: c.user.map(Into::into),
            body: c.body.unwrap_or_default(),
            web_url: c.html_url,
            created_at: c.created_at,
        })
    }

    /// Check runs (Actions and apps) plus legacy commit statuses. A token without the checks
    /// permission still gets the statuses.
    async fn ci_status(&self, repo: &RepoId, reference: &str) -> HostResult<CiStatus> {
        let runs_url = self.repo_url(
            repo,
            &["commits", reference, "check-runs"],
            &[("per_page", "100")],
        );
        let status_url = self.repo_url(
            repo,
            &["commits", reference, "status"],
            &[("per_page", "100")],
        );
        let mut checks = Vec::new();
        match self.api.get(runs_url).await {
            Ok(fetched) => {
                let runs: GhCheckRuns = self.api.decode(&fetched.body)?;
                checks.extend(runs.check_runs.into_iter().map(|run| Check {
                    state: run_state(&run.status, run.conclusion.as_deref()),
                    name: run.name,
                    description: run.output.and_then(|o| o.title),
                    url: run.html_url.or(run.details_url),
                }));
            }
            Err(HostError::Forbidden { .. }) => {}
            Err(err) => return Err(err),
        }
        let fetched = self.api.get(status_url).await?;
        let combined: GhCombinedStatus = self.api.decode(&fetched.body)?;
        checks.extend(combined.statuses.into_iter().map(|s| Check {
            state: match s.state.as_str() {
                "success" => CiState::Success,
                "pending" => CiState::Pending,
                _ => CiState::Failure,
            },
            name: s.context,
            description: s.description,
            url: s.target_url,
        }));
        Ok(CiStatus::from_checks(checks))
    }

    /// The GitHub account that authored `commit`; GitHub matches the commit's email itself.
    /// A commit whose email belongs to no account has a null `author`.
    async fn author_avatar(
        &self,
        repo: &RepoId,
        commit: &str,
        _email: &str,
    ) -> HostResult<Option<String>> {
        let fetched = self
            .api
            .get(self.repo_url(repo, &["commits", commit], &[]))
            .await?;
        let found: GhCommitAuthor = self.api.decode(&fetched.body)?;
        Ok(found
            .author
            .and_then(|a| a.avatar_url)
            .filter(|url| !url.is_empty())
            .map(|url| sized(&url)))
    }
}

/// `url` asking for a 64 px image (avatars are drawn small), unless it names a size already.
fn sized(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) if !parsed.query_pairs().any(|(k, _)| k == "s") => {
            parsed.query_pairs_mut().append_pair("s", "64");
            parsed.to_string()
        }
        _ => url.to_string(),
    }
}

fn run_state(status: &str, conclusion: Option<&str>) -> CiState {
    if status != "completed" {
        return CiState::Pending;
    }
    match conclusion {
        Some("success" | "neutral" | "skipped") => CiState::Success,
        Some(_) => CiState::Failure,
        None => CiState::Pending,
    }
}

// GitHub's JSON, only the fields we use.

#[derive(Deserialize)]
struct GhUser {
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

impl From<GhUser> for Person {
    fn from(u: GhUser) -> Self {
        Person {
            login: u.login,
            name: u.name.filter(|n| !n.is_empty()),
            avatar_url: u.avatar_url.filter(|n| !n.is_empty()),
            web_url: u.html_url,
        }
    }
}

/// `GET /repos/{o}/{r}/commits/{sha}`, only who wrote it.
#[derive(Deserialize)]
struct GhCommitAuthor {
    #[serde(default)]
    author: Option<GhAvatar>,
}

#[derive(Deserialize)]
struct GhAvatar {
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GhOwner {
    login: String,
}

#[derive(Deserialize)]
struct GhRepo {
    name: String,
    owner: GhOwner,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    default_branch: Option<String>,
    html_url: String,
    #[serde(default)]
    clone_url: Option<String>,
    #[serde(default)]
    ssh_url: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
    #[serde(default)]
    color: Option<String>,
}

fn labels(raw: Vec<GhLabel>) -> Vec<Label> {
    raw.into_iter()
        .map(|l| Label {
            name: l.name,
            color: l.color.filter(|c| !c.is_empty()),
        })
        .collect()
}

#[derive(Deserialize)]
struct GhRepoRef {
    full_name: String,
}

#[derive(Deserialize)]
struct GhBranchRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    #[serde(default)]
    repo: Option<GhRepoRef>,
}

#[derive(Deserialize)]
struct GhPull {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    head: GhBranchRef,
    base: GhBranchRef,
    #[serde(default)]
    labels: Vec<GhLabel>,
    html_url: String,
    created_at: String,
    updated_at: String,
}

impl From<GhPull> for PullRequest {
    fn from(p: GhPull) -> Self {
        let state = match (p.state.as_str(), &p.merged_at) {
            (_, Some(_)) => PrState::Merged,
            ("open", None) => PrState::Open,
            _ => PrState::Closed,
        };
        let base_repo = p.base.repo.as_ref().map(|r| r.full_name.as_str());
        let source_repo = p
            .head
            .repo
            .as_ref()
            .map(|r| r.full_name.clone())
            .filter(|head| Some(head.as_str()) != base_repo);
        PullRequest {
            number: p.number,
            title: p.title,
            body: p.body,
            state,
            draft: p.draft,
            author: p.user.map(Into::into),
            source_branch: p.head.name,
            source_repo,
            target_branch: p.base.name,
            head_sha: p.head.sha,
            labels: labels(p.labels),
            web_url: p.html_url,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    comments: u32,
    html_url: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    pull_request: Option<Value>,
}

impl From<GhIssue> for Issue {
    fn from(i: GhIssue) -> Self {
        Issue {
            number: i.number,
            title: i.title,
            body: i.body,
            state: if i.state == "open" {
                IssueState::Open
            } else {
                IssueState::Closed
            },
            author: i.user.map(Into::into),
            assignees: i.assignees.into_iter().map(Into::into).collect(),
            labels: labels(i.labels),
            comments: i.comments,
            web_url: i.html_url,
            created_at: i.created_at,
            updated_at: i.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct GhComment {
    id: u64,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    created_at: String,
}

#[derive(Deserialize)]
struct GhCheckRuns {
    #[serde(default)]
    check_runs: Vec<GhCheckRun>,
}

#[derive(Deserialize)]
struct GhCheckOutput {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct GhCheckRun {
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    output: Option<GhCheckOutput>,
}

#[derive(Deserialize)]
struct GhCombinedStatus {
    #[serde(default)]
    statuses: Vec<GhStatus>,
}

#[derive(Deserialize)]
struct GhStatus {
    context: String,
    state: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_bases() {
        let gh = |s: &str| api_base(&Url::parse(s).unwrap()).to_string();
        assert_eq!(gh("https://github.com"), "https://api.github.com/");
        assert_eq!(
            gh("https://ghe.example.com"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(
            gh("https://ghe.example.com:8443/"),
            "https://ghe.example.com:8443/api/v3"
        );
    }

    #[test]
    fn run_states() {
        assert_eq!(run_state("queued", None), CiState::Pending);
        assert_eq!(run_state("in_progress", None), CiState::Pending);
        assert_eq!(run_state("completed", Some("success")), CiState::Success);
        assert_eq!(run_state("completed", Some("skipped")), CiState::Success);
        assert_eq!(run_state("completed", Some("timed_out")), CiState::Failure);
        assert_eq!(
            run_state("completed", Some("action_required")),
            CiState::Failure
        );
    }

    #[test]
    fn avatar_sizes() {
        assert_eq!(
            sized("https://avatars.githubusercontent.com/u/1?v=4"),
            "https://avatars.githubusercontent.com/u/1?v=4&s=64"
        );
        assert_eq!(
            sized("https://avatars.githubusercontent.com/u/1?s=200&v=4"),
            "https://avatars.githubusercontent.com/u/1?s=200&v=4"
        );
        assert_eq!(sized("not a url"), "not a url");
    }

    #[test]
    fn needs_https() {
        let t: Arc<dyn Transport> = Arc::new(crate::replay::Replay::new());
        assert!(GitHub::new("http://ghe.example.com", None, t.clone()).is_err());
        assert!(GitHub::new("not a url", None, t.clone()).is_err());
        assert!(GitHub::new("https://github.com/", None, t).is_ok());
    }
}
