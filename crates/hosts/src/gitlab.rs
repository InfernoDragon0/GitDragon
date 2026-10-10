//! GitLab.com and self-managed GitLab over REST v4 at `<base>/api/v4` (the base may carry a
//! port and a path prefix). A project is addressed by its URL-encoded full path as ONE path
//! segment (`projects/group%2Fsub%2Fproj`), so subgroups live in [`RepoId::owner`].

use std::sync::Arc;
use std::time::Duration;

use accounts::{HostKind, Secret};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::api::{ApiClient, RateLimit};
use crate::boards::{Board, BoardColumn, BoardItem, BoardPage};
use crate::error::HostError;
use crate::host::{GitHost, HostResult};
use crate::http::{Auth, Method, Transport};
use crate::model::*;
use crate::remote::RepoId;

/// Issue boards.
#[path = "boards/gl.rs"]
mod boards;

/// Notes, approvals and diff discussions.
#[path = "threads/gl.rs"]
mod threads;

pub struct GitLab {
    api: ApiClient,
    /// Web base, e.g. `https://gitlab.com` or `https://git.example.com:8443/gitlab`.
    web: Url,
}

impl GitLab {
    /// `web_base` is the account's base URL; HTTPS only. A personal access token goes in the
    /// `PRIVATE-TOKEN` header, an OAuth access token (`oauth`) is sent as a Bearer token.
    /// Refreshing an expired OAuth token is the caller's job: refresh, then [`Self::set_token`].
    pub fn new(
        web_base: &str,
        token: Option<Secret>,
        oauth: bool,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, HostError> {
        let web = Url::parse(web_base.trim().trim_end_matches('/'))
            .map_err(|_| HostError::Config(format!("bad GitLab URL: {web_base}")))?;
        if web.scheme() != "https" || web.host_str().is_none() {
            return Err(HostError::Config(format!(
                "GitLab needs an https:// URL, got {web_base}"
            )));
        }
        let api = ApiClient::new(
            transport,
            api_base(&web),
            vec![("accept".into(), "application/json".into())],
        );
        let gitlab = Self { api, web };
        gitlab.set_token(token, oauth);
        Ok(gitlab)
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.api = self.api.with_ttl(ttl);
        self
    }

    /// Swaps the token (re-sign-in, OAuth refresh); clears the cache.
    pub fn set_token(&self, token: Option<Secret>, oauth: bool) {
        self.api.set_auth(token.filter(|t| !t.is_empty()).map(|t| {
            if oauth {
                Auth::Bearer(t)
            } else {
                Auth::PrivateToken(t)
            }
        }));
    }

    fn project_url(&self, repo: &RepoId, rest: &[&str], query: &[(&str, &str)]) -> Url {
        let project = project_path(repo);
        let mut segments = vec!["projects", project.as_str()];
        segments.extend_from_slice(rest);
        self.api.url(&segments, query)
    }

    fn web(&self, repo: &RepoId, rest: &[&str]) -> Url {
        let mut url = self.web.clone();
        url.path_segments_mut()
            .expect("https base")
            .pop_if_empty()
            .extend(repo.owner.split('/').filter(|s| !s.is_empty()))
            .push(&repo.repo)
            .extend(rest);
        url
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
            last: fetched.last_page,
        })
    }
}

fn api_base(web: &Url) -> Url {
    let mut url = web.clone();
    url.path_segments_mut()
        .expect("https base")
        .pop_if_empty()
        .extend(["api", "v4"]);
    url
}

/// `group/sub/proj`: the single path segment GitLab wants after `projects/`.
fn project_path(repo: &RepoId) -> String {
    format!("{}/{}", repo.owner, repo.repo)
}

/// GitLab's "merged" is its own state, so `Closed` asks for closed-unmerged only and `All`
/// includes merged requests.
fn state_param(state: StateFilter) -> &'static str {
    match state {
        StateFilter::Open => "opened",
        StateFilter::Closed => "closed",
        StateFilter::All => "all",
    }
}

