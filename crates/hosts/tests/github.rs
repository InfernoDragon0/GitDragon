//! GitHub provider against recorded responses (`fixtures/github.json`): mapping, paging,
//! caching (TTL + ETag), rate limits, error mapping and the redirect / token rules.

use std::sync::Arc;
use std::time::Duration;

use accounts::{Account, AuthMode, HostKind, Secret, net::block_on};
use hosts::http::Method;
use hosts::replay::{Replay, Route};
use hosts::*;
use serde_json::json;

const API: &str = "https://api.github.com";

fn setup(ttl: Duration) -> (Arc<Replay>, GitHub) {
    let replay = Arc::new(Replay::from_json(include_str!("fixtures/github.json")));
    let github = GitHub::new(
        "https://github.com",
        Some(Secret::new("ghp_testtoken")),
        replay.clone(),
    )
    .unwrap()
    .with_ttl(ttl);
    (replay, github)
}

fn repo() -> RepoId {
    RepoId::parse("git@github.com:InfernoDragon0/GitDragon.git").unwrap()
}

#[test]
fn whoami_and_headers() {
    let (replay, gh) = setup(Duration::from_secs(60));
    let me = block_on(gh.whoami()).unwrap();
    assert_eq!(me.login, "InfernoDragon0");
    assert_eq!(me.name.as_deref(), Some("Inferno Dragon"));
    let seen = &replay.seen()[0];
    assert_eq!(seen.auth, Some("Bearer"));
    assert_eq!(seen.header("accept"), Some("application/vnd.github+json"));
    assert_eq!(seen.header("x-github-api-version"), Some("2022-11-28"));
    let rate = gh.rate_limit();
    assert_eq!((rate.limit, rate.remaining), (Some(5000), Some(4999)));
    assert!(rate.blocked_until.is_none());
}

#[test]
fn repos_page() {
    let (_, gh) = setup(Duration::ZERO);
    let page = block_on(gh.repos(1)).unwrap();
    assert_eq!(page.next, Some(2));
    assert_eq!(page.items.len(), 2);
    let first = &page.items[0];
    assert_eq!(first.id, repo());
    assert!(first.private && !first.fork);
    assert_eq!(
        first.ssh_url.as_deref(),
        Some("git@github.com:InfernoDragon0/GitDragon.git")
    );
    assert!(page.items[1].fork);
    assert_eq!(page.items[1].description, None);
}

