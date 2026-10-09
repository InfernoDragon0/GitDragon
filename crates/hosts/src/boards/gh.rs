//! GitHub Projects (v2) over GraphQL (`<api>/graphql`; GHES `<base>/api/graphql`). Included
//! from `github.rs` as its child module, so it shares the host's [`ApiClient`].
//!
//! - Listing: the repository's projects, then its owner's (organisation or user).
//! - Columns: the options of the project's `Status` single-select field, after "No Status".
//!   A column id is `<field id> <option id>` (empty option = no status), so a move needs no
//!   extra lookup.
//! - Moves: `updateProjectV2ItemFieldValue` (or `clearProjectV2ItemFieldValue` for "No
//!   Status"), never cached. Queries go through the TTL cache ([`ApiClient::post_cached`]).
//! - Needs the `read:project` scope (`project` to move); GitHub answers 200 with an
//!   `INSUFFICIENT_SCOPES` error, which becomes [`HostError::MissingScope`].

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use super::GitHub;
use crate::boards::*;
use crate::error::HostError;
use crate::host::HostResult;
use crate::model::{IssueState, Label, Person, PrState};
use crate::remote::RepoId;

/// Items per page (GitHub allows 100; 50 keeps the query cost low with nested lists).
const PAGE: u32 = 50;
const STATUS_FIELD: &str = "Status";
const NO_STATUS: &str = "No Status";

const PROJECTS: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    projectsV2(first: 20, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { ...P } }
    owner {
      login
      ... on ProjectV2Owner {
        projectsV2(first: 20, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { ...P } }
      }
    }
  }
}
fragment P on ProjectV2 { id number title url closed items { totalCount } }";

const ITEM_FRAGMENT: &str = "fragment I on ProjectV2Item {
  id type isArchived
  fieldValues(first: 20) { nodes {
    __typename
    ... on ProjectV2ItemFieldSingleSelectValue { optionId field { ... on ProjectV2FieldCommon { id } } }
    ... on ProjectV2ItemFieldIterationValue { title }
  } }
  content {
    __typename
    ... on DraftIssue { title body updatedAt assignees(first: 10) { nodes { ...U } } }
    ... on Issue {
      number title body url state updatedAt
      repository { nameWithOwner }
      author { login avatarUrl url }
      assignees(first: 10) { nodes { ...U } }
      labels(first: 10) { nodes { name color } }
      linkedBranches(first: 5) { nodes { ref { name } } }
      closedByPullRequestsReferences(first: 5) { nodes { number url state isDraft headRefName } }
    }
    ... on PullRequest {
      number title body url state isDraft headRefName updatedAt
      repository { nameWithOwner }
      author { login avatarUrl url }
      assignees(first: 10) { nodes { ...U } }
      labels(first: 10) { nodes { name color } }
    }
  }
}
fragment U on User { login name avatarUrl url }";

fn board_query() -> String {
    format!(
        "query($id: ID!, $cursor: String) {{
  node(id: $id) {{
    ... on ProjectV2 {{
      field(name: \"{STATUS_FIELD}\") {{ ... on ProjectV2SingleSelectField {{ id options {{ id name color }} }} }}
      items(first: {PAGE}, after: $cursor) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{ ...I }}
      }}
    }}
  }}
}}
{ITEM_FRAGMENT}"
    )
}

fn item_query() -> String {
    format!("query($id: ID!) {{ node(id: $id) {{ ...I }} }}\n{ITEM_FRAGMENT}")
}

fn set_mutation() -> String {
    format!(
        "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) {{
  updateProjectV2ItemFieldValue(input: {{projectId: $project, itemId: $item, fieldId: $field, value: {{singleSelectOptionId: $option}}}}) {{
    projectV2Item {{ ...I }}
  }}
}}
{ITEM_FRAGMENT}"
    )
}

fn clear_mutation() -> String {
    format!(
        "mutation($project: ID!, $item: ID!, $field: ID!) {{
  clearProjectV2ItemFieldValue(input: {{projectId: $project, itemId: $item, fieldId: $field}}) {{
    projectV2Item {{ ...I }}
  }}
}}
{ITEM_FRAGMENT}"
    )
}

/// `<field id> <option id>`.
fn column_id(field: &str, option: &str) -> String {
    format!("{field} {option}")
}

fn split_column(id: &str) -> (&str, &str) {
    id.split_once(' ').unwrap_or((id, ""))
}

