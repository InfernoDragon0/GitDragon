//! Host-neutral data the UI shows: people, repos, pull requests, issues, CI.
//! Timestamps stay ISO 8601 strings as the hosts send them.

use accounts::HostKind;

use crate::remote::RepoId;

/// What a host can do; the UI hides what [`crate::GitHost::supports`] denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Any API at all (`Generic` servers have none).
    Api,
    Whoami,
    Repos,
    PullRequests,
    CreatePullRequest,
    Issues,
    CreateIssue,
    IssueComments,
    CiStatus,
    /// Author avatars by commit / email ([`crate::GitHost::author_avatar`]).
    Avatars,
}

impl Feature {
    pub fn label(self) -> &'static str {
        match self {
            Feature::Api => "the host API",
            Feature::Whoami => "account info",
            Feature::Repos => "the repository list",
            Feature::PullRequests => "pull requests",
            Feature::CreatePullRequest => "creating pull requests",
            Feature::Issues => "issues",
            Feature::CreateIssue => "creating issues",
            Feature::IssueComments => "issue comments",
            Feature::CiStatus => "CI status",
            Feature::Avatars => "avatars",
        }
    }
}

/// Each host's words for the same things (GitLab "merge request", Azure "work item").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostVocab {
    pub service: &'static str,
    pub pull_request: &'static str,
    pub pull_requests: &'static str,
    /// Badge / tab label, e.g. `PR`, `MR`.
    pub pr_short: &'static str,
    pub issue: &'static str,
    pub issues: &'static str,
    pub ci: &'static str,
    /// What owns repositories: organisation / group / project.
    pub namespace: &'static str,
}

impl HostVocab {
    pub const GITHUB: HostVocab = HostVocab {
        service: "GitHub",
        pull_request: "Pull request",
        pull_requests: "Pull requests",
        pr_short: "PR",
        issue: "Issue",
        issues: "Issues",
        ci: "Checks",
        namespace: "Organization",
    };
    pub const GITLAB: HostVocab = HostVocab {
        service: "GitLab",
        pull_request: "Merge request",
        pull_requests: "Merge requests",
        pr_short: "MR",
        issue: "Issue",
        issues: "Issues",
        ci: "Pipelines",
        namespace: "Group",
    };
    pub const AZURE_DEVOPS: HostVocab = HostVocab {
        service: "Azure DevOps",
        pull_request: "Pull request",
        pull_requests: "Pull requests",
        pr_short: "PR",
        issue: "Work item",
        issues: "Work items",
        ci: "Pipelines",
        namespace: "Project",
    };
    pub const GENERIC: HostVocab = HostVocab {
        service: "Git server",
        pull_request: "Pull request",
        pull_requests: "Pull requests",
        pr_short: "PR",
        issue: "Issue",
        issues: "Issues",
        ci: "CI",
        namespace: "Owner",
    };

    pub fn of(kind: HostKind) -> &'static HostVocab {
        match kind {
            HostKind::GitHub => &Self::GITHUB,
            HostKind::GitLab => &Self::GITLAB,
            HostKind::AzureDevOps => &Self::AZURE_DEVOPS,
            HostKind::Generic => &Self::GENERIC,
        }
    }
}

/// A user on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSummary {
    pub id: RepoId,
    pub description: Option<String>,
    pub private: bool,
    pub fork: bool,
    pub default_branch: Option<String>,
    pub web_url: String,
    pub clone_url: Option<String>,
    pub ssh_url: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
    All,
}

/// Paging and state for list calls; pages are 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListQuery {
    pub state: StateFilter,
    pub page: u32,
    pub per_page: u8,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            state: StateFilter::Open,
            page: 1,
            per_page: 30,
        }
    }
}

/// One page of results; `next` is the next page number when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// Hex without `#`, when the host has one.
    pub color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: PrState,
    pub draft: bool,
    pub author: Option<Person>,
    pub source_branch: String,
    /// `owner/repo` of the source when it is a fork.
    pub source_repo: Option<String>,
    pub target_branch: String,
    pub head_sha: String,
    pub labels: Vec<Label>,
    pub web_url: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequest {
    pub title: String,
    pub body: Option<String>,
    /// Branch to merge; `owner:branch` for a fork on GitHub.
    pub source_branch: String,
    pub target_branch: String,
    pub draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: IssueState,
    pub author: Option<Person>,
    pub assignees: Vec<Person>,
    pub labels: Vec<Label>,
    pub comments: u32,
    pub web_url: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIssue {
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub author: Option<Person>,
    pub body: String,
    pub web_url: Option<String>,
    pub created_at: String,
}

/// Worst first: `Failure` beats `Pending` beats `Success`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CiState {
    /// No checks reported.
    None,
    Success,
    Pending,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: CiState,
    pub description: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiStatus {
    pub state: CiState,
    pub checks: Vec<Check>,
}

impl CiStatus {
    pub fn from_checks(checks: Vec<Check>) -> Self {
        let state = checks
            .iter()
            .map(|c| c.state)
            .max()
            .unwrap_or(CiState::None);
        Self { state, checks }
    }
}

/// Pages a host can open in the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebTarget {
    Repo,
    Commit(String),
    Branch(String),
    PullRequest(u64),
    PullRequests,
    Issue(u64),
    Issues,
    /// The "open a pull request" page for `source` into `target`.
    NewPullRequest {
        source: String,
        target: String,
    },
}
