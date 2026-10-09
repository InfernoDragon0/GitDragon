//! Azure DevOps Services (`https://dev.azure.com/<org>`, legacy `<org>.visualstudio.com`) over
//! REST 7.1, on the crate's own [`ApiClient`] like GitHub (no Azure SDK crate).
//!
//! - Auth: PATs go as Basic with an empty user, OAuth / Entra tokens as Bearer. A GCM account
//!   has no token here: calls answer 401 and surface as [`HostError::Unauthorized`].
//! - Every request carries `api-version=7.1` (a few endpoints need a preview version).
//! - Issues are work items: a WIQL query lists the ids, a batch GET fetches them.
//! - Repos are addressed `{project}/_apis/git/repositories/{repo}`, so a [`RepoId`] needs its
//!   project.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use accounts::{HostKind, Secret};
use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use crate::api::{ApiClient, RateLimit};
use crate::error::HostError;
use crate::host::{GitHost, HostResult};
use crate::http::{Auth, Method, Transport};
use crate::model::*;
use crate::remote::RepoId;

const API_VERSION: &str = "7.1";
/// `connectionData` only exists as a preview resource.
const CONNECTION_VERSION: &str = "7.1-preview";
const COMMENTS_VERSION: &str = "7.1-preview.4";
/// Work items per batch GET (Azure's limit).
const MAX_IDS: usize = 200;
/// Azure refuses a WIQL `$top` above this.
const WIQL_MAX: usize = 20_000;
/// Recent builds looked at for CI status.
const BUILDS_TOP: &str = "25";
const CLOSED_STATES: &str = "'Closed','Done','Removed','Resolved'";
const WORK_ITEM_FIELDS: &str = "System.Id,System.Title,System.State,System.CreatedBy,System.AssignedTo,System.Tags,System.CreatedDate,System.ChangedDate,System.CommentCount,System.Description";
/// Work item type `create_issue` makes: Agile, Basic and CMMI processes have "Issue"; Scrum
/// projects do not (they would need "Product Backlog Item" or "Bug").
const NEW_ITEM_TYPE: &str = "$Issue";

pub struct AzureDevOps {
    api: ApiClient,
    /// Organisation URL, e.g. `https://dev.azure.com/org`; also the API base.
    web: Url,
    org: String,
}

impl AzureDevOps {
    /// `web_base` is the account's base URL (`https://dev.azure.com/<org>` or
    /// `https://<org>.visualstudio.com`); HTTPS only. `oauth` tokens go as Bearer, PATs as Basic.
    pub fn new(
        web_base: &str,
        token: Option<Secret>,
        oauth: bool,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, HostError> {
        let web = Url::parse(web_base.trim().trim_end_matches('/'))
            .map_err(|_| HostError::Config(format!("bad Azure DevOps URL: {web_base}")))?;
        if web.scheme() != "https" || web.host_str().is_none() {
            return Err(HostError::Config(format!(
                "Azure DevOps needs an https:// URL, got {web_base}"
            )));
        }
        let org = org_of(&web).ok_or_else(|| {
            HostError::Config(format!(
                "Azure DevOps needs https://dev.azure.com/<organization>, got {web_base}"
            ))
        })?;
        let api = ApiClient::new(
            transport,
            web.clone(),
            vec![("accept".into(), "application/json".into())],
        );
        let azure = Self { api, web, org };
        azure.set_token(token, oauth);
        Ok(azure)
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.api = self.api.with_ttl(ttl);
        self
    }

    /// Swaps the token (re-sign-in or OAuth refresh); clears the cache.
    pub fn set_token(&self, token: Option<Secret>, oauth: bool) {
        self.api.set_auth(token.filter(|t| !t.is_empty()).map(|t| {
            if oauth {
                Auth::Bearer(t)
            } else {
                Auth::Basic(t)
            }
        }));
    }

    /// API URL with `api-version=7.1`.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        self.url_v(segments, query, API_VERSION)
    }

