//! The pure parts of the Boards tab: filters, Mine, grouping items into columns, the board
//! picker's sections, the paged board model and the branch name made from an item. No GPUI
//! here, so all of it is unit tested.

use std::collections::BTreeSet;

use ::hosts::{
    Board, BoardColumn, BoardItem, BoardPage, BoardScope, HostError, ItemKind, same_repo,
};

use crate::hosts::logic::ListError;

// ---- filters ------------------------------------------------------------------------------

/// What the toolbar narrows the items to. `None` = anyone / any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub assignee: Option<String>,
    pub label: Option<String>,
    pub iteration: Option<String>,
    /// Only items of this repository (`owner/repo`).
    pub repo: Option<String>,
    /// Only items assigned to this login (the Mine view).
    pub mine: Option<String>,
    /// Over title, number and labels.
    pub text: String,
}

impl Filters {
    pub fn matches(&self, item: &BoardItem) -> bool {
        let assigned = |login: &str| {
            item.assignees
                .iter()
                .any(|a| a.login.eq_ignore_ascii_case(login))
        };
        if self.assignee.as_deref().is_some_and(|a| !assigned(a)) {
            return false;
        }
        if self.mine.as_deref().is_some_and(|me| !assigned(me)) {
            return false;
        }
        if let Some(label) = &self.label
            && !item.labels.iter().any(|l| &l.name == label)
        {
            return false;
        }
        if self.iteration.is_some() && item.iteration != self.iteration {
            return false;
        }
        if let Some(repo) = &self.repo
            && !item.repo.as_deref().is_some_and(|r| same_repo(r, repo))
        {
            return false;
        }
        let text = self.text.trim().to_lowercase();
        if text.is_empty() {
            return true;
        }
        let number = text.trim_start_matches('#');
        item.title.to_lowercase().contains(&text)
            || item.number.is_some_and(|n| n.to_string() == number)
            || item
                .labels
                .iter()
                .any(|l| l.name.to_lowercase().contains(&text))
    }

    pub fn is_empty(&self) -> bool {
        self == &Filters::default()
    }
}

/// Distinct values for the filter menus, sorted.
pub fn assignees(items: &[BoardItem]) -> Vec<String> {
    distinct(
        items
            .iter()
            .flat_map(|i| i.assignees.iter().map(|a| a.login.clone())),
    )
}

pub fn labels(items: &[BoardItem]) -> Vec<String> {
    distinct(
        items
            .iter()
            .flat_map(|i| i.labels.iter().map(|l| l.name.clone())),
    )
}

pub fn iterations(items: &[BoardItem]) -> Vec<String> {
    distinct(items.iter().filter_map(|i| i.iteration.clone()))
}

fn distinct(values: impl Iterator<Item = String>) -> Vec<String> {
    values.collect::<BTreeSet<_>>().into_iter().collect()
}

/// Items assigned to `me`.
pub fn mine_count(items: &[BoardItem], me: Option<&str>) -> usize {
    let Some(me) = me else {
        return 0;
    };
    items
        .iter()
        .filter(|i| i.assignees.iter().any(|a| a.login.eq_ignore_ascii_case(me)))
        .count()
}

// ---- columns ------------------------------------------------------------------------------

/// The items under each column, in board order; an item whose column is unknown goes to the
/// first one.
pub fn group<'a>(columns: &[BoardColumn], items: &[&'a BoardItem]) -> Vec<Vec<&'a BoardItem>> {
    let mut out = vec![Vec::new(); columns.len()];
    if columns.is_empty() {
        return out;
    }
    for item in items {
        let ix = columns
            .iter()
            .position(|c| c.id == item.column)
            .unwrap_or(0);
        out[ix].push(item);
    }
    out
}

/// The column an item sits in, by name, for the table.
pub fn column_name<'a>(columns: &'a [BoardColumn], item: &BoardItem) -> &'a str {
    columns
        .iter()
        .find(|c| c.id == item.column)
        .or(columns.first())
        .map_or("", |c| c.name.as_str())
}

// ---- picker -------------------------------------------------------------------------------

/// One board in the picker and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickEntry {
    /// Index of the source (0 = the active tab's repository).
    pub source: usize,
    pub board: Board,
}

/// Where a picker section's boards come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    ThisRepo,
    Namespace,
    OtherRepos,
}

/// The picker's sections in order, each with its entries (indices into `entries`); empty
/// sections are left out.
pub fn sections(entries: &[PickEntry]) -> Vec<(Section, Vec<usize>)> {
    let section = |e: &PickEntry| match (e.source, e.board.scope) {
        (0, BoardScope::Repo) => Section::ThisRepo,
        (0, BoardScope::Namespace) => Section::Namespace,
        _ => Section::OtherRepos,
    };
    [Section::ThisRepo, Section::Namespace, Section::OtherRepos]
        .into_iter()
        .map(|s| {
            let ix: Vec<usize> = (0..entries.len())
                .filter(|&i| section(&entries[i]) == s)
                .collect();
            (s, ix)
        })
        .filter(|(_, ix)| !ix.is_empty())
        .collect()
}

