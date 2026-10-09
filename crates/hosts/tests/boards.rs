//! Boards against recorded responses, per provider: listing, columns + items (paging, column
//! placement), moves (the GraphQL mutation body, GitLab's label swap, Azure's JSON patch) and
//! the GitHub missing-scope answer.

use std::sync::Arc;
use std::time::Duration;

use accounts::{Secret, net::block_on};
use hosts::http::Method;
use hosts::replay::{Replay, Route};
use hosts::*;
use serde_json::{Value, json};
use url::Url;

/// `base` with `pairs` as the query, encoded the way the client encodes it.
fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse(base).unwrap();
    url.query_pairs_mut().extend_pairs(pairs);
    url.to_string()
}

fn column<'a>(page: &'a BoardPage, name: &str) -> &'a BoardColumn {
    page.columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("column {name}"))
}

fn item(page: &BoardPage, number: u64) -> &BoardItem {
    page.items
        .iter()
        .find(|i| i.number == Some(number))
        .unwrap_or_else(|| panic!("item {number}"))
}

// ---- GitHub Projects (GraphQL) ------------------------------------------------------------

const GQL: &str = "https://api.github.com/graphql";

fn github(replay: &Arc<Replay>) -> GitHub {
    GitHub::new(
        "https://github.com",
        Some(Secret::new("gho_token")),
        replay.clone(),
    )
    .unwrap()
    .with_ttl(Duration::from_secs(60))
}

fn gh_repo() -> RepoId {
    RepoId::parse("git@github.com:InfernoDragon0/GitDragon.git").unwrap()
}

fn gh_board() -> Board {
    Board {
        id: "PVT_1".into(),
        name: "Roadmap".into(),
        scope: BoardScope::Repo,
        owner: "InfernoDragon0/GitDragon".into(),
        number: Some(3),
        item_count: None,
        web_url: None,
    }
}

fn project(id: &str, number: u64, title: &str, closed: bool) -> Value {
    json!({
        "id": id, "number": number, "title": title, "closed": closed,
        "url": format!("https://github.com/users/InfernoDragon0/projects/{number}"),
        "items": { "totalCount": 24 }
    })
}

#[test]
fn github_lists_repo_then_owner_projects() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "POST",
        GQL,
        200,
        json!({ "data": { "repository": {
            "nameWithOwner": "InfernoDragon0/GitDragon",
            "projectsV2": { "nodes": [project("PVT_1", 3, "Roadmap", false), project("PVT_9", 9, "Old", true)] },
            "owner": {
                "login": "InfernoDragon0",
                "projectsV2": { "nodes": [project("PVT_1", 3, "Roadmap", false), project("PVT_5", 5, "Bugs triage", false)] }
            }
        } } }),
    ));
    let gh = github(&replay);
    assert!(gh.supports(Feature::Boards));
    let boards = block_on(gh.boards(&gh_repo())).unwrap();
    let names: Vec<(&str, BoardScope)> =
        boards.iter().map(|b| (b.name.as_str(), b.scope)).collect();
    assert_eq!(
        names,
        [
            ("Roadmap", BoardScope::Repo),
            ("Bugs triage", BoardScope::Namespace)
        ],
        "closed projects are left out, repo-linked ones are not repeated"
    );
    assert_eq!(boards[0].number, Some(3));
    assert_eq!(boards[0].item_count, Some(24));
    assert_eq!(boards[1].owner, "InfernoDragon0");
    let seen = &replay.seen()[0];
    assert_eq!(seen.method, Method::Post);
    assert_eq!(seen.auth, Some("Bearer"));
    let body = seen.body.as_ref().unwrap();
    assert_eq!(
        body["variables"],
        json!({ "owner": "InfernoDragon0", "name": "GitDragon" })
    );
    assert!(body["query"].as_str().unwrap().contains("projectsV2"));
}

