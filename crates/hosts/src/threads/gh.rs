//! GitHub conversations over REST. Included from `github.rs` as its child module, so it
//! shares the host's [`ApiClient`](crate::api::ApiClient) and its JSON types.
//!
//! - Timeline: `issues/{n}/timeline` covers comments and events of issues and pull requests
//!   alike. Inline review comments show there as `line-commented`; they are left out (the
//!   review comments call has them, with their diff position).
//! - Reviews: `pulls/{n}/reviews`. Review comments: `pulls/{n}/comments`.
//! - Every list is paged at 100; the `Link` header gives `next` and `last`.

use serde::Deserialize;
use serde_json::Value;

use super::{GhUser, GitHub};
use crate::host::HostResult;
use crate::model::*;
use crate::remote::RepoId;

const PER_PAGE: &str = "100";

pub(super) async fn timeline(
    gh: &GitHub,
    repo: &RepoId,
    thread: Thread,
    page: u32,
) -> HostResult<Page<TimelineItem>> {
    let (n, page_s) = (thread.number().to_string(), page.to_string());
    let url = gh.repo_url(
        repo,
        &["issues", &n, "timeline"],
        &[("per_page", PER_PAGE), ("page", &page_s)],
    );
    gh.page(url, page, timeline_item).await
}

pub(super) async fn reviews(
    gh: &GitHub,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<Review>> {
    let (n, page_s) = (number.to_string(), page.to_string());
    let url = gh.repo_url(
        repo,
        &["pulls", &n, "reviews"],
        &[("per_page", PER_PAGE), ("page", &page_s)],
    );
    gh.page(url, page, |r: GhReview| {
        Some(Review {
            id: r.id,
            author: r.user.map(Into::into),
            state: review_state(&r.state),
            body: r.body.filter(|b| !b.trim().is_empty()),
            submitted_at: r.submitted_at,
            web_url: r.html_url,
        })
    })
    .await
}

pub(super) async fn review_comments(
    gh: &GitHub,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<ReviewComment>> {
    let (n, page_s) = (number.to_string(), page.to_string());
    let url = gh.repo_url(
        repo,
        &["pulls", &n, "comments"],
        &[("per_page", PER_PAGE), ("page", &page_s)],
    );
    gh.page(url, page, |c: GhReviewComment| {
        Some(ReviewComment {
            id: c.id,
            author: c.user.map(Into::into),
            path: c.path,
            line: c.line,
            original_line: c.original_line,
            side: c.side.as_deref().map(|s| {
                if s.eq_ignore_ascii_case("left") {
                    DiffSide::Left
                } else {
                    DiffSide::Right
                }
            }),
            body: c.body.unwrap_or_default(),
            diff_hunk: c.diff_hunk.filter(|h| !h.is_empty()),
            in_reply_to: c.in_reply_to_id,
            created_at: c.created_at,
            web_url: c.html_url,
        })
    })
    .await
}

fn review_state(state: &str) -> ReviewState {
    match state {
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        "DISMISSED" => ReviewState::Dismissed,
        "PENDING" => ReviewState::Pending,
        _ => ReviewState::Commented,
    }
}

fn text<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn person(v: &Value, key: &str) -> Option<Person> {
    let user = v.get(key).filter(|u| u.is_object())?;
    serde_json::from_value::<GhUser>(user.clone())
        .ok()
        .map(Into::into)
}

/// Someone known only by the name in a commit.
fn named(name: &str) -> Person {
    Person {
        login: name.to_string(),
        name: None,
        avatar_url: None,
        web_url: None,
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// One timeline entry; `None` for noise (subscriptions, mentions) and for inline review
/// comments.
fn timeline_item(v: Value) -> Option<TimelineItem> {
    let event = v.get("event")?.as_str()?;
    let created_at = text(&v, "/created_at").unwrap_or_default().to_string();
    let detail = |pointer: &str| text(&v, pointer).map(str::to_string);
    let (kind, detail) = match event {
        "commented" => {
            let updated = text(&v, "/updated_at");
            return Some(TimelineItem::Comment(ThreadComment {
                id: v.get("id").and_then(Value::as_u64).unwrap_or_default(),
                author: person(&v, "user").or_else(|| person(&v, "actor")),
                body: detail("/body").unwrap_or_default(),
                format: BodyFormat::Markdown,
                edited: updated.is_some_and(|u| u != created_at),
                created_at,
                web_url: detail("/html_url"),
            }));
        }
        "committed" => {
            let message = text(&v, "/message").unwrap_or_default();
            let subject = message.lines().next().unwrap_or_default();
            let sha = text(&v, "/sha").map(short_sha).unwrap_or_default();
            return Some(TimelineItem::Event(TimelineEvent {
                kind: EventKind::Committed,
                actor: text(&v, "/author/name").map(named),
                created_at: text(&v, "/committer/date")
                    .or(text(&v, "/author/date"))
                    .unwrap_or_default()
                    .to_string(),
                detail: Some(format!("{sha} {subject}").trim().to_string()),
            }));
        }
        "reviewed" => {
            let state = text(&v, "/state").map(review_state);
            return Some(TimelineItem::Event(TimelineEvent {
                kind: EventKind::Reviewed,
                actor: person(&v, "user"),
                created_at: text(&v, "/submitted_at").unwrap_or(&created_at).to_string(),
                detail: state.map(|s| review_word(s).to_string()),
            }));
        }
        "labeled" => (EventKind::Labeled, detail("/label/name")),
        "unlabeled" => (EventKind::Unlabeled, detail("/label/name")),
        "assigned" => (EventKind::Assigned, detail("/assignee/login")),
        "unassigned" => (EventKind::Unassigned, detail("/assignee/login")),
        "closed" => (EventKind::Closed, None),
        "reopened" => (EventKind::Reopened, None),
        "merged" => (EventKind::Merged, text(&v, "/commit_id").map(short_sha)),
        "renamed" => (
            EventKind::Renamed,
            match (text(&v, "/rename/from"), text(&v, "/rename/to")) {
                (Some(from), Some(to)) => Some(format!("{from} \u{2192} {to}")),
                (None, Some(to)) => Some(to.to_string()),
                _ => None,
            },
        ),
        "referenced" => (
            EventKind::Referenced,
            text(&v, "/commit_id").map(|sha| format!("in commit {}", short_sha(sha))),
        ),
        "cross-referenced" => (
            EventKind::Referenced,
            v.pointer("/source/issue").map(|issue| {
                let number = issue.get("number").and_then(Value::as_u64);
                let title = text(issue, "/title").unwrap_or_default();
                let repo = text(issue, "/repository/full_name");
                match (repo, number) {
                    (Some(repo), Some(n)) => format!("from {repo}#{n} {title}"),
                    (None, Some(n)) => format!("from #{n} {title}"),
                    _ => format!("from {title}"),
                }
                .trim()
                .to_string()
            }),
        ),
        "head_ref_force_pushed" => (EventKind::ForcePushed, None),
        "head_ref_deleted" => (EventKind::BranchDeleted, None),
        "review_requested" => (
            EventKind::ReviewRequested,
            detail("/requested_reviewer/login").or_else(|| detail("/requested_team/name")),
        ),
        "review_request_removed" => (
            EventKind::ReviewRequestRemoved,
            detail("/requested_reviewer/login").or_else(|| detail("/requested_team/name")),
        ),
        "milestoned" => (EventKind::Milestoned, detail("/milestone/title")),
        "demilestoned" => (EventKind::Demilestoned, detail("/milestone/title")),
        "locked" => (EventKind::Locked, detail("/lock_reason")),
        "unlocked" => (EventKind::Unlocked, None),
        "ready_for_review" => (EventKind::ReadyForReview, None),
        "convert_to_draft" => (EventKind::ConvertedToDraft, None),
        "mentioned" | "subscribed" | "unsubscribed" | "line-commented" | "commit-commented" => {
            return None;
        }
        other => (EventKind::Other(other.replace(['_', '-'], " ")), None),
    };
    Some(TimelineItem::Event(TimelineEvent {
        kind,
        actor: person(&v, "actor"),
        created_at,
        detail,
    }))
}

fn review_word(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "commented",
        ReviewState::Dismissed => "dismissed",
        ReviewState::Pending => "pending",
    }
}

#[derive(Deserialize)]
struct GhReview {
    id: u64,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct GhReviewComment {
    id: u64,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    body: Option<String>,
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    original_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    diff_hunk: Option<String>,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    created_at: String,
    #[serde(default)]
    html_url: Option<String>,
}
