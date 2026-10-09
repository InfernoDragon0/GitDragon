//! GitLab issue boards over REST v4. Included from `gitlab.rs` as its child module.
//!
//! - Listing: the project's boards (`/projects/:id/boards`), then its group's
//!   (`/groups/:id/boards`; a user namespace has none, its 404 is ignored).
//! - Columns: `Open`, one per label list (by position), `Closed`. Assignee / milestone /
//!   iteration lists (paid tiers) are left out.
//! - Items: the project's (or group's) issues, newest change first, 100 per page; an open issue
//!   sits in the first list whose label it carries, else in `Open`.
//! - Moves: `PUT /projects/:id/issues/:iid` with `add_labels` / `remove_labels` (every board
//!   list label the issue carries goes) and `state_event` (`Closed` closes, the others reopen).
//!
//! Ids: board `p/<board id>/<project path>` or `g/<board id>/<group path>`; column `open`,
//! `closed` or `label:<name>`; item `<project id>/<iid>`.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{GitLab, GlUser};
use crate::boards::*;
use crate::error::HostError;
use crate::host::HostResult;
use crate::http::Method;
use crate::model::{IssueState, Label, PrState};
use crate::remote::RepoId;

const PER_PAGE: &str = "100";
const OPEN: &str = "open";
const CLOSED: &str = "closed";
const LABEL: &str = "label:";

/// Where a board lives: `(group?, board id, full path)`.
fn parse_board(id: &str) -> Option<(bool, &str, &str)> {
    let mut parts = id.splitn(3, '/');
    let group = match parts.next()? {
        "p" => false,
        "g" => true,
        _ => return None,
    };
    Some((group, parts.next()?, parts.next()?))
}

fn parse_item(id: &str) -> Option<(&str, &str)> {
    id.split_once('/')
}

impl GitLab {
    fn bad_id(&self, what: &str) -> HostError {
        HostError::Invalid {
            host: self.api.host().to_string(),
            message: format!("not a GitLab {what} id"),
        }
    }

    fn board_web(&self, group: bool, path: &str, board: &str) -> String {
        let mut url = self.web.clone();
        {
            let mut segs = url.path_segments_mut().expect("https base");
            segs.pop_if_empty();
            if group {
                segs.push("groups");
            }
            segs.extend(path.split('/').filter(|s| !s.is_empty()))
                .extend(["-", "boards", board]);
        }
        url.to_string()
    }

    /// The board definition (cached GET).
    async fn board_def(&self, board: &Board) -> HostResult<GlBoard> {
        let (group, id, path) = parse_board(&board.id).ok_or_else(|| self.bad_id("board"))?;
        let root = if group { "groups" } else { "projects" };
        let url = self.api.url(&[root, path, "boards", id], &[]);
        let fetched = self.api.get(url).await?;
        self.api.decode(&fetched.body)
    }

    fn issue_url(&self, item: &BoardItem, rest: &[&str]) -> HostResult<url::Url> {
        let (project, iid) = parse_item(&item.id).ok_or_else(|| self.bad_id("issue"))?;
        let mut segments = vec!["projects", project, "issues", iid];
        segments.extend_from_slice(rest);
        Ok(self.api.url(&segments, &[]))
    }
}

pub(super) async fn boards(gl: &GitLab, repo: &RepoId) -> HostResult<Vec<Board>> {
    let project = super::project_path(repo);
    let fetched = gl
        .api
        .get(gl.api.url(&["projects", &project, "boards"], &[]))
        .await?;
    let list: Vec<GlBoard> = gl.api.decode(&fetched.body)?;
    let mut out: Vec<Board> = list
        .into_iter()
        .map(|b| board_of(gl, b, false, &project))
        .collect();
    let group_url = gl.api.url(&["groups", &repo.owner, "boards"], &[]);
    match gl.api.get(group_url).await {
        Ok(fetched) => {
            let list: Vec<GlBoard> = gl.api.decode(&fetched.body)?;
            out.extend(list.into_iter().map(|b| board_of(gl, b, true, &repo.owner)));
        }
        // A personal namespace is no group; a token may not see the group.
        Err(HostError::NotFound { .. } | HostError::Forbidden { .. }) => {}
        Err(err) => return Err(err),
    }
    Ok(out)
}

fn board_of(gl: &GitLab, b: GlBoard, group: bool, path: &str) -> Board {
    let id = b.id.to_string();
    Board {
        web_url: Some(gl.board_web(group, path, &id)),
        id: format!("{}/{id}/{path}", if group { "g" } else { "p" }),
        name: b
            .name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Development".into()),
        scope: if group {
            BoardScope::Namespace
        } else {
            BoardScope::Repo
        },
        owner: path.to_string(),
        number: None,
        item_count: None,
    }
}

