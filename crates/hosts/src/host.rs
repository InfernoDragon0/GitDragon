//! The [`GitHost`] trait every provider implements. Calls are async and runtime-neutral: the
//! HTTP transport hops onto the network runtime itself, so the GPUI executor can await them.

use accounts::HostKind;
use async_trait::async_trait;

use crate::api::RateLimit;
use crate::boards::{Board, BoardColumn, BoardItem, BoardPage};
use crate::error::HostError;
use crate::model::*;
use crate::remote::RepoId;

pub type HostResult<T> = Result<T, HostError>;

/// One hosting service. Defaults answer [`HostError::Unsupported`]; a provider overrides what
/// it has and says so in [`GitHost::supports`].
#[async_trait]
pub trait GitHost: Send + Sync {
    fn kind(&self) -> HostKind;

    fn vocab(&self) -> &'static HostVocab {
        HostVocab::of(self.kind())
    }

    /// The UI shows a feature only when this is true.
    fn supports(&self, feature: Feature) -> bool;

    /// Browser link for `target`; `None` when the host has no such page.
    fn web_url(&self, repo: &RepoId, target: &WebTarget) -> Option<String>;

    /// The ref (on the remote the repo was cloned from) that holds the head of pull request
    /// `number`, fetchable even for forks; `None` when the host has no such refs.
    fn pull_head_ref(&self, _number: u64) -> Option<String> {
        None
    }

    /// Last known API quota.
    fn rate_limit(&self) -> RateLimit {
        RateLimit::default()
    }

    /// Drops cached answers for `repo` (all repos with `None`), e.g. after a push.
    fn invalidate(&self, _repo: Option<&RepoId>) {}

    async fn whoami(&self) -> HostResult<Person> {
        Err(HostError::Unsupported(Feature::Whoami))
    }

    /// Repositories the signed-in user can see, most recently updated first.
    async fn repos(&self, _page: u32) -> HostResult<Page<RepoSummary>> {
        Err(HostError::Unsupported(Feature::Repos))
    }

    async fn pull_requests(
        &self,
        _repo: &RepoId,
        _query: &ListQuery,
    ) -> HostResult<Page<PullRequest>> {
        Err(HostError::Unsupported(Feature::PullRequests))
    }

    async fn pull_request(&self, _repo: &RepoId, _number: u64) -> HostResult<PullRequest> {
        Err(HostError::Unsupported(Feature::PullRequests))
    }

    async fn create_pull_request(
        &self,
        _repo: &RepoId,
        _new: &NewPullRequest,
    ) -> HostResult<PullRequest> {
        Err(HostError::Unsupported(Feature::CreatePullRequest))
    }

    async fn issues(&self, _repo: &RepoId, _query: &ListQuery) -> HostResult<Page<Issue>> {
        Err(HostError::Unsupported(Feature::Issues))
    }

    async fn create_issue(&self, _repo: &RepoId, _new: &NewIssue) -> HostResult<Issue> {
        Err(HostError::Unsupported(Feature::CreateIssue))
    }

    async fn comment_on_issue(
        &self,
        _repo: &RepoId,
        _number: u64,
        _body: &str,
    ) -> HostResult<Comment> {
        Err(HostError::Unsupported(Feature::IssueComments))
    }

    /// Closes (`open` false) or reopens an issue; answers the updated issue.
    async fn set_issue_state(
        &self,
        _repo: &RepoId,
        _number: u64,
        _open: bool,
    ) -> HostResult<Issue> {
        Err(HostError::Unsupported(Feature::CloseIssue))
    }

    /// Combined CI state of a commit or branch.
    async fn ci_status(&self, _repo: &RepoId, _reference: &str) -> HostResult<CiStatus> {
        Err(HostError::Unsupported(Feature::CiStatus))
    }

    /// The avatar image URL of the person behind `email`, who authored `commit` in `repo`.
    /// `Ok(None)`: the host knows no account for that author. Hosts that look people up by
    /// email ignore `commit`. The URL is public (no credentials are needed to fetch it).
    async fn author_avatar(
        &self,
        _repo: &RepoId,
        _commit: &str,
        _email: &str,
    ) -> HostResult<Option<String>> {
        Err(HostError::Unsupported(Feature::Avatars))
    }

    /// Boards for `repo`: the ones linked to it first ([`crate::BoardScope::Repo`]), then
    /// those of its organisation / group / project.
    async fn boards(&self, _repo: &RepoId) -> HostResult<Vec<Board>> {
        Err(HostError::Unsupported(Feature::Boards))
    }

    /// One page of `board`'s items with its columns; `cursor` is the previous page's `next`.
    async fn board(
        &self,
        _repo: &RepoId,
        _board: &Board,
        _cursor: Option<&str>,
    ) -> HostResult<BoardPage> {
        Err(HostError::Unsupported(Feature::Boards))
    }

    /// Moves `item` into column `to`; answers the item as it is now.
    async fn move_board_item(
        &self,
        _repo: &RepoId,
        _board: &Board,
        _item: &BoardItem,
        _to: &BoardColumn,
    ) -> HostResult<BoardItem> {
        Err(HostError::Unsupported(Feature::Boards))
    }

    /// `item` with what the board listing leaves out (linked branches, merge requests).
    async fn board_item(
        &self,
        _repo: &RepoId,
        _board: &Board,
        _item: &BoardItem,
    ) -> HostResult<BoardItem> {
        Err(HostError::Unsupported(Feature::Boards))
    }
}
