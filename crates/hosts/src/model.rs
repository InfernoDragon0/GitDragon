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
    /// Closing and reopening issues ([`crate::GitHost::set_issue_state`]).
    CloseIssue,
    CiStatus,
    /// Author avatars by commit / email ([`crate::GitHost::author_avatar`]).
    Avatars,
    /// Project boards: GitHub Projects, GitLab issue boards, Azure Boards
    /// ([`crate::GitHost::boards`]).
    Boards,
    /// Comments and activity of an issue or pull request ([`crate::GitHost::timeline`]).
    Timeline,
    /// Pull request reviews: approvals, change requests ([`crate::GitHost::pr_reviews`]).
    Reviews,
    /// Comments on lines of a pull request's diff ([`crate::GitHost::pr_review_comments`]).
    ReviewComments,
    /// A pull request's changed files with their patches ([`crate::GitHost::pr_files`]).
    PrFiles,
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
            Feature::CloseIssue => "closing issues",
            Feature::CiStatus => "CI status",
            Feature::Avatars => "avatars",
            Feature::Boards => "boards",
            Feature::Timeline => "comments and activity",
            Feature::Reviews => "reviews",
            Feature::ReviewComments => "review comments",
            Feature::PrFiles => "changed files",
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
    /// The last page number, when the host says (GitHub's `rel="last"` link, GitLab's
    /// `x-total-pages`): the pages in between can then be asked for at once.
    pub last: Option<u32>,
}

impl<T> Page<T> {
    /// A page that says nothing about the pages after it but `next`.
    pub fn new(items: Vec<T>, next: Option<u32>) -> Self {
        Self {
            items,
            next,
            last: None,
        }
    }
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
    /// The target branch's commit the PR was based on / merged into, when the host says.
    pub base_sha: Option<String>,
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

/// The ref that holds a pull request's changes on the remote the repo was cloned from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRef {
    /// Without `refs/`, e.g. `pull/5/head`.
    pub reference: String,
    /// The host's merge preview (the head merged into the target), not the head itself.
    pub merge_preview: bool,
}

/// How a pull request changes one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
}

/// One file of a pull request as the host's API lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    /// The path before a rename or copy.
    pub old_path: Option<String>,
    pub status: PrFileStatus,
    pub additions: u32,
    pub deletions: u32,
    /// The hunks (`@@ ... @@` onwards, no file header); `None` when the host leaves them
    /// out (binary, too large).
    pub patch: Option<String>,
}

/// A pull request's files from the API.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrFiles {
    pub files: Vec<PrFile>,
    /// The host stopped listing (GitHub lists at most 3000 files) or more pages were left.
    pub truncated: bool,
}

/// Files per page of [`crate::GitHost::pr_files`].
pub const PR_FILES_PER_PAGE: u32 = 100;
/// Pages [`crate::GitHost::pr_files`] reads at most (GitHub lists up to 3000 files).
pub const PR_FILES_MAX_PAGES: u32 = 30;

/// Added and deleted lines of bare hunks (`@@` headers and `\ No newline` markers are neither).
pub fn patch_line_counts(patch: &str) -> (u32, u32) {
    let mut counts = (0u32, 0u32);
    for line in patch.lines() {
        if line.starts_with('+') {
            counts.0 = counts.0.saturating_add(1);
        } else if line.starts_with('-') {
            counts.1 = counts.1.saturating_add(1);
        }
    }
    counts
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

// ---- conversations ----------------------------------------------------------------------

/// The issue or pull request a conversation belongs to. GitHub has one endpoint for both;
/// GitLab and Azure DevOps keep them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Thread {
    Issue(u64),
    PullRequest(u64),
}

impl Thread {
    pub fn number(self) -> u64 {
        match self {
            Thread::Issue(n) | Thread::PullRequest(n) => n,
        }
    }
}

/// How a comment body is written: Markdown everywhere but Azure work items (HTML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyFormat {
    #[default]
    Markdown,
    Html,
}

/// A comment in an issue's or pull request's conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: u64,
    pub author: Option<Person>,
    pub body: String,
    pub format: BodyFormat,
    pub created_at: String,
    /// Changed after it was posted.
    pub edited: bool,
    pub web_url: Option<String>,
}

impl From<Comment> for ThreadComment {
    fn from(c: Comment) -> Self {
        ThreadComment {
            id: c.id,
            author: c.author,
            body: c.body,
            format: BodyFormat::Markdown,
            created_at: c.created_at,
            edited: false,
            web_url: c.web_url,
        }
    }
}

/// What happened in an activity line. Hosts word these differently; what does not map is
/// [`EventKind::Other`] with the host's own name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Labeled,
    Unlabeled,
    Assigned,
    Unassigned,
    Closed,
    Reopened,
    Merged,
    Renamed,
    /// Mentioned from a commit or another issue / pull request.
    Referenced,
    Committed,
    ForcePushed,
    ReviewRequested,
    ReviewRequestRemoved,
    Reviewed,
    Milestoned,
    Demilestoned,
    Locked,
    Unlocked,
    ReadyForReview,
    ConvertedToDraft,
    BranchDeleted,
    Other(String),
}

/// A one-line activity entry: who did what, when, plus the detail (label name, new title,
/// commit subject...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub kind: EventKind,
    pub actor: Option<Person>,
    pub created_at: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineItem {
    Comment(ThreadComment),
    Event(TimelineEvent),
}

impl TimelineItem {
    pub fn created_at(&self) -> &str {
        match self {
            TimelineItem::Comment(c) => &c.created_at,
            TimelineItem::Event(e) => &e.created_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    /// Started but not submitted (only its author sees it).
    Pending,
}

/// A pull request review (GitHub reviews, GitLab approvals, Azure votes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub id: u64,
    pub author: Option<Person>,
    pub state: ReviewState,
    pub body: Option<String>,
    /// `None` where the host keeps no time (GitLab approvals, Azure votes).
    pub submitted_at: Option<String>,
    pub web_url: Option<String>,
}

/// The side of a diff a line comment sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// The old file (a removed line).
    Left,
    /// The new file.
    Right,
}

/// A comment on a line of a pull request's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: u64,
    pub author: Option<Person>,
    pub path: String,
    /// The line in the current diff; `None` when the comment is outdated.
    pub line: Option<u32>,
    /// The line when the comment was made.
    pub original_line: Option<u32>,
    pub side: Option<DiffSide>,
    pub body: String,
    /// The diff around the line (GitHub only).
    pub diff_hunk: Option<String>,
    /// The comment this one answers.
    pub in_reply_to: Option<u64>,
    pub created_at: String,
    pub web_url: Option<String>,
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
