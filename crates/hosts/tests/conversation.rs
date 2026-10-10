//! Issue / pull request conversations against recorded responses (routes inline): the
//! timeline (comments + events), reviews and review comments of each provider, paging with
//! `next` / `last`, and ETag revalidation of a re-opened conversation.

use std::sync::Arc;
use std::time::Duration;

use accounts::{Secret, net::block_on};
use hosts::replay::{Replay, Route};
use hosts::*;
use serde_json::json;

// ---- GitHub -------------------------------------------------------------------------------

const GH: &str = "https://api.github.com/repos/InfernoDragon0/GitDragon";

fn github(replay: &Arc<Replay>, ttl: Duration) -> GitHub {
    GitHub::new(
        "https://github.com",
        Some(Secret::new("ghp_testtoken")),
        replay.clone(),
    )
    .unwrap()
    .with_ttl(ttl)
}

fn gh_repo() -> RepoId {
    RepoId::parse("git@github.com:InfernoDragon0/GitDragon.git").unwrap()
}

fn gh_user(login: &str) -> serde_json::Value {
    json!({ "login": login, "avatar_url": format!("https://avatars.example/{login}"), "html_url": format!("https://github.com/{login}") })
}

#[test]
fn github_timeline_maps_comments_and_events() {
    let replay = Arc::new(Replay::new());
    let page1 = format!("{GH}/issues/7/timeline?per_page=100&page=1");
    replay.push(
        Route::new(
            "GET",
            &page1,
            200,
            json!([
                { "event": "labeled", "actor": gh_user("octo"), "created_at": "2024-05-01T10:00:00Z", "label": { "name": "bug", "color": "d73a4a" } },
                { "event": "commented", "id": 11, "user": gh_user("ana"), "body": "Looks **good**", "created_at": "2024-05-01T11:00:00Z", "updated_at": "2024-05-01T12:00:00Z", "html_url": "https://github.com/c/11" },
                { "event": "subscribed", "actor": gh_user("ana"), "created_at": "2024-05-01T11:00:00Z" },
                { "event": "renamed", "actor": gh_user("octo"), "created_at": "2024-05-02T10:00:00Z", "rename": { "from": "Old", "to": "New" } },
                { "event": "committed", "sha": "0123456789abcdef", "message": "Fix the thing\n\nbody", "author": { "name": "Ana", "date": "2024-05-02T11:00:00Z" }, "committer": { "date": "2024-05-02T11:30:00Z" } },
                { "event": "cross-referenced", "actor": gh_user("bob"), "created_at": "2024-05-03T10:00:00Z", "source": { "issue": { "number": 9, "title": "Other", "repository": { "full_name": "o/r" } } } },
                { "event": "review_requested", "actor": gh_user("octo"), "created_at": "2024-05-03T11:00:00Z", "requested_reviewer": gh_user("ana") },
                { "event": "line-commented", "comments": [] },
                { "event": "connected", "actor": gh_user("octo"), "created_at": "2024-05-03T12:00:00Z" }
            ]),
        )
        .header(
            "link",
            &format!(
                "<{GH}/issues/7/timeline?per_page=100&page=2>; rel=\"next\", <{GH}/issues/7/timeline?per_page=100&page=3>; rel=\"last\""
            ),
        ),
    );
    let gh = github(&replay, Duration::from_secs(60));
    let page = block_on(gh.timeline(&gh_repo(), Thread::Issue(7), 1)).unwrap();
    assert_eq!((page.next, page.last), (Some(2), Some(3)));
    assert_eq!(page.items.len(), 7, "subscribed and line-commented dropped");
    let TimelineItem::Event(labeled) = &page.items[0] else {
        panic!("event first")
    };
    assert_eq!(labeled.kind, EventKind::Labeled);
    assert_eq!(labeled.detail.as_deref(), Some("bug"));
    assert_eq!(labeled.actor.as_ref().unwrap().login, "octo");
    let TimelineItem::Comment(comment) = &page.items[1] else {
        panic!("comment second")
    };
    assert_eq!(comment.body, "Looks **good**");
    assert_eq!(comment.format, BodyFormat::Markdown);
    assert!(comment.edited);
    assert_eq!(comment.author.as_ref().unwrap().login, "ana");
    let TimelineItem::Event(renamed) = &page.items[2] else {
        panic!()
    };
    assert_eq!(renamed.detail.as_deref(), Some("Old \u{2192} New"));
    let TimelineItem::Event(commit) = &page.items[3] else {
        panic!()
    };
    assert_eq!(commit.kind, EventKind::Committed);
    assert_eq!(commit.detail.as_deref(), Some("0123456 Fix the thing"));
    assert_eq!(commit.created_at, "2024-05-02T11:30:00Z");
    assert_eq!(commit.actor.as_ref().unwrap().login, "Ana");
    let TimelineItem::Event(xref) = &page.items[4] else {
        panic!()
    };
    assert_eq!(xref.kind, EventKind::Referenced);
    assert_eq!(xref.detail.as_deref(), Some("from o/r#9 Other"));
    let TimelineItem::Event(requested) = &page.items[5] else {
        panic!()
    };
    assert_eq!(requested.detail.as_deref(), Some("ana"));
    let TimelineItem::Event(other) = &page.items[6] else {
        panic!()
    };
    assert_eq!(other.kind, EventKind::Other("connected".into()));
}

