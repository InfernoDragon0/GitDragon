//! GitLab provider against recorded responses (`fixtures/gitlab.json`): auth kinds, paging,
//! merge request / issue / pipeline mapping, project addressing, self-managed bases, errors.

use std::sync::Arc;
use std::time::Duration;

use accounts::{Account, AuthMode, HostKind, Secret, net::block_on};
use hosts::http::Method;
use hosts::replay::{Replay, Route};
use hosts::*;
use serde_json::json;

const API: &str = "https://gitlab.com/api/v4";
const PROJECT: &str = "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproj";

fn setup(ttl: Duration) -> (Arc<Replay>, GitLab) {
    let replay = Arc::new(Replay::from_json(include_str!("fixtures/gitlab.json")));
    let gitlab = GitLab::new(
        "https://gitlab.com",
        Some(Secret::new("glpat-testtoken")),
        false,
        replay.clone(),
    )
    .unwrap()
    .with_ttl(ttl);
    (replay, gitlab)
}

fn repo() -> RepoId {
    RepoId::parse("git@gitlab.com:group/sub/proj.git").unwrap()
}

#[test]
fn whoami_and_auth_kinds() {
    let (replay, gl) = setup(Duration::from_secs(60));
    let me = block_on(gl.whoami()).unwrap();
    assert_eq!(me.login, "infernodragon0");
    assert_eq!(me.name.as_deref(), Some("Inferno Dragon"));
    assert_eq!(
        me.web_url.as_deref(),
        Some("https://gitlab.com/infernodragon0")
    );
    let seen = &replay.seen()[0];
    assert_eq!(seen.auth, Some("PrivateToken"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    let rate = gl.rate_limit();
    assert_eq!((rate.limit, rate.remaining), (Some(2000), Some(1999)));

    // OAuth access tokens are Bearer; swapping the token clears the cache.
    gl.set_token(Some(Secret::new("oauth-access")), true);
    block_on(gl.whoami()).unwrap();
    assert_eq!(replay.seen()[1].auth, Some("Bearer"));
    gl.set_token(None, false);
    block_on(gl.whoami()).unwrap();
    assert_eq!(replay.seen()[2].auth, None);

    let replay = Arc::new(Replay::from_json(include_str!("fixtures/gitlab.json")));
    let oauth = GitLab::new(
        "https://gitlab.com",
        Some(Secret::new("oauth-access")),
        true,
        replay.clone(),
    )
    .unwrap();
    block_on(oauth.whoami()).unwrap();
    assert_eq!(replay.seen()[0].auth, Some("Bearer"));
}

#[test]
fn repos_page_via_x_next_page() {
    let (_, gl) = setup(Duration::ZERO);
    let page = block_on(gl.repos(1)).unwrap();
    assert_eq!(page.next, Some(2));
    let [nested, fork] = page.items.as_slice() else {
        panic!("two projects");
    };
    assert_eq!(nested.id, repo());
    assert_eq!(nested.id.owner, "group/sub");
    assert!(nested.private && !nested.fork);
    assert_eq!(nested.description.as_deref(), Some("Nested project"));
    assert_eq!(nested.default_branch.as_deref(), Some("main"));
    assert_eq!(
        nested.clone_url.as_deref(),
        Some("https://gitlab.com/group/sub/proj.git")
    );
    assert_eq!(
        nested.ssh_url.as_deref(),
        Some("git@gitlab.com:group/sub/proj.git")
    );
    assert_eq!(
        nested.updated_at.as_deref(),
        Some("2026-10-09T10:00:00.000Z")
    );
    assert!(fork.fork && !fork.private);
    assert_eq!(fork.description, None, "empty description");
    assert_eq!(fork.id.owner, "infernodragon0");

    let last = block_on(gl.repos(2)).unwrap();
    assert_eq!(last.next, None, "empty x-next-page ends paging");
    assert!(last.items[0].private, "internal is not public");
}

#[test]
fn merge_requests() {
    let (_, gl) = setup(Duration::ZERO);
    let page = block_on(gl.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(page.next, None);
    let [draft, plain] = page.items.as_slice() else {
        panic!("two MRs");
    };
    assert_eq!(draft.number, 12);
    assert!(draft.draft);
    assert_eq!(draft.state, PrState::Open);
    assert_eq!(draft.source_branch, "feature/gitlab");
    assert_eq!(draft.target_branch, "main");
    assert_eq!(draft.head_sha, "a".repeat(40));
    assert_eq!(draft.body.as_deref(), Some("Closes #7"));
    assert_eq!(draft.labels.len(), 2);
    assert_eq!(draft.labels[0].name, "backend");
    assert_eq!(draft.labels[0].color, None);
    assert_eq!(draft.author.as_ref().unwrap().login, "tanuki");
    assert_eq!(
        draft.web_url,
        "https://gitlab.com/group/sub/proj/-/merge_requests/12"
    );
    assert_eq!(plain.number, 11);
    assert!(!plain.draft);
    assert_eq!(plain.body, None);

    let all = ListQuery {
        state: StateFilter::All,
        ..ListQuery::default()
    };
    let page = block_on(gl.pull_requests(&repo(), &all)).unwrap();
    let states: Vec<_> = page.items.iter().map(|p| p.state).collect();
    assert_eq!(states, [PrState::Merged, PrState::Closed]);
    assert_eq!(page.items[1].head_sha, "", "no sha yet");

    let merged = block_on(gl.pull_request(&repo(), 10)).unwrap();
    assert_eq!(merged.state, PrState::Merged);
    assert_eq!(merged.body.as_deref(), Some("Done"));
    assert_eq!(merged.source_repo, None);
}

#[test]
fn create_merge_request_body() {
    let (replay, gl) = setup(Duration::from_secs(600));
    let list = format!(
        "{PROJECT}/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=30&page=1"
    );
    block_on(gl.pull_requests(&repo(), &ListQuery::default())).unwrap();
    block_on(gl.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(replay.count(&list), 1, "second list from the TTL cache");

    let new = NewPullRequest {
        title: "New thing".into(),
        body: Some("Body".into()),
        source_branch: "new-thing".into(),
        target_branch: "main".into(),
        draft: true,
    };
    let mr = block_on(gl.create_pull_request(&repo(), &new)).unwrap();
    assert_eq!(mr.number, 13);
    assert!(mr.draft);
    let post = replay
        .seen()
        .into_iter()
        .find(|s| s.method == Method::Post)
        .unwrap();
    assert_eq!(
        post.body.unwrap(),
        json!({"source_branch": "new-thing", "target_branch": "main", "title": "Draft: New thing", "description": "Body"})
    );
    block_on(gl.pull_requests(&repo(), &ListQuery::default())).unwrap();
    assert_eq!(
        replay.count(&list),
        2,
        "creating an MR drops the project's cache"
    );

    // No body, not a draft: plain title, no description key.
    block_on(gl.create_pull_request(
        &repo(),
        &NewPullRequest {
            draft: false,
            body: None,
            ..new
        },
    ))
    .unwrap();
    let post = replay.seen().pop().unwrap();
    assert_eq!(
        post.body.unwrap(),
        json!({"source_branch": "new-thing", "target_branch": "main", "title": "New thing"})
    );
}

#[test]
fn issues_and_notes() {
    let (replay, gl) = setup(Duration::ZERO);
    let page = block_on(gl.issues(&repo(), &ListQuery::default())).unwrap();
    let [open, closed] = page.items.as_slice() else {
        panic!("two issues");
    };
    assert_eq!(open.number, 7);
    assert_eq!(open.state, IssueState::Open);
    assert_eq!(open.comments, 3);
    assert_eq!(open.assignees[0].login, "infernodragon0");
    assert_eq!(open.labels[0].name, "bug");
    assert_eq!(open.body.as_deref(), Some("Steps..."));
    assert_eq!(closed.state, IssueState::Closed);
    assert!(closed.author.is_none());

    let created = block_on(gl.create_issue(
        &repo(),
        &NewIssue {
            title: "Crash on open".into(),
            body: Some("Steps...".into()),
            labels: vec!["bug".into(), "ui".into()],
        },
    ))
    .unwrap();
    assert_eq!(created.number, 8);
    assert_eq!(created.state, IssueState::Open);
    let post = replay.seen().pop().unwrap();
    assert_eq!(
        post.body.unwrap(),
        json!({"title": "Crash on open", "description": "Steps...", "labels": "bug,ui"})
    );

    let note = block_on(gl.comment_on_issue(&repo(), 7, "On it")).unwrap();
    assert_eq!(note.id, 9001);
    assert_eq!(note.body, "On it");
    assert_eq!(note.author.unwrap().login, "infernodragon0");
    assert_eq!(note.web_url, None);
    let last = replay.seen().pop().unwrap();
    assert_eq!(last.url, format!("{PROJECT}/issues/7/notes"));
    assert_eq!(last.body.unwrap(), json!({"body": "On it"}));
}

#[test]
fn ci_status_from_pipeline_jobs() {
    let (replay, gl) = setup(Duration::ZERO);
    let ci = block_on(gl.ci_status(&repo(), "main")).unwrap();
    assert_eq!(ci.state, CiState::Pending, "one job still running");
    let states: Vec<_> = ci
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.state))
        .collect();
    assert_eq!(
        states,
        [
            ("build", CiState::Success),
            ("test", CiState::Pending),
            ("lint", CiState::Success),
            ("deploy", CiState::Success),
        ],
        "an allowed failure and a manual job do not fail the pipeline"
    );
    assert_eq!(ci.checks[1].description.as_deref(), Some("test"));
    assert_eq!(
        ci.checks[0].url.as_deref(),
        Some("https://gitlab.com/group/sub/proj/-/jobs/1")
    );
    assert!(replay.seen()[0].url.contains("?ref=main&per_page=1"));

    // No pipeline for the ref: no state, no jobs request.
    let before = replay.seen().len();
    let ci = block_on(gl.ci_status(&repo(), "no-ci")).unwrap();
    assert_eq!(ci.state, CiState::None);
    assert!(ci.checks.is_empty());
    assert_eq!(replay.seen().len(), before + 1);
}

#[test]
fn ci_status_by_sha_falls_back_to_pipeline() {
    let (replay, gl) = setup(Duration::ZERO);
    let sha = "0123456789abcdef0123456789abcdef01234567";
    // The jobs list is forbidden: the pipeline's own status stands in.
    let ci = block_on(gl.ci_status(&repo(), sha)).unwrap();
    assert!(
        replay.seen()[0]
            .url
            .contains(&format!("?sha={sha}&per_page=1"))
    );
    assert_eq!(ci.state, CiState::Failure);
    let [check] = ci.checks.as_slice() else {
        panic!("one check");
    };
    assert_eq!(check.name, "Pipeline #101");
    assert_eq!(
        check.url.as_deref(),
        Some("https://gitlab.com/group/sub/proj/-/pipelines/101")
    );

    // Other job-list failures are errors.
    replay.push(Route::new(
        "GET",
        &format!("{PROJECT}/pipelines?ref=boom&per_page=1"),
        200,
        json!([{"id": 103, "status": "failed"}]),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{PROJECT}/pipelines/103/jobs?per_page=100"),
        500,
        json!({"message": "500 Internal Server Error"}),
    ));
    let err = block_on(gl.ci_status(&repo(), "boom")).unwrap_err();
    assert!(
        matches!(err, HostError::Server { status: 500, .. }),
        "{err:?}"
    );
}

#[test]
fn self_managed_base_with_port_and_prefix() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        "https://git.example.com:8443/gitlab/api/v4/user",
        200,
        json!({"username": "me"}),
    ));
    replay.push(Route::new(
        "GET",
        "https://git.example.com:8443/gitlab/api/v4/projects/team%2Fapp/merge_requests/3",
        200,
        json!({
            "iid": 3, "title": "t", "state": "locked", "source_branch": "a", "target_branch": "main",
            "web_url": "https://git.example.com:8443/gitlab/team/app/-/merge_requests/3",
            "created_at": "2026-10-09T00:00:00Z", "updated_at": "2026-10-09T00:00:00Z"
        }),
    ));
    let gl = GitLab::new(
        "https://git.example.com:8443/gitlab/",
        Some(Secret::new("glpat-x")),
        false,
        replay.clone(),
    )
    .unwrap();
    assert_eq!(block_on(gl.whoami()).unwrap().login, "me");
    assert_eq!(replay.seen()[0].auth, Some("PrivateToken"));

    let repo = RepoId {
        host: "git.example.com".into(),
        web_port: Some(8443),
        owner: "team".into(),
        project: None,
        repo: "app".into(),
    };
    let mr = block_on(gl.pull_request(&repo, 3)).unwrap();
    assert_eq!(mr.state, PrState::Open, "locked is still being merged");
    assert!(!mr.draft);
    assert_eq!(
        gl.web_url(&repo, &WebTarget::PullRequest(3)).unwrap(),
        "https://git.example.com:8443/gitlab/team/app/-/merge_requests/3"
    );
}