    fn url_v(&self, segments: &[&str], query: &[(&str, &str)], version: &str) -> Url {
        let mut query = query.to_vec();
        query.push(("api-version", version));
        self.api.url(segments, &query)
    }

    fn repo_url(&self, repo: &RepoId, rest: &[&str], query: &[(&str, &str)]) -> HostResult<Url> {
        let mut segments = vec![
            project(repo)?,
            "_apis",
            "git",
            "repositories",
            repo.repo.as_str(),
        ];
        segments.extend_from_slice(rest);
        Ok(self.url(&segments, query))
    }

    fn web(&self, segments: &[&str], query: &[(&str, &str)]) -> String {
        let mut url = self.web.clone();
        url.path_segments_mut()
            .expect("https base")
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url.to_string()
    }

    fn repo_id(&self, project: &str, name: &str) -> RepoId {
        RepoId {
            host: self.web.host_str().unwrap_or_default().to_ascii_lowercase(),
            web_port: self.web.port(),
            owner: self.org.clone(),
            project: Some(project.to_string()),
            repo: name.to_string(),
        }
    }

    /// Azure answers a rejected PAT with a 203 and an HTML sign-in page; that is a 401 to us.
    fn decode<T: DeserializeOwned>(&self, body: &[u8]) -> HostResult<T> {
        if body.trim_ascii_start().starts_with(b"<") {
            return Err(HostError::Unauthorized {
                host: self.api.host().to_string(),
            });
        }
        self.api.decode(body)
    }

    fn pull(&self, project: &str, repo: &str, p: AzPull) -> PullRequest {
        let state = match p.status.as_str() {
            "active" => PrState::Open,
            "completed" => PrState::Merged,
            _ => PrState::Closed,
        };
        let source_repo = p.fork_source.and_then(|f| f.repository).map(|r| {
            match r.project.and_then(|p| p.name) {
                Some(project) => format!("{project}/{}", r.name),
                None => r.name,
            }
        });
        let id = p.pull_request_id.to_string();
        PullRequest {
            number: p.pull_request_id,
            title: p.title,
            body: non_empty(p.description),
            state,
            draft: p.is_draft,
            author: p.created_by.map(Into::into),
            source_branch: strip_heads(&p.source_ref_name).to_string(),
            source_repo,
            target_branch: strip_heads(&p.target_ref_name).to_string(),
            head_sha: p
                .last_merge_source_commit
                .map(|c| c.commit_id)
                .unwrap_or_default(),
            labels: (p.labels.into_iter())
                .filter(|l| l.active.unwrap_or(true))
                .map(|l| Label {
                    name: l.name,
                    color: None,
                })
                .collect(),
            web_url: self.web(&[project, "_git", repo, "pullrequest", &id], &[]),
            updated_at: p.closed_date.unwrap_or_else(|| p.creation_date.clone()),
            created_at: p.creation_date,
        }
    }

    fn issue(&self, project: &str, w: AzWorkItem) -> Issue {
        let f = w.fields;
        let closed = ["Closed", "Done", "Removed", "Resolved"].contains(&f.state.as_str());
        let id = w.id.to_string();
        Issue {
            number: w.id,
            title: f.title,
            body: non_empty(f.description),
            state: if closed {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            author: f.created_by.map(Into::into),
            assignees: f.assigned_to.into_iter().map(Into::into).collect(),
            labels: (f.tags.unwrap_or_default().split(';'))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| Label {
                    name: t.to_string(),
                    color: None,
                })
                .collect(),
            comments: f.comment_count,
            web_url: self.web(&[project, "_workitems", "edit", &id], &[]),
            created_at: f.created_date,
            updated_at: f.changed_date,
        }
    }
}