/// `Open`, the label lists by position, `Closed`.
fn columns(def: &GlBoard) -> Vec<BoardColumn> {
    let mut lists: Vec<&GlList> = def.lists.iter().filter(|l| l.label.is_some()).collect();
    lists.sort_by_key(|l| l.position.unwrap_or(i64::MAX));
    let mut out = vec![BoardColumn {
        id: OPEN.into(),
        name: "Open".into(),
        color: None,
        limit: None,
        done: false,
    }];
    out.extend(lists.into_iter().filter_map(|l| {
        let label = l.label.as_ref()?;
        Some(BoardColumn {
            id: format!("{LABEL}{}", label.name),
            name: label.name.clone(),
            color: label
                .color
                .as_deref()
                .map(|c| c.trim_start_matches('#').to_string()),
            limit: l.max_issue_count.filter(|n| *n > 0),
            done: false,
        })
    }));
    out.push(BoardColumn {
        id: CLOSED.into(),
        name: "Closed".into(),
        color: None,
        limit: None,
        done: true,
    });
    out
}

/// The column an issue sits in: closed, else its first list label, else open.
fn column_of(columns: &[BoardColumn], closed: bool, labels: &[String]) -> String {
    if closed {
        return CLOSED.into();
    }
    columns
        .iter()
        .filter_map(|c| c.id.strip_prefix(LABEL).map(|name| (c, name)))
        .find(|(_, name)| labels.iter().any(|l| l == name))
        .map_or_else(|| OPEN.into(), |(c, _)| c.id.clone())
}

pub(super) async fn board(
    gl: &GitLab,
    board: &Board,
    cursor: Option<&str>,
) -> HostResult<BoardPage> {
    let def = gl.board_def(board).await?;
    let columns = columns(&def);
    let (group, _, path) = parse_board(&board.id).ok_or_else(|| gl.bad_id("board"))?;
    let page: u32 = cursor.and_then(|c| c.parse().ok()).unwrap_or(1).max(1);
    let page_s = page.to_string();
    let root = if group { "groups" } else { "projects" };
    let url = gl.api.url(
        &[root, path, "issues"],
        &[
            ("state", "all"),
            ("order_by", "updated_at"),
            ("sort", "desc"),
            ("per_page", PER_PAGE),
            ("page", &page_s),
        ],
    );
    let fetched = gl.api.get(url).await?;
    let raw: Vec<GlBoardIssue> = gl.api.decode(&fetched.body)?;
    let items = raw.into_iter().map(|i| item(i, &columns)).collect();
    Ok(BoardPage {
        columns,
        items,
        next: fetched.next_page.then(|| (page + 1).to_string()),
    })
}

/// The `PUT` body that moves an issue carrying `labels` into `to`.
pub(super) fn move_body(columns: &[BoardColumn], labels: &[Label], to: &BoardColumn) -> Value {
    let list_labels: Vec<&str> = columns
        .iter()
        .filter_map(|c| c.id.strip_prefix(LABEL))
        .collect();
    let target = to.id.strip_prefix(LABEL);
    let remove: Vec<&str> = labels
        .iter()
        .map(|l| l.name.as_str())
        .filter(|l| list_labels.contains(l) && Some(*l) != target)
        .collect();
    let mut body = Map::new();
    if let Some(label) = target {
        body.insert("add_labels".into(), json!(label));
    }
    if !remove.is_empty() {
        body.insert("remove_labels".into(), json!(remove.join(",")));
    }
    body.insert(
        "state_event".into(),
        json!(if to.id == CLOSED { "close" } else { "reopen" }),
    );
    Value::Object(body)
}

pub(super) async fn move_item(
    gl: &GitLab,
    repo: &RepoId,
    board: &Board,
    target: &BoardItem,
    to: &BoardColumn,
) -> HostResult<BoardItem> {
    let columns = columns(&gl.board_def(board).await?);
    let body = move_body(&columns, &target.labels, to);
    let answer = gl
        .api
        .send_json(Method::Put, gl.issue_url(target, &[])?, &body)
        .await?;
    invalidate_board(gl, repo, board);
    let raw: GlBoardIssue = gl.api.decode(&answer)?;
    Ok(item(raw, &columns))
}

pub(super) fn invalidate_board(gl: &GitLab, repo: &RepoId, board: &Board) {
    gl.api.invalidate(&["projects", &super::project_path(repo)]);
    if let Some((true, _, path)) = parse_board(&board.id) {
        gl.api.invalidate(&["groups", path]);
    }
}

/// The issue again plus its related merge requests (their source branches become the
/// item's branches).
pub(super) async fn item_detail(
    gl: &GitLab,
    board: &Board,
    target: &BoardItem,
) -> HostResult<BoardItem> {
    let columns = columns(&gl.board_def(board).await?);
    let fetched = gl.api.get(gl.issue_url(target, &[])?).await?;
    let mut out = item(gl.api.decode(&fetched.body)?, &columns);
    let url = gl.issue_url(target, &["related_merge_requests"])?;
    match gl.api.get(url).await {
        Ok(fetched) => {
            let mrs: Vec<GlRelatedMr> = gl.api.decode(&fetched.body)?;
            for mr in mrs {
                if !out.branches.contains(&mr.source_branch) {
                    out.branches.push(mr.source_branch.clone());
                }
                out.pull_requests.push(LinkedPr {
                    number: mr.iid,
                    state: match mr.state.as_str() {
                        "merged" => PrState::Merged,
                        "closed" => PrState::Closed,
                        _ => PrState::Open,
                    },
                    draft: mr.draft,
                    branch: Some(mr.source_branch),
                    web_url: mr.web_url,
                });
            }
        }
        Err(HostError::Forbidden { .. } | HostError::NotFound { .. }) => {}
        Err(err) => return Err(err),
    }
    Ok(out)
}

