//! GitLab conversations over REST v4. Included from `gitlab.rs` as its child module.
//!
//! - Timeline: the notes of an issue or merge request, oldest first. System notes ("added
//!   ~bug label", "closed") become events, read from their text; diff notes are left out
//!   (they are the review comments).
//! - Reviews: the merge request's approvals (GitLab keeps no time for them, and has no
//!   "changes requested" outside Premium; only approvals map).
//! - Review comments: the diff notes of the merge request's discussions; a reply points at
//!   the discussion's first note. GitLab sends no diff hunk.
//! - Paged at 100 with `x-next-page` / `x-total-pages`.

use serde::Deserialize;

use super::{GitLab, GlUser};
use crate::host::HostResult;
use crate::model::*;
use crate::remote::RepoId;

const PER_PAGE: &str = "100";

fn noun(thread: Thread) -> &'static str {
    match thread {
        Thread::Issue(_) => "issues",
        Thread::PullRequest(_) => "merge_requests",
    }
}

pub(super) async fn timeline(
    gl: &GitLab,
    repo: &RepoId,
    thread: Thread,
    page: u32,
) -> HostResult<Page<TimelineItem>> {
    let (n, page_s) = (thread.number().to_string(), page.to_string());
    let url = gl.project_url(
        repo,
        &[noun(thread), &n, "notes"],
        &[
            ("sort", "asc"),
            ("order_by", "created_at"),
            ("per_page", PER_PAGE),
            ("page", &page_s),
        ],
    );
    gl.page(url, page, |note: GlNote| {
        if note.kind.as_deref() == Some("DiffNote") {
            return None;
        }
        let body = note.body.unwrap_or_default();
        if note.system {
            let (kind, detail) = system_event(&body);
            return Some(TimelineItem::Event(TimelineEvent {
                kind,
                actor: note.author.map(Into::into),
                created_at: note.created_at,
                detail,
            }));
        }
        Some(TimelineItem::Comment(ThreadComment {
            id: note.id,
            author: note.author.map(Into::into),
            body,
            format: BodyFormat::Markdown,
            edited: note.updated_at.is_some_and(|u| u != note.created_at),
            created_at: note.created_at,
            web_url: None,
        }))
    })
    .await
}