fn gh_items_page() -> Value {
    json!({ "data": { "node": {
        "field": { "id": "F_status", "options": [
            { "id": "o_todo", "name": "Todo", "color": "GRAY" },
            { "id": "o_prog", "name": "In Progress", "color": "YELLOW" },
            { "id": "o_done", "name": "Done", "color": "GREEN" }
        ] },
        "items": {
            "pageInfo": { "hasNextPage": true, "endCursor": "Y3Vyc29yOjUw" },
            "nodes": [
                {
                    "id": "PVTI_9", "type": "ISSUE", "isArchived": false,
                    "fieldValues": { "nodes": [
                        { "__typename": "ProjectV2ItemFieldTextValue" },
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue", "optionId": "o_prog", "field": { "id": "F_status" } },
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue", "optionId": "o_p1", "field": { "id": "F_priority" } },
                        { "__typename": "ProjectV2ItemFieldIterationValue", "title": "Sprint 42" }
                    ] },
                    "content": {
                        "__typename": "Issue", "number": 9, "title": "Graph flickers", "body": "When fetch completes",
                        "url": "https://github.com/InfernoDragon0/GitDragon/issues/9", "state": "OPEN",
                        "updatedAt": "2026-10-01T10:00:00Z",
                        "repository": { "nameWithOwner": "InfernoDragon0/GitDragon" },
                        "author": { "login": "kai", "avatarUrl": "https://a/kai", "url": "https://github.com/kai" },
                        "assignees": { "nodes": [{ "login": "mushr", "name": "Mushr", "avatarUrl": null, "url": null }] },
                        "labels": { "nodes": [{ "name": "bug", "color": "d73a4a" }] },
                        "linkedBranches": { "nodes": [{ "ref": { "name": "fix/graph-flicker" } }] },
                        "closedByPullRequestsReferences": { "nodes": [
                            { "number": 13, "url": "https://github.com/InfernoDragon0/GitDragon/pull/13", "state": "OPEN", "isDraft": true, "headRefName": "fix/graph-flicker" }
                        ] }
                    }
                },
                {
                    "id": "PVTI_14", "type": "PULL_REQUEST", "isArchived": false,
                    "fieldValues": { "nodes": [] },
                    "content": {
                        "__typename": "PullRequest", "number": 14, "title": "Incremental lanes",
                        "url": "https://github.com/InfernoDragon0/GitDragon/pull/14", "state": "MERGED", "isDraft": false,
                        "headRefName": "feat/lanes", "updatedAt": "2026-10-02T10:00:00Z",
                        "repository": { "nameWithOwner": "InfernoDragon0/GitDragon" },
                        "assignees": { "nodes": [] }, "labels": { "nodes": [] }
                    }
                },
                {
                    "id": "PVTI_d", "type": "DRAFT_ISSUE", "isArchived": false,
                    "fieldValues": { "nodes": [
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue", "optionId": "o_todo", "field": { "id": "F_status" } }
                    ] },
                    "content": { "__typename": "DraftIssue", "title": "Flatpak packaging", "body": "", "updatedAt": "2026-09-01T00:00:00Z", "assignees": { "nodes": [] } }
                },
                { "id": "PVTI_old", "type": "ISSUE", "isArchived": true, "content": { "__typename": "Issue", "number": 1, "title": "old" } },
                { "id": "PVTI_x", "type": "REDACTED", "isArchived": false, "content": null }
            ]
        }
    } } })
}