impl GitHub {
    /// `https://api.github.com/graphql`, or `<web>/api/graphql` on GHES.
    pub(super) fn graphql_url(&self) -> Url {
        if self.web.host_str() == Some("github.com") {
            return Url::parse("https://api.github.com/graphql").expect("static URL");
        }
        let mut url = self.web.clone();
        url.path_segments_mut()
            .expect("https base")
            .pop_if_empty()
            .extend(["api", "graphql"]);
        url
    }

    /// Runs a query (cached for the TTL) or a mutation (never cached) and answers `data`.
    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
        mutation: bool,
    ) -> HostResult<T> {
        let body = json!({ "query": query, "variables": variables });
        let url = self.graphql_url();
        let answer: Vec<u8> = if mutation {
            self.api
                .send_json(crate::http::Method::Post, url, &body)
                .await?
        } else {
            self.api.post_cached(url, &body).await?.body.to_vec()
        };
        let reply: GqlReply<T> = self.api.decode(&answer)?;
        let host = || self.api.host().to_string();
        if let Some(scope) = reply
            .errors
            .iter()
            .find(|e| e.kind == "INSUFFICIENT_SCOPES")
        {
            return Err(HostError::MissingScope {
                host: host(),
                scope: missing_scope(&scope.message).into(),
            });
        }
        match (reply.data, reply.errors.into_iter().next()) {
            (Some(data), _) => Ok(data),
            (None, Some(err)) => Err(match err.kind.as_str() {
                "NOT_FOUND" => HostError::NotFound { host: host() },
                "FORBIDDEN" => HostError::Forbidden {
                    host: host(),
                    message: err.message,
                },
                "RATE_LIMITED" => HostError::RateLimited {
                    host: host(),
                    until: None,
                },
                _ => HostError::Invalid {
                    host: host(),
                    message: err.message,
                },
            }),
            (None, None) => Err(HostError::Decode {
                host: host(),
                message: "GraphQL answer without data".into(),
            }),
        }
    }

    fn after_board_write(&self) {
        self.api.invalidate_url(&self.graphql_url());
    }
}

/// `read:project` unless GitHub names a write scope.
fn missing_scope(message: &str) -> &'static str {
    if message.contains("'project'") {
        "project"
    } else {
        "read:project"
    }
}

pub(super) async fn boards(gh: &GitHub, repo: &RepoId) -> HostResult<Vec<Board>> {
    let data: ProjectsData = gh
        .graphql(
            PROJECTS,
            json!({ "owner": repo.owner, "name": repo.repo }),
            false,
        )
        .await?;
    let Some(repository) = data.repository else {
        return Err(HostError::NotFound {
            host: gh.api.host().to_string(),
        });
    };
    let mut out: Vec<Board> = Vec::new();
    let mut add = |p: GqlProject, scope: BoardScope, owner: &str| {
        if p.closed || out.iter().any(|b| b.id == p.id) {
            return;
        }
        out.push(Board {
            id: p.id,
            name: p.title,
            scope,
            owner: owner.to_string(),
            number: Some(p.number),
            item_count: p.items.map(|i| i.total_count),
            web_url: Some(p.url),
        });
    };
    for p in repository.projects_v2.map(|c| c.nodes).unwrap_or_default() {
        add(p, BoardScope::Repo, &repository.name_with_owner);
    }
    let owner = repository.owner;
    for p in owner.projects_v2.map(|c| c.nodes).unwrap_or_default() {
        add(p, BoardScope::Namespace, &owner.login);
    }
    Ok(out)
}

pub(super) async fn board(
    gh: &GitHub,
    board: &Board,
    cursor: Option<&str>,
) -> HostResult<BoardPage> {
    let data: NodeData<GqlProjectItems> = gh
        .graphql(
            &board_query(),
            json!({ "id": board.id, "cursor": cursor }),
            false,
        )
        .await?;
    let Some(project) = data.node else {
        return Err(HostError::NotFound {
            host: gh.api.host().to_string(),
        });
    };
    let field = project.field.filter(|f| !f.id.is_empty());
    let field_id = field.as_ref().map_or("", |f| f.id.as_str()).to_string();
    let mut columns = vec![BoardColumn {
        id: column_id(&field_id, ""),
        name: NO_STATUS.into(),
        color: None,
        limit: None,
        done: false,
    }];
    columns.extend(
        field
            .into_iter()
            .flat_map(|f| f.options)
            .map(|o| BoardColumn {
                id: column_id(&field_id, &o.id),
                done: o.name.eq_ignore_ascii_case("done"),
                name: o.name,
                color: o.color,
                limit: None,
            }),
    );
    let items = project
        .items
        .nodes
        .into_iter()
        .filter_map(|i| item(i, &field_id))
        .collect();
    let info = project.items.page_info;
    Ok(BoardPage {
        columns,
        items,
        next: info.has_next_page.then_some(info.end_cursor).flatten(),
    })
}