/// A section's heading: `This repo · owner/repo`, `Organization · acme`, `Other repos …`.
pub fn section_title(section: Section, repo: &str, namespace_word: &str, owner: &str) -> String {
    match section {
        Section::ThisRepo => format!("This repo \u{b7} {repo}"),
        Section::Namespace => format!("{namespace_word} \u{b7} {owner}"),
        Section::OtherRepos => "Other repos in this workspace".into(),
    }
}

/// Adds boards from another source, skipping ones already listed (two tabs on one repo, an
/// organisation board seen from two repos).
pub fn merge_boards(entries: &mut Vec<PickEntry>, source: usize, boards: Vec<Board>) {
    for board in boards {
        if !entries.iter().any(|e| e.board.id == board.id) {
            entries.push(PickEntry { source, board });
        }
    }
}

// ---- the loaded board ---------------------------------------------------------------------

/// Pages loaded per refresh without asking ("Load more" after that).
pub const AUTO_PAGES: u32 = 4;

/// The board on screen. A failed load keeps the last good items (with the error beside them).
#[derive(Debug, Default)]
pub struct BoardModel {
    pub columns: Vec<BoardColumn>,
    pub items: Vec<BoardItem>,
    pub next: Option<String>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<ListError>,
    /// The host lacks a permission: show "sign in again" instead of Retry.
    pub scope: Option<String>,
    /// Unix seconds of the last good load.
    pub updated: Option<i64>,
    /// Pages fetched in the current run of loads.
    pub pages: u32,
    /// A refresh run collects its pages here and swaps them in when it ends, so a board of
    /// several pages does not shrink to the first one meanwhile.
    staging: Vec<BoardItem>,
    req: u64,
}

/// Adds `items` to `into`, replacing ones with the same id.
fn merge(into: &mut Vec<BoardItem>, items: Vec<BoardItem>) {
    for item in items {
        match into.iter_mut().find(|have| have.id == item.id) {
            Some(have) => *have = item,
            None => into.push(item),
        }
    }
}

impl BoardModel {
    pub fn begin(&mut self) -> u64 {
        self.req += 1;
        self.loading = true;
        self.req
    }

    pub fn reset(&mut self) {
        *self = Self {
            req: self.req + 1,
            ..Self::default()
        };
    }

    /// A page arrived: `append` for a further page, otherwise the first of a new run. Pages
    /// of a run (up to [`AUTO_PAGES`]) replace the items together when the run ends (the very
    /// first load shows them as they come); "Load more" pages after that add directly. False
    /// when the answer is outdated.
    pub fn finish(
        &mut self,
        req: u64,
        result: Result<BoardPage, HostError>,
        append: bool,
        now: i64,
    ) -> bool {
        if req != self.req {
            return false;
        }
        self.loading = false;
        match result {
            Ok(page) => {
                self.columns = page.columns;
                if !append {
                    self.staging.clear();
                    self.pages = 0;
                }
                self.pages += 1;
                self.next = page.next;
                if self.pages > AUTO_PAGES {
                    merge(&mut self.items, page.items);
                } else {
                    merge(&mut self.staging, page.items);
                    let run_done = self.next.is_none() || self.pages >= AUTO_PAGES;
                    if run_done || !self.loaded {
                        self.items = self.staging.clone();
                    }
                }
                self.loaded = true;
                self.error = None;
                self.scope = None;
                self.updated = Some(now);
            }
            Err(err) => {
                self.scope = match &err {
                    HostError::MissingScope { scope, .. } => Some(scope.clone()),
                    _ => None,
                };
                self.error = Some(ListError::of(&err));
            }
        }
        true
    }

    /// Fetch the next page on its own (still within the automatic pages).
    pub fn wants_more(&self) -> bool {
        self.next.is_some() && self.pages < AUTO_PAGES
    }

    /// Puts an item the host answered after a move or detail load in place.
    pub fn replace(&mut self, item: BoardItem) {
        if let Some(have) = self.staging.iter_mut().find(|i| i.id == item.id) {
            *have = item.clone();
        }
        if let Some(have) = self.items.iter_mut().find(|i| i.id == item.id) {
            *have = item;
        }
    }

    /// Moves an item locally before the host confirms; answers the column it left.
    pub fn place(&mut self, id: &str, column: &str) -> Option<String> {
        if let Some(staged) = self.staging.iter_mut().find(|i| i.id == id) {
            staged.column = column.to_string();
        }
        let item = self.items.iter_mut().find(|i| i.id == id)?;
        Some(std::mem::replace(&mut item.column, column.to_string()))
    }

