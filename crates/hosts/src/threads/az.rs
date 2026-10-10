//! Azure DevOps conversations. Included from `azure.rs` as its child module.
//!
//! - Pull requests: one GET of the PR's threads (not paged) feeds two calls. Threads without
//!   a file are the timeline: text comments, and system comments ("Ana voted 10", "updated
//!   the pull request") as events whose sentence is the detail. Threads on a file are the
//!   review comments; a comment id is `<thread id> << 20 | <comment id>` (Azure numbers
//!   comments per thread).
//! - Reviews: the reviewers' votes (10 approved, 5 approved with suggestions, -5 waiting for
//!   the author, -10 rejected); Azure keeps no time for a vote.
//! - Work items: their comments (HTML), the first 200; no activity events.

use serde::Deserialize;
use serde_json::Value;

use super::{AzPerson, AzureDevOps, COMMENTS_VERSION, project};
use crate::host::HostResult;
use crate::model::*;
use crate::remote::RepoId;

/// Work item comments asked for at once (the most the API gives per call).
const WORK_ITEM_COMMENTS: &str = "200";

fn uid(thread: u64, comment: u64) -> u64 {
    (thread << 20) | (comment & 0xF_FFFF)
}

async fn threads(az: &AzureDevOps, repo: &RepoId, number: u64) -> HostResult<Vec<AzThread>> {
    let n = number.to_string();
    let url = az.repo_url(repo, &["pullrequests", &n, "threads"], &[])?;
    let fetched = az.api.get(url).await?;
    let list: super::AzList<AzThread> = az.decode(&fetched.body)?;
    Ok(list.value.into_iter().filter(|t| !t.is_deleted).collect())
}

pub(super) async fn timeline(
    az: &AzureDevOps,
    repo: &RepoId,
    thread: Thread,
    page: u32,
) -> HostResult<Page<TimelineItem>> {
    if page > 1 {
        return Ok(Page::new(Vec::new(), None));
    }
    match thread {
        Thread::Issue(number) => work_item_comments(az, repo, number).await,
        Thread::PullRequest(number) => {
            let mut items = Vec::new();
            for t in threads(az, repo, number).await? {
                if t.file_path().is_some() {
                    continue;
                }
                let kind = t.kind();
                for c in t.comments.into_iter().filter(|c| !c.is_deleted) {
                    let content = c.content.unwrap_or_default();
                    if c.comment_type.as_deref() == Some("system") {
                        items.push(TimelineItem::Event(TimelineEvent {
                            kind: system_kind(kind.as_deref(), &content),
                            // The author is the service; the sentence names the person.
                            actor: None,
                            created_at: c.published_date,
                            detail: Some(content).filter(|d| !d.is_empty()),
                        }));
                    } else {
                        items.push(TimelineItem::Comment(ThreadComment {
                            id: uid(t.id, c.id),
                            author: c.author.map(Into::into),
                            body: content,
                            format: BodyFormat::Markdown,
                            edited: c
                                .last_content_updated_date
                                .is_some_and(|d| d != c.published_date),
                            created_at: c.published_date,
                            web_url: None,
                        }));
                    }
                }
            }
            items.sort_by(|a, b| a.created_at().cmp(b.created_at()));
            Ok(Page::new(items, None))
        }
    }
}

async fn work_item_comments(
    az: &AzureDevOps,
    repo: &RepoId,
    number: u64,
) -> HostResult<Page<TimelineItem>> {
    let n = number.to_string();
    let url = az.url_v(
        &[project(repo)?, "_apis", "wit", "workItems", &n, "comments"],
        &[("$top", WORK_ITEM_COMMENTS), ("order", "asc")],
        COMMENTS_VERSION,
    );
    let fetched = az.api.get(url).await?;
    let list: AzComments = az.decode(&fetched.body)?;
    let items = (list.comments.into_iter())
        .filter(|c| !c.is_deleted)
        .map(|c| {
            TimelineItem::Comment(ThreadComment {
                id: c.id,
                author: c.created_by.map(Into::into),
                body: c.text.unwrap_or_default(),
                format: BodyFormat::Html,
                edited: c.modified_date.is_some_and(|d| d != c.created_date),
                created_at: c.created_date,
                web_url: None,
            })
        })
        .collect();
    Ok(Page::new(items, None))
}