#[test]
fn github_reviews_and_review_comments() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GH}/pulls/42/reviews?per_page=100&page=1"),
        200,
        json!([
            { "id": 1, "user": gh_user("ana"), "state": "APPROVED", "body": "", "submitted_at": "2024-05-04T10:00:00Z" },
            { "id": 2, "user": gh_user("bob"), "state": "CHANGES_REQUESTED", "body": "Please fix", "submitted_at": "2024-05-04T11:00:00Z", "html_url": "https://github.com/r/2" },
            { "id": 3, "user": gh_user("eve"), "state": "PENDING" }
        ]),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GH}/pulls/42/comments?per_page=100&page=1"),
        200,
        json!([
            { "id": 100, "user": gh_user("bob"), "body": "Typo", "path": "src/a.rs", "line": 12, "original_line": 10, "side": "RIGHT", "diff_hunk": "@@ -1,3 +1,4 @@\n fn a() {}", "created_at": "2024-05-04T11:00:00Z" },
            { "id": 101, "user": gh_user("ana"), "body": "Fixed", "path": "src/a.rs", "line": null, "original_line": 10, "side": "LEFT", "in_reply_to_id": 100, "created_at": "2024-05-04T12:00:00Z" }
        ]),
    ));
    let gh = github(&replay, Duration::from_secs(60));
    let reviews = block_on(gh.pr_reviews(&gh_repo(), 42, 1)).unwrap();
    assert_eq!(reviews.next, None);
    let states: Vec<_> = reviews.items.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [
            ReviewState::Approved,
            ReviewState::ChangesRequested,
            ReviewState::Pending
        ]
    );
    assert_eq!(reviews.items[0].body, None, "empty body dropped");
    assert_eq!(reviews.items[1].body.as_deref(), Some("Please fix"));
    assert_eq!(reviews.items[2].submitted_at, None);

    let comments = block_on(gh.pr_review_comments(&gh_repo(), 42, 1)).unwrap();
    let [first, reply] = comments.items.as_slice() else {
        panic!("two comments")
    };
    assert_eq!(first.path, "src/a.rs");
    assert_eq!((first.line, first.original_line), (Some(12), Some(10)));
    assert_eq!(first.side, Some(DiffSide::Right));
    assert!(first.diff_hunk.as_deref().unwrap().starts_with("@@"));
    assert_eq!(reply.line, None, "outdated");
    assert_eq!(reply.side, Some(DiffSide::Left));
    assert_eq!(reply.in_reply_to, Some(100));
}