    /// Items per column id.
    pub fn count(&self, column: &str) -> usize {
        self.items.iter().filter(|i| i.column == column).count()
    }
}

// ---- branches -----------------------------------------------------------------------------

const SLUG_MAX: usize = 48;

/// The branch "Create branch" offers: `<number>-<slug of the title>` (GitLab's and GitHub's
/// own convention), only the slug for drafts.
pub fn branch_name(item: &BoardItem) -> String {
    let mut slug = String::new();
    for c in item.title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= SLUG_MAX {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    match (item.number, item.kind, slug.is_empty()) {
        (Some(n), _, true) => format!("{n}-item"),
        (Some(n), ItemKind::Issue | ItemKind::PullRequest, false) => format!("{n}-{slug}"),
        (_, _, true) => "board-item".into(),
        (_, _, false) => slug.to_string(),
    }
}

/// The branch "Checkout branch" offers: a linked branch, else a linked pull request's.
pub fn linked_branch(item: &BoardItem) -> Option<String> {
    item.branches
        .first()
        .cloned()
        .or_else(|| item.pull_requests.iter().find_map(|p| p.branch.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::hosts::{IssueState, Label, LinkedPr, Person, PrState};

    fn person(login: &str) -> Person {
        Person {
            login: login.into(),
            name: None,
            avatar_url: None,
            web_url: None,
        }
    }

    fn item(id: &str, number: Option<u64>, title: &str, column: &str) -> BoardItem {
        BoardItem {
            id: id.into(),
            column: column.into(),
            kind: ItemKind::Issue,
            number,
            title: title.into(),
            body: None,
            state: IssueState::Open,
            repo: Some("o/r".into()),
            item_type: None,
            author: None,
            assignees: vec![],
            labels: vec![],
            iteration: None,
            branches: vec![],
            pull_requests: vec![],
            web_url: None,
            updated_at: String::new(),
        }
    }

    fn col(id: &str) -> BoardColumn {
        BoardColumn {
            id: id.into(),
            name: id.to_uppercase(),
            color: None,
            limit: None,
            done: false,
        }
    }

    fn board(id: &str, scope: BoardScope) -> Board {
        Board {
            id: id.into(),
            name: id.into(),
            scope,
            owner: "o".into(),
            number: None,
            item_count: None,
            web_url: None,
        }
    }

    #[test]
    fn filters_combine() {
        let mut a = item("a", Some(9), "Graph flickers", "c1");
        a.assignees = vec![person("Mushr")];
        a.labels = vec![Label {
            name: "bug".into(),
            color: None,
        }];
        a.iteration = Some("Sprint 42".into());
        let mut b = item("b", Some(3), "Status renames", "c1");
        b.repo = Some("other/repo".into());
        let all = [&a, &b];
        let count = |f: &Filters| all.iter().filter(|i| f.matches(i)).count();
        assert_eq!(count(&Filters::default()), 2);
        assert!(Filters::default().is_empty());
        let f = Filters {
            assignee: Some("mushr".into()),
            ..Filters::default()
        };
        assert_eq!(count(&f), 1);
        let f = Filters {
            mine: Some("someone".into()),
            ..Filters::default()
        };
        assert_eq!(count(&f), 0);
        let f = Filters {
            label: Some("bug".into()),
            iteration: Some("Sprint 42".into()),
            ..Filters::default()
        };
        assert_eq!(count(&f), 1);
        let f = Filters {
            repo: Some("O/R".into()),
            ..Filters::default()
        };
        assert_eq!(count(&f), 1, "only items of this repo");
        let f = Filters {
            text: "#3".into(),
            ..Filters::default()
        };
        assert_eq!(count(&f), 1);
        let f = Filters {
            text: "BUG".into(),
            ..Filters::default()
        };
        assert_eq!(count(&f), 1, "labels match the text too");
    }

    #[test]
    fn menus_and_mine() {
        let mut a = item("a", Some(1), "x", "c");
        a.assignees = vec![person("kai"), person("mushr")];
        a.iteration = Some("S2".into());
        let mut b = item("b", Some(2), "y", "c");
        b.assignees = vec![person("mushr")];
        b.iteration = Some("S1".into());
        let items = [a, b];
        assert_eq!(assignees(&items), ["kai", "mushr"]);
        assert_eq!(iterations(&items), ["S1", "S2"]);
        assert!(labels(&items).is_empty());
        assert_eq!(mine_count(&items, Some("MUSHR")), 2);
        assert_eq!(mine_count(&items, None), 0);
    }

    #[test]
    fn grouping_keeps_order_and_catches_strays() {
        let cols = [col("todo"), col("doing")];
        let a = item("a", None, "a", "doing");
        let b = item("b", None, "b", "gone");
        let c = item("c", None, "c", "doing");
        let groups = group(&cols, &[&a, &b, &c]);
        let ids = |g: &Vec<&BoardItem>| g.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&groups[0]), ["b"]);
        assert_eq!(ids(&groups[1]), ["a", "c"]);
        assert!(group(&[], &[&a]).is_empty());
        assert_eq!(column_name(&cols, &a), "DOING");
        assert_eq!(column_name(&cols, &b), "TODO");
    }

    #[test]
    fn picker_sections_by_source_and_scope() {
        let mut entries = Vec::new();
        merge_boards(
            &mut entries,
            0,
            vec![
                board("org", BoardScope::Namespace),
                board("repo", BoardScope::Repo),
            ],
        );
        merge_boards(
            &mut entries,
            1,
            vec![
                board("org", BoardScope::Namespace),
                board("gl", BoardScope::Repo),
            ],
        );
        assert_eq!(entries.len(), 3, "a board seen twice is listed once");
        let s = sections(&entries);
        assert_eq!(
            s,
            [
                (Section::ThisRepo, vec![1]),
                (Section::Namespace, vec![0]),
                (Section::OtherRepos, vec![2])
            ]
        );
        assert_eq!(
            section_title(Section::Namespace, "o/r", "Group", "acme"),
            "Group \u{b7} acme"
        );
    }

    #[test]
    fn board_model_pages_and_errors() {
        let page = |ids: &[&str], next: Option<&str>| BoardPage {
            columns: vec![col("c")],
            items: ids.iter().map(|id| item(id, None, id, "c")).collect(),
            next: next.map(Into::into),
        };
        let mut m = BoardModel::default();
        let r = m.begin();
        assert!(m.finish(r, Ok(page(&["a", "b"], Some("p2"))), false, 10));
        assert!(m.wants_more());
        let r = m.begin();
        assert!(m.finish(r, Ok(page(&["b", "c"], None)), true, 11));
        assert_eq!(m.items.len(), 3);
        assert!(!m.wants_more());
        // A refresh keeps all three on screen until its run of pages ends.
        let r = m.begin();
        assert!(m.finish(r, Ok(page(&["a"], Some("p2"))), false, 11));
        assert_eq!(m.items.len(), 3);
        let r = m.begin();
        assert!(m.finish(r, Ok(page(&["c"], None)), true, 11));
        let ids: Vec<&str> = m.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"], "b is gone from the board");
        let old = m.begin();
        let r = m.begin();
        assert!(!m.finish(old, Ok(page(&[], None)), false, 12), "outdated");
        let err = HostError::MissingScope {
            host: "api.github.com".into(),
            scope: "read:project".into(),
        };
        assert!(m.finish(r, Err(err), false, 12));
        assert_eq!(m.items.len(), 2, "stale items stay");
        assert_eq!(m.scope.as_deref(), Some("read:project"));
        assert!(m.error.as_ref().unwrap().auth);
        assert_eq!(m.place("a", "z").as_deref(), Some("c"));
        assert_eq!(m.count("z"), 1);
        m.reset();
        assert!(m.items.is_empty() && !m.loaded);
    }

    #[test]
    fn branch_names_from_items() {
        let a = item("a", Some(9), "Graph flickers when fetch finishes!", "c");
        assert_eq!(branch_name(&a), "9-graph-flickers-when-fetch-finishes");
        let b = item("b", Some(12), "  Ünïcode: «stuff» / more  ", "c");
        assert_eq!(branch_name(&b), "12-n-code-stuff-more");
        let mut draft = item("d", None, "Flatpak packaging", "c");
        draft.kind = ItemKind::Draft;
        assert_eq!(branch_name(&draft), "flatpak-packaging");
        assert_eq!(branch_name(&item("e", Some(4), "!!!", "c")), "4-item");
        assert_eq!(branch_name(&item("f", None, "", "c")), "board-item");
        let long = item("g", Some(1), &"word ".repeat(40), "c");
        assert!(branch_name(&long).len() <= SLUG_MAX + 2);
        assert!(!branch_name(&long).ends_with('-'));
    }

    #[test]
    fn linked_branch_prefers_branches_then_prs() {
        let mut a = item("a", Some(1), "x", "c");
        assert_eq!(linked_branch(&a), None);
        a.pull_requests = vec![LinkedPr {
            number: 2,
            state: PrState::Open,
            draft: false,
            branch: Some("pr-branch".into()),
            web_url: None,
        }];
        assert_eq!(linked_branch(&a).as_deref(), Some("pr-branch"));
        a.branches = vec!["own".into()];
        assert_eq!(linked_branch(&a).as_deref(), Some("own"));
    }
}