#[test]
fn pull_requests() {
    let (_, gh) = setup(Duration::ZERO);
    let page = block_on(gh.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(page.next, None);
    let [fork, local] = page.items.as_slice() else {
        panic!("two PRs");
    };
    assert_eq!(fork.number, 42);
    assert!(fork.draft);
    assert_eq!(fork.state, PrState::Open);
    assert_eq!(fork.source_branch, "feature/hosts");
    assert_eq!(fork.source_repo.as_deref(), Some("octocat/GitDragon"));
    assert_eq!(fork.target_branch, "main");
    assert_eq!(fork.labels[0].color.as_deref(), Some("a2eeef"));
    assert_eq!(fork.author.as_ref().unwrap().login, "octocat");
    assert_eq!(local.source_repo, None, "same-repo PR has no source repo");

    let merged = block_on(gh.pull_request(&repo(), 40)).unwrap();
    assert_eq!(merged.state, PrState::Merged);
    assert_eq!(merged.source_repo, None, "deleted fork");
}

#[test]
fn create_pull_request_posts_and_invalidates() {
    let (replay, gh) = setup(Duration::from_secs(600));
    let list = format!(
        "{API}/repos/InfernoDragon0/GitDragon/pulls?state=open&sort=updated&direction=desc&per_page=30&page=1"
    );
    block_on(gh.pull_requests(&repo(), &ListQuery::default())).unwrap();
    block_on(gh.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(replay.count(&list), 1, "second list from the TTL cache");

    let new = NewPullRequest {
        title: "New thing".into(),
        body: Some("Body".into()),
        source_branch: "new-thing".into(),
        target_branch: "main".into(),
        draft: false,
    };
    let pr = block_on(gh.create_pull_request(&repo(), &new)).unwrap();
    assert_eq!(pr.number, 43);
    let post = replay
        .seen()
        .into_iter()
        .find(|s| s.method == Method::Post)
        .unwrap();
    assert_eq!(
        post.body.unwrap(),
        json!({"title": "New thing", "body": "Body", "head": "new-thing", "base": "main", "draft": false})
    );
    block_on(gh.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(
        replay.count(&list),
        2,
        "creating a PR drops the repo's cache"
    );
}

#[test]
fn issues_skip_pull_requests() {
    let (replay, gh) = setup(Duration::ZERO);
    let page = block_on(gh.issues(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(page.items.len(), 1);
    let issue = &page.items[0];
    assert_eq!(issue.number, 7);
    assert_eq!(issue.comments, 3);
    assert_eq!(issue.assignees[0].login, "InfernoDragon0");
    assert_eq!(issue.labels[0].color, None);

    let created = block_on(gh.create_issue(
        &repo(),
        &NewIssue {
            title: "Crash on open".into(),
            body: Some("Steps…".into()),
            labels: vec!["bug".into()],
        },
    ))
    .unwrap();
    assert_eq!(created.number, 8);
    assert_eq!(created.state, IssueState::Open);

    let comment = block_on(gh.comment_on_issue(&repo(), 7, "On it")).unwrap();
    assert_eq!(comment.id, 9001);
    assert_eq!(comment.body, "On it");
    let last = replay.seen().pop().unwrap();
    assert_eq!(last.body.unwrap(), json!({"body": "On it"}));
}

#[test]
fn ci_status_combines_checks_and_statuses() {
    let (_, gh) = setup(Duration::ZERO);
    let ci = block_on(gh.ci_status(&repo(), "main")).unwrap();
    assert_eq!(ci.state, CiState::Pending, "one run still in progress");
    assert_eq!(ci.checks.len(), 3);
    assert_eq!(ci.checks[0].description.as_deref(), Some("Build passed"));
    assert_eq!(
        ci.checks[1].url.as_deref(),
        Some("https://github.com/InfernoDragon0/GitDragon/runs/2")
    );
    // No checks permission: statuses still count.
    let ci = block_on(gh.ci_status(&repo(), "broken")).unwrap();
    assert_eq!(ci.state, CiState::Failure);
    assert_eq!(ci.checks.len(), 2);
}

#[test]
fn author_avatars() {
    let (replay, gh) = setup(Duration::from_secs(60));
    assert!(gh.supports(Feature::Avatars));
    let avatar = |sha: &str| block_on(gh.author_avatar(&repo(), sha, "me@example.com"));
    assert_eq!(
        avatar("aaaa1111").unwrap().as_deref(),
        Some("https://avatars.githubusercontent.com/u/583231?v=4&s=64")
    );
    assert_eq!(gh.rate_limit().remaining, Some(4000));
    // A commit by an email GitHub cannot match has no author: not an error.
    assert_eq!(avatar("bbbb2222").unwrap(), None);
    // Answers come through the cache.
    avatar("aaaa1111").unwrap();
    let url = format!("{API}/repos/InfernoDragon0/GitDragon/commits/aaaa1111");
    assert_eq!(replay.count(&url), 1);
    assert!(matches!(
        avatar("cccc3333"),
        Err(e) if e.is_transient()
    ));
}

#[test]
fn etag_revalidation() {
    let (replay, gh) = setup(Duration::ZERO);
    let url = format!("{API}/user");
    replay.first(
        Route::new("GET", &url, 304, json!(null))
            .when("if-none-match", "W/\"user-1\"")
            .header("x-ratelimit-remaining", "4999")
            .header("x-ratelimit-reset", "4102444800"),
    );
    let first = block_on(gh.whoami()).unwrap();
    let second = block_on(gh.whoami()).unwrap();
    assert_eq!(first, second);
    let seen = replay.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].header("if-none-match"), None);
    assert_eq!(seen[1].header("if-none-match"), Some("W/\"user-1\""));
}

#[test]
fn rate_limit_blocks_and_serves_stale() {
    let (replay, gh) = setup(Duration::ZERO);
    let user = format!("{API}/user");
    // Cache /user first, then the quota runs out.
    block_on(gh.whoami()).unwrap();
    replay.first(
        Route::new(
            "GET",
            &user,
            403,
            json!({"message": "API rate limit exceeded for user."}),
        )
        .header("x-ratelimit-remaining", "0")
        .header("x-ratelimit-reset", "4102444800"),
    );
    // Revalidation hits the limit: an error, and the block is remembered.
    let err = block_on(gh.whoami()).unwrap_err();
    assert!(matches!(err, HostError::RateLimited { .. }), "{err}");
    assert!(gh.rate_limit().blocked_until.is_some());
    let calls = replay.seen().len();
    // While blocked: cached answers still work, uncached ones fail without a request.
    assert_eq!(block_on(gh.whoami()).unwrap().login, "InfernoDragon0");
    let err = block_on(gh.repos(1)).unwrap_err();
    assert!(matches!(err, HostError::RateLimited { .. }));
    assert!(err.is_transient());
    assert_eq!(replay.seen().len(), calls, "no requests while blocked");
}

/// GitHub answers GraphQL without a token with `403` and a quota of 0. That is "sign in", not
/// a wait, and it must not block the REST calls (pull requests, issues).
#[test]
fn graphql_without_a_quota_is_not_a_rate_limit() {
    let (replay, gh) = setup(Duration::ZERO);
    replay.first(
        Route::new(
            "POST",
            &format!("{API}/graphql"),
            403,
            json!({"message": "API rate limit exceeded"}),
        )
        .header("x-ratelimit-limit", "0")
        .header("x-ratelimit-remaining", "0")
        .header("x-ratelimit-resource", "graphql")
        .header("x-ratelimit-reset", "4102444800"),
    );
    let err = block_on(gh.boards(&repo())).unwrap_err();
    assert!(matches!(err, HostError::Forbidden { .. }), "{err}");
    assert!(err.to_string().contains("signed-in account"), "{err}");
    assert!(gh.rate_limit().blocked_until.is_none());
    assert_eq!(block_on(gh.whoami()).unwrap().login, "InfernoDragon0");
}

/// A spent GraphQL quota blocks GraphQL only; REST keeps its own.
#[test]
fn graphql_rate_limit_leaves_rest_alone() {
    let (replay, gh) = setup(Duration::ZERO);
    replay.first(
        Route::new(
            "POST",
            &format!("{API}/graphql"),
            403,
            json!({"message": "API rate limit exceeded"}),
        )
        .header("x-ratelimit-limit", "5000")
        .header("x-ratelimit-remaining", "0")
        .header("x-ratelimit-resource", "graphql")
        .header("x-ratelimit-reset", "4102444800"),
    );
    let err = block_on(gh.boards(&repo())).unwrap_err();
    assert!(matches!(err, HostError::RateLimited { .. }), "{err}");
    let calls = replay.seen().len();
    // GraphQL stays blocked without asking again...
    let err = block_on(gh.boards(&repo())).unwrap_err();
    assert!(matches!(err, HostError::RateLimited { .. }), "{err}");
    assert_eq!(
        replay.seen().len(),
        calls,
        "no GraphQL request while blocked"
    );
    // ...while REST still works.
    assert!(gh.rate_limit().blocked_until.is_none());
    assert_eq!(block_on(gh.whoami()).unwrap().login, "InfernoDragon0");
}

#[test]
fn secondary_rate_limit_uses_retry_after() {
    let (replay, gh) = setup(Duration::ZERO);
    replay.first(
        Route::new(
            "GET",
            &format!("{API}/user"),
            429,
            json!({"message": "slow down"}),
        )
        .header("retry-after", "120"),
    );
    let err = block_on(gh.whoami()).unwrap_err();
    let HostError::RateLimited {
        until: Some(until), ..
    } = err
    else {
        panic!("{err:?}");
    };
    let wait = until.duration_since(std::time::SystemTime::now()).unwrap();
    assert!(wait > Duration::from_secs(100) && wait <= Duration::from_secs(120));
    assert!(err.to_string().contains("try again in 2 min"), "{err}");
}

#[test]
fn error_mapping() {
    let (replay, gh) = setup(Duration::ZERO);
    let pulls = |n: u64| format!("{API}/repos/InfernoDragon0/GitDragon/pulls/{n}");
    replay.push(Route::new(
        "GET",
        &pulls(1),
        401,
        json!({"message": "Bad credentials"}),
    ));
    replay.push(Route::new(
        "GET",
        &pulls(2),
        404,
        json!({"message": "Not Found"}),
    ));
    replay.push(Route::new(
        "GET",
        &pulls(3),
        403,
        json!({"message": "Must have admin rights to Repository."}),
    ));
    replay.push(Route::new(
        "GET",
        &pulls(4),
        502,
        json!({"message": "Server Error"}),
    ));
    let get = |n| block_on(gh.pull_request(&repo(), n)).unwrap_err();
    assert!(matches!(get(1), HostError::Unauthorized { .. }));
    assert!(matches!(get(2), HostError::NotFound { .. }));
    assert!(
        matches!(get(3), HostError::Forbidden { ref message, .. } if message.contains("admin rights"))
    );
    assert!(matches!(get(4), HostError::Server { status: 502, .. }));
    assert!(get(5).is_transient(), "unrecorded = network error");

    replay.first(Route::new(
        "POST",
        &format!("{API}/repos/InfernoDragon0/GitDragon/pulls"),
        422,
        json!({"message": "Validation Failed", "errors": [{"message": "A pull request already exists for InfernoDragon0:new-thing."}]}),
    ));
    let new = NewPullRequest {
        title: "dup".into(),
        body: None,
        source_branch: "new-thing".into(),
        target_branch: "main".into(),
        draft: false,
    };
    let err = block_on(gh.create_pull_request(&repo(), &new)).unwrap_err();
    assert!(
        matches!(err, HostError::Invalid { ref message, .. } if message.contains("already exists")),
        "{err:?}"
    );
}

#[test]
fn error_messages_are_redacted() {
    let (replay, gh) = setup(Duration::ZERO);
    git_core::redact::register("ghp_testtoken");
    replay.push(Route::new(
        "GET",
        &format!("{API}/repos/InfernoDragon0/GitDragon/pulls/9"),
        403,
        json!({"message": "token ghp_testtoken lacks scope"}),
    ));
    let err = block_on(gh.pull_request(&repo(), 9)).unwrap_err();
    assert!(!err.to_string().contains("ghp_testtoken"), "{err}");
}

#[test]
fn redirects_keep_the_token_only_on_the_same_origin() {
    let (replay, gh) = setup(Duration::ZERO);
    let old = format!("{API}/repos/InfernoDragon0/GitDragon/pulls/50");
    let moved = format!("{API}/repositories/10/pulls/50");
    let pr = json!({
        "number": 50, "title": "t", "state": "open",
        "head": {"ref": "a", "sha": "1"}, "base": {"ref": "main", "sha": "2"},
        "html_url": "https://github.com/x/y/pull/50",
        "created_at": "2026-10-09T00:00:00Z", "updated_at": "2026-10-09T00:00:00Z"
    });
    replay.push(Route::new("GET", &old, 301, json!(null)).header("location", &moved));
    replay.push(Route::new("GET", &moved, 200, pr.clone()));
    assert_eq!(block_on(gh.pull_request(&repo(), 50)).unwrap().number, 50);
    let seen = replay.seen();
    assert_eq!(seen[1].url, moved);
    assert_eq!(seen[1].auth, Some("Bearer"), "same origin keeps the token");

    // Another host: the token is dropped and stays dropped.
    let away = "https://objects.example.com/pr/51";
    let back = format!("{API}/repos/InfernoDragon0/GitDragon/pulls/51-back");
    replay.push(
        Route::new(
            "GET",
            &format!("{API}/repos/InfernoDragon0/GitDragon/pulls/51"),
            302,
            json!(null),
        )
        .header("location", away),
    );
    replay.push(Route::new("GET", away, 307, json!(null)).header("location", &back));
    replay.push(Route::new("GET", &back, 200, pr));
    block_on(gh.pull_request(&repo(), 51)).unwrap();
    let seen = replay.seen();
    let n = seen.len();
    assert_eq!(seen[n - 2].url, away);
    assert_eq!(seen[n - 2].auth, None);
    assert_eq!(seen[n - 1].auth, None, "not re-added on the way back");

    // Never down to plain HTTP.
    replay.push(
        Route::new(
            "GET",
            &format!("{API}/repos/InfernoDragon0/GitDragon/pulls/52"),
            301,
            json!(null),
        )
        .header("location", "http://api.github.com/insecure"),
    );
    let err = block_on(gh.pull_request(&repo(), 52)).unwrap_err();
    assert!(matches!(err, HostError::Redirect(_)), "{err:?}");
    assert!(!replay.seen().iter().any(|s| s.url.starts_with("http://")));
}

#[test]
fn redirected_writes_are_refused() {
    let (replay, gh) = setup(Duration::ZERO);
    replay.first(
        Route::new(
            "POST",
            &format!("{API}/repos/InfernoDragon0/GitDragon/issues"),
            307,
            json!(null),
        )
        .header("location", "https://evil.example.com/issues"),
    );
    let err = block_on(gh.create_issue(
        &repo(),
        &NewIssue {
            title: "x".into(),
            body: None,
            labels: vec![],
        },
    ))
    .unwrap_err();
    assert!(matches!(err, HostError::Redirect(_)));
    assert!(!replay.seen().iter().any(|s| s.url.contains("evil")));
}

#[test]
fn enterprise_server_base_and_web_urls() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        "https://ghe.corp.example:8443/api/v3/user",
        200,
        json!({"login": "me"}),
    ));
    let gh = GitHub::new("https://ghe.corp.example:8443/", None, replay.clone()).unwrap();
    assert_eq!(block_on(gh.whoami()).unwrap().login, "me");
    assert_eq!(replay.seen()[0].auth, None, "no token, no header");

    let repo = RepoId::parse("https://ghe.corp.example:8443/team/app.git").unwrap();
    let web = |t: WebTarget| gh.web_url(&repo, &t).unwrap();
    assert_eq!(
        web(WebTarget::Repo),
        "https://ghe.corp.example:8443/team/app"
    );
    assert_eq!(
        web(WebTarget::Branch("feature/x y".into())),
        "https://ghe.corp.example:8443/team/app/tree/feature%2Fx%20y"
    );
    assert_eq!(
        web(WebTarget::PullRequest(4)),
        "https://ghe.corp.example:8443/team/app/pull/4"
    );
    assert_eq!(
        web(WebTarget::Issues),
        "https://ghe.corp.example:8443/team/app/issues"
    );
    assert_eq!(
        web(WebTarget::NewPullRequest {
            source: "topic".into(),
            target: "main".into()
        }),
        "https://ghe.corp.example:8443/team/app/compare/main...topic?expand=1"
    );
}

