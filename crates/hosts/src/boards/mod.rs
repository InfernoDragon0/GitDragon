//! Project boards, host-neutral: GitHub Projects (v2), GitLab issue boards and Azure Boards
//! all become a [`Board`] with [`BoardColumn`]s holding [`BoardItem`]s.
//!
//! Ids are opaque strings each provider encodes for itself (a GraphQL node id, `<project>/<board>`,
//! `<project>/<team>/<board>`); the UI only hands them back. The provider code lives next to
//! its host (`github.rs` includes `boards/gh.rs`, `gitlab.rs` `boards/gl.rs`, `azure.rs`
//! `boards/az.rs`) so it can share that host's private JSON types.
//!
//! How a move works differs per host:
//! - GitHub: sets the project's `Status` single-select field (GraphQL
//!   `updateProjectV2ItemFieldValue`; "No Status" clears it).
//! - GitLab: columns are label lists: the old list labels go, the new one comes; the
//!   `Closed` column closes the issue, `Open` reopens it without a list label.
//! - Azure: the board's Kanban column field and `System.State` are set together, the state
//!   being what the column maps that work item type to (Azure rejects a column whose mapped
//!   state differs from the item's state).

use crate::model::{IssueState, Label, Person, PrState};

/// Where a board lives, seen from the repository it was listed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoardScope {
    /// Linked to this repository (GitHub repo projects, GitLab project boards).
    Repo,
    /// The repository's organisation / group / Azure project.
    Namespace,
}

/// One board the account can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    /// Opaque, provider-encoded.
    pub id: String,
    pub name: String,
    pub scope: BoardScope,
    /// Who owns it: `owner/repo`, the organisation, group path or Azure project (+ team).
    pub owner: String,
    /// GitHub's project number.
    pub number: Option<u64>,
    pub item_count: Option<u32>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardColumn {
    /// Opaque, provider-encoded; [`BoardItem::column`] points here.
    pub id: String,
    pub name: String,
    /// Hex without `#` or a GitHub colour word (`GREEN`), when the host has one.
    pub color: Option<String>,
    /// Work-in-progress limit.
    pub limit: Option<u32>,
    /// Items here are finished (GitLab `Closed`, Azure "outgoing", GitHub `Done`).
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Issue,
    PullRequest,
    /// A GitHub draft item: only a title, no issue behind it.
    Draft,
}

/// A pull / merge request linked to a board item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedPr {
    pub number: u64,
    pub state: PrState,
    pub draft: bool,
    /// The source branch, when the host says.
    pub branch: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardItem {
    /// Opaque, provider-encoded.
    pub id: String,
    /// The [`BoardColumn::id`] it sits in.
    pub column: String,
    pub kind: ItemKind,
    /// Issue / pull request / work item number; `None` for drafts.
    pub number: Option<u64>,
    pub title: String,
    pub body: Option<String>,
    pub state: IssueState,
    /// `owner/repo` (GitLab: full project path) the issue lives in, when the host says.
    pub repo: Option<String>,
    /// Azure work item type (`User Story`, `Bug`).
    pub item_type: Option<String>,
    pub author: Option<Person>,
    pub assignees: Vec<Person>,
    pub labels: Vec<Label>,
    /// Iteration / sprint (GitLab: milestone when there is no iteration).
    pub iteration: Option<String>,
    /// Branches linked to the item.
    pub branches: Vec<String>,
    pub pull_requests: Vec<LinkedPr>,
    pub web_url: Option<String>,
    pub updated_at: String,
}

/// One page of a board: its columns (the same on every page) and items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardPage {
    pub columns: Vec<BoardColumn>,
    pub items: Vec<BoardItem>,
    /// Hand back to [`crate::GitHost::board`] for the next page.
    pub next: Option<String>,
}

/// `owner/repo` of a full name like `owner/repo` or `group/sub/proj`; case-insensitive match
/// helper for "linked to this repo".
pub fn same_repo(a: &str, b: &str) -> bool {
    a.trim_matches('/')
        .eq_ignore_ascii_case(b.trim_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_compare_loosely() {
        assert!(same_repo("Owner/Repo", "owner/repo"));
        assert!(same_repo("/g/sub/p/", "g/sub/p"));
        assert!(!same_repo("o/r", "o/r2"));
    }
}