fn looks_like_sha(reference: &str) -> bool {
    matches!(reference.len(), 40 | 64) && reference.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Pipeline and job statuses. A failed job that is allowed to fail is a warning, not a failure.
fn ci_state(status: &str, allow_failure: bool) -> CiState {
    match status {
        "success" | "skipped" | "manual" => CiState::Success,
        "failed" if allow_failure => CiState::Success,
        "failed" | "canceled" => CiState::Failure,
        _ => CiState::Pending,
    }
}

#[async_trait]
impl GitHost for GitLab {
    fn kind(&self) -> HostKind {
        HostKind::GitLab
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
                | Feature::CloseIssue
                | Feature::CiStatus
                | Feature::Avatars
                | Feature::Boards
                | Feature::Timeline
                | Feature::Reviews
                | Feature::ReviewComments
                | Feature::PrFiles
        )
    }

    fn web_url(&self, repo: &RepoId, target: &WebTarget) -> Option<String> {
        let n;
        let rest: Vec<&str> = match target {
            WebTarget::Repo => vec![],
            WebTarget::Commit(sha) => vec!["-", "commit", sha],
            WebTarget::Branch(branch) => vec!["-", "tree", branch],
            WebTarget::PullRequest(number) => {
                n = number.to_string();
                vec!["-", "merge_requests", &n]
            }
            WebTarget::PullRequests => vec!["-", "merge_requests"],
            WebTarget::Issue(number) => {
                n = number.to_string();
                vec!["-", "issues", &n]
            }
            WebTarget::Issues => vec!["-", "issues"],
            WebTarget::NewPullRequest { .. } => vec!["-", "merge_requests", "new"],
        };
        let mut url = self.web(repo, &rest);
        if let WebTarget::NewPullRequest { source, target } = target {
            url.query_pairs_mut()
                .append_pair("merge_request[source_branch]", source)
                .append_pair("merge_request[target_branch]", target);
        }
        Some(url.to_string())
    }

    fn pull_head_ref(&self, number: u64) -> Option<String> {
        Some(format!("merge-requests/{number}/head"))
    }

    fn rate_limit(&self) -> RateLimit {
        self.api.rate_limit()
    }

    fn invalidate(&self, repo: Option<&RepoId>) {
        match repo {
            Some(repo) => {
                self.api.invalidate(&["projects", &project_path(repo)]);
                // Group boards and their issues.
                self.api.invalidate(&["groups", &repo.owner]);
            }
            None => self.api.clear_cache(),
        }
    }

    async fn whoami(&self) -> HostResult<Person> {
        let fetched = self.api.get(self.api.url(&["user"], &[])).await?;
        let user: GlUser = self.api.decode(&fetched.body)?;
        Ok(user.into())
    }

    async fn repos(&self, page: u32) -> HostResult<Page<RepoSummary>> {
        let page_s = page.to_string();
        let url = self.api.url(
            &["projects"],
            &[
                ("membership", "true"),
                ("order_by", "last_activity_at"),
                ("per_page", "50"),
                ("page", &page_s),
            ],
        );
        self.page(url, page, |p: GlProject| {
            // `path_with_namespace` is `<full group path>/<project path>`.
            let (owner, name) = p.path_with_namespace.rsplit_once('/')?;
            Some(RepoSummary {
                id: self.repo_id(owner, name),
                description: p.description.filter(|d| !d.is_empty()),
                private: p.visibility.is_some_and(|v| v != "public"),
                fork: p.forked_from_project.is_some(),
                default_branch: p.default_branch,
                web_url: p.web_url,
                clone_url: p.http_url_to_repo,
                ssh_url: p.ssh_url_to_repo,
                updated_at: p.last_activity_at,
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
        let url = self.project_url(
            repo,
            &["merge_requests"],
            &[
                ("state", state_param(query.state)),
                ("order_by", "updated_at"),
                ("sort", "desc"),
                ("per_page", &per_page),
                ("page", &page),
            ],
        );
        self.page(url, query.page, |m: GlMergeRequest| Some(m.into()))
            .await
    }

    async fn pull_request(&self, repo: &RepoId, number: u64) -> HostResult<PullRequest> {
        let n = number.to_string();
        let fetched = self
            .api
            .get(self.project_url(repo, &["merge_requests", &n], &[]))
            .await?;
        Ok(self.api.decode::<GlMergeRequest>(&fetched.body)?.into())
    }

    async fn create_pull_request(
        &self,
        repo: &RepoId,
        new: &NewPullRequest,
    ) -> HostResult<PullRequest> {
        let title = if new.draft && !new.title.starts_with("Draft:") {
            format!("Draft: {}", new.title)
        } else {
            new.title.clone()
        };
        let mut body = Map::new();
        body.insert("source_branch".into(), json!(new.source_branch));
        body.insert("target_branch".into(), json!(new.target_branch));
        body.insert("title".into(), json!(title));
        if let Some(text) = &new.body {
            body.insert("description".into(), json!(text));
        }
        let url = self.project_url(repo, &["merge_requests"], &[]);
        let answer = self
            .api
            .send_json(Method::Post, url, &Value::Object(body))
            .await?;
        self.invalidate(Some(repo));
        Ok(self.api.decode::<GlMergeRequest>(&answer)?.into())
    }

    async fn issues(&self, repo: &RepoId, query: &ListQuery) -> HostResult<Page<Issue>> {
        let (page, per_page) = (query.page.to_string(), query.per_page.to_string());
        let url = self.project_url(
            repo,
            &["issues"],
            &[
                ("state", state_param(query.state)),
                ("order_by", "updated_at"),
                ("sort", "desc"),
                ("per_page", &per_page),
                ("page", &page),
            ],
        );
        self.page(url, query.page, |i: GlIssue| Some(i.into()))
            .await
    }

    async fn create_issue(&self, repo: &RepoId, new: &NewIssue) -> HostResult<Issue> {
        let mut body = Map::new();
        body.insert("title".into(), json!(new.title));
        if let Some(text) = &new.body {
            body.insert("description".into(), json!(text));
        }
        if !new.labels.is_empty() {
            body.insert("labels".into(), json!(new.labels.join(",")));
        }
        let url = self.project_url(repo, &["issues"], &[]);
        let answer = self
            .api
            .send_json(Method::Post, url, &Value::Object(body))
            .await?;
        self.invalidate(Some(repo));
        Ok(self.api.decode::<GlIssue>(&answer)?.into())
    }

    async fn comment_on_issue(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
    ) -> HostResult<Comment> {
        let n = number.to_string();
        let url = self.project_url(repo, &["issues", &n, "notes"], &[]);
        let answer = self
            .api
            .send_json(Method::Post, url, &json!({ "body": body }))
            .await?;
        self.invalidate(Some(repo));
        let note: GlNote = self.api.decode(&answer)?;
        Ok(Comment {
            id: note.id,
            author: note.author.map(Into::into),
            body: note.body.unwrap_or_default(),
            web_url: None,
            created_at: note.created_at,
        })
    }

    async fn set_issue_state(&self, repo: &RepoId, number: u64, open: bool) -> HostResult<Issue> {
        let n = number.to_string();
        let url = self.project_url(repo, &["issues", &n], &[]);
        let event = if open { "reopen" } else { "close" };
        let answer = self
            .api
            .send_json(Method::Put, url, &json!({ "state_event": event }))
            .await?;
        self.invalidate(Some(repo));
        Ok(self.api.decode::<GlIssue>(&answer)?.into())
    }

    async fn timeline(
        &self,
        repo: &RepoId,
        thread: Thread,
        page: u32,
    ) -> HostResult<Page<TimelineItem>> {
        threads::timeline(self, repo, thread, page).await
    }

    async fn pr_reviews(&self, repo: &RepoId, number: u64, page: u32) -> HostResult<Page<Review>> {
        threads::reviews(self, repo, number, page).await
    }

    async fn pr_review_comments(
        &self,
        repo: &RepoId,
        number: u64,
        page: u32,
    ) -> HostResult<Page<ReviewComment>> {
        threads::review_comments(self, repo, number, page).await
    }

    /// `merge_requests/:iid/diffs` (GitLab 15.7+), 100 a page; `diff` is empty for binary,
    /// collapsed and too large files. Line counts come from the hunks.
    async fn pr_files(&self, repo: &RepoId, number: u64) -> HostResult<PrFiles> {
        let n = number.to_string();
        let per_page = PR_FILES_PER_PAGE.to_string();
        let mut out = PrFiles::default();
        for page in 1..=PR_FILES_MAX_PAGES {
            let p = page.to_string();
            let url = self.project_url(
                repo,
                &["merge_requests", &n, "diffs"],
                &[("page", &p), ("per_page", &per_page)],
            );
            let fetched = self.api.get(url).await?;
            let raw: Vec<GlDiff> = self.api.decode(&fetched.body)?;
            out.files.extend(raw.into_iter().map(PrFile::from));
            if !fetched.next_page {
                // A host may stop at its cap without saying so.
                out.truncated =
                    out.files.len() as u64 >= u64::from(PR_FILES_PER_PAGE * PR_FILES_MAX_PAGES);
                return Ok(out);
            }
        }
        out.truncated = true;
        Ok(out)
    }

    /// The latest pipeline for a branch / tag / sha, one check per job. Without the permission
    /// to list jobs the pipeline's own status is the single check.
    async fn ci_status(&self, repo: &RepoId, reference: &str) -> HostResult<CiStatus> {
        let key = if looks_like_sha(reference) {
            "sha"
        } else {
            "ref"
        };
        let url = self.project_url(repo, &["pipelines"], &[(key, reference), ("per_page", "1")]);
        let fetched = self.api.get(url).await?;
        let pipelines: Vec<GlPipeline> = self.api.decode(&fetched.body)?;
        let Some(pipeline) = pipelines.into_iter().next() else {
            return Ok(CiStatus::from_checks(Vec::new()));
        };
        let id = pipeline.id.to_string();
        let jobs_url = self.project_url(repo, &["pipelines", &id, "jobs"], &[("per_page", "100")]);
        let checks = match self.api.get(jobs_url).await {
            Ok(fetched) => {
                let jobs: Vec<GlJob> = self.api.decode(&fetched.body)?;
                jobs.into_iter()
                    .map(|job| Check {
                        state: ci_state(&job.status, job.allow_failure),
                        name: job.name,
                        description: job.stage,
                        url: job.web_url,
                    })
                    .collect()
            }
            Err(HostError::Forbidden { .. }) => vec![Check {
                state: ci_state(&pipeline.status, false),
                name: format!("Pipeline #{}", pipeline.id),
                description: Some(pipeline.status),
                url: pipeline.web_url,
            }],
            Err(err) => return Err(err),
        };
        Ok(CiStatus::from_checks(checks))
    }

    /// GitLab's own answer for `email` (`GET /avatar`; `commit` is not needed): the matching
    /// user's avatar, else a Gravatar URL. A non-https URL is dropped: avatars are fetched
    /// over HTTPS only.
    async fn author_avatar(
        &self,
        _repo: &RepoId,
        _commit: &str,
        email: &str,
    ) -> HostResult<Option<String>> {
        let url = self
            .api
            .url(&["avatar"], &[("email", email), ("size", "64")]);
        let fetched = self.api.get(url).await?;
        let found: GlAvatar = self.api.decode(&fetched.body)?;
        Ok(found
            .avatar_url
            .filter(|url| url.starts_with("https://") && url.len() > "https://".len()))
    }

    async fn boards(&self, repo: &RepoId) -> HostResult<Vec<Board>> {
        boards::boards(self, repo).await
    }

    async fn board(
        &self,
        _repo: &RepoId,
        board: &Board,
        cursor: Option<&str>,
    ) -> HostResult<BoardPage> {
        boards::board(self, board, cursor).await
    }

    async fn move_board_item(
        &self,
        repo: &RepoId,
        board: &Board,
        item: &BoardItem,
        to: &BoardColumn,
    ) -> HostResult<BoardItem> {
        boards::move_item(self, repo, board, item, to).await
    }

    async fn board_item(
        &self,
        _repo: &RepoId,
        board: &Board,
        item: &BoardItem,
    ) -> HostResult<BoardItem> {
        boards::item_detail(self, board, item).await
    }
}

// GitLab's JSON, only the fields we use.

#[derive(Deserialize)]
struct GlUser {
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    web_url: Option<String>,
}

impl From<GlUser> for Person {
    fn from(u: GlUser) -> Self {
        Person {
            login: u.username,
            name: u.name.filter(|n| !n.is_empty()),
            avatar_url: u.avatar_url.filter(|n| !n.is_empty()),
            web_url: u.web_url,
        }
    }
}

/// `GET /avatar?email=`.
#[derive(Deserialize)]
struct GlAvatar {
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GlProject {
    path_with_namespace: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    forked_from_project: Option<Value>,
    #[serde(default)]
    default_branch: Option<String>,
    web_url: String,
    #[serde(default)]
    http_url_to_repo: Option<String>,
    #[serde(default)]
    ssh_url_to_repo: Option<String>,
    #[serde(default)]
    last_activity_at: Option<String>,
}

/// Labels are plain names unless `with_labels_details` is asked for, so no colour.
#[derive(Deserialize)]
struct GlDiff {
    old_path: String,
    new_path: String,
    #[serde(default)]
    diff: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    renamed_file: bool,
    #[serde(default)]
    deleted_file: bool,
}

impl From<GlDiff> for PrFile {
    fn from(d: GlDiff) -> Self {
        let status = if d.new_file {
            PrFileStatus::Added
        } else if d.deleted_file {
            PrFileStatus::Removed
        } else if d.renamed_file {
            PrFileStatus::Renamed
        } else {
            PrFileStatus::Modified
        };
        let (additions, deletions) = patch_line_counts(&d.diff);
        let deleted = status == PrFileStatus::Removed;
        PrFile {
            path: if deleted {
                d.old_path.clone()
            } else {
                d.new_path
            },
            old_path: (status == PrFileStatus::Renamed).then_some(d.old_path),
            status,
            additions,
            deletions,
            patch: Some(d.diff).filter(|p| !p.trim().is_empty()),
        }
    }
}

fn labels(raw: Vec<String>) -> Vec<Label> {
    raw.into_iter()
        .map(|name| Label { name, color: None })
        .collect()
}

#[derive(Deserialize)]
struct GlMergeRequest {
    iid: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    work_in_progress: Option<bool>,
    #[serde(default)]
    author: Option<GlUser>,
    source_branch: String,
    target_branch: String,
    #[serde(default)]
    sha: Option<String>,
    /// Only the single-MR endpoint has it (the list does not): None then.
    #[serde(default)]
    diff_refs: Option<GlDiffRefs>,
    #[serde(default)]
    labels: Vec<String>,
    web_url: String,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct GlDiffRefs {
    #[serde(default)]
    base_sha: Option<String>,
}

impl From<GlMergeRequest> for PullRequest {
    fn from(m: GlMergeRequest) -> Self {
        let state = match m.state.as_str() {
            "merged" => PrState::Merged,
            "closed" => PrState::Closed,
            // `opened`, and `locked` (being merged right now, not final yet).
            _ => PrState::Open,
        };
        PullRequest {
            number: m.iid,
            title: m.title,
            body: m.description.filter(|d| !d.is_empty()),
            state,
            draft: m.draft.or(m.work_in_progress).unwrap_or(false),
            author: m.author.map(Into::into),
            source_branch: m.source_branch,
            // The payload only has project ids, not a path to show.
            source_repo: None,
            target_branch: m.target_branch,
            head_sha: m.sha.unwrap_or_default(),
            base_sha: m
                .diff_refs
                .and_then(|d| d.base_sha)
                .filter(|s| !s.is_empty()),
            labels: labels(m.labels),
            web_url: m.web_url,
            created_at: m.created_at,
            updated_at: m.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct GlIssue {
    iid: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String,
    #[serde(default)]
    author: Option<GlUser>,
    #[serde(default)]
    assignees: Vec<GlUser>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    user_notes_count: u32,
    web_url: String,
    created_at: String,
    updated_at: String,
}

impl From<GlIssue> for Issue {
    fn from(i: GlIssue) -> Self {
        Issue {
            number: i.iid,
            title: i.title,
            body: i.description.filter(|d| !d.is_empty()),
            state: if i.state == "opened" {
                IssueState::Open
            } else {
                IssueState::Closed
            },
            author: i.author.map(Into::into),
            assignees: i.assignees.into_iter().map(Into::into).collect(),
            labels: labels(i.labels),
            comments: i.user_notes_count,
            web_url: i.web_url,
            created_at: i.created_at,
            updated_at: i.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct GlNote {
    id: u64,
    #[serde(default)]
    author: Option<GlUser>,
    #[serde(default)]
    body: Option<String>,
    created_at: String,
}

#[derive(Deserialize)]
struct GlPipeline {
    id: u64,
    status: String,
    #[serde(default)]
    web_url: Option<String>,
}

#[derive(Deserialize)]
struct GlJob {
    name: String,
    status: String,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    allow_failure: bool,
    #[serde(default)]
    web_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_bases() {
        let gl = |s: &str| api_base(&Url::parse(s).unwrap()).to_string();
        assert_eq!(gl("https://gitlab.com"), "https://gitlab.com/api/v4");
        assert_eq!(
            gl("https://git.example.com:8443/gitlab/"),
            "https://git.example.com:8443/gitlab/api/v4"
        );
    }

    #[test]
    fn ci_states() {
        for s in ["success", "skipped", "manual"] {
            assert_eq!(ci_state(s, false), CiState::Success, "{s}");
        }
        for s in ["failed", "canceled"] {
            assert_eq!(ci_state(s, false), CiState::Failure, "{s}");
        }
        for s in [
            "created",
            "pending",
            "running",
            "preparing",
            "waiting_for_resource",
            "scheduled",
        ] {
            assert_eq!(ci_state(s, false), CiState::Pending, "{s}");
        }
        assert_eq!(
            ci_state("failed", true),
            CiState::Success,
            "allowed to fail"
        );
    }

    #[test]
    fn sha_or_ref() {
        assert!(looks_like_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!looks_like_sha("main"));
        assert!(!looks_like_sha("0123456789abcdef0123456789abcdef0123456z"));
        assert!(!looks_like_sha("abc1234"));
    }

    #[test]
    fn needs_https() {
        let t: Arc<dyn Transport> = Arc::new(crate::replay::Replay::new());
        assert!(GitLab::new("http://git.example.com", None, false, t.clone()).is_err());
        assert!(GitLab::new("not a url", None, false, t.clone()).is_err());
        assert!(GitLab::new("https://gitlab.com/", None, false, t).is_ok());
    }
}