#[test]
fn connect_and_account_matching() {
    let account = |kind, base: &str| Account {
        id: "a".into(),
        name: "n".into(),
        kind,
        base_url: base.into(),
        auth: AuthMode::Pat,
        username: None,
    };
    let transport: Arc<dyn Transport> = Arc::new(Replay::new());
    let host = connect(
        &account(HostKind::GitHub, "https://github.com"),
        None,
        transport.clone(),
    )
    .unwrap();
    assert_eq!(host.kind(), HostKind::GitHub);
    assert_eq!(host.vocab().pr_short, "PR");
    assert!(host.supports(Feature::CiStatus));
    assert!(matches!(
        connect(
            &account(HostKind::Generic, "https://git.example.com"),
            None,
            transport
        ),
        Err(HostError::Unsupported(Feature::Api))
    ));

    let accounts = [
        account(HostKind::GitLab, "https://gitlab.example.com:8443"),
        account(HostKind::GitHub, "https://github.com"),
    ];
    let hit =
        |remote: &str| account_for(&accounts, &RepoId::parse(remote).unwrap()).map(|a| a.kind);
    assert_eq!(hit("git@github.com:o/r.git"), Some(HostKind::GitHub));
    assert_eq!(
        hit("https://gitlab.example.com:8443/g/p.git"),
        Some(HostKind::GitLab)
    );
    assert_eq!(hit("https://gitlab.example.com/g/p.git"), None);
}