#[test]
fn github_timeline_pages_and_revalidates() {
    let replay = Arc::new(Replay::new());
    let url = |page: u32| format!("{GH}/issues/7/timeline?per_page=100&page={page}");
    let comment = |id: u64| json!({ "event": "commented", "id": id, "user": gh_user("ana"), "body": "hi", "created_at": "2024-05-01T11:00:00Z" });
    replay.push(
        Route::new("GET", &url(1), 304, json!(null))
            .when("if-none-match", "\"t1\"")
            .once(),
    );
    replay.push(
        Route::new("GET", &url(1), 200, json!([comment(1)]))
            .header("etag", "\"t1\"")
            .header(
                "link",
                &format!("<{}>; rel=\"next\", <{}>; rel=\"last\"", url(2), url(2)),
            ),
    );
    replay.push(Route::new("GET", &url(2), 200, json!([comment(2)])).header(
        "link",
        &format!("<{}>; rel=\"prev\", <{}>; rel=\"first\"", url(1), url(1)),
    ));
    let gh = github(&replay, Duration::ZERO);
    let first = block_on(gh.timeline(&gh_repo(), Thread::PullRequest(7), 1)).unwrap();
    assert_eq!((first.next, first.last), (Some(2), Some(2)));
    let second = block_on(gh.timeline(&gh_repo(), Thread::PullRequest(7), 2)).unwrap();
    assert_eq!((second.next, second.last), (None, None));
    assert_eq!(second.items.len(), 1);

    // Re-opened: a conditional request, answered 304 from the cache with the same paging.
    let again = block_on(gh.timeline(&gh_repo(), Thread::PullRequest(7), 1)).unwrap();
    assert_eq!(again, first);
    let seen: Vec<_> = replay
        .seen()
        .into_iter()
        .filter(|s| s.url == url(1))
        .collect();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].header("if-none-match"), Some("\"t1\""));
}

#[test]
fn github_timeline_errors_stay_per_call() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GH}/issues/7/timeline?per_page=100&page=1"),
        401,
        json!({ "message": "Bad credentials" }),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GH}/pulls/7/reviews?per_page=100&page=1"),
        200,
        json!([]),
    ));
    let gh = github(&replay, Duration::from_secs(60));
    let err = block_on(gh.timeline(&gh_repo(), Thread::PullRequest(7), 1)).unwrap_err();
    assert!(matches!(err, HostError::Unauthorized { .. }), "{err:?}");
    assert!(block_on(gh.pr_reviews(&gh_repo(), 7, 1)).is_ok());
}

#[test]
fn every_provider_supports_conversations() {
    let replay = Arc::new(Replay::new());
    let gh = github(&replay, Duration::ZERO);
    for feature in [Feature::Timeline, Feature::Reviews, Feature::ReviewComments] {
        assert!(gh.supports(feature));
        assert!(gitlab(&replay).supports(feature));
        assert!(azure(&replay).supports(feature));
    }
}

// ---- GitLab -------------------------------------------------------------------------------

const GL: &str = "https://gitlab.com/api/v4/projects/group%2Fproj";

fn gitlab(replay: &Arc<Replay>) -> GitLab {
    GitLab::new(
        "https://gitlab.com",
        Some(Secret::new("glpat-testtoken")),
        false,
        replay.clone(),
    )
    .unwrap()
}

fn gl_repo() -> RepoId {
    RepoId::parse("git@gitlab.com:group/proj.git").unwrap()
}

fn gl_user(name: &str) -> serde_json::Value {
    json!({ "username": name, "name": name.to_uppercase() })
}