fn item(i: GlBoardIssue, columns: &[BoardColumn]) -> BoardItem {
    let closed = i.state != "opened";
    let repo = i
        .references
        .and_then(|r| r.full)
        .and_then(|f| f.rsplit_once('#').map(|(path, _)| path.to_string()));
    BoardItem {
        id: format!("{}/{}", i.project_id, i.iid),
        column: column_of(columns, closed, &i.labels),
        kind: ItemKind::Issue,
        number: Some(i.iid),
        title: i.title,
        body: i.description.filter(|d| !d.is_empty()),
        state: if closed {
            IssueState::Closed
        } else {
            IssueState::Open
        },
        repo,
        item_type: None,
        author: i.author.map(Into::into),
        assignees: i.assignees.into_iter().map(Into::into).collect(),
        labels: super::labels(i.labels),
        iteration: i
            .iteration
            .and_then(|t| t.title)
            .or_else(|| i.milestone.and_then(|m| m.title))
            .filter(|t| !t.is_empty()),
        branches: Vec::new(),
        pull_requests: Vec::new(),
        web_url: Some(i.web_url),
        updated_at: i.updated_at,
    }
}

// GitLab's JSON, only the fields we use.

#[derive(Deserialize)]
struct GlBoard {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    lists: Vec<GlList>,
}

#[derive(Deserialize)]
struct GlList {
    #[serde(default)]
    label: Option<GlListLabel>,
    #[serde(default)]
    position: Option<i64>,
    #[serde(default)]
    max_issue_count: Option<u32>,
}

#[derive(Deserialize)]
struct GlListLabel {
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
struct GlTitled {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct GlReferences {
    #[serde(default)]
    full: Option<String>,
}

#[derive(Deserialize)]
struct GlBoardIssue {
    iid: u64,
    project_id: u64,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: String,
    #[serde(default)]
    author: Option<GlUser>,
    #[serde(default)]
    assignees: Vec<GlUser>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    milestone: Option<GlTitled>,
    #[serde(default)]
    iteration: Option<GlTitled>,
    #[serde(default)]
    references: Option<GlReferences>,
    web_url: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct GlRelatedMr {
    iid: u64,
    state: String,
    #[serde(default)]
    draft: bool,
    source_branch: String,
    #[serde(default)]
    web_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(id: &str) -> BoardColumn {
        BoardColumn {
            id: id.into(),
            name: id.into(),
            color: None,
            limit: None,
            done: id == CLOSED,
        }
    }

    fn label(name: &str) -> Label {
        Label {
            name: name.into(),
            color: None,
        }
    }

    #[test]
    fn ids_parse() {
        assert_eq!(
            parse_board("p/12/g/sub/proj"),
            Some((false, "12", "g/sub/proj"))
        );
        assert_eq!(parse_board("g/7/g"), Some((true, "7", "g")));
        assert_eq!(parse_board("x/1/a"), None);
        assert_eq!(parse_item("44/3"), Some(("44", "3")));
    }

    #[test]
    fn issues_land_in_the_first_matching_list() {
        let cols = [
            col(OPEN),
            col("label:Doing"),
            col("label:Review"),
            col(CLOSED),
        ];
        let labels = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            column_of(&cols, false, &labels(&["bug", "Review"])),
            "label:Review"
        );
        assert_eq!(
            column_of(&cols, false, &labels(&["Review", "Doing"])),
            "label:Doing"
        );
        assert_eq!(column_of(&cols, false, &labels(&["bug"])), OPEN);
        assert_eq!(column_of(&cols, true, &labels(&["Doing"])), CLOSED);
    }

    #[test]
    fn moves_swap_list_labels() {
        let cols = [
            col(OPEN),
            col("label:Doing"),
            col("label:Review"),
            col(CLOSED),
        ];
        let body = move_body(&cols, &[label("bug"), label("Doing")], &col("label:Review"));
        assert_eq!(
            body,
            json!({"add_labels": "Review", "remove_labels": "Doing", "state_event": "reopen"})
        );
        let body = move_body(&cols, &[label("Doing")], &col(CLOSED));
        assert_eq!(
            body,
            json!({"remove_labels": "Doing", "state_event": "close"})
        );
        let body = move_body(&cols, &[label("bug")], &col(OPEN));
        assert_eq!(body, json!({"state_event": "reopen"}));
    }
}