#[test]
fn subgroup_projects_are_one_encoded_segment() {
    let (replay, gl) = setup(Duration::from_secs(600));
    block_on(gl.pull_request(&repo(), 10)).unwrap();
    let url = &replay.seen()[0].url;
    assert_eq!(url, &format!("{PROJECT}/merge_requests/10"));
    assert!(url.contains("/projects/group%2Fsub%2Fproj/"), "{url}");

    // Invalidating the project drops what is cached under that one segment, nothing else.
    block_on(gl.whoami()).unwrap();
    block_on(gl.pull_request(&repo(), 10)).unwrap();
    assert_eq!(replay.count(url), 1, "cached");
    gl.invalidate(Some(&repo()));
    block_on(gl.pull_request(&repo(), 10)).unwrap();
    assert_eq!(replay.count(url), 2, "dropped");
    block_on(gl.whoami()).unwrap();
    assert_eq!(
        replay.count(&format!("{API}/user")),
        1,
        "other entries stay"
    );
    gl.invalidate(None);
    block_on(gl.whoami()).unwrap();
    assert_eq!(replay.count(&format!("{API}/user")), 2);
}

#[test]
fn web_urls() {
    let (_, gl) = setup(Duration::ZERO);
    let web = |t: WebTarget| gl.web_url(&repo(), &t).unwrap();
    let base = "https://gitlab.com/group/sub/proj";
    assert_eq!(web(WebTarget::Repo), base);
    assert_eq!(
        web(WebTarget::Commit("abc123".into())),
        format!("{base}/-/commit/abc123")
    );
    assert_eq!(
        web(WebTarget::Branch("feature/x y".into())),
        format!("{base}/-/tree/feature%2Fx%20y")
    );
    assert_eq!(
        web(WebTarget::PullRequest(4)),
        format!("{base}/-/merge_requests/4")
    );
    assert_eq!(
        web(WebTarget::PullRequests),
        format!("{base}/-/merge_requests")
    );
    assert_eq!(web(WebTarget::Issue(7)), format!("{base}/-/issues/7"));
    assert_eq!(web(WebTarget::Issues), format!("{base}/-/issues"));
    assert_eq!(
        web(WebTarget::NewPullRequest {
            source: "feature/x".into(),
            target: "main".into()
        }),
        format!(
            "{base}/-/merge_requests/new?merge_request%5Bsource_branch%5D=feature%2Fx&merge_request%5Btarget_branch%5D=main"
        )
    );
}