#[test]
fn gitlab_notes_become_comments_and_events() {
    let replay = Arc::new(Replay::new());
    replay.push(
        Route::new(
            "GET",
            &format!("{GL}/merge_requests/5/notes?sort=asc&order_by=created_at&per_page=100&page=1"),
            200,
            json!([
                { "id": 1, "body": "added ~bug label", "author": gl_user("octo"), "created_at": "2024-05-01T10:00:00Z", "system": true },
                { "id": 2, "body": "Nice work", "author": gl_user("ana"), "created_at": "2024-05-01T11:00:00Z", "updated_at": "2024-05-01T11:00:00Z", "system": false, "type": null },
                { "id": 3, "body": "On a line", "author": gl_user("bob"), "created_at": "2024-05-01T12:00:00Z", "system": false, "type": "DiffNote" },
                { "id": 4, "body": "merged", "author": gl_user("octo"), "created_at": "2024-05-02T10:00:00Z", "system": true }
            ]),
        )
        .header("x-next-page", "2")
        .header("x-total-pages", "4"),
    );
    let gl = gitlab(&replay);
    let page = block_on(gl.timeline(&gl_repo(), Thread::PullRequest(5), 1)).unwrap();
    assert_eq!((page.next, page.last), (Some(2), Some(4)));
    assert_eq!(page.items.len(), 3, "the diff note is a review comment");
    let TimelineItem::Event(labeled) = &page.items[0] else {
        panic!()
    };
    assert_eq!(
        (labeled.kind.clone(), labeled.detail.as_deref()),
        (EventKind::Labeled, Some("bug"))
    );
    let TimelineItem::Comment(comment) = &page.items[1] else {
        panic!()
    };
    assert_eq!(comment.body, "Nice work");
    assert!(!comment.edited);
    assert_eq!(comment.author.as_ref().unwrap().login, "ana");
    let TimelineItem::Event(merged) = &page.items[2] else {
        panic!()
    };
    assert_eq!(merged.kind, EventKind::Merged);
}

#[test]
fn gitlab_issue_notes_use_the_issue_endpoint() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GL}/issues/3/notes?sort=asc&order_by=created_at&per_page=100&page=1"),
        200,
        json!([{ "id": 1, "body": "closed", "author": gl_user("octo"), "created_at": "2024-05-01T10:00:00Z", "system": true }]),
    ));
    let page = block_on(gitlab(&replay).timeline(&gl_repo(), Thread::Issue(3), 1)).unwrap();
    assert_eq!(page.next, None);
    assert!(matches!(
        &page.items[0],
        TimelineItem::Event(TimelineEvent {
            kind: EventKind::Closed,
            ..
        })
    ));
}

#[test]
fn gitlab_approvals_and_diff_discussions() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GL}/merge_requests/5/approvals"),
        200,
        json!({ "approved_by": [{ "user": gl_user("ana") }] }),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GL}/merge_requests/5/discussions?per_page=100&page=1"),
        200,
        json!([
            { "id": "d1", "notes": [
                { "id": 10, "type": "DiffNote", "body": "Why?", "author": gl_user("bob"), "created_at": "2024-05-01T10:00:00Z", "system": false,
                  "position": { "new_path": "src/a.rs", "old_path": "src/a.rs", "new_line": 7, "old_line": null } },
                { "id": 11, "type": "DiffNote", "body": "Because", "author": gl_user("ana"), "created_at": "2024-05-01T11:00:00Z", "system": false,
                  "position": { "new_path": "src/a.rs", "old_path": "src/a.rs", "new_line": 7, "old_line": null } }
            ] },
            { "id": "d2", "notes": [
                { "id": 12, "type": "DiffNote", "body": "Gone", "author": gl_user("bob"), "created_at": "2024-05-01T12:00:00Z", "system": false,
                  "position": { "new_path": "src/b.rs", "old_path": "src/b.rs", "new_line": null, "old_line": 3 } }
            ] },
            { "id": "d3", "individual_note": true, "notes": [
                { "id": 13, "body": "plain", "author": gl_user("bob"), "created_at": "2024-05-01T12:00:00Z", "system": false }
            ] }
        ]),
    ));
    let gl = gitlab(&replay);
    let reviews = block_on(gl.pr_reviews(&gl_repo(), 5, 1)).unwrap();
    assert_eq!(reviews.items.len(), 1);
    assert_eq!(reviews.items[0].state, ReviewState::Approved);
    assert_eq!(reviews.items[0].author.as_ref().unwrap().login, "ana");
    assert!(
        block_on(gl.pr_reviews(&gl_repo(), 5, 2))
            .unwrap()
            .items
            .is_empty()
    );

    let comments = block_on(gl.pr_review_comments(&gl_repo(), 5, 1)).unwrap();
    let [why, because, gone] = comments.items.as_slice() else {
        panic!("three diff notes")
    };
    assert_eq!((why.path.as_str(), why.line), ("src/a.rs", Some(7)));
    assert_eq!(why.in_reply_to, None);
    assert_eq!(because.in_reply_to, Some(10));
    assert_eq!(
        (gone.path.as_str(), gone.line, gone.side),
        ("src/b.rs", Some(3), Some(DiffSide::Left))
    );
}

