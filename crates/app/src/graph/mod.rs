//! The Graph view: every ref's history as a lane graph (git-core `graph::History`), streamed
//! in batches so the first rows show while `git log` is still walking. Reloads when refs or
//! HEAD move; selection is kept by commit id across reloads. A clicked file of the details
//! panel opens in place of the graph: read-only for a commit, the staging diff for the WIP row.

mod actions;
mod details;
pub(crate) mod format;
mod lanes;
pub mod link;
mod names;
mod search;

pub use details::DetailsPanel;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use git_core::graph::{History, Pseudo, RowKind};
use git_core::parse::diff::FileDiff;
use git_core::parse::log::{Commit, RefLabel};
use git_core::{GitError, GitRepo as _, LogQuery};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, ScrollHandle,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, uniform_list,
};
use ui::diff::{self as diff_ui, DiffModel, Layout};
use ui::{Palette, zoomed};

use crate::changes::diff::{StagingDiff, StagingDiffEvent};
use crate::drag_scroll::DragScroll;
use crate::repo_tab::RepoTab;
use link::{GraphLink, LinkEvent};
use search::Query;

const ROW_HEIGHT: f32 = 30.;
const HEADER_HEIGHT: f32 = 26.;
/// Author + co-author avatars: three overlapping, then "+N".
const AUTHOR_WIDTH: f32 = 72.;
const DATE_WIDTH: f32 = 90.;
/// Avatars drawn in a row before the rest collapse into "+N".
const MAX_AVATARS: usize = 3;
/// Rows of the date-order preview shown while `--topo-order` is still working out its first
/// commits (1.2 s on the Linux kernel even with a commit-graph; date order answers in ~0.1 s).
const PREVIEW_ROWS: usize = 200;

/// What the loaded history reflects: reload when this changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    refs: Vec<(String, String)>,
    head: Option<String>,
    /// The worktree has changes: the WIP row shows.
    dirty: bool,
    /// Stash commit ids with their `stash@{n}`.
    stashes: Vec<(String, u32)>,
}

impl Snapshot {
    fn pseudo(&self) -> Pseudo {
        Pseudo {
            wip_parent: self.dirty.then(|| self.head.clone()).flatten(),
            stashes: self.stashes.clone(),
        }
    }
}

/// Staged / unstaged / untracked file counts for the WIP row.
fn wip_counts(tab: &RepoTab) -> (usize, usize, usize) {
    use git_core::parse::status::{EntryKind, FileState};
    let entries = tab.status.iter().flat_map(|s| &s.entries);
    entries.fold((0, 0, 0), |(s, u, n), e| match e.kind {
        EntryKind::Untracked => (s, u, n + 1),
        EntryKind::Unmerged => (s, u + 1, n),
        _ => (
            s + usize::from(e.index != FileState::Unmodified),
            u + usize::from(e.worktree != FileState::Unmodified),
            n,
        ),
    })
}

/// The lane under the pointer: highlighted, with its branch name in a tooltip.
#[derive(Debug, Clone, PartialEq)]
struct Hover {
    color: u32,
    row: usize,
    name: Option<SharedString>,
    /// Pointer position inside the graph body, for the tooltip.
    at: Point<Pixels>,
}

/// Which refs the graph walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Scope {
    #[default]
    All,
    /// Only what `HEAD` reaches.
    Current,
}

impl Scope {
    fn query(self) -> LogQuery {
        match self {
            Scope::All => LogQuery::graph(),
            Scope::Current => LogQuery {
                bodies: false,
                ..LogQuery::default()
            },
        }
    }
}

/// The search box's matches, kept up to date as rows stream in.
#[derive(Default)]
struct Search {
    query: Option<Query>,
    /// Commits touching the path of a `path:` query; `None` while git answers.
    path_ids: Option<Arc<HashSet<String>>>,
    /// Matching rows, ascending.
    hits: Vec<usize>,
    /// Rows already tested.
    scanned: usize,
    path_task: Option<Task<()>>,
}

/// A file of the selected commit shown in place of the graph.
struct OpenFile {
    path: SharedString,
    /// `None` while the model (syntax highlighting) is built.
    model: Option<Arc<DiffModel>>,
    _build: Task<()>,
}