/// A host that implements nothing answers Unsupported everywhere.
#[test]
fn defaults_are_unsupported() {
    struct Bare;
    impl GitHost for Bare {
        fn kind(&self) -> HostKind {
            HostKind::Generic
        }
        fn supports(&self, _: Feature) -> bool {
            false
        }
        fn web_url(&self, _: &RepoId, _: &WebTarget) -> Option<String> {
            None
        }
    }
    let bare = Bare;
    assert_eq!(bare.vocab().service, "Git server");
    assert_eq!(
        block_on(bare.whoami()).unwrap_err(),
        HostError::Unsupported(Feature::Whoami)
    );
    assert_eq!(
        block_on(bare.ci_status(&repo(), "main")).unwrap_err(),
        HostError::Unsupported(Feature::CiStatus)
    );
    assert_eq!(
        block_on(bare.author_avatar(&repo(), "abc", "a@b.c")).unwrap_err(),
        HostError::Unsupported(Feature::Avatars)
    );
    assert_eq!(
        HostVocab::of(HostKind::GitLab).pull_request,
        "Merge request"
    );
}

#[test]
fn closing_and_reopening_an_issue_patches_its_state() {
    let (replay, gh) = setup(Duration::ZERO);
    assert!(gh.supports(Feature::CloseIssue));
    let url = format!("{API}/repos/InfernoDragon0/GitDragon/issues/7");
    let issue = |state: &str| {
        json!({
            "number": 7, "title": "Crash", "state": state,
            "html_url": "https://github.com/InfernoDragon0/GitDragon/issues/7",
            "created_at": "2026-10-09T10:00:00Z", "updated_at": "2026-10-09T12:00:00Z"
        })
    };
    replay.push(Route::new("PATCH", &url, 200, issue("closed")).once());
    replay.push(Route::new("PATCH", &url, 200, issue("open")).once());

    let closed = block_on(gh.set_issue_state(&repo(), 7, false)).unwrap();
    assert_eq!(closed.state, IssueState::Closed);
    let sent = replay.seen().pop().unwrap();
    assert_eq!(sent.method, Method::Patch);
    assert_eq!(sent.body.unwrap(), json!({"state": "closed"}));

    let reopened = block_on(gh.set_issue_state(&repo(), 7, true)).unwrap();
    assert_eq!(reopened.state, IssueState::Open);
    assert_eq!(
        replay.seen().pop().unwrap().body.unwrap(),
        json!({"state": "open"})
    );
}