#[test]
fn error_mapping() {
    let (replay, gl) = setup(Duration::ZERO);
    let mr = |n: u64| format!("{PROJECT}/merge_requests/{n}");
    replay.push(Route::new(
        "GET",
        &mr(1),
        401,
        json!({"message": "401 Unauthorized"}),
    ));
    replay.push(Route::new(
        "GET",
        &mr(2),
        404,
        json!({"message": "404 Not found"}),
    ));
    replay.push(Route::new(
        "GET",
        &mr(3),
        403,
        json!({"message": "403 Forbidden"}),
    ));
    replay.push(Route::new(
        "GET",
        &mr(4),
        429,
        json!({"message": "Retry later"}),
    ));
    let get = |n| block_on(gl.pull_request(&repo(), n)).unwrap_err();
    assert!(matches!(get(1), HostError::Unauthorized { .. }));
    assert!(matches!(get(2), HostError::NotFound { .. }));
    assert!(matches!(get(3), HostError::Forbidden { .. }));

    replay.first(Route::new(
        "POST",
        &format!("{PROJECT}/merge_requests"),
        409,
        json!({"message": ["Another open merge request already exists for this source branch: !12"]}),
    ));
    let new = NewPullRequest {
        title: "dup".into(),
        body: None,
        source_branch: "new-thing".into(),
        target_branch: "main".into(),
        draft: false,
    };
    let err = block_on(gl.create_pull_request(&repo(), &new)).unwrap_err();
    assert!(
        matches!(err, HostError::Invalid { ref message, .. } if message.contains("already exists")),
        "{err:?}"
    );

    // A 429 blocks the host, so it goes last.
    let err = get(4);
    assert!(matches!(err, HostError::RateLimited { .. }));
    assert!(err.is_transient());
}

