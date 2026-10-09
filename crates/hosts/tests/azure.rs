//! Azure DevOps provider against recorded responses (`fixtures/azure.json`): auth kinds,
//! mapping, paging, work items (WIQL + batch), builds, URL encoding and error mapping.

use std::sync::Arc;
use std::time::Duration;

use accounts::{Account, AuthMode, HostKind, Secret, net::block_on};
use hosts::http::Method;
use hosts::replay::{Replay, Route};
use hosts::*;
use serde_json::{Value, json};

const ORG: &str = "https://dev.azure.com/infernodragon0";
const FIELDS_Q: &str = "System.Id%2CSystem.Title%2CSystem.State%2CSystem.CreatedBy%2CSystem.AssignedTo%2CSystem.Tags%2CSystem.CreatedDate%2CSystem.ChangedDate%2CSystem.CommentCount%2CSystem.Description";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn setup(oauth: bool, ttl: Duration) -> (Arc<Replay>, AzureDevOps) {
    let replay = Arc::new(Replay::from_json(include_str!("fixtures/azure.json")));
    let azure = AzureDevOps::new(ORG, Some(Secret::new("pat-token")), oauth, replay.clone())
        .unwrap()
        .with_ttl(ttl);
    (replay, azure)
}

fn repo() -> RepoId {
    RepoId::parse("https://infernodragon0@dev.azure.com/infernodragon0/GitDragon/_git/GitDragon")
        .unwrap()
}

fn spaced() -> RepoId {
    RepoId::parse("git@ssh.dev.azure.com:v3/infernodragon0/My%20Project/Open%20Docs").unwrap()
}

#[test]
fn whoami_with_pat_is_basic() {
    let (replay, az) = setup(false, Duration::ZERO);
    let me = block_on(az.whoami()).unwrap();
    assert_eq!(me.login, "inferno@example.com");
    assert_eq!(me.name.as_deref(), Some("Inferno Dragon"));
    let seen = &replay.seen()[0];
    assert_eq!(seen.auth, Some("Basic"));
    assert_eq!(seen.header("accept"), Some("application/json"));
}

#[test]
fn oauth_token_is_bearer_and_missing_token_sends_nothing() {
    let (replay, az) = setup(true, Duration::ZERO);
    block_on(az.whoami()).unwrap();
    assert_eq!(replay.seen()[0].auth, Some("Bearer"));

    // A GCM account has no token: no Authorization header (Azure answers 401).
    let bare = Arc::new(Replay::from_json(include_str!("fixtures/azure.json")));
    let az = AzureDevOps::new(ORG, None, false, bare.clone()).unwrap();
    block_on(az.whoami()).unwrap();
    assert_eq!(bare.seen()[0].auth, None);
    // Swapping the token (OAuth refresh) takes effect.
    az.set_token(Some(Secret::new("new")), true);
    block_on(az.whoami()).unwrap();
    assert_eq!(bare.seen()[1].auth, Some("Bearer"));
}

#[test]
fn repos_are_one_unpaged_list() {
    let (replay, az) = setup(false, Duration::ZERO);
    let page = block_on(az.repos(1)).unwrap();
    assert_eq!(page.next, None);
    let [first, second] = page.items.as_slice() else {
        panic!("disabled repos are dropped: {:?}", page.items);
    };
    assert_eq!(first.id, repo());
    assert!(first.private && !first.fork);
    assert_eq!(first.default_branch.as_deref(), Some("main"));
    assert_eq!(
        first.clone_url.as_deref(),
        Some("https://infernodragon0@dev.azure.com/infernodragon0/GitDragon/_git/GitDragon")
    );
    assert_eq!(
        first.ssh_url.as_deref(),
        Some("git@ssh.dev.azure.com:v3/infernodragon0/GitDragon/GitDragon")
    );
    assert_eq!(second.id, spaced());
    assert_eq!(second.id.project.as_deref(), Some("My Project"));
    assert!(!second.private, "public project");
    assert!(second.fork);
    assert_eq!(second.default_branch, None);
    assert_eq!(
        second.web_url,
        "https://dev.azure.com/infernodragon0/My%20Project/_git/Open%20Docs"
    );

    let calls = replay.seen().len();
    let more = block_on(az.repos(2)).unwrap();
    assert!(more.items.is_empty() && more.next.is_none());
    assert_eq!(replay.seen().len(), calls, "page 2 needs no request");
}