pub struct GraphView {
    tab: Entity<RepoTab>,
    link: Entity<GraphLink>,
    file: Option<OpenFile>,
    /// The WIP row's file being staged, shown instead of `file` while `staging_open`.
    staging: Entity<StagingDiff>,
    staging_open: bool,
    diff_layout: Layout,
    diff_scroll: UniformListScrollHandle,
    search_input: Entity<InputState>,
    search: Search,
    scope: Scope,
    /// Keeps the name dialog's Enter handler alive.
    dialog_sub: Option<Subscription>,
    history: History,
    loading: bool,
    error: Option<SharedString>,
    /// Selected commit id.
    selected: Option<String>,
    loaded_for: Option<Snapshot>,
    scroll: UniformListScrollHandle,
    drag: DragScroll,
    hover: Option<Hover>,
    /// Streams the log; dropping it stops git.
    load_task: Option<Task<()>>,
    /// The first-load date-order preview; replaced by the real graph's first batch.
    preview_task: Option<Task<()>>,
    /// Commit id -> first body line, fetched for the rows on screen.
    descriptions: HashMap<String, SharedString>,
    /// Ids already asked for (with or without a description).
    described: HashSet<String>,
    desc_task: Option<Task<()>>,
    /// Rows on screen at the last render, for the description fetch.
    desc_rows: Range<usize>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl GraphView {
    pub fn new(
        tab: Entity<RepoTab>,
        link: Entity<GraphLink>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search: text, author:, hash:, path:")
        });
        let staging = cx.new(|cx| StagingDiff::new(tab.clone(), cx));
        let _subs = vec![
            cx.subscribe(
                &staging,
                |this, _, event: &StagingDiffEvent, cx| match event {
                    StagingDiffEvent::Close => this.close_file(cx),
                },
            ),
            cx.subscribe(
                &search_input,
                |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => this.on_search_changed(cx),
                    InputEvent::PressEnter { shift, .. } => this.jump_to_hit(!shift, cx),
                    _ => {}
                },
            ),
            cx.observe(&tab, |this, _, cx| this.on_tab_changed(cx)),
            cx.observe(
                &crate::avatars::handle(cx).expect("avatars::init runs first"),
                |_, _, cx| cx.notify(),
            ),
            cx.subscribe(&link, |this, _, event: &LinkEvent, cx| {
                this.on_link_event(event, cx)
            }),
        ];
        let mut view = Self {
            tab,
            link,
            file: None,
            staging,
            staging_open: false,
            diff_layout: Layout::Unified,
            diff_scroll: UniformListScrollHandle::new(),
            search_input,
            search: Search::default(),
            scope: Scope::default(),
            dialog_sub: None,
            history: History::new(),
            loading: false,
            error: None,
            selected: None,
            loaded_for: None,
            scroll: UniformListScrollHandle::new(),
            drag: DragScroll::default(),
            hover: None,
            load_task: None,
            preview_task: None,
            descriptions: HashMap::new(),
            described: HashSet::new(),
            desc_task: None,
            desc_rows: 0..0,
            focus: cx.focus_handle(),
            _subs,
        };
        view.on_tab_changed(cx);
        view
    }

    fn on_tab_changed(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab.read(cx);
        if tab.repo.is_none() || tab.status.is_none() {
            return;
        }
        let snapshot = Snapshot {
            refs: tab
                .refs
                .iter()
                .map(|r| (r.full_name.clone(), r.oid.clone()))
                .collect(),
            head: tab.status.as_ref().and_then(|s| s.branch.oid.clone()),
            dirty: tab.status.as_ref().is_some_and(|s| {
                s.entries
                    .iter()
                    .any(|e| !matches!(e.kind, git_core::parse::status::EntryKind::Ignored))
            }),
            stashes: tab
                .stashes
                .iter()
                .map(|s| (s.oid.clone(), s.index))
                .collect(),
        };
        if self.loaded_for.as_ref() != Some(&snapshot) {
            self.loaded_for = Some(snapshot);
            self.reload(cx);
        }
        // The WIP row's counts follow every status refresh.
        cx.notify();
    }

    /// Streams the whole graph again. The old rows stay until the first new batch arrives.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.tab.read(cx).repo.clone() else {
            return;
        };
        self.loading = true;
        cx.notify();
        let pseudo = self
            .loaded_for
            .as_ref()
            .map(Snapshot::pseudo)
            .unwrap_or_default();
        let mut query = self.scope.query();
        if self.scope == Scope::All {
            query.stashes = pseudo.stashes.iter().map(|(id, _)| id.clone()).collect();
        }
        // First load only: a reload keeps showing the old rows until the new ones arrive.
        self.preview_task = self.history.is_empty().then(|| {
            let repo = repo.clone();
            let pseudo = pseudo.clone();
            let preview = LogQuery {
                topo_order: false,
                limit: Some(PREVIEW_ROWS),
                ..query.clone()
            };
            let rows = cx.background_executor().spawn(async move {
                let mut stream = repo.log_stream(&preview).ok()?;
                let mut commits = Vec::new();
                while let Some(batch) = stream.next_batch().await.ok()? {
                    commits.extend(batch);
                }
                Some(commits)
            });
            cx.spawn(async move |this, cx| {
                let Some(commits) = rows.await else { return };
                _ = this.update(cx, |view, cx| {
                    // Too late: the real graph already has rows.
                    if !view.loading || !view.history.is_empty() {
                        return;
                    }
                    let mut history = History::with_pseudo(pseudo);
                    history.extend(commits);
                    view.history = history;
                    view.rescan_search();
                    cx.notify();
                });
            })
        });
        let (tx, rx) = async_channel::bounded::<Result<Vec<Commit>, GitError>>(4);
        let feed = cx.background_executor().spawn(async move {
            let mut stream = match repo.log_stream(&query) {
                Ok(stream) => stream,
                Err(err) => {
                    _ = tx.send(Err(err)).await;
                    return;
                }
            };
            loop {
                let batch = stream.next_batch().await.transpose();
                let Some(batch) = batch else { return };
                let failed = batch.is_err();
                if tx.send(batch).await.is_err() || failed {
                    return;
                }
            }
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let _feed = feed;
            let mut fresh = Some(History::with_pseudo(pseudo));
            while let Ok(batch) = rx.recv().await {
                let updated = this.update(cx, |view, cx| {
                    match batch {
                        Ok(commits) => match fresh.take() {
                            Some(mut history) => {
                                history.extend(commits);
                                view.history = history;
                                view.hover = None;
                                view.error = None;
                                view.search.hits.clear();
                                view.search.scanned = 0;
                                view.rescan_search();
                                view.drop_stale_wip(cx);
                            }
                            None => {
                                view.history.extend(commits);
                                view.rescan_search();
                            }
                        },
                        Err(err) => view.error = Some(err.to_string().into()),
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
            _ = this.update(cx, |view, cx| {
                // An empty log (no commits yet) still replaces the old rows.
                if let Some(history) = fresh.take() {
                    view.history = history;
                    view.search.hits.clear();
                    view.search.scanned = 0;
                    view.drop_stale_wip(cx);
                }
                view.loading = false;
                cx.notify();
            });
        }));
    }

    /// Columns the graph column may show at the list's current width.
    fn column_cap(&self, unit: Pixels) -> u32 {
        lanes::max_columns(self.scroll_handle().bounds().size.width, unit)
    }

    fn graph_width(&self, unit: Pixels) -> Pixels {
        lanes::column_width(self.history.layout().width(), self.column_cap(unit), unit)
    }

    fn gutter(&self, unit: Pixels) -> Option<u32> {
        lanes::gutter(self.history.layout().width(), self.column_cap(unit))
    }

    fn scroll_handle(&self) -> ScrollHandle {
        self.scroll.0.borrow().base_handle.clone()
    }

    /// Hit-tests the lane under `position` (window coordinates) and updates the hover.
    fn update_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let handle = self.scroll_handle();
        let bounds = handle.bounds();
        let unit = zoomed(1., cx);
        let row_height = zoomed(ROW_HEIGHT, cx);
        let at = position - bounds.origin;
        let in_list = at.y - handle.offset().y;
        let row = (f32::from(in_list) / f32::from(row_height)).floor();
        let hit = (row >= 0. && bounds.contains(&position))
            .then_some(row as usize)
            .filter(|&row| row < self.history.len())
            .and_then(|row| {
                let lanes = lanes::rows_lanes(&self.history, row..row + 1).pop()?;
                // Rows start with a 2 px selection border before the graph cell.
                let x = at.x - gpui_kit::px(2.);
                let y = in_list - row_height * row as f32;
                let gutter = self.gutter(unit);
                lanes::hit(&lanes, x, y, row_height, unit, gutter).map(|color| (color, row))
            });
        let hover = match hit {
            None => None,
            Some((color, row)) => {
                let name = match &self.hover {
                    Some(old) if old.color == color && old.row == row => old.name.clone(),
                    _ => names::lane_name(&self.history, color, row).map(SharedString::from),
                };
                Some(Hover {
                    color,
                    row,
                    name,
                    at,
                })
            }
        };
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }

    fn end_drag(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let handle = self.scroll_handle();
        self.drag.release(&handle, cx, |view| &mut view.drag);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.reload(cx);
            // A `path:` search depends on the scope too.
            self.on_search_changed(cx);
        }
    }

    fn on_search_changed(&mut self, cx: &mut Context<Self>) {
        let query = Query::parse(&self.search_input.read(cx).value());
        let path = query.as_ref().and_then(Query::path).map(str::to_owned);
        self.search = Search {
            query,
            ..Search::default()
        };
        if let Some(path) = path {
            self.search.path_task = self.find_path_commits(path, cx);
        }
        self.rescan_search();
        if !self.search.hits.is_empty() {
            self.jump_to_hit(true, cx);
        }
        cx.notify();
    }

    /// Asks git which commits touch `path` (in the current scope).
    fn find_path_commits(&self, path: String, cx: &mut Context<Self>) -> Option<Task<()>> {
        let repo = self.tab.read(cx).repo.clone()?;
        let query = LogQuery {
            paths: vec![path.into()],
            topo_order: false,
            ..self.scope.query()
        };
        let job = cx
            .background_executor()
            .spawn(async move { repo.log(query).await });
        Some(cx.spawn(async move |this, cx| {
            let ids: HashSet<String> = match job.await {
                Ok(commits) => commits.into_iter().map(|c| c.id).collect(),
                Err(err) => {
                    tracing::warn!(%err, "path search failed");
                    HashSet::new()
                }
            };
            _ = this.update(cx, |this, cx| {
                this.search.path_ids = Some(Arc::new(ids));
                this.search.hits.clear();
                this.search.scanned = 0;
                this.rescan_search();
                if !this.search.hits.is_empty() {
                    this.jump_to_hit(true, cx);
                }
                cx.notify();
            });
        }))
    }

    /// Tests the rows that arrived since the last scan.
    fn rescan_search(&mut self) {
        let Some(query) = &self.search.query else {
            return;
        };
        let path_ids = self.search.path_ids.as_deref();
        if query.path().is_some() && path_ids.is_none() {
            return;
        }
        for ix in self.search.scanned..self.history.len() {
            if self
                .history
                .row(ix)
                .is_some_and(|row| query.matches(&row, path_ids))
            {
                self.search.hits.push(ix);
            }
        }
        self.search.scanned = self.history.len();
    }

    /// Selects the next (or previous) match after the selected row, wrapping around.
    fn jump_to_hit(&mut self, forward: bool, cx: &mut Context<Self>) {
        let hits = &self.search.hits;
        let current = self.selected_row();
        let target = if forward {
            current
                .and_then(|c| hits.iter().find(|&&h| h > c))
                .or(hits.first())
        } else {
            current
                .and_then(|c| hits.iter().rev().find(|&&h| h < c))
                .or(hits.last())
        };
        if let Some(&row) = target {
            self.select(row, cx);
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
    }

    fn render_toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let search = &self.search;
        let status: Option<String> = search.query.as_ref().map(|q| {
            if q.path().is_some() && search.path_ids.is_none() {
                return "Searching\u{2026}".into();
            }
            if search.hits.is_empty() {
                return "No matches".into();
            }
            let at = self
                .selected_row()
                .and_then(|row| search.hits.binary_search(&row).ok());
            match at {
                Some(i) => format!("{} of {}", i + 1, search.hits.len()),
                None => format!("{} matches", search.hits.len()),
            }
        });
        let has_hits = !search.hits.is_empty();
        let scope_button = |id: &'static str, label: &'static str, scope: Scope| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(label)
                .toggled(self.scope == scope)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_scope(scope, cx)))
        };
        h_flex()
            .flex_none()
            .h(zoomed(36., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .w(zoomed(280., cx))
                    .child(Input::new(&self.search_input).small().cleanable(true)),
            )
            .when_some(status, |d, status| {
                d.child(div().text_xs().text_color(palette.muted).child(status))
                    .child(
                        Button::new("search-prev")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronUp)
                            .tooltip("Previous match (Shift+Enter)")
                            .disabled(!has_hits)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.jump_to_hit(false, cx)
                            })),
                    )
                    .child(
                        Button::new("search-next")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronDown)
                            .tooltip("Next match (Enter)")
                            .disabled(!has_hits)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.jump_to_hit(true, cx)
                            })),
                    )
            })
            .child(div().flex_1())
            .child(scope_button("scope-all", "All branches", Scope::All))
            .child(scope_button(
                "scope-current",
                "Current branch",
                Scope::Current,
            ))
    }

    /// Fetches the descriptions of the commits in `rows` not asked for yet (one batch at a
    /// time; the next render asks for whatever is still missing).
    fn request_descriptions(&mut self, rows: Range<usize>, cx: &mut Context<Self>) {
        /// Let scrolling settle before asking git, so a fling is one fetch, not one per frame.
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(120);
        self.desc_rows = rows.clone();
        if self.desc_task.is_some() || self.missing_descriptions(rows).is_empty() {
            return;
        }
        self.desc_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let job = this.update(cx, |view, cx| {
                let repo = view.tab.read(cx).repo.clone()?;
                // What is on screen now, plus a screen either side.
                let rows = view.desc_rows.clone();
                let span = rows.len();
                let wide = rows.start.saturating_sub(span)..rows.end + span;
                let ids = view.missing_descriptions(wide);
                if ids.is_empty() {
                    return None;
                }
                view.described.extend(ids.iter().cloned());
                Some(
                    cx.background_executor()
                        .spawn(async move { repo.descriptions(&ids).await }),
                )
            });
            let found = match job {
                Ok(Some(job)) => job.await.ok(),
                _ => None,
            };
            _ = this.update(cx, |view, cx| {
                view.desc_task = None;
                if let Some(found) = found
                    && !found.is_empty()
                {
                    view.descriptions
                        .extend(found.into_iter().map(|(id, text)| (id, text.into())));
                    cx.notify();
                }
            });
        }));
    }

    /// Asks for the avatars of the authors (and co-authors) in `rows`.
    fn request_avatars(&self, rows: Range<usize>, cx: &mut Context<Self>) {
        let Some(avatars) = crate::avatars::handle(cx) else {
            return;
        };
        let mut emails: Vec<String> = Vec::new();
        // Each author with one commit of theirs on screen, for the hosted lookup. Co-authors
        // have no commit of their own to ask about; stash and WIP rows are not on the host.
        let mut authors: Vec<(String, String)> = Vec::new();
        for r in rows {
            let Some(row) = self.history.row(r) else {
                continue;
            };
            if self.history.kind(r) == RowKind::Commit {
                authors.push((row.author.email.clone(), row.id.to_owned()));
            }
            emails.extend(
                row.co_authors
                    .iter()
                    .filter_map(|&i| self.history.author(i))
                    .map(|a| a.email.clone()),
            );
        }
        authors.retain(|(email, _)| avatars.read(cx).pending(email));
        let hosted = if authors.is_empty() {
            None
        } else {
            crate::hosted::HostedRepo::of_tab(self.tab.read(cx), cx)
        };
        avatars.update(cx, |a, cx| {
            match &hosted {
                Some(repo) => a.request_hosted(
                    repo,
                    authors.iter().map(|(e, c)| (e.as_str(), c.as_str())),
                    cx,
                ),
                None => a.request(authors.iter().map(|(e, _)| e.as_str()), cx),
            }
            a.request(emails.iter().map(String::as_str), cx);
        });
    }

    /// Commit ids in `rows` whose description was not asked for yet.
    fn missing_descriptions(&mut self, rows: Range<usize>) -> Vec<String> {
        /// Bounded memory however far the user scrolls.
        const CACHE_LIMIT: usize = 20_000;
        if self.described.len() > CACHE_LIMIT {
            self.described.clear();
            self.descriptions.clear();
        }
        let rows = rows.start..rows.end.min(self.history.len());
        rows.filter(|&r| self.history.kind(r) == RowKind::Commit)
            .filter_map(|r| self.history.id(r))
            .filter(|id| !self.described.contains(*id))
            .map(str::to_owned)
            .collect()
    }

    /// After a reload: the WIP row was selected but the worktree is clean now (commit, stash,
    /// discard), so nothing is selected and the details panel stops showing the old changes.
    fn drop_stale_wip(&mut self, cx: &mut Context<Self>) {
        let stale = self.selected.as_deref().is_some_and(details::is_wip_id)
            && self.history.wip_id().is_none();
        if stale {
            self.selected = None;
            self.drop_file(cx);
            self.publish_selection(cx);
        }
    }

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        self.selected = self.history.id(row).map(str::to_owned);
        self.drop_file(cx);
        self.publish_selection(cx);
        cx.notify();
    }

    /// Selects (and scrolls to) the WIP row, where the commit box and staging live. Nothing
    /// happens on a clean worktree (no WIP row).
    pub fn select_wip(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.history.wip_id().and_then(|id| self.history.find(id)) else {
            return;
        };
        if self.selected_row() != Some(row) {
            self.select(row, cx);
        }
        self.scroll.scroll_to_item(row, ScrollStrategy::Center);
    }

    /// Tells the details panel which commit this tab has selected (also called when the tab
    /// becomes active).
    pub fn publish_selection(&self, cx: &mut Context<Self>) {
        let tab = Some(self.tab.downgrade());
        let commit = self.selected.clone();
        self.link
            .update(cx, |link, cx| link.set_selection(tab, commit, cx));
    }

    fn on_link_event(&mut self, event: &LinkEvent, cx: &mut Context<Self>) {
        match event {
            LinkEvent::ShowFile { tab, commit, file } if *tab == self.tab.downgrade() => {
                if self.selected.as_deref() == Some(commit.as_str()) {
                    self.show_file(file.clone(), cx);
                }
            }
            LinkEvent::ShowWorkingFile { tab, section, path } if *tab == self.tab.downgrade() => {
                if self.selected.as_deref().is_some_and(details::is_wip_id) {
                    self.file = None;
                    self.staging_open = true;
                    let (section, path) = (*section, path.clone());
                    self.staging
                        .update(cx, |staging, cx| staging.open(section, path, cx));
                    cx.notify();
                }
            }
            LinkEvent::Select { tab, commit } if *tab == self.tab.downgrade() => {
                if let Some(row) = self.history.find(commit) {
                    self.select(row, cx);
                    self.scroll.scroll_to_item(row, ScrollStrategy::Center);
                }
            }
            _ => {}
        }
    }

    /// Shows `file` in place of the graph; the model is built off the UI thread.
    fn show_file(&mut self, file: Arc<FileDiff>, cx: &mut Context<Self>) {
        let path = file
            .new_path
            .as_ref()
            .or(file.old_path.as_ref())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();
        let job = cx
            .background_executor()
            .spawn(async move { DiffModel::new((*file).clone()) });
        let build = cx.spawn(async move |this, cx| {
            let model = Arc::new(job.await);
            _ = this.update(cx, |this, cx| {
                if let Some(open) = &mut this.file {
                    open.model = Some(model);
                }
                cx.notify();
            });
        });
        self.drop_file(cx);
        self.file = Some(OpenFile {
            path: path.into(),
            model: None,
            _build: build,
        });
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Back to the graph; the details panel drops its file highlight.
    fn close_file(&mut self, cx: &mut Context<Self>) {
        if self.drop_file(cx) {
            self.link
                .update(cx, |link, cx| link.set_open_file(None, cx));
            cx.notify();
        }
    }

    /// Forgets the open file (commit or WIP); true when one was open.
    fn drop_file(&mut self, cx: &mut Context<Self>) -> bool {
        let staging = std::mem::take(&mut self.staging_open);
        if staging {
            self.staging.update(cx, |staging, cx| staging.close(cx));
        }
        self.file.take().is_some() || staging
    }

    fn render_file(&self, palette: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.file.as_ref()?;
        let layout = self.diff_layout;
        let (added, removed) = open.model.as_ref().map_or((0, 0), |m| (m.added, m.removed));
        let header = h_flex()
            .flex_none()
            .h(zoomed(36., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                Button::new("graph-back")
                    .ghost()
                    .small()
                    .label("\u{2190} Graph")
                    .tooltip("Back to the commit graph")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_file(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(palette.emph)
                    .child(open.path.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.ok)
                    .child(format!("+{added}")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.error)
                    .child(format!("\u{2212}{removed}")),
            )
            .child(
                Button::new("graph-diff-layout")
                    .ghost()
                    .small()
                    .label(match layout {
                        Layout::Unified => "Split",
                        Layout::Split => "Unified",
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.diff_layout = match this.diff_layout {
                            Layout::Unified => Layout::Split,
                            Layout::Split => Layout::Unified,
                        };
                        cx.notify();
                    })),
            );
        let message = |text: &'static str| {
            div()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child(text)
                .into_any_element()
        };
        let body = match &open.model {
            None => message("Loading diff\u{2026}"),
            Some(model) if model.lfs.is_some() => model
                .lfs
                .as_ref()
                .map_or_else(|| message(""), |c| diff_ui::lfs_summary(c, palette, cx)),
            Some(model) if model.file.binary => message("Binary file."),
            Some(model) if model.rows(layout).is_empty() => message("No content changes."),
            Some(model) => {
                let model = model.clone();
                uniform_list(
                    "graph-file-diff",
                    model.rows(layout).len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        let palette = Palette::of(cx);
                        range
                            .filter_map(|ix| {
                                diff_ui::read_only_row(&model, this.diff_layout, ix, &palette, cx)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.diff_scroll)
                .flex_1()
                .into_any_element()
            }
        };
        Some(
            v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(palette.panel)
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }

    fn selected_row(&self) -> Option<usize> {
        self.history.find(self.selected.as_deref()?)
    }

    fn render_header(
        &self,
        graph_width: gpui_kit::Pixels,
        palette: &Palette,
        cx: &App,
    ) -> impl IntoElement {
        let cell = |text: &'static str| div().child(SharedString::from(text));
        h_flex()
            .flex_none()
            .h(zoomed(HEADER_HEIGHT, cx))
            .px_2()
            .gap_2()
            .text_xs()
            .text_color(palette.muted)
            .border_b_1()
            .border_color(palette.border)
            .child(cell("GRAPH").w(graph_width).flex_none())
            .child(cell("MESSAGE").flex_1())
            .child(cell("AUTHOR").w(zoomed(AUTHOR_WIDTH, cx)).flex_none())
            .child(cell("DATE").w(zoomed(DATE_WIDTH, cx)).flex_none())
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let unit = zoomed(1., cx);
        let graph_width = self.graph_width(unit);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let selected = self.selected_row();
        let lanes = lanes::rows_lanes(&self.history, range.clone());
        let gutter = self.gutter(unit);
        let hovered = self.hover.as_ref().map(|h| h.color);
        let searching = self.search.query.is_some();
        let counts = wip_counts(self.tab.read(cx));
        range
            .zip(lanes)
            .filter_map(|(ix, lanes)| {
                let row = self.history.row(ix)?;
                let kind = self.history.kind(ix);
                let is_selected = selected == Some(ix);
                let dimmed = searching && self.search.hits.binary_search(&ix).is_err();
                let info = actions::RowInfo {
                    id: row.id.to_string(),
                    branches: row
                        .refs
                        .iter()
                        .filter_map(|r| match r {
                            RefLabel::LocalBranch(name) => Some(name.clone()),
                            _ => None,
                        })
                        .collect(),
                    merge: lanes.is_merge(),
                    head: self.history.head() == Some(ix),
                    wip: kind == RowKind::Wip,
                };
                let view = cx.entity().downgrade();
                let people: Vec<Person> = std::iter::once((row.author, "Author"))
                    .filter(|_| kind != RowKind::Wip)
                    .chain(
                        row.co_authors
                            .iter()
                            .filter_map(|&i| self.history.author(i))
                            .map(|a| (a, "Co-author")),
                    )
                    .map(|(a, role)| Person {
                        name: a.name.clone().into(),
                        email: a.email.clone().into(),
                        role,
                        color: palette.lane(format::avatar_slot(&a.email)),
                    })
                    .collect();
                // `refs/stash` is already said by the stash row's own `stash@{n}` pill.
                let pills = row
                    .refs
                    .iter()
                    .filter(|label| !matches!(label, RefLabel::Other(name) if name == "refs/stash"))
                    .map(|label| pill(label, &palette));
                let message = match kind {
                    RowKind::Wip => wip_message(counts, &palette).into_any_element(),
                    RowKind::Stash(n) => h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(
                            gpui_kit::component::Icon::new(IconName::Archive)
                                .xsmall()
                                .text_color(palette.muted),
                        )
                        .child(pill_text(format!("stash@{{{n}}}"), palette.muted))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .italic()
                                .text_color(palette.fg)
                                .child(SharedString::from(row.subject.to_string())),
                        )
                        .into_any_element(),
                    RowKind::Commit => h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(palette.emph)
                                .child(SharedString::from(row.subject.to_string())),
                        )
                        .when_some(self.descriptions.get(row.id).cloned(), |d, text| {
                            d.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .child(text),
                            )
                        })
                        .into_any_element(),
                };
                Some(
                    h_flex()
                        .id(ElementId::NamedInteger("graph-row".into(), ix as u64))
                        .h(zoomed(ROW_HEIGHT, cx))
                        .w_full()
                        .pr_2()
                        .gap_2()
                        .text_sm()
                        .border_l_2()
                        .border_color(if is_selected {
                            palette.accent
                        } else {
                            gpui_kit::transparent_black()
                        })
                        .when(is_selected, |d| d.bg(palette.accent_soft))
                        .when(dimmed, |d| d.opacity(0.35))
                        .when(!is_selected, |d| d.hover(|s| s.bg(palette.hover)))
                        .child(
                            div()
                                .w(graph_width)
                                .h_full()
                                .flex_none()
                                .child(lanes::lanes_canvas(lanes, palette, unit, hovered, gutter)),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1p5()
                                .overflow_hidden()
                                .children(pills)
                                .child(message),
                        )
                        .child(avatar_stack(ix, people, &palette, cx))
                        .child(
                            div()
                                .id(ElementId::NamedInteger("graph-date".into(), ix as u64))
                                .w(zoomed(DATE_WIDTH, cx))
                                .flex_none()
                                .tooltip({
                                    let full: SharedString =
                                        format::full_time(row.time, row.tz_offset_minutes).into();
                                    move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                                })
                                .truncate()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(format::relative_time(
                                    row.time,
                                    row.tz_offset_minutes,
                                    now,
                                ))),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| this.select(ix, cx)),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            // The release that ends a drag-scroll is not a click.
                            if !this.drag.dragged() {
                                this.select(ix, cx)
                            }
                        }))
                        .context_menu(move |menu, _, _| {
                            actions::build_menu(menu, &info, view.clone())
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }
}

/// Ref pill (mockup `.ref`): HEAD in green, local branches in the accent, remotes in lane 1,
/// tags in lane 4.
fn pill(label: &RefLabel, palette: &Palette) -> AnyElement {
    let (text, color): (&str, Hsla) = match label {
        RefLabel::Head => ("HEAD", palette.ok),
        RefLabel::LocalBranch(name) => (name, palette.accent),
        RefLabel::RemoteBranch(name) => (name, palette.lane(0)),
        RefLabel::Tag(name) => (name, palette.lane(3)),
        RefLabel::Other(name) => (name, palette.muted),
    };
    pill_text(text.to_string(), color)
}

/// The WIP row's message: "WIP" and what is waiting to be committed.
fn wip_message(
    (staged, unstaged, untracked): (usize, usize, usize),
    palette: &Palette,
) -> impl IntoElement {
    let count = |n: usize, what: &'static str, color: Hsla| {
        (n > 0).then(|| {
            div()
                .flex_none()
                .text_xs()
                .text_color(color)
                .child(SharedString::from(format!("{n} {what}")))
        })
    };
    h_flex()
        .min_w_0()
        .gap_2()
        .child(pill_text("WIP".into(), palette.warn))
        .children(count(staged, "staged", palette.ok))
        .children(count(unstaged, "unstaged", palette.warn))
        .children(count(untracked, "untracked", palette.muted))
        .when(staged + unstaged + untracked == 0, |d| {
            d.child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child("Uncommitted changes"),
            )
        })
}

