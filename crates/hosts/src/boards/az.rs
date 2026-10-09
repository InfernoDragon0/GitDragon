//! Azure Boards over REST 7.1. Included from `azure.rs` as its child module.
//!
//! - Listing: the boards (Stories / Features / Epics ...) of the Azure project's teams
//!   (`_apis/projects/{p}/teams`, then `{p}/{team}/_apis/work/boards`), first teams only.
//! - Columns: the board's Kanban columns; `outgoing` ones are done.
//! - Items: a team-scoped WIQL query (the work item types the columns map, the team's area
//!   paths, not Removed) answers the ids, newest change first; pages of 200 come from one
//!   batch GET each. The cursor is the index of the next id.
//! - Column of an item: the board's own Kanban column field (`WEF_<guid>_Kanban.Column`),
//!   else the column that maps its state.
//! - Moves: one JSON patch setting the Kanban column field AND `System.State` to the state the
//!   column maps the item's type to. Azure keeps column and state in step: a column change
//!   whose state does not match is rejected or snapped back, so both go together. A column
//!   without a mapping for the type gets only the column field.
//!
//! Ids: board `<project>/<team id>/<board id>`; column = its name (what the Kanban field
//! holds); item = the work item id.

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AzList, AzPerson, AzPull, AzureDevOps, non_empty, project, strip_heads};
use crate::boards::*;
use crate::error::HostError;
use crate::host::HostResult;
use crate::http::Method;
use crate::model::{IssueState, Label, PrState};
use crate::remote::RepoId;

/// Teams whose boards are listed.
const MAX_TEAMS: &str = "10";
/// Ids a board asks WIQL for.
const WIQL_TOP: usize = 2000;
const PAGE: usize = 200;
/// Linked pull requests looked up in an item's detail.
const MAX_PRS: usize = 3;
const BASE_FIELDS: &[&str] = &[
    "System.Id",
    "System.Title",
    "System.State",
    "System.WorkItemType",
    "System.AssignedTo",
    "System.CreatedBy",
    "System.Tags",
    "System.ChangedDate",
    "System.IterationPath",
    "System.Description",
    "System.BoardColumn",
];

fn parse_board(id: &str) -> Option<(&str, &str, &str)> {
    let mut parts = id.splitn(3, '/');
    Some((parts.next()?, parts.next()?, parts.next()?))
}

/// `'a''b'` WIQL string literal.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The WIQL for a board: its work item types, the team's areas, newest change first.
pub(super) fn board_wiql(types: &[String], areas: &[(String, bool)]) -> String {
    let mut wiql =
        String::from("Select [System.Id] From WorkItems Where [System.TeamProject] = @project");
    if !types.is_empty() {
        let list: Vec<String> = types.iter().map(|t| quote(t)).collect();
        wiql.push_str(&format!(
            " And [System.WorkItemType] In ({})",
            list.join(", ")
        ));
    }
    if !areas.is_empty() {
        let parts: Vec<String> = areas
            .iter()
            .map(|(path, children)| {
                let op = if *children { "Under" } else { "=" };
                format!("[System.AreaPath] {op} {}", quote(path))
            })
            .collect();
        wiql.push_str(&format!(" And ({})", parts.join(" Or ")));
    }
    wiql.push_str(" And [System.State] <> 'Removed' Order By [System.ChangedDate] Desc");
    wiql
}

/// The JSON patch that moves a work item of `item_type` into `to`.
pub(super) fn move_patch(def: &AzBoard, item_type: Option<&str>, to: &str) -> Value {
    let mut ops = Vec::new();
    let state = def
        .columns
        .iter()
        .find(|c| c.name == to)
        .and_then(|c| c.state_mappings.get(item_type?));
    if let Some(state) = state {
        ops.push(json!({ "op": "add", "path": "/fields/System.State", "value": state }));
    }
    if let Some(field) = def.column_field() {
        ops.push(json!({ "op": "add", "path": format!("/fields/{field}"), "value": to }));
    }
    Value::Array(ops)
}

impl AzBoard {
    fn column_field(&self) -> Option<&str> {
        self.fields
            .as_ref()?
            .column_field
            .as_ref()
            .map(|f| f.reference_name.as_str())
            .filter(|f| !f.is_empty())
    }