#[test]
fn pull_requests_map_status_and_refs() {
    let (_, az) = setup(false, Duration::ZERO);
    let page = block_on(az.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(page.next, None);
    let [draft, fork] = page.items.as_slice() else {
        panic!("two PRs");
    };
    assert_eq!(draft.number, 42);
    assert!(draft.draft);
    assert_eq!(draft.state, PrState::Open);
    assert_eq!(draft.source_branch, "feature/azure");
    assert_eq!(draft.target_branch, "main");
    assert_eq!(draft.head_sha, SHA_A);
    assert_eq!(draft.body.as_deref(), Some("Adds the **provider**."));
    assert_eq!(draft.labels.len(), 1, "inactive label dropped");
    assert_eq!(draft.labels[0].name, "backend");
    let author = draft.author.as_ref().unwrap();
    assert_eq!(author.login, "inferno@example.com");
    assert_eq!(author.name.as_deref(), Some("Inferno Dragon"));
    assert!(
        author
            .avatar_url
            .as_ref()
            .unwrap()
            .contains("MemberAvatars")
    );
    assert_eq!(
        draft.web_url,
        "https://dev.azure.com/infernodragon0/GitDragon/_git/GitDragon/pullrequest/42"
    );
    assert_eq!(draft.created_at, "2026-10-08T09:30:00.1234567Z");
    assert_eq!(draft.updated_at, draft.created_at, "no close date yet");
    assert_eq!(draft.source_repo, None);
    assert_eq!(fork.source_repo.as_deref(), Some("Forks/GitDragon-fork"));
    assert!(!fork.draft);
}

#[test]
fn closed_pull_requests_are_completed_and_abandoned() {
    let (_, az) = setup(false, Duration::ZERO);
    let query = |state| ListQuery {
        state,
        ..ListQuery::default()
    };
    let closed = block_on(az.pull_requests(&repo(), &query(StateFilter::Closed))).unwrap();
    let [merged, dropped] = closed.items.as_slice() else {
        panic!("active PRs are dropped: {:?}", closed.items);
    };
    assert_eq!(merged.state, PrState::Merged);
    assert_eq!(merged.updated_at, "2026-10-02T10:15:00Z");
    assert_eq!(dropped.state, PrState::Closed);
    assert_eq!(dropped.head_sha, "");
    assert_eq!(dropped.author, None);
    let all = block_on(az.pull_requests(&repo(), &query(StateFilter::All))).unwrap();
    assert_eq!(all.items.len(), 3);

    let one = block_on(az.pull_request(&repo(), 40)).unwrap();
    assert_eq!(one.state, PrState::Merged);
    assert_eq!(one.body, None, "empty description");
}

#[test]
fn pull_request_paging_follows_full_pages() {
    let (replay, az) = setup(false, Duration::ZERO);
    let pr = |id: u64| {
        json!({"pullRequestId": id, "status": "active", "creationDate": "2026-10-01T00:00:00Z",
            "title": "t", "sourceRefName": "refs/heads/a", "targetRefName": "refs/heads/main"})
    };
    let url = |skip: u32| {
        format!(
            "{ORG}/GitDragon/_apis/git/repositories/GitDragon/pullrequests?searchCriteria.status=active&%24top=2&%24skip={skip}&api-version=7.1"
        )
    };
    replay.push(Route::new(
        "GET",
        &url(0),
        200,
        json!({"value": [pr(3), pr(2)]}),
    ));
    replay.push(Route::new("GET", &url(2), 200, json!({"value": [pr(1)]})));
    let query = |page| ListQuery {
        state: StateFilter::Open,
        page,
        per_page: 2,
    };
    let first = block_on(az.pull_requests(&repo(), &query(1))).unwrap();
    assert_eq!((first.items.len(), first.next), (2, Some(2)));
    let second = block_on(az.pull_requests(&repo(), &query(2))).unwrap();
    assert_eq!((second.items.len(), second.next), (1, None));
}

#[test]
fn create_pull_request_posts_and_invalidates() {
    let (replay, az) = setup(false, Duration::from_secs(600));
    let list = format!(
        "{ORG}/GitDragon/_apis/git/repositories/GitDragon/pullrequests?searchCriteria.status=active&%24top=30&%24skip=0&api-version=7.1"
    );
    block_on(az.pull_requests(&repo(), &ListQuery::default())).unwrap();
    block_on(az.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(replay.count(&list), 1, "second list from the TTL cache");

    let new = NewPullRequest {
        title: "New thing".into(),
        body: Some("Body".into()),
        source_branch: "new-thing".into(),
        target_branch: "main".into(),
        draft: false,
    };
    let pr = block_on(az.create_pull_request(&repo(), &new)).unwrap();
    assert_eq!((pr.number, pr.state), (43, PrState::Open));
    let post = replay
        .seen()
        .into_iter()
        .find(|s| s.method == Method::Post)
        .unwrap();
    assert_eq!(
        post.body.clone().unwrap(),
        json!({"sourceRefName": "refs/heads/new-thing", "targetRefName": "refs/heads/main",
            "title": "New thing", "description": "Body", "isDraft": false})
    );
    assert_eq!(
        post.header("content-type"),
        None,
        "transport adds plain JSON"
    );
    block_on(az.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(
        replay.count(&list),
        2,
        "creating a PR drops the repo's cache"
    );

    // No description: the field is left out.
    let draft = NewPullRequest {
        body: None,
        draft: true,
        ..new
    };
    block_on(az.create_pull_request(&repo(), &draft)).unwrap();
    let body = replay.seen().pop().unwrap().body.unwrap();
    assert_eq!(body.get("description"), None);
    assert_eq!(body["isDraft"], true);
}

#[test]
fn work_items_are_wiql_then_a_batch() {
    let (replay, az) = setup(false, Duration::ZERO);
    let page = block_on(az.issues(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(page.next, None);
    // WIQL order (12, 11) wins over the batch's.
    let [slow, crash] = page.items.as_slice() else {
        panic!("two work items");
    };
    assert_eq!(slow.number, 12);
    assert_eq!(slow.state, IssueState::Closed, "Resolved");
    assert_eq!(slow.comments, 3);
    assert_eq!(slow.body.as_deref(), Some("<div>Scrolling lags.</div>"));
    let names: Vec<&str> = slow.labels.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["perf", "ui"]);
    assert_eq!(slow.assignees[0].login, "inferno@example.com");
    assert_eq!(
        slow.web_url,
        "https://dev.azure.com/infernodragon0/GitDragon/_workitems/edit/12"
    );
    assert_eq!(crash.state, IssueState::Open);
    assert!(crash.labels.is_empty() && crash.assignees.is_empty());
    assert_eq!(crash.author.as_ref().unwrap().login, "octo@example.com");

    let seen = replay.seen();
    assert_eq!(seen[0].method, Method::Post);
    assert_eq!(seen[0].auth, Some("Basic"));
    let wiql = seen[0].body.as_ref().unwrap()["query"].as_str().unwrap();
    assert!(wiql.contains("[System.TeamProject] = @project"), "{wiql}");
    assert!(
        wiql.contains("[System.State] Not In ('Closed','Done','Removed','Resolved')"),
        "{wiql}"
    );
    assert!(
        wiql.ends_with("Order By [System.ChangedDate] Desc"),
        "{wiql}"
    );
    assert_eq!(seen[1].method, Method::Get);

    let query = |state| ListQuery {
        state,
        ..ListQuery::default()
    };
    block_on(az.issues(&repo(), &query(StateFilter::Closed))).unwrap();
    block_on(az.issues(&repo(), &query(StateFilter::All))).unwrap();
    let wiql = |i: usize| {
        replay.seen()[i].body.as_ref().unwrap()["query"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(wiql(2).contains("[System.State] In ('Closed'"));
    assert!(!wiql(4).contains("[System.State]"), "all states");
}

#[test]
fn work_item_paging_slices_the_ids() {
    let (replay, az) = setup(false, Duration::ZERO);
    let item = |id: u64| {
        json!({"id": id, "fields": {"System.Id": id, "System.Title": format!("Item {id}"),
            "System.State": "New", "System.CreatedDate": "2026-10-01T00:00:00Z",
            "System.ChangedDate": "2026-10-02T00:00:00Z"}})
    };
    let wiql = |top: u32| format!("{ORG}/GitDragon/_apis/wit/wiql?%24top={top}&api-version=7.1");
    let batch = |ids: &str| {
        format!("{ORG}/_apis/wit/workitems?ids={ids}&fields={FIELDS_Q}&api-version=7.1")
    };
    let all = json!({"workItems": [{"id": 30}, {"id": 29}, {"id": 28}]});
    replay.push(Route::new("POST", &wiql(3), 200, all.clone()));
    replay.push(Route::new("POST", &wiql(5), 200, all));
    // The batch answers in another order than asked.
    replay.push(Route::new(
        "GET",
        &batch("30%2C29"),
        200,
        json!({"value": [item(29), item(30)]}),
    ));
    replay.push(Route::new(
        "GET",
        &batch("28"),
        200,
        json!({"value": [item(28)]}),
    ));
    let query = |page| ListQuery {
        state: StateFilter::Open,
        page,
        per_page: 2,
    };
    let first = block_on(az.issues(&repo(), &query(1))).unwrap();
    let numbers: Vec<u64> = first.items.iter().map(|i| i.number).collect();
    assert_eq!((numbers, first.next), (vec![30, 29], Some(2)));
    let second = block_on(az.issues(&repo(), &query(2))).unwrap();
    let numbers: Vec<u64> = second.items.iter().map(|i| i.number).collect();
    assert_eq!((numbers, second.next), (vec![28], None));

    // No ids, no batch request.
    replay.first(Route::new("POST", &wiql(3), 200, json!({"workItems": []})));
    let calls = replay.seen().len();
    let none = block_on(az.issues(&repo(), &query(1))).unwrap();
    assert!(none.items.is_empty() && none.next.is_none());
    assert_eq!(replay.seen().len(), calls + 1);
}

#[test]
fn create_work_item_sends_json_patch() {
    let (replay, az) = setup(false, Duration::ZERO);
    let created = block_on(az.create_issue(
        &repo(),
        &NewIssue {
            title: "Crash on open".into(),
            body: Some("Steps".into()),
            labels: vec!["bug".into(), "crash".into()],
        },
    ))
    .unwrap();
    assert_eq!(created.number, 13);
    assert_eq!(created.state, IssueState::Open);
    assert_eq!(created.labels.len(), 2);
    let post = replay.seen().pop().unwrap();
    assert_eq!(post.method, Method::Post);
    assert_eq!(
        post.url,
        format!("{ORG}/GitDragon/_apis/wit/workitems/$Issue?api-version=7.1")
    );
    assert_eq!(
        post.header("content-type"),
        Some("application/json-patch+json")
    );
    assert_eq!(
        post.body.clone().unwrap(),
        json!([
            {"op": "add", "path": "/fields/System.Title", "value": "Crash on open"},
            {"op": "add", "path": "/fields/System.Description", "value": "Steps"},
            {"op": "add", "path": "/fields/System.Tags", "value": "bug; crash"},
        ])
    );

    // Title only.
    block_on(az.create_issue(
        &repo(),
        &NewIssue {
            title: "x".into(),
            body: None,
            labels: vec![],
        },
    ))
    .unwrap();
    let ops = replay.seen().pop().unwrap().body.unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1);
}

#[test]
fn comments_use_the_preview_version() {
    let (replay, az) = setup(false, Duration::ZERO);
    let comment = block_on(az.comment_on_issue(&repo(), 12, "On it")).unwrap();
    assert_eq!(comment.id, 9001);
    assert_eq!(comment.body, "On it");
    assert_eq!(comment.author.unwrap().login, "inferno@example.com");
    let post = replay.seen().pop().unwrap();
    assert!(
        post.url
            .ends_with("/workItems/12/comments?api-version=7.1-preview.4")
    );
    assert_eq!(post.body.unwrap(), json!({"text": "On it"}));
}

#[test]
fn ci_status_keeps_the_latest_build_per_pipeline() {
    let (_, az) = setup(false, Duration::ZERO);
    let ci = block_on(az.ci_status(&repo(), "main")).unwrap();
    assert_eq!(ci.state, CiState::Pending, "Docs is still running");
    let [build, docs] = ci.checks.as_slice() else {
        panic!("older CI build is dropped: {:?}", ci.checks);
    };
    assert_eq!((build.name.as_str(), build.state), ("CI", CiState::Success));
    assert_eq!(
        build.description.as_deref(),
        Some("Build 20261009.2: succeeded")
    );
    assert_eq!(
        build.url.as_deref(),
        Some("https://dev.azure.com/infernodragon0/9d6a0000/_build/results?buildId=502")
    );
    assert_eq!((docs.name.as_str(), docs.state), ("Docs", CiState::Pending));
    assert_eq!(
        docs.description.as_deref(),
        Some("Build 20261009.1: inProgress")
    );

    // refs/heads/ prefix is accepted too.
    let again = block_on(az.ci_status(&repo(), "refs/heads/main")).unwrap();
    assert_eq!(again, ci);

    let broken = block_on(az.ci_status(&repo(), "broken")).unwrap();
    assert_eq!(broken.state, CiState::Failure);
    assert_eq!(broken.checks.len(), 2);
    assert_eq!(broken.checks[1].state, CiState::Failure, "canceled");
    assert_eq!(broken.checks[1].url, None);

    let none = block_on(az.ci_status(&repo(), "none")).unwrap();
    assert_eq!((none.state, none.checks.len()), (CiState::None, 0));
}

#[test]
fn ci_status_by_commit_filters_on_source_version() {
    let (_, az) = setup(false, Duration::ZERO);
    let ci = block_on(az.ci_status(&repo(), SHA_A)).unwrap();
    // partiallySucceeded counts as passing; the older CI build and other commits are ignored.
    assert_eq!(ci.state, CiState::Success);
    assert_eq!(ci.checks.len(), 1);
    assert_eq!(
        ci.checks[0].description.as_deref(),
        Some("Build 20261009.2: partiallySucceeded")
    );
}

#[test]
fn spaces_in_names_are_encoded() {
    let (replay, az) = setup(false, Duration::ZERO);
    let id = spaced();
    replay.push(Route::new(
        "GET",
        &format!(
            "{ORG}/My%20Project/_apis/git/repositories/Open%20Docs/pullrequests/5?api-version=7.1"
        ),
        200,
        json!({"pullRequestId": 5, "status": "active", "creationDate": "2026-10-01T00:00:00Z",
            "title": "t", "sourceRefName": "refs/heads/a", "targetRefName": "refs/heads/main"}),
    ));
    let pr = block_on(az.pull_request(&id, 5)).unwrap();
    assert_eq!(
        pr.web_url,
        "https://dev.azure.com/infernodragon0/My%20Project/_git/Open%20Docs/pullrequest/5"
    );
    assert_eq!(
        az.web_url(&id, &WebTarget::Repo).unwrap(),
        "https://dev.azure.com/infernodragon0/My%20Project/_git/Open%20Docs"
    );
    assert_eq!(
        az.web_url(&id, &WebTarget::Issue(9)).unwrap(),
        "https://dev.azure.com/infernodragon0/My%20Project/_workitems/edit/9"
    );
}

#[test]
fn legacy_visualstudio_base() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        "https://infernodragon0.visualstudio.com/_apis/connectionData?api-version=7.1-preview",
        200,
        json!({"authenticatedUser": {"providerDisplayName": "Inferno Dragon"}}),
    ));
    replay.push(Route::new(
        "GET",
        "https://infernodragon0.visualstudio.com/Proj/_apis/git/repositories/repo/pullrequests/1?api-version=7.1",
        200,
        json!({"pullRequestId": 1, "status": "abandoned", "creationDate": "2026-10-01T00:00:00Z",
            "title": "t", "sourceRefName": "refs/heads/a", "targetRefName": "refs/heads/main"}),
    ));
    let az = AzureDevOps::new(
        "https://infernodragon0.visualstudio.com/",
        Some(Secret::new("pat")),
        false,
        replay.clone(),
    )
    .unwrap();
    // Falls back to the display name when there is no account property.
    assert_eq!(block_on(az.whoami()).unwrap().login, "Inferno Dragon");
    let id =
        RepoId::parse("https://infernodragon0.visualstudio.com/DefaultCollection/Proj/_git/repo")
            .unwrap();
    let pr = block_on(az.pull_request(&id, 1)).unwrap();
    assert_eq!(pr.state, PrState::Closed);
    assert_eq!(
        pr.web_url,
        "https://infernodragon0.visualstudio.com/Proj/_git/repo/pullrequest/1"
    );
    assert_eq!(
        az.web_url(&id, &WebTarget::Commit("abc123".into()))
            .unwrap(),
        "https://infernodragon0.visualstudio.com/Proj/_git/repo/commit/abc123"
    );
}

#[test]
fn web_urls() {
    let (_, az) = setup(false, Duration::ZERO);
    let web = |t: WebTarget| az.web_url(&repo(), &t).unwrap();
    let base = "https://dev.azure.com/infernodragon0/GitDragon";
    assert_eq!(web(WebTarget::Repo), format!("{base}/_git/GitDragon"));
    assert_eq!(
        web(WebTarget::Commit("abc".into())),
        format!("{base}/_git/GitDragon/commit/abc")
    );
    assert_eq!(
        web(WebTarget::Branch("feature/x y".into())),
        format!("{base}/_git/GitDragon?version=GBfeature%2Fx+y")
    );
    assert_eq!(
        web(WebTarget::PullRequest(4)),
        format!("{base}/_git/GitDragon/pullrequest/4")
    );
    assert_eq!(
        web(WebTarget::PullRequests),
        format!("{base}/_git/GitDragon/pullrequests")
    );
    assert_eq!(
        web(WebTarget::Issue(7)),
        format!("{base}/_workitems/edit/7")
    );
    assert_eq!(web(WebTarget::Issues), format!("{base}/_workitems"));
    assert_eq!(
        web(WebTarget::NewPullRequest {
            source: "feature/a".into(),
            target: "main".into()
        }),
        format!("{base}/_git/GitDragon/pullrequestcreate?sourceRef=feature%2Fa&targetRef=main")
    );
    // No project, no page.
    let mut no_project = repo();
    no_project.project = None;
    assert_eq!(az.web_url(&no_project, &WebTarget::Repo), None);
}

#[test]
fn errors_map_to_host_errors() {
    let (replay, az) = setup(false, Duration::ZERO);
    let pulls = |n: u64| {
        format!("{ORG}/GitDragon/_apis/git/repositories/GitDragon/pullrequests/{n}?api-version=7.1")
    };
    let route = |n, status, body: Value| replay.push(Route::new("GET", &pulls(n), status, body));
    route(
        1,
        401,
        json!({"message": "TF400813: The user is not authorized."}),
    );
    route(
        2,
        404,
        json!({"message": "TF401180: The requested pull request was not found."}),
    );
    route(
        3,
        403,
        json!({"message": "TF401027: You need the GenericRead permission."}),
    );
    route(4, 503, json!({"message": "Service Unavailable"}));
    route(
        5,
        400,
        json!({"message": "TF401398: The pull request cannot be activated."}),
    );
    replay.push(
        Route::new("GET", &pulls(7), 429, json!({"message": "slow down"}))
            .header("retry-after", "30"),
    );
    let get = |n| block_on(az.pull_request(&repo(), n)).unwrap_err();
    assert!(matches!(get(1), HostError::Unauthorized { .. }));
    assert!(matches!(get(2), HostError::NotFound { .. }));
    assert!(
        matches!(get(3), HostError::Forbidden { ref message, .. } if message.contains("GenericRead"))
    );
    assert!(matches!(get(4), HostError::Server { status: 503, .. }));
    assert!(
        matches!(get(5), HostError::Invalid { ref message, .. } if message.contains("TF401398"))
    );
    assert!(matches!(get(7), HostError::RateLimited { .. }));
    assert!(az.rate_limit().blocked_until.is_some());
}

#[test]
fn project_is_required() {
    let (replay, az) = setup(false, Duration::ZERO);
    let mut id = repo();
    id.project = None;
    let calls = replay.seen().len();
    for err in [
        block_on(az.pull_requests(&id, &ListQuery::default())).unwrap_err(),
        block_on(az.issues(&id, &ListQuery::default())).unwrap_err(),
        block_on(az.ci_status(&id, "main")).unwrap_err(),
        block_on(az.pull_request(&id, 1)).unwrap_err(),
    ] {
        assert!(matches!(err, HostError::Config(_)), "{err:?}");
    }
    assert_eq!(replay.seen().len(), calls, "nothing sent");
}

#[test]
fn every_request_carries_an_api_version() {
    let (replay, az) = setup(false, Duration::ZERO);
    block_on(az.whoami()).unwrap();
    block_on(az.repos(1)).unwrap();
    block_on(az.pull_requests(&repo(), &ListQuery::default())).unwrap();
    block_on(az.pull_request(&repo(), 40)).unwrap();
    block_on(az.issues(&repo(), &ListQuery::default())).unwrap();
    block_on(az.ci_status(&repo(), "main")).unwrap();
    block_on(az.create_pull_request(
        &repo(),
        &NewPullRequest {
            title: "t".into(),
            body: None,
            source_branch: "a".into(),
            target_branch: "main".into(),
            draft: false,
        },
    ))
    .unwrap();
    block_on(az.create_issue(
        &repo(),
        &NewIssue {
            title: "t".into(),
            body: None,
            labels: vec![],
        },
    ))
    .unwrap();
    block_on(az.comment_on_issue(&repo(), 12, "hi")).unwrap();
    let seen = replay.seen();
    assert!(seen.len() >= 11);
    for s in &seen {
        let url = url::Url::parse(&s.url).unwrap();
        let versions: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "api-version")
            .collect();
        assert_eq!(versions.len(), 1, "{}", s.url);
        assert!(versions[0].1.starts_with("7.1"), "{}", s.url);
    }
}

#[test]
fn connect_builds_the_provider() {
    let account = |kind, base: &str, auth| Account {
        id: "a".into(),
        name: "n".into(),
        kind,
        base_url: base.into(),
        auth,
        username: None,
    };
    let replay = Arc::new(Replay::from_json(include_str!("fixtures/azure.json")));
    let transport: Arc<dyn Transport> = replay.clone();
    let host = connect(
        &account(HostKind::AzureDevOps, ORG, AuthMode::OAuth),
        Some(Secret::new("entra")),
        transport.clone(),
    )
    .unwrap();
    assert_eq!(host.kind(), HostKind::AzureDevOps);
    assert_eq!(host.vocab().issue, "Work item");
    assert_eq!(host.vocab().ci, "Pipelines");
    assert!(host.supports(Feature::Issues) && host.supports(Feature::CiStatus));
    block_on(host.whoami()).unwrap();
    assert_eq!(replay.seen()[0].auth, Some("Bearer"));

    let pat = connect(
        &account(HostKind::AzureDevOps, ORG, AuthMode::Pat),
        Some(Secret::new("pat")),
        transport.clone(),
    )
    .unwrap();
    block_on(pat.whoami()).unwrap();
    assert_eq!(replay.seen()[1].auth, Some("Basic"));

    // A base without an organisation is a config error, not a panic.
    let err = connect(
        &account(
            HostKind::AzureDevOps,
            "https://dev.azure.com",
            AuthMode::Pat,
        ),
        None,
        transport,
    )
    .err()
    .unwrap();
    assert!(matches!(err, HostError::Config(_)));
}