#[test]
fn github_board_columns_items_and_paging() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new("POST", GQL, 200, gh_items_page()).once());
    let gh = github(&replay);
    let page = block_on(gh.board(&gh_repo(), &gh_board(), None)).unwrap();
    let names: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["No Status", "Todo", "In Progress", "Done"]);
    assert!(column(&page, "Done").done);
    assert_eq!(column(&page, "Todo").color.as_deref(), Some("GRAY"));
    assert_eq!(page.next.as_deref(), Some("Y3Vyc29yOjUw"));
    assert_eq!(
        page.items.len(),
        3,
        "archived and redacted items are skipped"
    );

    let issue = item(&page, 9);
    assert_eq!(issue.column, column(&page, "In Progress").id);
    assert_eq!(issue.kind, ItemKind::Issue);
    assert_eq!(issue.iteration.as_deref(), Some("Sprint 42"));
    assert_eq!(issue.assignees[0].login, "mushr");
    assert_eq!(issue.labels[0].color.as_deref(), Some("d73a4a"));
    assert_eq!(issue.branches, ["fix/graph-flicker"]);
    assert_eq!(issue.pull_requests[0].number, 13);
    assert!(issue.pull_requests[0].draft);
    assert_eq!(issue.repo.as_deref(), Some("InfernoDragon0/GitDragon"));

    let pr = item(&page, 14);
    assert_eq!(pr.kind, ItemKind::PullRequest);
    assert_eq!(pr.column, column(&page, "No Status").id);
    assert_eq!(pr.state, IssueState::Closed);
    assert_eq!(pr.pull_requests[0].state, PrState::Merged);
    assert_eq!(pr.branches, ["feat/lanes"]);

    let draft = page
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Draft)
        .unwrap();
    assert_eq!(draft.number, None);
    assert_eq!(draft.body, None);
    assert_eq!(draft.column, column(&page, "Todo").id);

    let body = replay.seen()[0].body.clone().unwrap();
    assert_eq!(body["variables"], json!({ "id": "PVT_1", "cursor": null }));
    // Cached by body for the TTL: the same page asks nothing.
    block_on(gh.board(&gh_repo(), &gh_board(), None)).unwrap();
    assert_eq!(replay.count(GQL), 1);
}

#[test]
fn github_move_sets_the_status_field() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new("POST", GQL, 200, gh_items_page()).once());
    let moved = json!({ "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": {
        "id": "PVTI_9", "isArchived": false,
        "fieldValues": { "nodes": [
            { "__typename": "ProjectV2ItemFieldSingleSelectValue", "optionId": "o_done", "field": { "id": "F_status" } }
        ] },
        "content": { "__typename": "Issue", "number": 9, "title": "Graph flickers", "state": "OPEN", "updatedAt": "2026-10-03T00:00:00Z" }
    } } } });
    replay.push(Route::new("POST", GQL, 200, moved).once());
    let cleared = json!({ "data": { "clearProjectV2ItemFieldValue": { "projectV2Item": {
        "id": "PVTI_9", "isArchived": false, "fieldValues": { "nodes": [] },
        "content": { "__typename": "Issue", "number": 9, "title": "Graph flickers", "state": "OPEN" }
    } } } });
    replay.push(Route::new("POST", GQL, 200, cleared).once());
    let gh = github(&replay);
    let page = block_on(gh.board(&gh_repo(), &gh_board(), None)).unwrap();
    let done = column(&page, "Done");
    let updated =
        block_on(gh.move_board_item(&gh_repo(), &gh_board(), item(&page, 9), done)).unwrap();
    assert_eq!(updated.column, done.id);
    let seen = replay.seen();
    let body = seen[1].body.as_ref().unwrap();
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("updateProjectV2ItemFieldValue")
    );
    assert_eq!(
        body["variables"],
        json!({ "project": "PVT_1", "item": "PVTI_9", "field": "F_status", "option": "o_done" })
    );
    let none = column(&page, "No Status");
    let updated = block_on(gh.move_board_item(&gh_repo(), &gh_board(), &updated, none)).unwrap();
    assert_eq!(updated.column, none.id);
    let body = replay.seen()[2].body.clone().unwrap();
    assert!(
        body["query"]
            .as_str()
            .unwrap()
            .contains("clearProjectV2ItemFieldValue")
    );
    assert_eq!(body["variables"]["field"], "F_status");
}

#[test]
fn github_without_project_scope_asks_to_sign_in_again() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "POST",
        GQL,
        200,
        json!({
            "data": { "repository": null },
            "errors": [{ "type": "INSUFFICIENT_SCOPES", "message": "Your token has not been granted the required scopes to execute this query. The 'projectsV2' field requires one of the following scopes: ['read:project'], but your token has only been granted the: ['repo', 'read:org'] scopes." }]
        }),
    ));
    let err = block_on(github(&replay).boards(&gh_repo())).unwrap_err();
    assert_eq!(
        err,
        HostError::MissingScope {
            host: "api.github.com".into(),
            scope: "read:project".into()
        }
    );
    assert!(err.to_string().contains("sign in again"));
}