#[test]
fn connect_picks_gitlab_and_auth_mode() {
    let account = |auth| Account {
        id: "a".into(),
        name: "n".into(),
        kind: HostKind::GitLab,
        base_url: "https://gitlab.com".into(),
        auth,
        username: None,
    };
    let replay = Arc::new(Replay::from_json(include_str!("fixtures/gitlab.json")));
    let transport: Arc<dyn Transport> = replay.clone();
    let token = || Some(Secret::new("t"));
    let host = connect(&account(AuthMode::Pat), token(), transport.clone()).unwrap();
    assert_eq!(host.kind(), HostKind::GitLab);
    assert_eq!(host.vocab().pr_short, "MR");
    assert!(host.supports(Feature::CiStatus) && host.supports(Feature::CreateIssue));
    block_on(host.whoami()).unwrap();
    let host = connect(&account(AuthMode::OAuth), token(), transport.clone()).unwrap();
    block_on(host.whoami()).unwrap();
    let auth: Vec<_> = replay.seen().iter().map(|s| s.auth).collect();
    assert_eq!(auth, [Some("PrivateToken"), Some("Bearer")]);

    let mut http = account(AuthMode::Pat);
    http.base_url = "http://gitlab.example.com".into();
    assert!(matches!(
        connect(&http, None, transport),
        Err(HostError::Config(_))
    ));
}