#[test]
fn fork_pull_requests_are_fetched_through_the_pull_ref() {
    let (_, gh) = setup(Duration::ZERO);
    assert_eq!(gh.pull_head_ref(42).as_deref(), Some("pull/42/head"));
}

#[test]
fn pull_request_files_page_through_with_patches() {
    let (replay, gh) = setup(Duration::ZERO);
    let base = format!("{API}/repos/InfernoDragon0/GitDragon/pulls/42/files");
    replay.push(
        Route::new(
            "GET",
            &format!("{base}?per_page=100&page=1"),
            200,
            json!([
                {"filename": "src/a.rs", "status": "modified", "additions": 2, "deletions": 1,
                 "patch": "@@ -1 +1,2 @@\n-a\n+b\n+c"},
                {"filename": "img.png", "status": "added", "additions": 0, "deletions": 0}
            ]),
        )
        .header(
            "link",
            &format!("<{base}?per_page=100&page=2>; rel=\"next\""),
        ),
    );
    replay.push(Route::new(
        "GET",
        &format!("{base}?per_page=100&page=2"),
        200,
        json!([
            {"filename": "new/b.rs", "previous_filename": "old/b.rs", "status": "renamed",
             "additions": 0, "deletions": 0, "patch": ""},
            {"filename": "gone.rs", "status": "removed", "additions": 0, "deletions": 3,
             "patch": "@@ -1,3 +0,0 @@\n-x\n-y\n-z"}
        ]),
    ));
    let files = block_on(gh.pr_files(&repo(), 42)).unwrap();
    assert!(!files.truncated);
    let got: Vec<(&str, PrFileStatus, u32, u32, bool)> = files
        .files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                f.status,
                f.additions,
                f.deletions,
                f.patch.is_some(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("src/a.rs", PrFileStatus::Modified, 2, 1, true),
            ("img.png", PrFileStatus::Added, 0, 0, false),
            ("new/b.rs", PrFileStatus::Renamed, 0, 0, false),
            ("gone.rs", PrFileStatus::Removed, 0, 3, true),
        ]
    );
    assert_eq!(files.files[2].old_path.as_deref(), Some("old/b.rs"));
    assert!(gh.supports(Feature::PrFiles));
    assert_eq!(
        gh.pull_changes_ref(42),
        Some(PullRef {
            reference: "pull/42/head".into(),
            merge_preview: false
        })
    );
}