/// Approvals as reviews. The endpoint is not paged: page 2 on is empty.
pub(super) async fn reviews(
    gl: &GitLab,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<Review>> {
    if page > 1 {
        return Ok(Page::new(Vec::new(), None));
    }
    let n = number.to_string();
    let url = gl.project_url(repo, &["merge_requests", &n, "approvals"], &[]);
    let fetched = gl.api.get(url).await?;
    let approvals: GlApprovals = gl.api.decode(&fetched.body)?;
    let items = (approvals.approved_by.into_iter().enumerate())
        .map(|(ix, a)| Review {
            id: ix as u64,
            author: a.user.map(Into::into),
            state: ReviewState::Approved,
            body: None,
            submitted_at: None,
            web_url: None,
        })
        .collect();
    Ok(Page::new(items, None))
}

pub(super) async fn review_comments(
    gl: &GitLab,
    repo: &RepoId,
    number: u64,
    page: u32,
) -> HostResult<Page<ReviewComment>> {
    let (n, page_s) = (number.to_string(), page.to_string());
    let url = gl.project_url(
        repo,
        &["merge_requests", &n, "discussions"],
        &[("per_page", PER_PAGE), ("page", &page_s)],
    );
    let fetched = gl.api.get(url).await?;
    let discussions: Vec<GlDiscussion> = gl.api.decode(&fetched.body)?;
    let mut items = Vec::new();
    for discussion in discussions {
        let mut root = None;
        for note in discussion.notes {
            let Some(position) = note.position.filter(|_| !note.system) else {
                continue;
            };
            let (line, side) = match (position.new_line, position.old_line) {
                (Some(line), _) => (Some(line), DiffSide::Right),
                (None, Some(line)) => (Some(line), DiffSide::Left),
                (None, None) => (None, DiffSide::Right),
            };
            let Some(path) = position.new_path.or(position.old_path) else {
                continue;
            };
            items.push(ReviewComment {
                id: note.id,
                author: note.author.map(Into::into),
                path,
                line,
                original_line: line,
                side: Some(side),
                body: note.body.unwrap_or_default(),
                diff_hunk: None,
                in_reply_to: root,
                created_at: note.created_at,
                web_url: None,
            });
            root = root.or(Some(note.id));
        }
    }
    Ok(Page {
        items,
        next: fetched.next_page.then_some(page + 1),
        last: fetched.last_page,
    })
}

/// GitLab's diff markup in system notes: `{-old-}`, `{+new+}`, `**bold**`.
fn plain(text: &str) -> String {
    let mut out = text.to_string();
    for mark in ["{-", "-}", "{+", "+}", "**", "`"] {
        out = out.replace(mark, "");
    }
    out.trim().to_string()
}

/// `text` without its HTML tags (GitLab lists pushed commits as `<ul><li>`).
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// A system note's kind and detail, from its first line.
fn system_event(body: &str) -> (EventKind, Option<String>) {
    let line = plain(body.lines().next().unwrap_or_default());
    let prefixes = [
        ("added ", EventKind::Labeled),
        ("removed ", EventKind::Unlabeled),
        ("assigned to ", EventKind::Assigned),
        ("unassigned ", EventKind::Unassigned),
        ("changed title from ", EventKind::Renamed),
        ("mentioned in ", EventKind::Referenced),
        ("requested review from ", EventKind::ReviewRequested),
        ("changed milestone to ", EventKind::Milestoned),
    ];
    let detail = |rest: &str| Some(rest.trim().to_string()).filter(|d| !d.is_empty());
    // Exact words first: "added 2 commits" and "removed milestone" share prefixes above.
    if line.starts_with("added ") && line.contains(" commit") {
        let subject = body
            .lines()
            .skip(1)
            .map(|l| plain(strip_tags(l).trim_start_matches(['*', '-', ' '])))
            .find(|l| !l.is_empty());
        return (
            EventKind::Committed,
            subject.or(detail(&line["added ".len()..])),
        );
    }
    if line.starts_with("removed milestone") {
        return (EventKind::Demilestoned, None);
    }
    if let Some(rest) = line.strip_prefix("removed review request for ") {
        return (EventKind::ReviewRequestRemoved, detail(rest));
    }
    match line.as_str() {
        "closed" => return (EventKind::Closed, None),
        "reopened" => return (EventKind::Reopened, None),
        "locked this issue" | "locked this merge request" => return (EventKind::Locked, None),
        "unlocked this issue" | "unlocked this merge request" => {
            return (EventKind::Unlocked, None);
        }
        "approved this merge request" => {
            return (EventKind::Reviewed, Some("approved".into()));
        }
        "unapproved this merge request" => {
            return (EventKind::Reviewed, Some("unapproved".into()));
        }
        "deleted the source branch" => return (EventKind::BranchDeleted, None),
        _ => {}
    }
    if line.starts_with("merged") {
        return (EventKind::Merged, None);
    }
    if line.starts_with("closed via") {
        return (EventKind::Closed, detail(&line["closed ".len()..]));
    }
    if line.starts_with("marked this merge request as ready") {
        return (EventKind::ReadyForReview, None);
    }
    if line.starts_with("marked this merge request as draft") {
        return (EventKind::ConvertedToDraft, None);
    }
    for (prefix, kind) in prefixes {
        if let Some(rest) = line.strip_prefix(prefix) {
            let rest = match kind {
                EventKind::Labeled | EventKind::Unlabeled => {
                    // Only label changes start with "added ~" / "removed ~".
                    if !rest.starts_with('~') {
                        return (EventKind::Other(line.clone()), None);
                    }
                    rest.trim_end_matches(" labels")
                        .trim_end_matches(" label")
                        .replace('~', "")
                }
                EventKind::Renamed => rest.replacen(" to ", " \u{2192} ", 1),
                _ => rest.to_string(),
            };
            return (kind, detail(&rest));
        }
    }
    (EventKind::Other(line), None)
}

#[derive(Deserialize)]
struct GlNote {
    id: u64,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    author: Option<GlUser>,
    created_at: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    position: Option<GlPosition>,
}

#[derive(Deserialize)]
struct GlPosition {
    #[serde(default)]
    new_path: Option<String>,
    #[serde(default)]
    old_path: Option<String>,
    #[serde(default)]
    new_line: Option<u32>,
    #[serde(default)]
    old_line: Option<u32>,
}

#[derive(Deserialize)]
struct GlDiscussion {
    #[serde(default)]
    notes: Vec<GlNote>,
}

#[derive(Deserialize)]
struct GlApprovals {
    #[serde(default)]
    approved_by: Vec<GlApprover>,
}

#[derive(Deserialize)]
struct GlApprover {
    #[serde(default)]
    user: Option<GlUser>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_notes() {
        let ev = |s: &str| system_event(s);
        assert_eq!(
            ev("added ~bug ~\"needs review\" labels"),
            (EventKind::Labeled, Some("bug \"needs review\"".into()))
        );
        assert_eq!(
            ev("removed ~bug label"),
            (EventKind::Unlabeled, Some("bug".into()))
        );
        assert_eq!(
            ev("assigned to @octo"),
            (EventKind::Assigned, Some("@octo".into()))
        );
        assert_eq!(
            ev("changed title from **{-Old-}** to **{+New+}**"),
            (EventKind::Renamed, Some("Old \u{2192} New".into()))
        );
        assert_eq!(ev("closed"), (EventKind::Closed, None));
        assert_eq!(ev("merged"), (EventKind::Merged, None));
        assert_eq!(
            ev("added 2 commits\n\n<ul><li>abc1234 - Fix it</li></ul>"),
            (EventKind::Committed, Some("abc1234 - Fix it".into()))
        );
        assert_eq!(
            ev("approved this merge request"),
            (EventKind::Reviewed, Some("approved".into()))
        );
        assert_eq!(ev("removed milestone"), (EventKind::Demilestoned, None));
        assert_eq!(
            ev("requested review from @ana"),
            (EventKind::ReviewRequested, Some("@ana".into()))
        );
        assert_eq!(
            ev("enabled an automatic merge"),
            (EventKind::Other("enabled an automatic merge".into()), None)
        );
        assert_eq!(
            ev("added a to do item"),
            (EventKind::Other("added a to do item".into()), None)
        );
    }
}