#[test]
fn github_graphql_errors_map() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "POST",
        GQL,
        200,
        json!({ "data": null, "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to a node" }] }),
    ));
    let err = block_on(github(&replay).board(&gh_repo(), &gh_board(), None)).unwrap_err();
    assert!(matches!(err, HostError::NotFound { .. }), "{err:?}");
}

#[test]
fn ghes_graphql_lives_under_api() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "POST",
        "https://ghe.example.com/api/graphql",
        200,
        json!({ "data": { "repository": { "nameWithOwner": "o/r", "projectsV2": { "nodes": [] }, "owner": { "login": "o" } } } }),
    ));
    let gh = GitHub::new("https://ghe.example.com", None, replay.clone()).unwrap();
    let repo = RepoId::parse("https://ghe.example.com/o/r.git").unwrap();
    assert!(block_on(gh.boards(&repo)).unwrap().is_empty());
}

// ---- GitLab issue boards --------------------------------------------------------------------

const GL: &str = "https://gitlab.com/api/v4";

fn gitlab(replay: &Arc<Replay>) -> GitLab {
    GitLab::new(
        "https://gitlab.com",
        Some(Secret::new("glpat")),
        false,
        replay.clone(),
    )
    .unwrap()
    .with_ttl(Duration::from_secs(60))
}

fn gl_repo() -> RepoId {
    RepoId::parse("git@gitlab.com:group/proj.git").unwrap()
}

fn gl_board_json() -> Value {
    json!({
        "id": 5, "name": "Development",
        "lists": [
            { "id": 2, "label": { "name": "Review", "color": "#F0AD4E" }, "position": 1, "max_issue_count": 0 },
            { "id": 1, "label": { "name": "Doing", "color": "#428BCA" }, "position": 0, "max_issue_count": 3 },
            { "id": 3, "label": null, "assignee": { "username": "x" }, "position": 2 }
        ]
    })
}

fn gl_issue(iid: u64, state: &str, labels: &[&str]) -> Value {
    json!({
        "id": 1000 + iid, "iid": iid, "project_id": 44, "title": format!("Issue {iid}"),
        "description": "", "state": state, "labels": labels,
        "author": { "username": "kai" }, "assignees": [{ "username": "mushr", "name": "Mushr" }],
        "milestone": { "title": "M1" }, "iteration": null,
        "references": { "full": format!("group/proj#{iid}") },
        "web_url": format!("https://gitlab.com/group/proj/-/issues/{iid}"),
        "updated_at": "2026-10-01T00:00:00Z"
    })
}

fn gl_routes(replay: &Replay) {
    replay.push(Route::new(
        "GET",
        &format!("{GL}/projects/group%2Fproj/boards/5"),
        200,
        gl_board_json(),
    ));
    replay.push(
        Route::new(
            "GET",
            &with_query(
                &format!("{GL}/projects/group%2Fproj/issues"),
                &[
                    ("state", "all"),
                    ("order_by", "updated_at"),
                    ("sort", "desc"),
                    ("per_page", "100"),
                    ("page", "1"),
                ],
            ),
            200,
            json!([
                gl_issue(1, "opened", &["bug"]),
                gl_issue(2, "opened", &["Review", "Doing"]),
                gl_issue(3, "opened", &["bug", "Doing"]),
                gl_issue(4, "closed", &["Doing"])
            ]),
        )
        .header("x-next-page", "2"),
    );
}

fn gl_board() -> Board {
    Board {
        id: "p/5/group/proj".into(),
        name: "Development".into(),
        scope: BoardScope::Repo,
        owner: "group/proj".into(),
        number: None,
        item_count: None,
        web_url: None,
    }
}