/// An icon-worthy kind for a system comment, from the thread's type and its sentence.
fn system_kind(thread_type: Option<&str>, content: &str) -> EventKind {
    let lower = content.to_ascii_lowercase();
    match thread_type {
        Some("VoteUpdate") => EventKind::Reviewed,
        Some("ReviewersUpdate") => EventKind::ReviewRequested,
        Some("RefUpdate") => EventKind::Committed,
        Some("IsDraftUpdate") if lower.contains("draft") && !lower.contains("published") => {
            EventKind::ConvertedToDraft
        }
        Some("IsDraftUpdate") => EventKind::ReadyForReview,
        Some("StatusUpdate") if lower.contains("completed") => EventKind::Merged,
        Some("StatusUpdate") if lower.contains("abandoned") => EventKind::Closed,
        Some("StatusUpdate") if lower.contains("reactivated") => EventKind::Reopened,
        Some(other) => EventKind::Other(other.to_string()),
        None => EventKind::Other("update".into()),
    }
}

pub(super) async fn reviews(
    az: &AzureDevOps,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<Review>> {
    if page > 1 {
        return Ok(Page::new(Vec::new(), None));
    }
    let n = number.to_string();
    let url = az.repo_url(repo, &["pullrequests", &n, "reviewers"], &[])?;
    let fetched = az.api.get(url).await?;
    let list: super::AzList<AzReviewer> = az.decode(&fetched.body)?;
    let items = (list.value.into_iter().enumerate())
        .filter_map(|(ix, r)| {
            let (state, word) = match r.vote {
                10 => (ReviewState::Approved, None),
                5 => (ReviewState::Approved, Some("Approved with suggestions")),
                -5 => (
                    ReviewState::ChangesRequested,
                    Some("Waiting for the author"),
                ),
                -10 => (ReviewState::ChangesRequested, Some("Rejected")),
                _ => return None,
            };
            Some(Review {
                id: ix as u64,
                author: Some(r.person.into()),
                state,
                body: word.map(str::to_string),
                submitted_at: None,
                web_url: None,
            })
        })
        .collect();
    Ok(Page::new(items, None))
}

pub(super) async fn review_comments(
    az: &AzureDevOps,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<ReviewComment>> {
    if page > 1 {
        return Ok(Page::new(Vec::new(), None));
    }
    let mut items = Vec::new();
    for t in threads(az, repo, number).await? {
        let Some(context) = t.thread_context.as_ref() else {
            continue;
        };
        let Some(path) = context.file_path.clone() else {
            continue;
        };
        let (line, side) = match (&context.right_file_start, &context.left_file_start) {
            (Some(right), _) => (Some(right.line), DiffSide::Right),
            (None, Some(left)) => (Some(left.line), DiffSide::Left),
            (None, None) => (None, DiffSide::Right),
        };
        for c in t.comments.iter().filter(|c| !c.is_deleted) {
            if c.comment_type.as_deref() == Some("system") {
                continue;
            }
            items.push(ReviewComment {
                id: uid(t.id, c.id),
                author: c.author.clone().map(Into::into),
                path: path.trim_start_matches('/').to_string(),
                line,
                original_line: line,
                side: Some(side),
                body: c.content.clone().unwrap_or_default(),
                diff_hunk: None,
                in_reply_to: (c.parent_comment_id != 0).then(|| uid(t.id, c.parent_comment_id)),
                created_at: c.published_date.clone(),
                web_url: None,
            });
        }
    }
    items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(Page::new(items, None))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzThread {
    id: u64,
    #[serde(default)]
    is_deleted: bool,
    #[serde(default)]
    thread_context: Option<AzThreadContext>,
    #[serde(default)]
    properties: Value,
    #[serde(default)]
    comments: Vec<AzThreadComment>,
}

impl AzThread {
    fn file_path(&self) -> Option<&str> {
        self.thread_context.as_ref()?.file_path.as_deref()
    }

    /// `CodeReviewThreadType` (`VoteUpdate`, `StatusUpdate`, ...) of a system thread.
    fn kind(&self) -> Option<String> {
        (self.properties)
            .pointer("/CodeReviewThreadType/$value")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzThreadContext {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    right_file_start: Option<AzPosition>,
    #[serde(default)]
    left_file_start: Option<AzPosition>,
}

#[derive(Deserialize)]
struct AzPosition {
    line: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzThreadComment {
    id: u64,
    #[serde(default)]
    parent_comment_id: u64,
    #[serde(default)]
    author: Option<AzPerson>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    published_date: String,
    #[serde(default)]
    last_content_updated_date: Option<String>,
    #[serde(default)]
    comment_type: Option<String>,
    #[serde(default)]
    is_deleted: bool,
}

#[derive(Deserialize)]
struct AzReviewer {
    #[serde(default)]
    vote: i32,
    #[serde(flatten)]
    person: AzPerson,
}

#[derive(Deserialize)]
struct AzComments {
    #[serde(default)]
    comments: Vec<AzWorkItemComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzWorkItemComment {
    id: u64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    created_by: Option<AzPerson>,
    #[serde(default)]
    created_date: String,
    #[serde(default)]
    modified_date: Option<String>,
    #[serde(default)]
    is_deleted: bool,
}