pub(super) async fn move_item(
    gh: &GitHub,
    board: &Board,
    target: &BoardItem,
    to: &BoardColumn,
) -> HostResult<BoardItem> {
    let (field, option) = split_column(&to.id);
    if field.is_empty() {
        return Err(HostError::Invalid {
            host: gh.api.host().to_string(),
            message: format!("this project has no {STATUS_FIELD} field to move items with"),
        });
    }
    let vars = json!({ "project": board.id, "item": target.id, "field": field, "option": option });
    let raw = if option.is_empty() {
        let data: ClearData = gh.graphql(&clear_mutation(), vars, true).await?;
        data.clear.project_v2_item
    } else {
        let data: UpdateData = gh.graphql(&set_mutation(), vars, true).await?;
        data.update.project_v2_item
    };
    gh.after_board_write();
    raw.and_then(|r| item(r, field))
        .ok_or_else(|| HostError::Decode {
            host: gh.api.host().to_string(),
            message: "the moved item was not returned".into(),
        })
}

pub(super) async fn item_detail(gh: &GitHub, target: &BoardItem) -> HostResult<BoardItem> {
    let data: NodeData<GqlItem> = gh
        .graphql(&item_query(), json!({ "id": target.id }), false)
        .await?;
    let (field, _) = split_column(&target.column);
    data.node
        .and_then(|raw| item(raw, field))
        .ok_or_else(|| HostError::NotFound {
            host: gh.api.host().to_string(),
        })
}

/// A project item as a board item; archived and redacted (no access) items are skipped.
fn item(raw: GqlItem, field_id: &str) -> Option<BoardItem> {
    if raw.is_archived {
        return None;
    }
    let content = raw.content?;
    let kind = match content.typename.as_str() {
        "Issue" => ItemKind::Issue,
        "PullRequest" => ItemKind::PullRequest,
        "DraftIssue" => ItemKind::Draft,
        _ => return None,
    };
    let mut option = String::new();
    let mut iteration = None;
    for value in raw.field_values.map(|c| c.nodes).unwrap_or_default() {
        match value.typename.as_str() {
            "ProjectV2ItemFieldSingleSelectValue"
                if value.field.as_ref().is_some_and(|f| f.id == field_id) =>
            {
                option = value.option_id.unwrap_or_default();
            }
            "ProjectV2ItemFieldIterationValue" => iteration = value.title,
            _ => {}
        }
    }
    let state = match content.state.as_deref() {
        Some("CLOSED" | "MERGED") => IssueState::Closed,
        _ => IssueState::Open,
    };
    let mut branches: Vec<String> = (content.linked_branches.map(|c| c.nodes))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| b.r#ref.map(|r| r.name))
        .collect();
    let mut pull_requests: Vec<LinkedPr> = (content.closed_by_pull_requests_references)
        .map(|c| c.nodes)
        .unwrap_or_default()
        .into_iter()
        .map(|p| LinkedPr {
            number: p.number,
            state: pr_state(&p.state),
            draft: p.is_draft,
            branch: p.head_ref_name,
            web_url: p.url,
        })
        .collect();
    if kind == ItemKind::PullRequest
        && let Some(number) = content.number
    {
        if let Some(head) = &content.head_ref_name {
            branches.push(head.clone());
        }
        pull_requests.push(LinkedPr {
            number,
            state: pr_state(content.state.as_deref().unwrap_or("OPEN")),
            draft: content.is_draft,
            branch: content.head_ref_name.clone(),
            web_url: content.url.clone(),
        });
    }
    Some(BoardItem {
        id: raw.id,
        column: column_id(field_id, &option),
        kind,
        number: content.number,
        title: content.title.unwrap_or_default(),
        body: content.body.filter(|b| !b.is_empty()),
        state,
        repo: content.repository.map(|r| r.name_with_owner),
        item_type: None,
        author: content.author.map(Into::into),
        assignees: (content.assignees.map(|c| c.nodes))
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect(),
        labels: (content.labels.map(|c| c.nodes))
            .unwrap_or_default()
            .into_iter()
            .map(|l| Label {
                name: l.name,
                color: l.color.filter(|c| !c.is_empty()),
            })
            .collect(),
        iteration,
        branches,
        pull_requests,
        web_url: content.url,
        updated_at: content.updated_at.unwrap_or_default(),
    })
}