/// The organisation: first path segment on `dev.azure.com`, the sub-domain on `visualstudio.com`.
fn org_of(web: &Url) -> Option<String> {
    let host = web.host_str()?;
    if let Some(org) = host.strip_suffix(".visualstudio.com") {
        return Some(org.to_string());
    }
    web.path_segments()?
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn project(repo: &RepoId) -> HostResult<&str> {
    repo.project.as_deref().ok_or_else(|| {
        HostError::Config(format!(
            "{} has no Azure DevOps project in its URL",
            repo.full_name()
        ))
    })
}

fn strip_heads(name: &str) -> &str {
    name.strip_prefix("refs/heads/").unwrap_or(name)
}

fn branch_ref(name: &str) -> String {
    format!("refs/heads/{}", strip_heads(name))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

fn is_sha(reference: &str) -> bool {
    reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit())
}

fn build_state(status: &str, result: Option<&str>) -> CiState {
    if status != "completed" {
        return CiState::Pending;
    }
    match result {
        Some("succeeded" | "partiallySucceeded") => CiState::Success,
        Some(_) => CiState::Failure,
        None => CiState::Pending,
    }
}

#[async_trait]
impl GitHost for AzureDevOps {
    fn kind(&self) -> HostKind {
        HostKind::AzureDevOps
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
        )
    }

    fn web_url(&self, repo: &RepoId, target: &WebTarget) -> Option<String> {
        let (p, r) = (repo.project.as_deref()?, repo.repo.as_str());
        let n;
        Some(match target {
            WebTarget::Repo => self.web(&[p, "_git", r], &[]),
            WebTarget::Commit(sha) => self.web(&[p, "_git", r, "commit", sha], &[]),
            WebTarget::Branch(branch) => {
                let version = format!("GB{branch}");
                self.web(&[p, "_git", r], &[("version", &version)])
            }
            WebTarget::PullRequest(number) => {
                n = number.to_string();
                self.web(&[p, "_git", r, "pullrequest", &n], &[])
            }
            WebTarget::PullRequests => self.web(&[p, "_git", r, "pullrequests"], &[]),
            WebTarget::Issue(number) => {
                n = number.to_string();
                self.web(&[p, "_workitems", "edit", &n], &[])
            }
            WebTarget::Issues => self.web(&[p, "_workitems"], &[]),
            WebTarget::NewPullRequest { source, target } => self.web(
                &[p, "_git", r, "pullrequestcreate"],
                &[("sourceRef", source), ("targetRef", target)],
            ),
        })
    }

    fn rate_limit(&self) -> RateLimit {
        self.api.rate_limit()
    }

    fn invalidate(&self, repo: Option<&RepoId>) {
        match repo.and_then(|r| Some((r, r.project.as_deref()?))) {
            Some((repo, project)) => {
                self.api
                    .invalidate(&[project, "_apis", "git", "repositories", &repo.repo]);
                self.api.invalidate(&[project, "_apis", "build"]);
                // Work item batches are organisation-wide: drop them all.
                self.api.invalidate(&["_apis", "wit", "workitems"]);
            }
            None => self.api.clear_cache(),
        }
    }

    async fn whoami(&self) -> HostResult<Person> {
        let url = self.url_v(&["_apis", "connectionData"], &[], CONNECTION_VERSION);
        let fetched = self.api.get(url).await?;
        let data: AzConnection = self.decode(&fetched.body)?;
        let user = data.authenticated_user;
        let account = (user.properties.pointer("/Account/$value")).and_then(Value::as_str);
        let name = non_empty(user.provider_display_name);
        let login = non_empty(account.map(str::to_string))
            .or_else(|| name.clone())
            .ok_or_else(|| HostError::Decode {
                host: self.api.host().to_string(),
                message: "connectionData without a user".into(),
            })?;
        Ok(Person {
            login,
            name,
            avatar_url: None,
            web_url: None,
        })
    }

    /// Everything the account sees in the organisation; the API does not page.
    async fn repos(&self, page: u32) -> HostResult<Page<RepoSummary>> {
        if page > 1 {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        }
        let url = self.url(&["_apis", "git", "repositories"], &[]);
        let fetched = self.api.get(url).await?;
        let list: AzList<AzRepo> = self.decode(&fetched.body)?;
        let items = (list.value.into_iter())
            .filter(|r| !r.is_disabled)
            .filter_map(|r| {
                let project = r.project?;
                let web_url = r
                    .web_url
                    .unwrap_or_else(|| self.web(&[&project.name, "_git", &r.name], &[]));
                Some(RepoSummary {
                    id: self.repo_id(&project.name, &r.name),
                    description: None,
                    private: project.visibility.as_deref() != Some("public"),
                    fork: r.is_fork,
                    default_branch: r.default_branch.map(|b| strip_heads(&b).to_string()),
                    web_url,
                    clone_url: r.remote_url,
                    ssh_url: r.ssh_url,
                    updated_at: None,
                })
            })
            .collect();
        Ok(Page { items, next: None })
    }

    /// Closed asks Azure for every status and drops the active ones here, so a page can come
    /// back short while `next` still points on (paging follows the unfiltered count).
    async fn pull_requests(
        &self,
        repo: &RepoId,
        query: &ListQuery,
    ) -> HostResult<Page<PullRequest>> {
        let project = project(repo)?;
        let per_page = usize::from(query.per_page.max(1));
        let skip = (query.page.saturating_sub(1) as usize * per_page).to_string();
        let top = per_page.to_string();
        let status = match query.state {
            StateFilter::Open => "active",
            StateFilter::Closed | StateFilter::All => "all",
        };
        let url = self.repo_url(
            repo,
            &["pullrequests"],
            &[
                ("searchCriteria.status", status),
                ("$top", &top),
                ("$skip", &skip),
            ],
        )?;
        let fetched = self.api.get(url).await?;
        let list: AzList<AzPull> = self.decode(&fetched.body)?;
        let full = list.value.len() >= per_page;
        let items = (list.value.into_iter())
            .map(|p| self.pull(project, &repo.repo, p))
            .filter(|p| query.state != StateFilter::Closed || p.state != PrState::Open)
            .collect();
        Ok(Page {
            items,
            next: full.then_some(query.page.max(1) + 1),
        })
    }

    async fn pull_request(&self, repo: &RepoId, number: u64) -> HostResult<PullRequest> {
        let n = number.to_string();
        let fetched = self
            .api
            .get(self.repo_url(repo, &["pullrequests", &n], &[])?)
            .await?;
        let raw: AzPull = self.decode(&fetched.body)?;
        Ok(self.pull(project(repo)?, &repo.repo, raw))
    }

    async fn create_pull_request(
        &self,
        repo: &RepoId,
        new: &NewPullRequest,
    ) -> HostResult<PullRequest> {
        let mut body = json!({
            "sourceRefName": branch_ref(&new.source_branch),
            "targetRefName": branch_ref(&new.target_branch),
            "title": new.title,
            "isDraft": new.draft,
        });
        if let Some(description) = &new.body {
            body["description"] = json!(description);
        }
        let url = self.repo_url(repo, &["pullrequests"], &[])?;
        let answer = self.api.send_json(Method::Post, url, &body).await?;
        self.invalidate(Some(repo));
        let raw: AzPull = self.decode(&answer)?;
        Ok(self.pull(project(repo)?, &repo.repo, raw))
    }

    /// Work items of the project, newest change first. WIQL answers the ids (cut at the end
    /// of the wanted page, so one extra tells whether more follow), then one batch GET.
    async fn issues(&self, repo: &RepoId, query: &ListQuery) -> HostResult<Page<Issue>> {
        let project = project(repo)?;
        let per_page = usize::from(query.per_page).clamp(1, MAX_IDS);
        let page = query.page.max(1);
        let start = (page as usize - 1) * per_page;
        let end = start + per_page;
        let state = match query.state {
            StateFilter::Open => format!(" And [System.State] Not In ({CLOSED_STATES})"),
            StateFilter::Closed => format!(" And [System.State] In ({CLOSED_STATES})"),
            StateFilter::All => String::new(),
        };
        // `@project` is the project in the URL, so no caller text goes into the query.
        let wiql = format!(
            "Select [System.Id] From WorkItems Where [System.TeamProject] = @project{state} Order By [System.ChangedDate] Desc"
        );
        let top = (end + 1).min(WIQL_MAX).to_string();
        let url = self.url(&[project, "_apis", "wit", "wiql"], &[("$top", &top)]);
        let answer = (self.api)
            .send_json(Method::Post, url, &json!({ "query": wiql }))
            .await?;
        let found: AzWiql = self.decode(&answer)?;
        let more = found.work_items.len() > end;
        let ids: Vec<u64> = (found.work_items.iter())
            .skip(start)
            .take(per_page)
            .map(|w| w.id)
            .collect();
        let next = more.then_some(page + 1);
        if ids.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                next,
            });
        }
        let joined = (ids.iter())
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let url = self.url(
            &["_apis", "wit", "workitems"],
            &[("ids", &joined), ("fields", WORK_ITEM_FIELDS)],
        );
        let fetched = self.api.get(url).await?;
        let mut list: AzList<AzWorkItem> = self.decode(&fetched.body)?;
        // The batch GET does not promise the order of the ids.
        list.value
            .sort_by_key(|w| ids.iter().position(|id| *id == w.id));
        let items = (list.value.into_iter())
            .map(|w| self.issue(project, w))
            .collect();
        Ok(Page { items, next })
    }

    /// A work item of type `Issue` (see `NEW_ITEM_TYPE`); labels become tags. The description
    /// is HTML in Azure and is sent as given.
    async fn create_issue(&self, repo: &RepoId, new: &NewIssue) -> HostResult<Issue> {
        let project = project(repo)?;
        let field = |name: &str, value: &str| json!({ "op": "add", "path": format!("/fields/{name}"), "value": value });
        let mut ops = vec![field("System.Title", &new.title)];
        if let Some(body) = new.body.as_deref().filter(|b| !b.is_empty()) {
            ops.push(field("System.Description", body));
        }
        if !new.labels.is_empty() {
            ops.push(field("System.Tags", &new.labels.join("; ")));
        }
        let url = self.url(&[project, "_apis", "wit", "workitems", NEW_ITEM_TYPE], &[]);
        let answer = (self.api)
            .send_json_as(
                Method::Post,
                url,
                &Value::Array(ops),
                Some("application/json-patch+json"),
            )
            .await?;
        self.invalidate(Some(repo));
        let raw: AzWorkItem = self.decode(&answer)?;
        Ok(self.issue(project, raw))
    }

    async fn comment_on_issue(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
    ) -> HostResult<Comment> {
        let n = number.to_string();
        let url = self.url_v(
            &[project(repo)?, "_apis", "wit", "workItems", &n, "comments"],
            &[],
            COMMENTS_VERSION,
        );
        let answer = (self.api)
            .send_json(Method::Post, url, &json!({ "text": body }))
            .await?;
        self.invalidate(Some(repo));
        let c: AzComment = self.decode(&answer)?;
        Ok(Comment {
            id: c.id,
            author: c.created_by.map(Into::into),
            body: c.text.unwrap_or_default(),
            web_url: None,
            created_at: c.created_date,
        })
    }

    /// The latest build of every pipeline for a branch (or, for a 40-hex commit, among the
    /// recent builds of the repo). Builds are filtered by repository id, which one extra
    /// (cached) GET resolves from the name.
    async fn ci_status(&self, repo: &RepoId, reference: &str) -> HostResult<CiStatus> {
        let fetched = self.api.get(self.repo_url(repo, &[], &[])?).await?;
        let git: AzRepoRef = self.decode(&fetched.body)?;
        let by_commit = is_sha(reference);
        let branch = branch_ref(reference);
        let mut query = vec![
            ("repositoryId", git.id.as_str()),
            ("repositoryType", "TfsGit"),
        ];
        if !by_commit {
            query.push(("branchName", &branch));
        }
        query.extend([("$top", BUILDS_TOP), ("queryOrder", "queueTimeDescending")]);
        let url = self.url(&[project(repo)?, "_apis", "build", "builds"], &query);
        let fetched = self.api.get(url).await?;
        let list: AzList<AzBuild> = self.decode(&fetched.body)?;
        let mut pipelines = HashSet::new();
        let checks = (list.value.into_iter())
            .filter(|b| !by_commit || b.source_version.eq_ignore_ascii_case(reference))
            // Newest first: the first build of a pipeline is its current state.
            .filter(|b| pipelines.insert(b.definition.as_ref().map(|d| d.id)))
            .map(|b| {
                let text = b.result.as_deref().unwrap_or(&b.status);
                Check {
                    state: build_state(&b.status, b.result.as_deref()),
                    description: Some(format!("Build {}: {text}", b.build_number)),
                    name: b.definition.map_or_else(|| "Build".into(), |d| d.name),
                    url: b.links.and_then(|l| l.web).map(|w| w.href),
                }
            })
            .collect();
        Ok(CiStatus::from_checks(checks))
    }
}