    fn columns(&self) -> Vec<BoardColumn> {
        self.columns
            .iter()
            .map(|c| BoardColumn {
                id: c.name.clone(),
                name: c.name.clone(),
                color: None,
                limit: c.item_limit.filter(|n| *n > 0),
                done: c.column_type.as_deref() == Some("outgoing"),
            })
            .collect()
    }

    /// The work item types the columns map (the board's backlog level).
    fn types(&self) -> Vec<String> {
        let mut types: Vec<String> = self
            .columns
            .iter()
            .flat_map(|c| c.state_mappings.keys().cloned())
            .collect();
        types.sort();
        types.dedup();
        types
    }

    /// The column of a work item: its Kanban field, else the one mapping its state.
    fn column_of(
        &self,
        fields: &Map<String, Value>,
        item_type: Option<&str>,
        state: &str,
    ) -> String {
        let text = |name: &str| {
            fields
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let named = self
            .column_field()
            .and_then(text)
            .or_else(|| text("System.BoardColumn"))
            .filter(|name| self.columns.iter().any(|c| c.name == *name));
        if let Some(name) = named {
            return name.to_string();
        }
        let mapped = item_type.and_then(|t| {
            self.columns
                .iter()
                .find(|c| c.state_mappings.get(t).is_some_and(|s| s == state))
        });
        mapped
            .or(self.columns.first())
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }
}

impl AzureDevOps {
    fn bad_board(&self) -> HostError {
        HostError::Invalid {
            host: self.api.host().to_string(),
            message: "not an Azure Boards board id".into(),
        }
    }

    async fn board_def(&self, board: &Board) -> HostResult<AzBoard> {
        let (project, team, id) = parse_board(&board.id).ok_or_else(|| self.bad_board())?;
        let url = self.url(&[project, team, "_apis", "work", "boards", id], &[]);
        let fetched = self.api.get(url).await?;
        self.decode(&fetched.body)
    }