#[test]
fn gitlab_lists_project_and_group_boards() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GL}/projects/group%2Fproj/boards"),
        200,
        json!([gl_board_json()]),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GL}/groups/group/boards"),
        200,
        json!([{ "id": 7, "name": "Group roadmap", "lists": [] }]),
    ));
    let gl = gitlab(&replay);
    assert!(gl.supports(Feature::Boards));
    let boards = block_on(gl.boards(&gl_repo())).unwrap();
    assert_eq!(boards.len(), 2);
    assert_eq!(boards[0].id, "p/5/group/proj");
    assert_eq!(boards[0].scope, BoardScope::Repo);
    assert_eq!(
        boards[0].web_url.as_deref(),
        Some("https://gitlab.com/group/proj/-/boards/5")
    );
    assert_eq!(boards[1].scope, BoardScope::Namespace);
    assert_eq!(
        boards[1].web_url.as_deref(),
        Some("https://gitlab.com/groups/group/-/boards/7")
    );
}

#[test]
fn gitlab_personal_namespace_has_no_group_boards() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &format!("{GL}/projects/group%2Fproj/boards"),
        200,
        json!([]),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GL}/groups/group/boards"),
        404,
        json!({ "message": "404 Group Not Found" }),
    ));
    assert!(
        block_on(gitlab(&replay).boards(&gl_repo()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn gitlab_board_places_issues_by_list_label() {
    let replay = Arc::new(Replay::new());
    gl_routes(&replay);
    let page = block_on(gitlab(&replay).board(&gl_repo(), &gl_board(), None)).unwrap();
    let names: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["Open", "Doing", "Review", "Closed"],
        "label lists by position"
    );
    assert_eq!(column(&page, "Doing").limit, Some(3));
    assert_eq!(column(&page, "Review").limit, None);
    assert_eq!(column(&page, "Doing").color.as_deref(), Some("428BCA"));
    assert!(column(&page, "Closed").done);
    assert_eq!(item(&page, 1).column, "open");
    assert_eq!(item(&page, 2).column, "label:Doing", "first list wins");
    assert_eq!(item(&page, 3).column, "label:Doing");
    assert_eq!(item(&page, 4).column, "closed");
    assert_eq!(item(&page, 1).iteration.as_deref(), Some("M1"));
    assert_eq!(item(&page, 1).repo.as_deref(), Some("group/proj"));
    assert_eq!(item(&page, 1).id, "44/1");
    assert_eq!(page.next.as_deref(), Some("2"));
}

#[test]
fn gitlab_move_swaps_labels_and_closes() {
    let replay = Arc::new(Replay::new());
    gl_routes(&replay);
    let issue_url = format!("{GL}/projects/44/issues/3");
    replay.push(
        Route::new(
            "PUT",
            &issue_url,
            200,
            gl_issue(3, "opened", &["bug", "Review"]),
        )
        .once(),
    );
    replay.push(Route::new("PUT", &issue_url, 200, gl_issue(3, "closed", &["bug"])).once());
    let gl = gitlab(&replay);
    let page = block_on(gl.board(&gl_repo(), &gl_board(), None)).unwrap();
    let moved = block_on(gl.move_board_item(
        &gl_repo(),
        &gl_board(),
        item(&page, 3),
        column(&page, "Review"),
    ))
    .unwrap();
    assert_eq!(moved.column, "label:Review");
    let puts: Vec<_> = replay
        .seen()
        .into_iter()
        .filter(|s| s.method == Method::Put)
        .collect();
    assert_eq!(
        puts[0].body,
        Some(json!({ "add_labels": "Review", "remove_labels": "Doing", "state_event": "reopen" }))
    );
    let closed =
        block_on(gl.move_board_item(&gl_repo(), &gl_board(), &moved, column(&page, "Closed")))
            .unwrap();
    assert_eq!(closed.column, "closed");
    assert_eq!(closed.state, IssueState::Closed);
    let puts: Vec<_> = replay
        .seen()
        .into_iter()
        .filter(|s| s.method == Method::Put)
        .collect();
    assert_eq!(
        puts[1].body,
        Some(json!({ "remove_labels": "Review", "state_event": "close" }))
    );
    // The move dropped the cached issue list.
    block_on(gl.board(&gl_repo(), &gl_board(), None)).unwrap();
    assert_eq!(
        replay
            .seen()
            .iter()
            .filter(|s| s.url.contains("/issues?"))
            .count(),
        2
    );
}

#[test]
fn gitlab_detail_adds_related_merge_requests() {
    let replay = Arc::new(Replay::new());
    gl_routes(&replay);
    replay.push(Route::new(
        "GET",
        &format!("{GL}/projects/44/issues/3"),
        200,
        gl_issue(3, "opened", &["Doing"]),
    ));
    replay.push(Route::new(
        "GET",
        &format!("{GL}/projects/44/issues/3/related_merge_requests"),
        200,
        json!([{ "iid": 8, "state": "opened", "draft": true, "source_branch": "3-graph-flicker", "web_url": "https://gitlab.com/group/proj/-/merge_requests/8" }]),
    ));
    let gl = gitlab(&replay);
    let page = block_on(gl.board(&gl_repo(), &gl_board(), None)).unwrap();
    let detail = block_on(gl.board_item(&gl_repo(), &gl_board(), item(&page, 3))).unwrap();
    assert_eq!(detail.branches, ["3-graph-flicker"]);
    assert_eq!(detail.pull_requests[0].number, 8);
    assert!(detail.pull_requests[0].draft);
}

// ---- Azure Boards ---------------------------------------------------------------------------

const ORG: &str = "https://dev.azure.com/acme";
const COLUMN_FIELD: &str = "WEF_ABC_Kanban.Column";

fn azure(replay: &Arc<Replay>) -> AzureDevOps {
    AzureDevOps::new(ORG, Some(Secret::new("pat")), false, replay.clone())
        .unwrap()
        .with_ttl(Duration::from_secs(60))
}

fn az_repo() -> RepoId {
    RepoId::parse("https://acme@dev.azure.com/acme/Platform/_git/api").unwrap()
}

fn az_board() -> Board {
    Board {
        id: "Platform/t1/b1".into(),
        name: "Platform Team \u{b7} Stories".into(),
        scope: BoardScope::Namespace,
        owner: "Platform".into(),
        number: None,
        item_count: None,
        web_url: None,
    }
}

fn v(url: &str, pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.push(("api-version", "7.1"));
    with_query(url, &pairs)
}

fn az_work_item(id: u64, state: &str, column: Option<&str>) -> Value {
    let mut fields = json!({
        "System.Id": id, "System.Title": format!("Story {id}"), "System.State": state,
        "System.WorkItemType": "User Story",
        "System.AssignedTo": { "displayName": "Mushr", "uniqueName": "mushr@example.com" },
        "System.Tags": "ui; perf", "System.ChangedDate": "2026-10-01T00:00:00Z",
        "System.IterationPath": "Platform\\Sprint 42"
    });
    if let Some(column) = column {
        fields[COLUMN_FIELD] = json!(column);
    }
    json!({ "id": id, "fields": fields })
}

fn az_routes(replay: &Replay) {
    replay.push(Route::new(
        "GET",
        &v(&format!("{ORG}/Platform/t1/_apis/work/boards/b1"), &[]),
        200,
        json!({
            "id": "b1", "name": "Stories",
            "columns": [
                { "name": "New", "columnType": "incoming", "stateMappings": { "User Story": "New", "Bug": "New" } },
                { "name": "Active", "itemLimit": 5, "columnType": "inProgress", "stateMappings": { "User Story": "Active", "Bug": "Active" } },
                { "name": "Review", "columnType": "inProgress", "stateMappings": { "User Story": "Active", "Bug": "Resolved" } },
                { "name": "Closed", "columnType": "outgoing", "stateMappings": { "User Story": "Closed", "Bug": "Closed" } }
            ],
            "fields": { "columnField": { "referenceName": COLUMN_FIELD } }
        }),
    ));
    replay.push(Route::new(
        "GET",
        &v(
            &format!("{ORG}/Platform/t1/_apis/work/teamsettings/teamfieldvalues"),
            &[],
        ),
        200,
        json!({ "field": { "referenceName": "System.AreaPath" }, "values": [{ "value": "Platform\\Team", "includeChildren": true }] }),
    ));
    replay.push(Route::new(
        "POST",
        &v(
            &format!("{ORG}/Platform/t1/_apis/wit/wiql"),
            &[("$top", "2000")],
        ),
        200,
        json!({ "workItems": [{ "id": 12 }, { "id": 11 }, { "id": 10 }] }),
    ));
    let fields = "System.Id,System.Title,System.State,System.WorkItemType,System.AssignedTo,System.CreatedBy,System.Tags,System.ChangedDate,System.IterationPath,System.Description,System.BoardColumn,WEF_ABC_Kanban.Column";
    replay.push(Route::new(
        "GET",
        &v(
            &format!("{ORG}/_apis/wit/workitems"),
            &[("ids", "12,11,10"), ("fields", fields)],
        ),
        200,
        json!({ "value": [
            az_work_item(10, "Closed", Some("Closed")),
            az_work_item(12, "Active", Some("Review")),
            az_work_item(11, "Active", None)
        ] }),
    ));
}

#[test]
fn azure_lists_team_boards() {
    let replay = Arc::new(Replay::new());
    replay.push(Route::new(
        "GET",
        &v(&format!("{ORG}/_apis/projects/Platform/teams"), &[("$top", "10")]),
        200,
        json!({ "value": [{ "id": "t1", "name": "Platform Team" }, { "id": "t2", "name": "Ops" }] }),
    ));
    replay.push(Route::new(
        "GET",
        &v(&format!("{ORG}/Platform/t1/_apis/work/boards"), &[]),
        200,
        json!({ "value": [{ "id": "b1", "name": "Stories" }, { "id": "b2", "name": "Features" }] }),
    ));
    replay.push(Route::new(
        "GET",
        &v(&format!("{ORG}/Platform/t2/_apis/work/boards"), &[]),
        403,
        json!({ "message": "no access" }),
    ));
    let az = azure(&replay);
    assert!(az.supports(Feature::Boards));
    let boards = block_on(az.boards(&az_repo())).unwrap();
    let names: Vec<&str> = boards.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Platform Team \u{b7} Stories",
            "Platform Team \u{b7} Features"
        ]
    );
    assert_eq!(boards[0].id, "Platform/t1/b1");
    assert_eq!(
        boards[0].web_url.as_deref(),
        Some("https://dev.azure.com/acme/Platform/_boards/board/t/Platform%20Team/Stories")
    );
    assert_eq!(replay.seen()[0].auth, Some("Basic"));
}