// ---- Azure DevOps -------------------------------------------------------------------------

const AZ: &str = "https://dev.azure.com/infernodragon0/GitDragon";

fn azure(replay: &Arc<Replay>) -> AzureDevOps {
    AzureDevOps::new(
        "https://dev.azure.com/infernodragon0",
        Some(Secret::new("pat-token")),
        false,
        replay.clone(),
    )
    .unwrap()
}

fn az_repo() -> RepoId {
    RepoId::parse("https://dev.azure.com/infernodragon0/GitDragon/_git/GitDragon").unwrap()
}

fn az_user(name: &str) -> serde_json::Value {
    json!({ "displayName": name, "uniqueName": format!("{name}@example.com") })
}

fn az_threads(replay: &Replay) {
    replay.push(Route::new(
        "GET",
        &format!("{AZ}/_apis/git/repositories/GitDragon/pullrequests/12/threads?api-version=7.1"),
        200,
        json!({ "value": [
            { "id": 1, "publishedDate": "2024-05-01T10:00:00Z", "threadContext": null,
              "properties": { "CodeReviewThreadType": { "$type": "System.String", "$value": "VoteUpdate" } },
              "comments": [{ "id": 1, "parentCommentId": 0, "author": az_user("TFS"), "content": "Ana voted 10", "publishedDate": "2024-05-01T10:00:00Z", "commentType": "system" }] },
            { "id": 2, "publishedDate": "2024-05-01T09:00:00Z", "threadContext": null, "properties": {},
              "comments": [
                { "id": 1, "parentCommentId": 0, "author": az_user("Bob"), "content": "General **note**", "publishedDate": "2024-05-01T09:00:00Z", "lastContentUpdatedDate": "2024-05-01T09:30:00Z", "commentType": "text" },
                { "id": 2, "parentCommentId": 1, "author": az_user("Ana"), "content": "deleted", "publishedDate": "2024-05-01T09:40:00Z", "commentType": "text", "isDeleted": true }
              ] },
            { "id": 3, "publishedDate": "2024-05-01T11:00:00Z",
              "threadContext": { "filePath": "/src/a.rs", "rightFileStart": { "line": 4, "offset": 1 }, "rightFileEnd": { "line": 4, "offset": 9 } },
              "comments": [
                { "id": 1, "parentCommentId": 0, "author": az_user("Bob"), "content": "Rename this", "publishedDate": "2024-05-01T11:00:00Z", "commentType": "text" },
                { "id": 2, "parentCommentId": 1, "author": az_user("Ana"), "content": "Done", "publishedDate": "2024-05-01T11:10:00Z", "commentType": "text" }
              ] },
            { "id": 4, "isDeleted": true, "publishedDate": "2024-05-01T12:00:00Z", "comments": [] }
        ] }),
    ));
}