fn pill_text(text: String, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.14))
        .text_xs()
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .text_color(color)
        .max_w_48()
        .truncate()
        .child(SharedString::from(text))
        .into_any_element()
}

/// Small tooltip by the pointer: lane colour dot + branch name.
fn lane_tooltip(hover: &Hover, palette: &Palette, cx: &App) -> impl IntoElement {
    let (text, color) = match &hover.name {
        Some(name) => (name.clone(), palette.emph),
        None => ("Unnamed branch".into(), palette.muted),
    };
    h_flex()
        .absolute()
        .left(hover.at.x + zoomed(14., cx))
        .top(hover.at.y + zoomed(14., cx))
        .gap_1p5()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(palette.border_strong)
        .bg(palette.panel2)
        .shadow_md()
        .text_xs()
        .text_color(color)
        .child(
            div()
                .size(zoomed(8., cx))
                .rounded_full()
                .bg(palette.lane(hover.color as usize)),
        )
        .child(text)
}

/// Someone shown in a row's author column.
#[derive(Clone)]
struct Person {
    name: SharedString,
    email: SharedString,
    role: &'static str,
    color: Hsla,
}

/// The author column: the author's avatar, then co-authors' overlapping it, at most
/// [`MAX_AVATARS`] and then "+N"; hovering lists everyone with their avatars.
fn avatar_stack(ix: usize, people: Vec<Person>, palette: &Palette, cx: &App) -> impl IntoElement {
    let more = people.len().saturating_sub(MAX_AVATARS);
    let ring = zoomed(1.5, cx);
    let shown = people.iter().take(MAX_AVATARS).enumerate().map(|(i, p)| {
        div()
            .flex_none()
            .rounded_full()
            .border(ring)
            .border_color(palette.panel)
            .when(i > 0, |d| d.ml(zoomed(-7., cx)))
            .child(avatar(&p.name, &p.email, p.color, palette, cx))
    });
    let tip_palette = *palette;
    h_flex()
        .id(ElementId::NamedInteger("graph-author".into(), ix as u64))
        .w(zoomed(AUTHOR_WIDTH, cx))
        .flex_none()
        .overflow_hidden()
        .children(shown)
        .when(more > 0, |d| {
            d.child(
                div()
                    .flex_none()
                    .ml_1()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(format!("+{more}"))),
            )
        })
        .tooltip(move |window, cx| {
            let people = people.clone();
            Tooltip::element(move |_, cx| {
                v_flex().gap_1().py_0p5().children(people.iter().map(|p| {
                    h_flex()
                        .gap_2()
                        .child(avatar(&p.name, &p.email, p.color, &tip_palette, cx))
                        .child(
                            v_flex()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(tip_palette.emph)
                                        .child(p.name.clone()),
                                )
                                .child(div().text_xs().text_color(tip_palette.muted).child(
                                    if p.email.is_empty() {
                                        SharedString::from(p.role)
                                    } else {
                                        format!("{} \u{b7} {}", p.role, p.email).into()
                                    },
                                )),
                        )
                }))
            })
            .build(window, cx)
        })
}