fn pr_state(state: &str) -> PrState {
    match state {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

// GitHub's GraphQL JSON, only the fields we use.

#[derive(Deserialize)]
struct GqlReply<T> {
    #[serde(default = "Option::default")]
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct ProjectsData {
    repository: Option<GqlRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepository {
    name_with_owner: String,
    #[serde(default)]
    projects_v2: Option<Nodes<GqlProject>>,
    owner: GqlOwner,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlOwner {
    login: String,
    #[serde(default)]
    projects_v2: Option<Nodes<GqlProject>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCount {
    total_count: u32,
}

#[derive(Deserialize)]
struct GqlProject {
    id: String,
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    items: Option<GqlCount>,
}

#[derive(Deserialize)]
struct NodeData<T> {
    node: Option<T>,
}

#[derive(Deserialize)]
struct GqlProjectItems {
    #[serde(default)]
    field: Option<GqlStatusField>,
    items: GqlItemPage,
}

#[derive(Deserialize)]
struct GqlStatusField {
    /// Absent when a field named Status exists but is not a single-select.
    #[serde(default)]
    id: String,
    #[serde(default)]
    options: Vec<GqlOption>,
}

#[derive(Deserialize)]
struct GqlOption {
    id: String,
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlItemPage {
    page_info: GqlPageInfo,
    #[serde(default)]
    nodes: Vec<GqlItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    #[serde(default)]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlItem {
    id: String,
    #[serde(default)]
    is_archived: bool,
    #[serde(default)]
    field_values: Option<Nodes<GqlFieldValue>>,
    #[serde(default)]
    content: Option<GqlContent>,
}

#[derive(Deserialize)]
struct GqlFieldId {
    #[serde(default)]
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlFieldValue {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    option_id: Option<String>,
    #[serde(default)]
    field: Option<GqlFieldId>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepoName {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlUser {
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl From<GqlUser> for Person {
    fn from(u: GqlUser) -> Self {
        Person {
            login: u.login,
            name: u.name.filter(|n| !n.is_empty()),
            avatar_url: u.avatar_url.filter(|n| !n.is_empty()),
            web_url: u.url,
        }
    }
}

#[derive(Deserialize)]
struct GqlLabel {
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
struct GqlRefName {
    name: String,
}

#[derive(Deserialize)]
struct GqlLinkedBranch {
    #[serde(default)]
    r#ref: Option<GqlRefName>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPrRef {
    number: u64,
    #[serde(default)]
    url: Option<String>,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    head_ref_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlContent {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    head_ref_name: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    repository: Option<GqlRepoName>,
    #[serde(default)]
    author: Option<GqlUser>,
    #[serde(default)]
    assignees: Option<Nodes<GqlUser>>,
    #[serde(default)]
    labels: Option<Nodes<GqlLabel>>,
    #[serde(default)]
    linked_branches: Option<Nodes<GqlLinkedBranch>>,
    #[serde(default)]
    closed_by_pull_requests_references: Option<Nodes<GqlPrRef>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlMoved {
    #[serde(default)]
    project_v2_item: Option<GqlItem>,
}

#[derive(Deserialize)]
struct UpdateData {
    #[serde(rename = "updateProjectV2ItemFieldValue")]
    update: GqlMoved,
}

#[derive(Deserialize)]
struct ClearData {
    #[serde(rename = "clearProjectV2ItemFieldValue")]
    clear: GqlMoved,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_ids_round_trip() {
        assert_eq!(split_column(&column_id("F1", "o2")), ("F1", "o2"));
        assert_eq!(split_column(&column_id("F1", "")), ("F1", ""));
        assert_eq!(split_column(""), ("", ""));
    }

    #[test]
    fn scopes_from_messages() {
        assert_eq!(
            missing_scope(
                "Your token has not been granted the required scopes to execute this query. The 'id' field requires one of the following scopes: ['read:project']"
            ),
            "read:project"
        );
        assert_eq!(missing_scope("requires one of: ['project']"), "project");
    }
}