#[test]
fn azure_pr_threads_split_into_timeline_and_review_comments() {
    let replay = Arc::new(Replay::new());
    az_threads(&replay);
    let az = azure(&replay);
    let page = block_on(az.timeline(&az_repo(), Thread::PullRequest(12), 1)).unwrap();
    assert_eq!(page.next, None);
    let [comment, vote] = page.items.as_slice() else {
        panic!("one comment and one vote, oldest first: {:?}", page.items)
    };
    let TimelineItem::Comment(comment) = comment else {
        panic!()
    };
    assert_eq!(comment.body, "General **note**");
    assert!(comment.edited);
    assert_eq!(
        comment.author.as_ref().unwrap().name.as_deref(),
        Some("Bob")
    );
    let TimelineItem::Event(vote) = vote else {
        panic!()
    };
    assert_eq!(vote.kind, EventKind::Reviewed);
    assert_eq!(vote.detail.as_deref(), Some("Ana voted 10"));
    assert_eq!(vote.actor, None);

    let comments = block_on(az.pr_review_comments(&az_repo(), 12, 1)).unwrap();
    let [first, reply] = comments.items.as_slice() else {
        panic!("two line comments")
    };
    assert_eq!((first.path.as_str(), first.line), ("src/a.rs", Some(4)));
    assert_eq!(first.side, Some(DiffSide::Right));
    assert_eq!(reply.in_reply_to, Some(first.id));
    assert_ne!(first.id, reply.id);
}

#[test]
fn azure_votes_are_reviews() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{AZ}/_apis/git/repositories/GitDragon/pullrequests/12/reviewers?api-version=7.1"),
        200,
        json!({ "value": [
            { "displayName": "Ana", "uniqueName": "ana@example.com", "vote": 10 },
            { "displayName": "Bob", "uniqueName": "bob@example.com", "vote": -10 },
            { "displayName": "Eve", "uniqueName": "eve@example.com", "vote": 0 },
            { "displayName": "Joe", "uniqueName": "joe@example.com", "vote": 5 }
        ] }),
    ));
    let reviews = block_on(azure(&replay).pr_reviews(&az_repo(), 12, 1)).unwrap();
    let got: Vec<_> = reviews
        .items
        .iter()
        .map(|r| (r.author.as_ref().unwrap().login.as_str(), r.state))
        .collect();
    assert_eq!(
        got,
        [
            ("ana@example.com", ReviewState::Approved),
            ("bob@example.com", ReviewState::ChangesRequested),
            ("joe@example.com", ReviewState::Approved)
        ]
    );
    assert_eq!(reviews.items[1].body.as_deref(), Some("Rejected"));
}

#[test]
fn azure_work_item_comments_are_html() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{AZ}/_apis/wit/workItems/77/comments?%24top=200&order=asc&api-version=7.1-preview.4"),
        200,
        json!({ "totalCount": 2, "count": 2, "comments": [
            { "id": 1, "text": "<p>First</p>", "createdBy": az_user("Ana"), "createdDate": "2024-05-01T10:00:00Z", "modifiedDate": "2024-05-01T10:00:00Z" },
            { "id": 2, "text": "<p>Second</p>", "createdBy": az_user("Bob"), "createdDate": "2024-05-02T10:00:00Z", "modifiedDate": "2024-05-03T10:00:00Z" }
        ] }),
    ));
    let page = block_on(azure(&replay).timeline(&az_repo(), Thread::Issue(77), 1)).unwrap();
    let comments: Vec<_> = page
        .items
        .iter()
        .map(|i| match i {
            TimelineItem::Comment(c) => (c.body.as_str(), c.format, c.edited),
            TimelineItem::Event(_) => panic!("no events for work items"),
        })
        .collect();
    assert_eq!(
        comments,
        [
            ("<p>First</p>", BodyFormat::Html, false),
            ("<p>Second</p>", BodyFormat::Html, true)
        ]
    );
}