#[test]
fn azure_board_is_wiql_then_batch() {
    let replay = Arc::new(Replay::new());
    az_routes(&replay);
    let page = block_on(azure(&replay).board(&az_repo(), &az_board(), None)).unwrap();
    let names: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["New", "Active", "Review", "Closed"]);
    assert!(column(&page, "Closed").done);
    assert_eq!(column(&page, "Active").limit, Some(5));
    let order: Vec<u64> = page.items.iter().filter_map(|i| i.number).collect();
    assert_eq!(order, [12, 11, 10], "the WIQL order is kept");
    assert_eq!(item(&page, 12).column, "Review", "the Kanban field wins");
    assert_eq!(item(&page, 11).column, "Active", "else the state mapping");
    assert_eq!(item(&page, 10).state, IssueState::Closed);
    assert_eq!(item(&page, 11).iteration.as_deref(), Some("Sprint 42"));
    assert_eq!(item(&page, 11).item_type.as_deref(), Some("User Story"));
    assert_eq!(item(&page, 11).assignees[0].login, "mushr@example.com");
    assert_eq!(item(&page, 11).labels.len(), 2);
    assert_eq!(page.next, None);
    let wiql = replay
        .seen()
        .into_iter()
        .find(|s| s.method == Method::Post)
        .and_then(|s| s.body)
        .unwrap();
    let query = wiql["query"].as_str().unwrap();
    assert!(
        query.contains("[System.WorkItemType] In ('Bug', 'User Story')"),
        "{query}"
    );
    assert!(
        query.contains("[System.AreaPath] Under 'Platform\\Team'"),
        "{query}"
    );
}