    /// `(area path, include children)` of the team; empty when Azure will not say.
    async fn team_areas(&self, project: &str, team: &str) -> HostResult<Vec<(String, bool)>> {
        let url = self.url(
            &[
                project,
                team,
                "_apis",
                "work",
                "teamsettings",
                "teamfieldvalues",
            ],
            &[],
        );
        match self.api.get(url).await {
            Ok(fetched) => {
                let values: AzTeamFields = self.decode(&fetched.body)?;
                let area = values
                    .field
                    .is_none_or(|f| f.reference_name == "System.AreaPath");
                Ok(if area {
                    values
                        .values
                        .into_iter()
                        .map(|v| (v.value, v.include_children))
                        .collect()
                } else {
                    Vec::new()
                })
            }
            Err(HostError::NotFound { .. } | HostError::Forbidden { .. }) => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    }

    fn to_board_item(&self, project: &str, def: &AzBoard, w: AzBoardItem) -> BoardItem {
        let f = w.fields;
        let text = |name: &str| f.get(name).and_then(Value::as_str).map(str::to_string);
        let person = |name: &str| {
            f.get(name)
                .cloned()
                .and_then(|v| serde_json::from_value::<AzPerson>(v).ok())
                .map(Into::into)
        };
        let state = text("System.State").unwrap_or_default();
        let item_type = text("System.WorkItemType");
        let closed = ["Closed", "Done", "Removed", "Resolved"].contains(&state.as_str());
        let id = w.id.to_string();
        BoardItem {
            id: id.clone(),
            column: def.column_of(&f, item_type.as_deref(), &state),
            kind: ItemKind::Issue,
            number: Some(w.id),
            title: text("System.Title").unwrap_or_default(),
            body: non_empty(text("System.Description")),
            state: if closed {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            repo: None,
            item_type,
            author: person("System.CreatedBy"),
            assignees: person("System.AssignedTo").into_iter().collect(),
            labels: text("System.Tags")
                .unwrap_or_default()
                .split(';')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| Label {
                    name: t.to_string(),
                    color: None,
                })
                .collect(),
            iteration: text("System.IterationPath")
                .and_then(|p| p.rsplit('\\').next().map(str::to_string))
                .filter(|p| !p.is_empty()),
            branches: Vec::new(),
            pull_requests: Vec::new(),
            web_url: Some(self.web(&[project, "_workitems", "edit", &id], &[])),
            updated_at: text("System.ChangedDate").unwrap_or_default(),
        }
    }
}

pub(super) async fn boards(az: &AzureDevOps, repo: &RepoId) -> HostResult<Vec<Board>> {
    let project = project(repo)?;
    let url = az.url(
        &["_apis", "projects", project, "teams"],
        &[("$top", MAX_TEAMS)],
    );
    let fetched = az.api.get(url).await?;
    let teams: AzList<AzTeam> = az.decode(&fetched.body)?;
    let mut out = Vec::new();
    for team in teams.value {
        let url = az.url(&[project, &team.id, "_apis", "work", "boards"], &[]);
        let fetched = match az.api.get(url).await {
            Ok(fetched) => fetched,
            // A team without boards access: skip it, not the whole list.
            Err(HostError::NotFound { .. } | HostError::Forbidden { .. }) => continue,
            Err(err) => return Err(err),
        };
        let list: AzList<AzBoardRef> = az.decode(&fetched.body)?;
        for b in list.value {
            out.push(Board {
                web_url: Some(az.web(
                    &[project, "_boards", "board", "t", &team.name, &b.name],
                    &[],
                )),
                id: format!("{project}/{}/{}", team.id, b.id),
                name: format!("{} \u{b7} {}", team.name, b.name),
                scope: BoardScope::Namespace,
                owner: project.to_string(),
                number: None,
                item_count: None,
            });
        }
    }
    Ok(out)
}

pub(super) async fn board(
    az: &AzureDevOps,
    board: &Board,
    cursor: Option<&str>,
) -> HostResult<BoardPage> {
    let def = az.board_def(board).await?;
    let (project, team, _) = parse_board(&board.id).ok_or_else(|| az.bad_board())?;
    let areas = az.team_areas(project, team).await?;
    let wiql = board_wiql(&def.types(), &areas);
    let top = WIQL_TOP.to_string();
    let url = az.url(&[project, team, "_apis", "wit", "wiql"], &[("$top", &top)]);
    let fetched = az.api.post_cached(url, &json!({ "query": wiql })).await?;
    let found: super::AzWiql = az.decode(&fetched.body)?;
    let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let ids: Vec<u64> = found
        .work_items
        .iter()
        .skip(start)
        .take(PAGE)
        .map(|w| w.id)
        .collect();
    let end = start + ids.len();
    let next = (end < found.work_items.len()).then(|| end.to_string());
    let columns = def.columns();
    if ids.is_empty() {
        return Ok(BoardPage {
            columns,
            items: Vec::new(),
            next,
        });
    }
    let mut fields: Vec<&str> = BASE_FIELDS.to_vec();
    if let Some(field) = def.column_field() {
        fields.push(field);
    }
    let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let fields = fields.join(",");
    let url = az.url(
        &["_apis", "wit", "workitems"],
        &[("ids", &joined), ("fields", &fields)],
    );
    let fetched = az.api.get(url).await?;
    let mut list: AzList<AzBoardItem> = az.decode(&fetched.body)?;
    list.value
        .sort_by_key(|w| ids.iter().position(|id| *id == w.id));
    let items = list
        .value
        .into_iter()
        .map(|w| az.to_board_item(project, &def, w))
        .collect();
    Ok(BoardPage {
        columns,
        items,
        next,
    })
}

pub(super) async fn move_item(
    az: &AzureDevOps,
    board: &Board,
    target: &BoardItem,
    to: &BoardColumn,
) -> HostResult<BoardItem> {
    let def = az.board_def(board).await?;
    let (project, ..) = parse_board(&board.id).ok_or_else(|| az.bad_board())?;
    let ops = move_patch(&def, target.item_type.as_deref(), &to.id);
    let url = az.url(&[project, "_apis", "wit", "workitems", &target.id], &[]);
    let answer = az
        .api
        .send_json_as(
            Method::Patch,
            url,
            &ops,
            Some("application/json-patch+json"),
        )
        .await?;
    invalidate_board(az, board);
    let raw: AzBoardItem = az.decode(&answer)?;
    Ok(az.to_board_item(project, &def, raw))
}

pub(super) fn invalidate_board(az: &AzureDevOps, board: &Board) {
    if let Some((project, ..)) = parse_board(&board.id) {
        az.api.invalidate(&[project]);
    }
    az.api.invalidate(&["_apis", "wit", "workitems"]);
}

/// The work item with its relations: linked branches and pull requests (the first few
/// looked up for their state and branch).
pub(super) async fn item_detail(
    az: &AzureDevOps,
    board: &Board,
    target: &BoardItem,
) -> HostResult<BoardItem> {
    let def = az.board_def(board).await?;
    let (project, ..) = parse_board(&board.id).ok_or_else(|| az.bad_board())?;
    let url = az.url(
        &[project, "_apis", "wit", "workitems", &target.id],
        &[("$expand", "relations")],
    );
    let fetched = az.api.get(url).await?;
    let mut raw: AzBoardItem = az.decode(&fetched.body)?;
    let relations = std::mem::take(&mut raw.relations);
    let mut out = az.to_board_item(project, &def, raw);
    for link in relations.iter().filter_map(|r| artifact(&r.url)) {
        match link {
            Artifact::Branch(name) => {
                if !out.branches.contains(&name) {
                    out.branches.push(name);
                }
            }
            Artifact::Pull { project, repo, id } => {
                if out.pull_requests.len() >= MAX_PRS {
                    continue;
                }
                let n = id.to_string();
                let web = az.web(&[&project, "_git", &repo, "pullrequest", &n], &[]);
                let url = az.url(&["_apis", "git", "pullrequests", &n], &[]);
                let pull: Option<AzPull> = match az.api.get(url).await {
                    Ok(fetched) => az.decode(&fetched.body).ok(),
                    Err(err) if err.is_transient() => return Err(err),
                    Err(_) => None,
                };
                out.pull_requests.push(match pull {
                    Some(p) => LinkedPr {
                        number: id,
                        state: match p.status.as_str() {
                            "active" => PrState::Open,
                            "completed" => PrState::Merged,
                            _ => PrState::Closed,
                        },
                        draft: p.is_draft,
                        branch: Some(strip_heads(&p.source_ref_name).to_string()),
                        web_url: Some(web),
                    },
                    None => LinkedPr {
                        number: id,
                        state: PrState::Open,
                        draft: false,
                        branch: None,
                        web_url: Some(web),
                    },
                });
            }
        }
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Artifact {
    Branch(String),
    Pull {
        project: String,
        repo: String,
        id: u64,
    },
}

/// `vstfs:///Git/Ref/<project>%2F<repo>%2FGB<branch>` and
/// `vstfs:///Git/PullRequestId/<project>%2F<repo>%2F<id>` (ids are GUIDs; the branch is
/// percent-encoded too).
pub(super) fn artifact(url: &str) -> Option<Artifact> {
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    if let Some(rest) = url.strip_prefix("vstfs:///Git/Ref/") {
        let mut parts = rest.splitn(3, "%2F");
        let (_, _, name) = (parts.next()?, parts.next()?, parts.next()?);
        let name = decode(name);
        return name
            .strip_prefix("GB")
            .map(|b| Artifact::Branch(b.to_string()));
    }
    let rest = url.strip_prefix("vstfs:///Git/PullRequestId/")?;
    let mut parts = rest.splitn(3, "%2F");
    let (project, repo, id) = (parts.next()?, parts.next()?, parts.next()?);
    Some(Artifact::Pull {
        project: decode(project),
        repo: decode(repo),
        id: id.parse().ok()?,
    })
}

// Azure's JSON, only the fields we use.

#[derive(Deserialize)]
struct AzTeam {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct AzBoardRef {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AzBoard {
    #[serde(default)]
    columns: Vec<AzBoardColumn>,
    #[serde(default)]
    fields: Option<AzBoardFields>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzBoardColumn {
    name: String,
    #[serde(default)]
    item_limit: Option<u32>,
    #[serde(default)]
    column_type: Option<String>,
    /// Work item type to state.
    #[serde(default)]
    state_mappings: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzBoardFields {
    #[serde(default)]
    column_field: Option<AzFieldRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzFieldRef {
    #[serde(default)]
    reference_name: String,
}

#[derive(Deserialize)]
struct AzTeamFields {
    #[serde(default)]
    field: Option<AzFieldRef>,
    #[serde(default)]
    values: Vec<AzTeamField>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzTeamField {
    value: String,
    #[serde(default)]
    include_children: bool,
}

#[derive(Deserialize)]
struct AzRelation {
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
struct AzBoardItem {
    id: u64,
    #[serde(default)]
    fields: Map<String, Value>,
    #[serde(default)]
    relations: Vec<AzRelation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> AzBoard {
        serde_json::from_value(json!({
            "columns": [
                { "name": "New", "columnType": "incoming", "stateMappings": { "User Story": "New", "Bug": "New" } },
                { "name": "Active", "itemLimit": 5, "columnType": "inProgress", "stateMappings": { "User Story": "Active", "Bug": "Active" } },
                { "name": "Review", "itemLimit": 0, "columnType": "inProgress", "stateMappings": { "User Story": "Active", "Bug": "Resolved" } },
                { "name": "Closed", "columnType": "outgoing", "stateMappings": { "User Story": "Closed", "Bug": "Closed" } }
            ],
            "fields": { "columnField": { "referenceName": "WEF_ABC_Kanban.Column" } }
        }))
        .unwrap()
    }

    #[test]
    fn columns_and_types() {
        let def = def();
        let cols = def.columns();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[1].limit, Some(5));
        assert_eq!(cols[2].limit, None);
        assert!(cols[3].done && !cols[0].done);
        assert_eq!(def.types(), ["Bug", "User Story"]);
    }

    #[test]
    fn column_from_kanban_field_then_state() {
        let def = def();
        let mut f = Map::new();
        f.insert("WEF_ABC_Kanban.Column".into(), json!("Review"));
        assert_eq!(def.column_of(&f, Some("User Story"), "Active"), "Review");
        let f = Map::new();
        assert_eq!(def.column_of(&f, Some("User Story"), "Active"), "Active");
        assert_eq!(def.column_of(&f, Some("Task"), "Whatever"), "New");
    }

    #[test]
    fn moves_set_column_and_mapped_state() {
        let def = def();
        assert_eq!(
            move_patch(&def, Some("Bug"), "Review"),
            json!([
                { "op": "add", "path": "/fields/System.State", "value": "Resolved" },
                { "op": "add", "path": "/fields/WEF_ABC_Kanban.Column", "value": "Review" }
            ])
        );
        assert_eq!(
            move_patch(&def, None, "Active"),
            json!([{ "op": "add", "path": "/fields/WEF_ABC_Kanban.Column", "value": "Active" }])
        );
    }

    #[test]
    fn wiql_quotes_names() {
        let wiql = board_wiql(
            &["Bug".into(), "Product's Item".into()],
            &[("P\\Team".into(), true), ("P".into(), false)],
        );
        assert!(wiql.contains("In ('Bug', 'Product''s Item')"), "{wiql}");
        assert!(wiql.contains("([System.AreaPath] Under 'P\\Team' Or [System.AreaPath] = 'P')"));
        assert!(wiql.ends_with("Order By [System.ChangedDate] Desc"));
    }

    #[test]
    fn artifact_links() {
        assert_eq!(
            artifact("vstfs:///Git/Ref/p1%2Fr1%2FGBfeature%2F12-login"),
            Some(Artifact::Branch("feature/12-login".into()))
        );
        assert_eq!(
            artifact("vstfs:///Git/PullRequestId/p1%2Fr1%2F42"),
            Some(Artifact::Pull {
                project: "p1".into(),
                repo: "r1".into(),
                id: 42
            })
        );
        assert_eq!(artifact("vstfs:///Build/Build/1"), None);
        assert_eq!(artifact("vstfs:///Git/Ref/p1%2Fr1%2FGTv1"), None);
    }
}