// Azure's JSON, only the fields we use.

#[derive(Deserialize)]
struct AzList<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzPerson {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    unique_name: Option<String>,
    #[serde(default)]
    image_url: Option<String>,
}

impl From<AzPerson> for Person {
    fn from(p: AzPerson) -> Self {
        let name = non_empty(p.display_name);
        Person {
            login: non_empty(p.unique_name)
                .or_else(|| name.clone())
                .unwrap_or_default(),
            name,
            avatar_url: non_empty(p.image_url),
            web_url: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzConnection {
    authenticated_user: AzAuthUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzAuthUser {
    #[serde(default)]
    provider_display_name: Option<String>,
    #[serde(default)]
    properties: Value,
}

#[derive(Deserialize)]
struct AzNamed {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct AzProject {
    name: String,
    #[serde(default)]
    visibility: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzRepo {
    name: String,
    #[serde(default)]
    project: Option<AzProject>,
    #[serde(default)]
    remote_url: Option<String>,
    #[serde(default)]
    ssh_url: Option<String>,
    #[serde(default)]
    web_url: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    is_fork: bool,
    #[serde(default)]
    is_disabled: bool,
}

#[derive(Deserialize)]
struct AzRepoRef {
    id: String,
}

#[derive(Deserialize)]
struct AzLabel {
    name: String,
    #[serde(default)]
    active: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzCommit {
    commit_id: String,
}

#[derive(Deserialize)]
struct AzForkRepo {
    name: String,
    #[serde(default)]
    project: Option<AzNamed>,
}

#[derive(Deserialize)]
struct AzForkSource {
    #[serde(default)]
    repository: Option<AzForkRepo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzPull {
    pull_request_id: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    status: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    created_by: Option<AzPerson>,
    creation_date: String,
    /// The list has no "updated" date; a finished PR's close date is the nearest thing.
    #[serde(default)]
    closed_date: Option<String>,
    source_ref_name: String,
    target_ref_name: String,
    #[serde(default)]
    last_merge_source_commit: Option<AzCommit>,
    #[serde(default)]
    labels: Vec<AzLabel>,
    #[serde(default)]
    fork_source: Option<AzForkSource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzWiql {
    #[serde(default)]
    work_items: Vec<AzWiqlItem>,
}

#[derive(Deserialize)]
struct AzWiqlItem {
    id: u64,
}

#[derive(Deserialize)]
struct AzWorkItem {
    id: u64,
    fields: AzFields,
}

#[derive(Deserialize)]
struct AzFields {
    #[serde(rename = "System.Title", default)]
    title: String,
    #[serde(rename = "System.State", default)]
    state: String,
    #[serde(rename = "System.CreatedBy", default)]
    created_by: Option<AzPerson>,
    #[serde(rename = "System.AssignedTo", default)]
    assigned_to: Option<AzPerson>,
    /// `a; b; c`.
    #[serde(rename = "System.Tags", default)]
    tags: Option<String>,
    #[serde(rename = "System.CreatedDate", default)]
    created_date: String,
    #[serde(rename = "System.ChangedDate", default)]
    changed_date: String,
    #[serde(rename = "System.CommentCount", default)]
    comment_count: u32,
    /// HTML.
    #[serde(rename = "System.Description", default)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzComment {
    id: u64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    created_by: Option<AzPerson>,
    #[serde(default)]
    created_date: String,
}

#[derive(Deserialize)]
struct AzDefinition {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct AzHref {
    href: String,
}

#[derive(Deserialize)]
struct AzBuildLinks {
    #[serde(default)]
    web: Option<AzHref>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzBuild {
    #[serde(default)]
    build_number: String,
    status: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    source_version: String,
    #[serde(default)]
    definition: Option<AzDefinition>,
    #[serde(default, rename = "_links")]
    links: Option<AzBuildLinks>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organisations() {
        let org = |s: &str| org_of(&Url::parse(s).unwrap());
        assert_eq!(org("https://dev.azure.com/acme").as_deref(), Some("acme"));
        assert_eq!(org("https://dev.azure.com/acme/").as_deref(), Some("acme"));
        assert_eq!(
            org("https://acme.visualstudio.com").as_deref(),
            Some("acme")
        );
        assert_eq!(org("https://dev.azure.com"), None);
    }

    #[test]
    fn build_states() {
        assert_eq!(build_state("inProgress", None), CiState::Pending);
        assert_eq!(build_state("notStarted", None), CiState::Pending);
        assert_eq!(
            build_state("completed", Some("succeeded")),
            CiState::Success
        );
        assert_eq!(
            build_state("completed", Some("partiallySucceeded")),
            CiState::Success
        );
        assert_eq!(build_state("completed", Some("failed")), CiState::Failure);
        assert_eq!(build_state("completed", Some("canceled")), CiState::Failure);
    }

    #[test]
    fn refs_and_shas() {
        assert_eq!(branch_ref("main"), "refs/heads/main");
        assert_eq!(branch_ref("refs/heads/a/b"), "refs/heads/a/b");
        assert_eq!(strip_heads("refs/tags/v1"), "refs/tags/v1");
        assert!(is_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_sha("0123456789abcdef"));
        assert!(!is_sha("main"));
    }

    #[test]
    fn html_answer_is_a_sign_in_problem() {
        let t: Arc<dyn Transport> = Arc::new(crate::replay::Replay::new());
        let az = AzureDevOps::new("https://dev.azure.com/acme", None, false, t).unwrap();
        let err = az
            .decode::<Value>(
                b"
<!DOCTYPE html><html>Sign In</html>",
            )
            .unwrap_err();
        assert!(matches!(err, HostError::Unauthorized { .. }), "{err:?}");
        assert!(az.decode::<Value>(br#"{"a":1}"#).is_ok());
    }

    #[test]
    fn needs_https_and_an_organisation() {
        let t: Arc<dyn Transport> = Arc::new(crate::replay::Replay::new());
        let new = |s: &str| AzureDevOps::new(s, None, false, t.clone());
        assert!(new("http://dev.azure.com/acme").is_err());
        assert!(new("not a url").is_err());
        assert!(new("https://dev.azure.com").is_err());
        assert!(new("https://dev.azure.com/acme").is_ok());
        assert!(new("https://acme.visualstudio.com/").is_ok());
    }
}