/// An author's picture when one is loaded ([`crate::avatars`]), else coloured initials.
fn avatar(name: &str, email: &str, color: Hsla, palette: &Palette, cx: &App) -> AnyElement {
    if let Some(image) = crate::avatars::image(email, cx) {
        return gpui_kit::img(image)
            .flex_none()
            .size(zoomed(18., cx))
            .rounded_full()
            .into_any_element();
    }
    div()
        .flex_none()
        .size(zoomed(18., cx))
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .text_size(zoomed(9., cx))
        .font_weight(gpui_kit::FontWeight::BOLD)
        .text_color(palette.white)
        .child(SharedString::from(format::initials(name)))
        .into_any_element()
}

impl Focusable for GraphView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for GraphView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        if self.staging_open {
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(self.staging.clone())
                .into_any_element();
        }
        if let Some(file) = self.render_file(&palette, cx) {
            return file;
        }
        let graph_width = self.graph_width(zoomed(1., cx));
        let count = self.history.len();
        let body = if count == 0 {
            // Nothing loaded yet (the tab's status is still being read) counts as loading too.
            let loading = self.loading || self.loaded_for.is_none();
            // A repo that failed to open never loads: show why instead.
            let error = self
                .error
                .clone()
                .or_else(|| self.tab.read(cx).error.clone());
            let text = match (error, loading) {
                (Some(err), _) => Some(err),
                (None, true) => Some("Loading history…".into()),
                (None, false) => None,
            };
            match text {
                Some(text) => div()
                    .flex_1()
                    .p_4()
                    .text_sm()
                    .text_color(palette.muted)
                    .child(text)
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .child(
                        crate::empty::EmptyState::new(
                            gpui_kit::assets::IconName::GitGraph,
                            "No commits yet",
                            "The first commit shows up here. Stage your files and write a message in Changes.",
                        )
                        .action("Open changes", |_, window, cx| {
                            window.dispatch_action(Box::new(crate::actions::ShowChanges), cx)
                        }),
                    )
                    .into_any_element(),
            }
        } else {
            div()
                .id("graph-body")
                .flex_1()
                .min_h_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, e: &MouseDownEvent, _, _| this.drag.press(e.position)),
                )
                .relative()
                .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                    if e.pressed_button == Some(MouseButton::Left) {
                        let handle = this.scroll_handle();
                        if this.drag.drag(e.position, &handle) {
                            this.clear_hover(cx);
                            cx.notify();
                        }
                    }
                    if !this.drag.dragged() || e.pressed_button.is_none() {
                        this.update_hover(e.position, cx);
                    }
                }))
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if !hovered {
                        this.clear_hover(cx);
                    }
                }))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::end_drag))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_drag))
                .child(
                    uniform_list(
                        "graph-rows",
                        count,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.request_descriptions(range.clone(), cx);
                            this.request_avatars(range.clone(), cx);
                            this.render_rows(range, cx)
                        }),
                    )
                    .track_scroll(&self.scroll)
                    .size_full(),
                )
                .when_some(self.hover.clone(), |d, hover| {
                    d.child(lane_tooltip(&hover, &palette, cx))
                })
                .into_any_element()
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.panel)
            .child(self.render_toolbar(&palette, cx))
            .child(self.render_header(graph_width, &palette, cx))
            .child(body)
            .when_some(self.error.clone().filter(|_| count > 0), |d, err| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(palette.error)
                        .child(err),
                )
            })
            .when_some(self.tab.read(cx).op_error.clone(), |d, err| {
                d.child(
                    div()
                        .id("graph-op-error")
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(palette.error)
                        .cursor_pointer()
                        .child(err)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.tab.update(cx, |tab, cx| tab.clear_op_error(cx))
                        })),
                )
            })
            .into_any_element()
    }
}