#[test]
fn azure_move_patches_column_and_state() {
    let replay = Arc::new(Replay::new());
    az_routes(&replay);
    let item_url = v(&format!("{ORG}/Platform/_apis/wit/workitems/11"), &[]);
    replay.push(Route::new(
        "PATCH",
        &item_url,
        200,
        az_work_item(11, "Active", Some("Review")),
    ));
    let az = azure(&replay);
    let page = block_on(az.board(&az_repo(), &az_board(), None)).unwrap();
    let moved = block_on(az.move_board_item(
        &az_repo(),
        &az_board(),
        item(&page, 11),
        column(&page, "Review"),
    ))
    .unwrap();
    assert_eq!(moved.column, "Review");
    let patch = replay
        .seen()
        .into_iter()
        .find(|s| s.method == Method::Patch)
        .unwrap();
    assert_eq!(
        patch.header("content-type"),
        Some("application/json-patch+json")
    );
    assert_eq!(
        patch.body,
        Some(json!([
            { "op": "add", "path": "/fields/System.State", "value": "Active" },
            { "op": "add", "path": format!("/fields/{COLUMN_FIELD}"), "value": "Review" }
        ]))
    );
}

#[test]
fn azure_detail_reads_branch_and_pr_links() {
    let replay = Arc::new(Replay::new());
    az_routes(&replay);
    let mut detail = az_work_item(11, "Active", None);
    detail["relations"] = json!([
        { "rel": "ArtifactLink", "url": "vstfs:///Git/Ref/p1%2Fr1%2FGBfeature%2F11-login", "attributes": { "name": "Branch" } },
        { "rel": "ArtifactLink", "url": "vstfs:///Git/PullRequestId/p1%2Fr1%2F42", "attributes": { "name": "Pull Request" } }
    ]);
    replay.push(Route::new(
        "GET",
        &v(
            &format!("{ORG}/Platform/_apis/wit/workitems/11"),
            &[("$expand", "relations")],
        ),
        200,
        detail,
    ));
    replay.push(Route::new(
        "GET",
        &v(&format!("{ORG}/_apis/git/pullrequests/42"), &[]),
        200,
        json!({
            "pullRequestId": 42, "title": "Login", "status": "active", "isDraft": false,
            "creationDate": "2026-10-01T00:00:00Z",
            "sourceRefName": "refs/heads/feature/11-login", "targetRefName": "refs/heads/main"
        }),
    ));
    let az = azure(&replay);
    let page = block_on(az.board(&az_repo(), &az_board(), None)).unwrap();
    let detail = block_on(az.board_item(&az_repo(), &az_board(), item(&page, 11))).unwrap();
    assert_eq!(detail.branches, ["feature/11-login"]);
    assert_eq!(detail.pull_requests[0].number, 42);
    assert_eq!(detail.pull_requests[0].state, PrState::Open);
    assert_eq!(
        detail.pull_requests[0].branch.as_deref(),
        Some("feature/11-login")
    );
}

#[test]
fn hosts_without_boards_say_so() {
    struct Bare;
    #[async_trait::async_trait]
    impl GitHost for Bare {
        fn kind(&self) -> accounts::HostKind {
            accounts::HostKind::Generic
        }
        fn supports(&self, _: Feature) -> bool {
            false
        }
        fn web_url(&self, _: &RepoId, _: &WebTarget) -> Option<String> {
            None
        }
    }
    let err = block_on(Bare.boards(&gh_repo())).unwrap_err();
    assert_eq!(err, HostError::Unsupported(Feature::Boards));
}
