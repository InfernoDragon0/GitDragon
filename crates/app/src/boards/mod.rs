//! The Boards tab of one repository tab: GitHub Projects, GitLab issue boards and Azure Boards
//! through the `GitHost` trait only (a host without `Feature::Boards` gets the empty state).
//!
//! - Picker: boards of this repo, of its organisation / group / project, and of the other open
//!   repository tabs (any host). A board from another tab is read and moved with that tab's
//!   account; branch actions always run in this tab.
//! - Views: Board (columns, drag a card onto another column to move it), Table, Mine
//!   (assigned to the signed-in user). Filters: assignee, label, iteration, this repo only,
//!   text.
//! - Card peek on the right: Status (move), assignees, iteration, linked branches and pull
//!   requests; Checkout branch, Open PR (the PR view for this repo, else the browser), Create
//!   branch (`<number>-<slug>` off HEAD, checked out).
//! - Loading: pages of the board load in a row up to `logic::AUTO_PAGES`, then "Load more";
//!   answers are cached by the host client for a short TTL; refresh on focus and on the poll
//!   schedule; rate-limit / missing-permission banners with Retry or Accounts.
//! - [`logic`]: the pure parts, unit tested.

pub mod logic;

use std::ops::Range;
use std::time::{Duration, Instant};

use ::hosts::{
    Board, BoardColumn, BoardItem, Feature, HostVocab, IssueState, ItemKind, PollSchedule, PrState,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, ElementId, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_kit::{FocusHandle, KeyDownEvent, MouseButton, NavigationDirection, ScrollHandle};
use ui::{Palette, zoomed};

use crate::hosted::{HostedRepo, Resolution, Session};
use crate::hosts::logic::{ListError, now_secs, relative};
use crate::hosts::page::{self, WidthProbe};
use crate::hosts::{Link, dot, empty_panel, error_banner, label_color, label_pill, pill, resolve};
use crate::repo_tab::RepoTab;
use logic::{BoardModel, Filters, PickEntry, Section};

const TICK: Duration = Duration::from_secs(15);
const COLUMN_WIDTH: f32 = 264.;
/// Widest the description column reads well; the metadata column sits beside it.
const PROSE_WIDTH: f32 = 960.;
const META_WIDTH: f32 = 300.;
/// Below this window width the metadata stacks above the description.
const WIDE_VIEW: f32 = 900.;
const ROW_HEIGHT: f32 = 34.;
const MAX_BODY: usize = 12_000;

/// What the centre panel should do for the Boards view.
pub enum BoardsEvent {
    /// Show pull request `number` of this repository in the pull request view.
    OpenPullRequest(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Board,
    Table,
    Mine,
}

/// Where boards come from: this tab's host (index 0), then other open tabs'.
struct Source {
    session: Session,
    /// `owner/repo` for the picker.
    repo: String,
}

/// A card being dragged to another column.
#[derive(Clone)]
struct DragCard {
    id: String,
    title: SharedString,
}

impl Render for DragCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        div()
            .w(px(COLUMN_WIDTH - 24.))
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(palette.accent)
            .bg(palette.panel)
            .shadow_md()
            .text_sm()
            .text_color(palette.emph)
            .child(self.title.clone())
    }
}

pub struct BoardsView {
    tab: Entity<RepoTab>,
    link: Link,
    vocab: &'static HostVocab,
    remotes_seen: Option<Vec<(String, Option<String>)>>,
    resolve_task: Option<Task<()>>,
    /// The other repository tabs, for the picker's "other repos" section.
    other_tabs: Vec<WeakEntity<RepoTab>>,
    sources: Vec<Source>,
    entries: Vec<PickEntry>,
    boards_loading: bool,
    boards_error: Option<ListError>,
    boards_task: Option<Task<()>>,
    /// The picked board (index into `entries`) and its id, kept across reloads.
    picked: Option<usize>,
    picked_id: Option<String>,
    /// The board whose items `model` holds.
    shown_id: Option<String>,
    model: BoardModel,
    board_task: Option<Task<()>>,
    mode: Mode,
    filters: Filters,
    text: Entity<InputState>,
    /// The signed-in login (Mine view, "Assignee: me").
    me: Option<String>,
    me_task: Option<Task<()>>,
    selected: Option<String>,
    /// The selected item fills the whole panel instead of the board / table.
    detail_open: bool,
    detail_scroll: ScrollHandle,
    /// The compact list beside an open item: its scroll, the user's fold (kept for the
    /// session), an unfold on a narrow panel, and the panel width probe.
    side_scroll: UniformListScrollHandle,
    side_folded: bool,
    side_pinned: bool,
    panel_probe: WidthProbe,
    focus: FocusHandle,
    /// Kept on the view so the board keeps its scroll positions while a detail is open.
    board_scroll: ScrollHandle,
    column_scrolls: Vec<ScrollHandle>,
    /// Which scrollbars have a real scroll range (columns, then the board's horizontal one),
    /// as of the last layout; empty content must not show a thumb.
    bar_flags: Vec<bool>,
    detail_task: Option<Task<()>>,
    detail_loading: bool,
    acting: bool,
    act_task: Option<Task<()>>,
    /// `(is_error, text)` about the last action.
    notice: Option<(bool, SharedString)>,
    schedule: PollSchedule,
    active: bool,
    table_scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
    _timer: Option<Task<()>>,
}

impl EventEmitter<BoardsEvent> for BoardsView {}

impl BoardsView {
    pub fn new(
        tab: Entity<RepoTab>,
        other_tabs: Vec<WeakEntity<RepoTab>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut schedule = PollSchedule::host_refresh(&crate::prefs::get(cx).poll);
        schedule.set_focused(window.is_window_active(), Instant::now());
        let text = cx.new(|cx| InputState::new(window, cx).placeholder("Filter cards"));
        let subs = vec![
            cx.observe(&tab, |this, _, cx| this.on_tab_changed(cx)),
            cx.subscribe(&text, |this, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filters.text = state.read(cx).value().to_string();
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &crate::prefs::handle(cx).expect("prefs::init runs before the views"),
                window,
                |this, _, _: &crate::prefs::PollChanged, window, cx| {
                    let mut schedule = PollSchedule::host_refresh(&crate::prefs::get(cx).poll);
                    schedule.set_focused(window.is_window_active(), Instant::now());
                    this.schedule = schedule;
                },
            ),
            cx.observe_window_activation(window, |this, window, cx| {
                let regained = this
                    .schedule
                    .set_focused(window.is_window_active(), Instant::now());
                if regained && this.active && matches!(this.link, Link::Ready(_)) {
                    this.refresh(false, cx);
                }
            }),
        ];
        let timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            tab,
            link: Link::Resolving,
            vocab: &HostVocab::GENERIC,
            remotes_seen: None,
            resolve_task: None,
            other_tabs,
            sources: Vec::new(),
            entries: Vec::new(),
            boards_loading: false,
            boards_error: None,
            boards_task: None,
            picked: None,
            picked_id: None,
            shown_id: None,
            model: BoardModel::default(),
            board_task: None,
            mode: Mode::Board,
            filters: Filters::default(),
            text,
            me: None,
            me_task: None,
            selected: None,
            detail_open: false,
            detail_scroll: ScrollHandle::new(),
            side_scroll: UniformListScrollHandle::new(),
            side_folded: false,
            side_pinned: false,
            panel_probe: WidthProbe::new(page::WIDE_PANEL),
            focus: cx.focus_handle(),
            board_scroll: ScrollHandle::new(),
            column_scrolls: Vec::new(),
            bar_flags: Vec::new(),
            detail_task: None,
            detail_loading: false,
            acting: false,
            act_task: None,
            notice: None,
            schedule,
            active: true,
            table_scroll: UniformListScrollHandle::new(),
            _subs: subs,
            _timer: Some(timer),
        };
        view.on_tab_changed(cx);
        view
    }

    // ---- connection -------------------------------------------------------------------

    fn on_tab_changed(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab.read(cx);
        if tab.generation == 0 {
            return;
        }
        let sig: Vec<(String, Option<String>)> = tab
            .remotes
            .iter()
            .map(|r| (r.name.clone(), r.fetch_url.clone().or(r.push_url.clone())))
            .collect();
        if self.remotes_seen.as_ref() != Some(&sig) {
            self.remotes_seen = Some(sig);
            self.reconnect(cx);
        }
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        self.vocab = HostedRepo::of_tab(self.tab.read(cx), cx)
            .and_then(|h| h.vocab())
            .unwrap_or(&HostVocab::GENERIC);
        self.link = Link::Resolving;
        self.sources.clear();
        self.entries.clear();
        self.picked = None;
        self.shown_id = None;
        self.boards_task = None;
        self.board_task = None;
        self.model.reset();
        self.selected = None;
        self.detail_open = false;
        self.notice = None;
        self.me = None;
        self.me_task = None;
        self.resolve_task = Some(resolve(&self.tab, Feature::Boards, cx, |this, link, cx| {
            this.resolve_task = None;
            if let Link::Ready(session) = &link {
                this.vocab = session.host.vocab();
                this.sources = vec![Source {
                    repo: session.repo.full_name(),
                    session: session.clone(),
                }];
            }
            this.link = link;
            if matches!(this.link, Link::Ready(_)) {
                this.load_boards(cx);
            }
            cx.notify();
        }));
        cx.notify();
    }

    /// Shown or hidden by the centre panel; visible views refresh when due.
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            return;
        }
        match self.link {
            Link::Empty(_) => self.reconnect(cx),
            Link::Ready(_) => self.tick(cx),
            Link::Resolving => {}
        }
    }

    /// The other open repository tabs changed; their boards show up on the next list load.
    pub fn set_other_tabs(&mut self, tabs: Vec<WeakEntity<RepoTab>>) {
        self.other_tabs = tabs;
    }

    fn board_source(&self) -> Option<(&Board, &Source)> {
        let entry = self.entries.get(self.picked?)?;
        Some((&entry.board, self.sources.get(entry.source)?))
    }

    fn own_session(&self) -> Option<&Session> {
        match &self.link {
            Link::Ready(session) => Some(session),
            _ => None,
        }
    }

    // ---- loading ----------------------------------------------------------------------

    fn tick(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.model.loading {
            return;
        }
        let Some((_, source)) = self.board_source() else {
            return;
        };
        let blocked = source.session.host.rate_limit().blocked_until;
        if self.schedule.is_due(Instant::now(), blocked) {
            self.refresh(false, cx);
        }
    }

    /// Reloads the board's first page. `manual` also drops cached answers and lists the
    /// boards again.
    fn refresh(&mut self, manual: bool, cx: &mut Context<Self>) {
        if manual {
            for source in &self.sources {
                source.session.host.invalidate(Some(&source.session.repo));
            }
            self.load_boards(cx);
            return;
        }
        self.load_board(None, false, cx);
    }

    /// Lists the boards of this repo, then of the other open tabs.
    fn load_boards(&mut self, cx: &mut Context<Self>) {
        let Some(own) = self.own_session().cloned() else {
            return;
        };
        let others: Vec<HostedRepo> = self
            .other_tabs
            .iter()
            .filter_map(WeakEntity::upgrade)
            .filter(|t| *t != self.tab)
            .filter_map(|t| HostedRepo::of_tab(t.read(cx), cx))
            .collect();
        self.boards_loading = true;
        self.boards_task = Some(cx.spawn(async move |this, cx| {
            let result = own.host.boards(&own.repo).await;
            if let Err(err) = &result
                && own.retry_with_other_account(err)
            {
                _ = this.update(cx, |this, cx| this.reconnect(cx));
                return;
            }
            let ok = this
                .update(cx, |this, cx| {
                    this.boards_loading = !others.is_empty();
                    this.sources.truncate(1);
                    this.entries.clear();
                    match result {
                        Ok(boards) => {
                            this.boards_error = None;
                            logic::merge_boards(&mut this.entries, 0, boards);
                        }
                        Err(err) => this.boards_error = Some(ListError::of(&err)),
                    }
                    this.restore_pick(cx);
                    cx.notify();
                })
                .is_ok();
            if !ok {
                return;
            }
            let own_repo = own.repo.clone();
            for hosted in others {
                let found = cx
                    .background_executor()
                    .spawn(async move { hosted.resolve() })
                    .await;
                let Resolution::Ready(session) = found else {
                    continue;
                };
                if session.repo == own_repo || !session.host.supports(Feature::Boards) {
                    continue;
                }
                let Ok(boards) = session.host.boards(&session.repo).await else {
                    continue;
                };
                let alive = this.update(cx, |this, cx| {
                    let source = this.sources.len();
                    this.sources.push(Source {
                        repo: session.repo.full_name(),
                        session,
                    });
                    logic::merge_boards(&mut this.entries, source, boards);
                    // The board picked before the reload came from this tab.
                    let wished = this
                        .picked_id
                        .as_ref()
                        .and_then(|id| this.entries.iter().position(|e| &e.board.id == id));
                    if let Some(ix) = wished
                        && this.picked != Some(ix)
                    {
                        this.pick(ix, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                this.boards_loading = false;
                // A wished-for board that did not come back: the shown one is the pick now.
                this.picked_id = this
                    .picked
                    .and_then(|ix| this.entries.get(ix))
                    .map(|e| e.board.id.clone());
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Picks the board picked before (by id), else the first, and loads it. While other
    /// tabs' boards are still coming, a wish for one of those is kept (see `load_boards`).
    fn restore_pick(&mut self, cx: &mut Context<Self>) {
        let again = self
            .picked_id
            .as_ref()
            .and_then(|id| self.entries.iter().position(|e| &e.board.id == id));
        let Some(ix) = again.or((!self.entries.is_empty()).then_some(0)) else {
            self.picked = None;
            self.shown_id = None;
            self.model.reset();
            return;
        };
        let id = self.entries[ix].board.id.clone();
        if again.is_some() || !self.boards_loading {
            self.picked_id = Some(id.clone());
        }
        self.picked = Some(ix);
        if self.shown_id.as_deref() != Some(id.as_str()) {
            self.model.reset();
            self.selected = None;
            self.detail_open = false;
            self.shown_id = Some(id);
        }
        self.load_board(None, false, cx);
    }

    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        if self.picked == Some(ix) {
            return;
        }
        self.picked = Some(ix);
        self.picked_id = Some(entry.board.id.clone());
        self.shown_id = Some(entry.board.id.clone());
        self.model.reset();
        self.selected = None;
        self.detail_open = false;
        self.notice = None;
        self.filters = Filters {
            text: self.filters.text.clone(),
            ..Filters::default()
        };
        self.load_board(None, false, cx);
    }

    fn load_board(&mut self, cursor: Option<String>, append: bool, cx: &mut Context<Self>) {
        let Some((board, source)) = self.board_source() else {
            return;
        };
        let (board, host, repo) = (
            board.clone(),
            source.session.host.clone(),
            source.session.repo.clone(),
        );
        let req = self.model.begin();
        self.board_task = Some(cx.spawn(async move |this, cx| {
            let result = host.board(&repo, &board, cursor.as_deref()).await;
            _ = this.update(cx, |this, cx| {
                let ok = result.is_ok();
                if !this.model.finish(req, result, append, now_secs()) {
                    return;
                }
                if !append {
                    this.schedule.record(Instant::now(), ok);
                }
                if ok && this.model.wants_more() {
                    let next = this.model.next.clone();
                    this.load_board(next, true, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.model.loading {
            return;
        }
        if let Some(next) = self.model.next.clone() {
            self.load_board(Some(next), true, cx);
        }
    }

    /// The signed-in login, asked once per connection (for Mine).
    fn ensure_me(&mut self, cx: &mut Context<Self>) {
        if self.me.is_some() || self.me_task.is_some() {
            return;
        }
        let Some((_, source)) = self.board_source() else {
            return;
        };
        let host = source.session.host.clone();
        self.me_task = Some(cx.spawn(async move |this, cx| {
            let me = host.whoami().await;
            _ = this.update(cx, |this, cx| {
                match me {
                    Ok(person) => this.me = Some(person.login),
                    Err(err) => this.notice = Some((true, err.to_string().into())),
                }
                this.me_task = None;
                cx.notify();
            });
        }));
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        if mode == Mode::Mine {
            self.ensure_me(cx);
        }
        cx.notify();
    }

    /// The filters on screen, with Mine applied.
    fn active_filters(&self) -> Filters {
        let mut filters = self.filters.clone();
        if self.mode == Mode::Mine {
            filters.mine = Some(self.me.clone().unwrap_or_else(|| "\u{0}".into()));
        }
        filters
    }

    fn visible(&self) -> Vec<&BoardItem> {
        let filters = self.active_filters();
        self.model
            .items
            .iter()
            .filter(|i| filters.matches(i))
            .collect()
    }

    // ---- actions ----------------------------------------------------------------------

    fn selected_item(&self) -> Option<BoardItem> {
        let id = self.selected.as_ref()?;
        self.model.items.iter().find(|i| &i.id == id).cloned()
    }

    /// The ids the side list shows: the table's order in table mode, else every column's
    /// items in column order.
    fn detail_order(&self) -> Vec<String> {
        let visible = self.visible();
        if self.mode == Mode::Table {
            return visible.iter().map(|i| i.id.clone()).collect();
        }
        logic::group(&self.model.columns, &visible)
            .into_iter()
            .flatten()
            .map(|i| i.id.clone())
            .collect()
    }

    /// Back to the board / table; the selection and every scroll position stay.
    fn close_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.detail_open {
            self.detail_open = false;
            self.notice = None;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        self.detail_open = true;
        self.detail_scroll = ScrollHandle::new();
        self.notice = None;
        if let Some(ix) = self.detail_order().iter().position(|i| *i == id) {
            self.side_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        window.focus(&self.focus, cx);
        let Some((board, source)) = self.board_source() else {
            return;
        };
        let Some(item) = self.model.items.iter().find(|i| i.id == id).cloned() else {
            return;
        };
        let (board, host, repo) = (
            board.clone(),
            source.session.host.clone(),
            source.session.repo.clone(),
        );
        self.detail_loading = true;
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let result = host.board_item(&repo, &board, &item).await;
            _ = this.update(cx, |this, cx| {
                this.detail_loading = false;
                match result {
                    Ok(item) => this.model.replace(item),
                    Err(err) => tracing::debug!(%err, "board item detail"),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Moves an item to `column` (drag and drop, Status menu): shown at once, put back when
    /// the host refuses.
    fn move_item(&mut self, id: String, column: String, cx: &mut Context<Self>) {
        let Some((board, source)) = self.board_source() else {
            return;
        };
        let Some(item) = self.model.items.iter().find(|i| i.id == id).cloned() else {
            return;
        };
        let Some(to) = self.model.columns.iter().find(|c| c.id == column).cloned() else {
            return;
        };
        if item.column == to.id || self.acting {
            return;
        }
        let (board, host, repo) = (
            board.clone(),
            source.session.host.clone(),
            source.session.repo.clone(),
        );
        let from = self.model.place(&id, &to.id);
        self.acting = true;
        self.notice = None;
        self.act_task = Some(cx.spawn(async move |this, cx| {
            let result = host.move_board_item(&repo, &board, &item, &to).await;
            _ = this.update(cx, |this, cx| {
                this.acting = false;
                match result {
                    Ok(moved) => this.model.replace(moved),
                    Err(err) => {
                        if let Some(from) = from {
                            this.model.place(&id, &from);
                        }
                        this.notice = Some((true, format!("Could not move: {err}").into()));
                        if this.selected.is_none() {
                            this.selected = Some(id);
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_url(url: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(url) = url.filter(|u| !u.is_empty()) {
            crate::links::open(url, Some(window), cx);
        }
    }

    /// Is the picked board's source this tab (so its pull requests are this repo's)?
    fn own_item(&self, item: &BoardItem) -> bool {
        let own = self.own_session().map(|s| s.repo.full_name());
        let from_here = self
            .picked
            .and_then(|ix| self.entries.get(ix))
            .is_some_and(|e| e.source == 0);
        from_here
            && match (&item.repo, &own) {
                (Some(repo), Some(own)) => ::hosts::same_repo(repo, own),
                // Azure work items have no repository: their links point into this project.
                (None, Some(_)) => item.kind != ItemKind::Draft,
                _ => false,
            }
    }

    fn open_pr(&mut self, item: &BoardItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = item.pull_requests.first() else {
            return;
        };
        if self.own_item(item) && item.repo.is_some() {
            cx.emit(BoardsEvent::OpenPullRequest(pr.number));
        } else {
            Self::open_url(pr.web_url.as_deref(), window, cx);
        }
    }

    fn checkout(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(session) = self.own_session() else {
            return;
        };
        let remote = session.remote.clone();
        self.notice = None;
        self.tab.update(cx, |tab, cx| {
            tab.run_op(
                format!("Checking out {branch}"),
                cx,
                move |repo| async move {
                    match repo.checkout_remote_branch(&remote, &branch).await {
                        Ok(_) => Ok(()),
                        // Not pushed (yet): a local branch of that name still does.
                        Err(err) => repo.switch_branch(&branch).await.map_err(|_| err),
                    }
                },
            )
        });
    }

    fn create_branch_dialog(
        &mut self,
        item: &BoardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = logic::branch_name(item);
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(name, window, cx);
            state
        });
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |d, _, _| {
            let (this, ok_input) = (this.clone(), input.clone());
            d.title("Create branch")
                .w(px(440.))
                .child(
                    v_flex().gap_2().child(Input::new(&input)).child(
                        div()
                            .text_xs()
                            .child("Created from the current HEAD and checked out."),
                    ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("board-branch-cancel")
                                .small()
                                .ghost()
                                .label("Cancel")
                                .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("board-branch-create")
                                .small()
                                .primary()
                                .label("Create")
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    let name = ok_input.read(cx).value().trim().to_string();
                                    if name.is_empty() {
                                        return;
                                    }
                                    _ = this.update(cx, |view, cx| view.create_branch(name, cx));
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn create_branch(&mut self, name: String, cx: &mut Context<Self>) {
        self.tab.update(cx, |tab, cx| {
            tab.run_op(
                format!("Creating branch {name}"),
                cx,
                move |repo| async move { repo.create_branch(&name, "HEAD", true).await },
            )
        });
    }
}

// ---- rendering ------------------------------------------------------------------------------

impl Render for BoardsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        match &self.link {
            Link::Resolving => centered("boards-connecting", "Connecting\u{2026}", &palette),
            Link::Empty(empty) => {
                empty_panel(empty, "Boards", "boards", &palette, "boards-empty", cx)
            }
            Link::Ready(_) => self.render_ready(&palette, window, cx),
        }
    }
}

fn centered(id: &'static str, text: impl Into<SharedString>, palette: &Palette) -> AnyElement {
    div()
        .id(id)
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(palette.panel)
        .text_sm()
        .text_color(palette.muted)
        .child(text.into())
        .into_any_element()
}

/// Two-letter host badge text.
fn host_badge(vocab: &HostVocab) -> &'static str {
    match vocab.service {
        "GitHub" => "GH",
        "GitLab" => "GL",
        "Azure DevOps" => "AZ",
        _ => "\u{2022}",
    }
}

/// A host colour word (`GREEN`) or hex as a colour.
fn column_color(color: Option<&str>, palette: &Palette) -> gpui_kit::Hsla {
    let word = color.unwrap_or_default().to_ascii_uppercase();
    match word.as_str() {
        "GREEN" => palette.ok,
        "YELLOW" | "ORANGE" => palette.warn,
        "RED" => palette.error,
        "BLUE" => palette.accent,
        "PURPLE" | "PINK" => palette.lanes[1],
        "" | "GRAY" => palette.muted,
        _ => label_color(color).unwrap_or(palette.muted),
    }
}

fn initials(login: &str) -> String {
    login
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

impl BoardsView {
    fn render_ready(
        &mut self,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let toolbar = self.render_toolbar(palette, cx);
        // An open item's page is its own heading, so the tab header steps aside for it.
        let header = (!self.detail_open).then(|| {
            crate::page_header::page_header(
                "Boards",
                "Plan work across your project boards.",
                None,
                cx,
            )
        });
        let filter_bar = (!self.detail_open).then(|| self.render_filters(palette, cx));
        self.column_scrolls
            .resize_with(self.model.columns.len(), ScrollHandle::new);
        let weak = cx.entity().downgrade();
        self.bar_flags = self.scroll_flags();
        // Scroll ranges only exist after layout, so re-check next frame and re-render only
        // when a scrollbar should flip (settles in one pass, no loop).
        let seen = self.bar_flags.clone();
        let handles: Vec<ScrollHandle> = self
            .column_scrolls
            .iter()
            .cloned()
            .chain([self.board_scroll.clone()])
            .collect();
        let recheck = weak.clone();
        window.on_next_frame(move |_, cx| {
            let last = handles.len() - 1;
            let now: Vec<bool> = handles
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    let o = h.max_offset();
                    if i == last { o.x > px(1.) } else { o.y > px(1.) }
                })
                .collect();
            if now != seen {
                _ = recheck.update(cx, |_, cx| cx.notify());
            }
        });
        let banner = self
            .model
            .error
            .clone()
            .or_else(|| self.boards_error.clone())
            .map(|error| {
                error_banner(&error, palette, "boards-error", move |_, cx| {
                    _ = weak.update(cx, |view, cx| view.refresh(true, cx));
                })
            });
        let notice = self
            .notice
            .clone()
            .filter(|_| !self.detail_open)
            .map(|(error, text)| {
                div()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(if error { palette.error } else { palette.ok })
                    .child(SharedString::from(ui::sanitize_display(&text).into_owned()))
            });
        let body: AnyElement = if self.entries.is_empty() {
            if self.boards_loading {
                centered("boards-loading", "Loading boards\u{2026}", palette)
            } else if self.boards_error.is_some() {
                centered("boards-none", "Could not list the boards.", palette)
            } else {
                centered(
                    "boards-none",
                    format!(
                        "No boards for this repository or its {} yet.",
                        self.vocab.namespace.to_lowercase()
                    ),
                    palette,
                )
            }
        } else if !self.model.loaded {
            centered(
                "board-loading",
                if self.model.error.is_some() {
                    "Could not load the board."
                } else {
                    "Loading board\u{2026}"
                },
                palette,
            )
        } else {
            let detail = self
                .selected_item()
                .filter(|_| self.detail_open)
                .map(|item| self.render_open(item, palette, window, cx));
            match detail {
                Some(detail) => detail,
                None => match self.mode {
                    Mode::Table => self.render_table(palette, cx),
                    Mode::Board | Mode::Mine => self.render_board(palette, cx),
                },
            }
        };
        v_flex()
            .id("boards-view")
            .key_context("Boards")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" if this.detail_open => {
                        this.close_detail(window, cx);
                        cx.stop_propagation();
                    }
                    "up" | "down" if this.detail_open && event.keystroke.modifiers.alt => {
                        let order = this.detail_order();
                        let at = this
                            .selected
                            .as_ref()
                            .and_then(|s| order.iter().position(|i| i == s));
                        let down = event.keystroke.key == "down";
                        if let Some(to) = crate::hosts::logic::step(at, order.len(), down) {
                            this.select(order[to].clone(), window, cx);
                        }
                        cx.stop_propagation();
                    }
                    "enter" if !this.detail_open && this.focus.is_focused(window) => {
                        if let Some(id) = this.selected.clone() {
                            this.select(id, window, cx);
                            cx.stop_propagation();
                        }
                    }
                    _ => {}
                }
            }))
            .on_mouse_up(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.close_detail(window, cx)),
            )
            .size_full()
            .bg(palette.panel)
            .children(header)
            .child(toolbar)
            .children(filter_bar)
            .children(banner)
            .children(notice)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_picker(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let picked = self.picked.and_then(|ix| self.entries.get(ix));
        let label = match picked {
            Some(entry) => {
                let vocab = self
                    .sources
                    .get(entry.source)
                    .map_or(self.vocab, |s| s.session.host.vocab());
                let mut text = format!("{}  {}", host_badge(vocab), entry.board.name);
                if let Some(n) = entry.board.number {
                    text.push_str(&format!(" \u{b7} #{n}"));
                }
                text.push_str(&format!(" \u{b7} {} items", self.model.items.len()));
                text
            }
            None if self.boards_loading => "Loading boards\u{2026}".into(),
            None => "No board".into(),
        };
        let own_repo = self
            .sources
            .first()
            .map(|s| s.repo.clone())
            .unwrap_or_default();
        let namespace = self.vocab.namespace;
        let rows: Vec<(Option<String>, usize, String, bool)> = {
            let mut rows = Vec::new();
            for (section, ix) in logic::sections(&self.entries) {
                let owner = ix
                    .first()
                    .map(|&i| self.entries[i].board.owner.clone())
                    .unwrap_or_default();
                rows.push((
                    Some(logic::section_title(section, &own_repo, namespace, &owner)),
                    0,
                    String::new(),
                    false,
                ));
                for i in ix {
                    let entry = &self.entries[i];
                    let source = self.sources.get(entry.source);
                    let vocab = source.map_or(self.vocab, |s| s.session.host.vocab());
                    let mut text = format!("{}  {}", host_badge(vocab), entry.board.name);
                    if section == Section::OtherRepos
                        && let Some(source) = source
                    {
                        text.push_str(&format!(" \u{b7} {}", source.repo));
                    }
                    if let Some(n) = entry.board.number {
                        text.push_str(&format!(" \u{b7} #{n}"));
                    }
                    if let Some(count) = entry.board.item_count {
                        text.push_str(&format!(" \u{b7} {count} items"));
                    }
                    rows.push((None, i, text, self.picked == Some(i)));
                }
            }
            rows
        };
        let this = cx.entity().downgrade();
        let _ = palette;
        Button::new("board-picker")
            .small()
            .outline()
            .icon(IconName::SquareKanban)
            .label(SharedString::from(ui::sanitize_line(&label).into_owned()))
            .disabled(self.entries.is_empty())
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.max_h(px(420.)).scrollable(true);
                for (heading, ix, text, checked) in &rows {
                    if let Some(heading) = heading {
                        menu =
                            menu.label(SharedString::from(ui::sanitize_line(heading).into_owned()));
                        continue;
                    }
                    let (this, ix) = (this.clone(), *ix);
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(
                            ui::sanitize_line(text).into_owned(),
                        ))
                        .checked(*checked)
                        .on_click(move |_, _, cx| {
                            _ = this.update(cx, |view, cx| view.pick(ix, cx));
                        }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    fn render_toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let picker = self.render_picker(palette, cx);
        let mode_button = |id: &'static str, label: String, mode: Mode| {
            Button::new(id)
                .xsmall()
                .label(SharedString::from(label))
                .with_variant(if self.mode == mode {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_mode(mode, cx)))
        };
        let mine = logic::mine_count(&self.model.items, self.me.as_deref());
        let mine_label = if self.me.is_some() {
            format!("Mine {mine}")
        } else {
            "Mine".into()
        };
        let status = if self.model.loading || self.boards_loading {
            "Loading\u{2026}".to_string()
        } else {
            self.model
                .updated
                .map(|t| {
                    let ago = relative(t, now_secs());
                    if self.model.error.is_some() {
                        format!("Showing data from {ago}")
                    } else {
                        format!("Synced {ago}")
                    }
                })
                .unwrap_or_default()
        };
        let web = self
            .picked
            .and_then(|ix| self.entries.get(ix))
            .and_then(|e| e.board.web_url.clone());
        let service = self.vocab.service;
        h_flex()
            .id("boards-toolbar")
            .flex_none()
            .h(zoomed(38., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .child(picker)
            .child(mode_button("board-mode-board", "Board".into(), Mode::Board))
            .child(mode_button("board-mode-table", "Table".into(), Mode::Table))
            .child(mode_button("board-mode-mine", mine_label, Mode::Mine))
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(status)),
            )
            .when(self.model.next.is_some() && !self.model.wants_more(), |d| {
                d.child(
                    Button::new("board-load-more")
                        .xsmall()
                        .outline()
                        .label("Load more")
                        .loading(self.model.loading)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.load_more(cx))),
                )
            })
            .when_some(web, |d, url| {
                d.child(
                    Button::new("board-open-web")
                        .small()
                        .ghost()
                        .icon(IconName::ExternalLink)
                        .tooltip(SharedString::from(format!("Open on {service}")))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            crate::links::open(&url, Some(window), cx)
                        }),
                )
            })
            .child(
                Button::new("boards-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh")
                    .loading(self.model.loading)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(true, cx))),
            )
            .into_any_element()
    }

    /// One "Name: value ▾" filter menu.
    fn filter_menu(
        &self,
        id: &'static str,
        name: &'static str,
        current: Option<String>,
        values: Vec<String>,
        set: fn(&mut Filters, Option<String>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match &current {
            Some(value) => format!("{name}: {value}"),
            None => format!("{name}: any"),
        };
        let this = cx.entity().downgrade();
        Button::new(id)
            .xsmall()
            .with_variant(if current.is_some() {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Ghost
            })
            .label(SharedString::from(ui::sanitize_line(&label).into_owned()))
            .disabled(values.is_empty() && current.is_none())
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.max_h(px(360.)).scrollable(true);
                let any = this.clone();
                menu = menu.item(
                    PopupMenuItem::new("Any")
                        .checked(current.is_none())
                        .on_click(move |_, _, cx| {
                            _ = any.update(cx, |view, cx| {
                                set(&mut view.filters, None);
                                cx.notify();
                            });
                        }),
                );
                for value in &values {
                    let (this, picked) = (this.clone(), value.clone());
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(
                            ui::sanitize_line(value).into_owned(),
                        ))
                        .checked(current.as_ref() == Some(value))
                        .on_click(move |_, _, cx| {
                            let picked = picked.clone();
                            _ = this.update(cx, |view, cx| {
                                set(&mut view.filters, Some(picked));
                                cx.notify();
                            });
                        }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    fn render_filters(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let items = &self.model.items;
        let own_repo = self.own_session().map(|s| s.repo.full_name());
        let has_repos = items.iter().any(|i| i.repo.is_some());
        let linked = self.filters.repo.is_some();
        let shown = self.visible().len();
        h_flex()
            .id("boards-filters")
            .flex_none()
            .px_2()
            .py_1()
            .gap_1()
            .flex_wrap()
            .border_b_1()
            .border_color(palette.border)
            .child(self.filter_menu(
                "board-filter-assignee",
                "Assignee",
                self.filters.assignee.clone(),
                logic::assignees(items),
                |f, v| f.assignee = v,
                cx,
            ))
            .child(self.filter_menu(
                "board-filter-label",
                "Label",
                self.filters.label.clone(),
                logic::labels(items),
                |f, v| f.label = v,
                cx,
            ))
            .child(self.filter_menu(
                "board-filter-iteration",
                "Iteration",
                self.filters.iteration.clone(),
                logic::iterations(items),
                |f, v| f.iteration = v,
                cx,
            ))
            .when(has_repos && own_repo.is_some(), |d| {
                d.child(
                    Button::new("board-filter-repo")
                        .xsmall()
                        .label("Linked to this repo")
                        .with_variant(if linked {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Ghost
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.filters.repo = if this.filters.repo.is_some() {
                                None
                            } else {
                                own_repo.clone()
                            };
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .w(px(200.))
                    .child(Input::new(&self.text).small().cleanable(true)),
            )
            .when(!self.filters.is_empty(), |d| {
                d.child(
                    Button::new("board-filter-clear")
                        .xsmall()
                        .ghost()
                        .label("Clear filters")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.filters = Filters::default();
                            this.text
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(format!("{shown} of {}", items.len()))),
            )
            .into_any_element()
    }

    /// Whether each column (and, last, the board) really overflows. Judged from the laid-out
    /// cards rather than `max_offset`, which reports a small range for empty or fitting columns
    /// and kept their thumbs on screen.
    fn scroll_flags(&self) -> Vec<bool> {
        let mut flags: Vec<bool> = self
            .column_scrolls
            .iter()
            .map(overflows_y)
            .collect();
        flags.push(overflows_x(&self.board_scroll));
        flags
    }

    fn render_board(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if self.mode == Mode::Mine && self.me.is_none() {
            return centered(
                "board-mine-wait",
                "Finding out who you are\u{2026}",
                palette,
            );
        }
        let visible = self.visible();
        let groups = logic::group(&self.model.columns, &visible);
        let columns: Vec<AnyElement> = self
            .model
            .columns
            .iter()
            .zip(groups)
            .enumerate()
            .map(|(ix, (column, items))| self.render_column(ix, column, items, palette, cx))
            .collect();
        // The scrollbar overlays this wrapper (the real viewport), not the scrolled row, and
        // shows only when the columns overflow; the row's bottom padding keeps it off the cards.
        div()
            .relative()
            .size_full()
            .child(
                h_flex()
                    .id("board-columns")
                    .size_full()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .pb_4()
                    .overflow_x_scroll()
                    .track_scroll(&self.board_scroll)
                    .children(columns),
            )
            .when(self.bar_flags.last().copied().unwrap_or(false), |d| {
                d.horizontal_scrollbar(&self.board_scroll)
            })
            .into_any_element()
    }

    fn render_column(
        &self,
        ix: usize,
        column: &BoardColumn,
        items: Vec<&BoardItem>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let column_id = column.id.clone();
        let total = self.model.count(&column.id);
        let over = column.limit.is_some_and(|l| total as u32 > l);
        let accent = palette.accent;
        let soft = palette.accent_soft;
        let cards: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(row, item)| self.render_card(row, item, palette, cx))
            .collect();
        let scroll = self.column_scrolls.get(ix).cloned().unwrap_or_default();
        v_flex()
            .id(ElementId::NamedInteger("board-column".into(), ix as u64))
            .flex_none()
            .w(px(COLUMN_WIDTH))
            .h_full()
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel2)
            .drag_over::<DragCard>(move |style, _, _, _| style.border_color(accent).bg(soft))
            .on_drop(cx.listener(move |this, drag: &DragCard, _, cx| {
                this.move_item(drag.id.clone(), column_id.clone(), cx)
            }))
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1p5()
                    .gap_1p5()
                    .child(dot(column_color(column.color.as_deref(), palette), 8.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(
                                ui::sanitize_line(&column.name).into_owned(),
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(items.len().to_string())),
                    )
                    .when_some(column.limit, |d, limit| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(if over { palette.error } else { palette.muted })
                                .child(SharedString::from(format!("WIP {total}/{limit}"))),
                        )
                    }),
            )
            .child(
                // The scrollbar overlays this wrapper (the real viewport) and shows only when
                // the cards overflow; the right gutter keeps the thumb off the cards.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id(ElementId::NamedInteger(
                                "board-column-cards".into(),
                                ix as u64,
                            ))
                            .size_full()
                            // Own handle per column: the offset survives the board being swapped out.
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .pl_1p5()
                            .pr_3()
                            .pb_2()
                            .gap_1p5()
                            .children(cards),
                    )
                    .when(self.bar_flags.get(ix).copied().unwrap_or(false), |d| {
                        d.vertical_scrollbar(&scroll)
                    }),
            )
            .into_any_element()
    }

    fn render_card(
        &self,
        row: usize,
        item: &BoardItem,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(item.id.as_str());
        let done = item.state == IssueState::Closed;
        let id = item.id.clone();
        let drag = DragCard {
            id: item.id.clone(),
            title: SharedString::from(ui::sanitize_line(&item.title).into_owned()),
        };
        let number = match (item.kind, item.number) {
            (ItemKind::Draft, _) => "draft".to_string(),
            (_, Some(n)) => format!("#{n}"),
            _ => String::new(),
        };
        let link = logic::linked_branch(item)
            .map(|b| format!("\u{2387} {b}"))
            .or_else(|| {
                item.pull_requests.first().map(|p| {
                    format!(
                        "\u{21c4} {}{}{}",
                        self.vocab.pr_short,
                        p.number,
                        if p.draft { " draft" } else { "" }
                    )
                })
            });
        v_flex()
            .id(ElementId::NamedInteger("board-card".into(), row as u64))
            .w_full()
            .min_w_0()
            // A long column overflows (and scrolls) instead of squashing its cards.
            .flex_shrink_0()
            .overflow_hidden()
            .px_3()
            .py_2p5()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(if selected {
                palette.accent
            } else {
                palette.border
            })
            .bg(palette.panel)
            .cursor_pointer()
            .when(done, |d| d.opacity(0.7))
            .hover(|s| s.bg(palette.hover))
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select(id.clone(), window, cx)
            }))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(number)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(
                                ui::sanitize_line(&item.title).into_owned(),
                            )),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .flex_wrap()
                    .children(
                        item.labels
                            .iter()
                            .take(3)
                            .map(|l| label_pill(l, palette).into_any_element()),
                    )
                    .when_some(item.iteration.clone(), |d, it| {
                        d.child(
                            div()
                                .min_w_0()
                                .max_w_full()
                                .truncate()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(ui::sanitize_line(&it).into_owned())),
                        )
                    })
                    .child(div().flex_1())
                    .children(item.assignees.iter().take(3).map(|a| {
                        div()
                            .flex_none()
                            .size(px(18.))
                            .rounded_full()
                            .bg(palette.lanes[2].opacity(0.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(9.))
                            .text_color(palette.emph)
                            .child(SharedString::from(initials(&a.login)))
                            .into_any_element()
                    })),
            )
            .when_some(link, |d, link| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.lanes[1])
                        .truncate()
                        .child(SharedString::from(ui::sanitize_line(&link).into_owned())),
                )
            })
            .into_any_element()
    }

    fn render_table(&mut self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if self.mode == Mode::Mine && self.me.is_none() {
            return centered(
                "board-mine-wait",
                "Finding out who you are\u{2026}",
                palette,
            );
        }
        let count = self.visible().len();
        let header = h_flex()
            .flex_none()
            .h(zoomed(ROW_HEIGHT, cx))
            .w_full()
            .pl_3()
            .pr_6()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .text_xs()
            .text_color(palette.muted)
            .child(div().flex_none().w(px(56.)).child("#"))
            .child(div().flex_1().min_w_0().child("Title"))
            .child(div().flex_none().w(px(120.)).child("Status"))
            .child(div().flex_none().w(px(120.)).child("Assignee"))
            .child(div().flex_none().w(px(110.)).child("Iteration"))
            .child(div().flex_none().w(px(90.)).child(self.vocab.pr_short))
            .child(div().flex_none().w(px(80.)).child("Updated"));
        if count == 0 {
            return v_flex()
                .size_full()
                .child(header)
                .child(centered(
                    "board-table-empty",
                    "No items match the filters.",
                    palette,
                ))
                .into_any_element();
        }
        v_flex()
            .size_full()
            .child(header)
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "board-table",
                            count,
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                this.render_table_rows(range, cx)
                            }),
                        )
                        .track_scroll(&self.table_scroll)
                        // Keeps the overlay scrollbar off the rows' right-hand badges.
                        .pr_3()
                        .w_full()
                        .flex_1(),
                    )
                    .vertical_scrollbar(&self.table_scroll),
            )
            .into_any_element()
    }

    fn render_table_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let now = now_secs();
        let rows: Vec<BoardItem> = self.visible().into_iter().cloned().collect();
        let height = zoomed(ROW_HEIGHT, cx);
        range
            .filter_map(|ix| {
                let item = rows.get(ix)?;
                let selected = self.selected.as_deref() == Some(item.id.as_str());
                let id = item.id.clone();
                let assignee = item
                    .assignees
                    .iter()
                    .map(|a| a.login.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let pr = item
                    .pull_requests
                    .first()
                    .map(|p| format!("{}{}", self.vocab.pr_short, p.number))
                    .unwrap_or_default();
                let number = item
                    .number
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| "draft".into());
                Some(
                    h_flex()
                        .id(ElementId::NamedInteger("board-row".into(), ix as u64))
                        .w_full()
                        .h(height)
                        .px_3()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .border_b_1()
                        .border_color(palette.border.opacity(0.5))
                        .when(selected, |d| d.bg(palette.accent_soft))
                        .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.select(id.clone(), window, cx)
                        }))
                        .child(
                            div()
                                .flex_none()
                                .w(px(56.))
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(number)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(palette.emph)
                                .child(SharedString::from(
                                    ui::sanitize_line(&item.title).into_owned(),
                                )),
                        )
                        .child(
                            div().flex_none().w(px(120.)).truncate().child(SharedString::from(
                                ui::sanitize_line(logic::column_name(&self.model.columns, item))
                                    .into_owned(),
                            )),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(120.))
                                .truncate()
                                .text_xs()
                                .child(SharedString::from(
                                    ui::sanitize_line(&assignee).into_owned(),
                                )),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(110.))
                                .truncate()
                                .text_xs()
                                .child(SharedString::from(
                                    ui::sanitize_line(
                                        item.iteration.as_deref().unwrap_or_default(),
                                    )
                                    .into_owned(),
                                )),
                        )
                        .child(div().flex_none().w(px(90.)).text_xs().child(SharedString::from(pr)))
                        .child(div().flex_none().w(px(80.)).text_xs().text_color(palette.muted).child(
                            SharedString::from(crate::hosts::logic::ago(&item.updated_at, now)),
                        ))
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// An open item: the compact list on the left (unless folded or the panel is narrow),
    /// its page beside it.
    fn render_open(
        &mut self,
        item: BoardItem,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_side = !self.side_folded && (self.panel_probe.wide() || self.side_pinned);
        let side = show_side.then(|| {
            let count = self.detail_order().len();
            let list = v_flex()
                .relative()
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "board-side-rows",
                        count,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.render_side_rows(range, cx)
                        }),
                    )
                    .track_scroll(&self.side_scroll)
                    .pr_3()
                    .w_full()
                    .flex_1(),
                )
                .vertical_scrollbar(&self.side_scroll)
                .into_any_element();
            page::side_column(
                "Items",
                list,
                palette,
                cx,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.side_folded = true;
                    this.side_pinned = false;
                    cx.notify();
                }),
            )
        });
        let unfold = (!show_side).then(|| {
            page::unfold_button(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.side_folded = false;
                this.side_pinned = !this.panel_probe.wide();
                cx.notify();
            }))
        });
        let page = self.render_detail(item, unfold, palette, window, cx);
        h_flex()
            .relative()
            .size_full()
            .bg(palette.panel)
            .child(self.panel_probe.element())
            .children(side)
            .child(div().flex_1().min_w_0().h_full().child(page))
            .into_any_element()
    }

    fn render_side_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let order = self.detail_order();
        let height = zoomed(page::SIDE_ROW_HEIGHT, cx);
        range
            .filter_map(|ix| {
                let id = order.get(ix)?.clone();
                let item = self.model.items.iter().find(|i| i.id == id)?;
                let open = self.selected.as_deref() == Some(id.as_str());
                let full = SharedString::from(ui::sanitize_line(&item.title).into_owned());
                let tip = full.clone();
                let status = SharedString::from(
                    ui::sanitize_line(logic::column_name(&self.model.columns, item)).into_owned(),
                );
                let number = item.number.map(|n| format!("#{n}"));
                let click_id = id.clone();
                Some(
                    h_flex()
                        .id(ElementId::NamedInteger("board-side-row".into(), ix as u64))
                        .h(height)
                        .w_full()
                        .px_2()
                        .gap_2()
                        .cursor_pointer()
                        .border_l_2()
                        .border_color(if open {
                            palette.accent
                        } else {
                            palette.accent.opacity(0.)
                        })
                        .when(open, |d| d.bg(palette.accent_soft))
                        .when(!open, |d| d.hover(|s| s.bg(palette.hover)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .min_w_0()
                                        .text_xs()
                                        .when_some(number, |d, n| {
                                            d.child(
                                                div()
                                                    .flex_none()
                                                    .text_color(palette.muted)
                                                    .child(SharedString::from(n)),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(palette.emph)
                                                .child(full),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(pill(status, palette.accent, &palette)),
                                ),
                        )
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                .build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.select(click_id.clone(), window, cx)
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_detail(
        &self,
        item: BoardItem,
        unfold: Option<AnyElement>,
        palette: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = item.state == IssueState::Open;
        let noun = match item.kind {
            ItemKind::Issue => self.vocab.issue,
            ItemKind::PullRequest => self.vocab.pull_request,
            ItemKind::Draft => "Draft",
        };
        let kind = item.item_type.clone().unwrap_or_else(|| noun.to_string());
        let number = item.number.map(|n| format!("#{n}")).unwrap_or_default();
        let status = logic::column_name(&self.model.columns, &item).to_string();
        let columns: Vec<(String, String)> = self
            .model
            .columns
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        let item_id = item.id.clone();
        let this = cx.entity().downgrade();
        let current_column = item.column.clone();
        let status_menu = Button::new("board-detail-status")
            .xsmall()
            .outline()
            .label(SharedString::from(ui::sanitize_line(&status).into_owned()))
            .loading(self.acting)
            .disabled(self.acting || columns.is_empty())
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (id, name) in &columns {
                    let (this, item_id, id) = (this.clone(), item_id.clone(), id.clone());
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(
                            ui::sanitize_line(name).into_owned(),
                        ))
                        .checked(id == current_column)
                        .on_click(move |_, _, cx| {
                            let (item_id, id) = (item_id.clone(), id.clone());
                            _ = this.update(cx, |view, cx| view.move_item(item_id, id, cx));
                        }),
                    );
                }
                menu
            });
        let assignees = item
            .assignees
            .iter()
            .map(|a| a.name.clone().unwrap_or_else(|| a.login.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        let branch = logic::linked_branch(&item);
        let prs = item
            .pull_requests
            .iter()
            .map(|p| {
                let state = match (p.state, p.draft) {
                    (PrState::Merged, _) => "merged",
                    (PrState::Closed, _) => "closed",
                    (PrState::Open, true) => "draft",
                    (PrState::Open, false) => "open",
                };
                format!("{}{} {state}", self.vocab.pr_short, p.number)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let kv = |key: &'static str, value: AnyElement| {
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .w(px(76.))
                        .flex_none()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(key),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(palette.fg)
                        .child(value),
                )
        };
        let text =
            |s: String| SharedString::from(ui::sanitize_line(&s).into_owned()).into_any_element();
        let body = item
            .body
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|b| b.chars().take(MAX_BODY).collect::<String>());
        let own = self.own_item(&item);
        let item_web = item.web_url.clone();
        let for_pr = item.clone();
        let for_branch = item.clone();
        let service = self
            .picked
            .and_then(|ix| self.entries.get(ix))
            .and_then(|e| self.sources.get(e.source))
            .map_or(self.vocab.service, |s| s.session.host.vocab().service);
        let back = cx.listener(|this, _: &ClickEvent, window, cx| this.close_detail(window, cx));
        let wide = window.viewport_size().width >= px(WIDE_VIEW);
        let updated = (!item.updated_at.is_empty())
            .then(|| crate::hosts::logic::ago(&item.updated_at, now_secs()));
        let repo = item.repo.clone();
        let actions = h_flex()
            .gap_1p5()
            .flex_wrap()
            .when_some(branch.clone().filter(|_| own), |d, branch| {
                d.child(
                    Button::new("board-checkout")
                        .small()
                        .outline()
                        .icon(IconName::GitBranch)
                        .label("Checkout branch")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.checkout(branch.clone(), cx)
                        })),
                )
            })
            .when(!item.pull_requests.is_empty(), |d| {
                d.child(
                    Button::new("board-open-pr")
                        .small()
                        .outline()
                        .icon(IconName::GitPullRequest)
                        .label(SharedString::from(format!("Open {}", self.vocab.pr_short)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.open_pr(&for_pr, window, cx)
                        })),
                )
            })
            .when(item.kind != ItemKind::PullRequest, |d| {
                d.child(
                    Button::new("board-create-branch")
                        .small()
                        .ghost()
                        .label("Create branch\u{2026}")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.create_branch_dialog(&for_branch, window, cx)
                        })),
                )
            });
        let meta = v_flex()
            .gap_2()
            .when(!wide, |d| d.w_full())
            .when(wide, |d| d.w(px(META_WIDTH)).flex_none())
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel2)
            .child(kv("Status", status_menu.into_any_element()))
            .when_some(item.author.as_ref(), |d, a| {
                d.child(kv("Author", text(a.login.clone())))
            })
            .when_some(repo, |d, repo| d.child(kv("Repository", text(repo))))
            .when_some(updated, |d, at| d.child(kv("Updated", text(at))))
            .when_some(item.iteration.clone(), |d, it| {
                d.child(kv("Iteration", text(it)))
            })
            .when(!item.branches.is_empty(), |d| {
                d.child(kv("Branch", text(item.branches.join(", "))))
            })
            .when(!prs.is_empty(), |d| {
                d.child(kv(self.vocab.pr_short, text(prs)))
            })
            .when(self.detail_loading, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child("Loading links\u{2026}"),
                )
            })
            .child(actions)
            .children(self.notice.clone().map(|(error, text)| {
                div()
                    .text_xs()
                    .text_color(if error { palette.error } else { palette.ok })
                    .child(SharedString::from(ui::sanitize_display(&text).into_owned()))
            }));
        let format = if service == "Azure DevOps" {
            ::hosts::BodyFormat::Html
        } else {
            ::hosts::BodyFormat::Markdown
        };
        let description = v_flex()
            .flex_1()
            .min_w_0()
            .max_w(px(PROSE_WIDTH))
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child("Description"),
            )
            .child(match &body {
                Some(text) => crate::hosts::conversation::body_view(
                    ElementId::Name(format!("board-desc-{}", item.id).into()),
                    text,
                    format,
                ),
                None => div()
                    .text_sm()
                    .text_color(palette.muted)
                    .child("No description.")
                    .into_any_element(),
            });
        let header = h_flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .children(unfold)
            .child(
                Button::new("board-detail-back")
                    .small()
                    .outline()
                    .icon(IconName::ArrowLeft)
                    .label("Back")
                    .tooltip("Back to the board (Esc)")
                    .on_click(back),
            )
            .child(pill(
                SharedString::from(kind),
                if open { palette.ok } else { palette.lanes[1] },
                palette,
            ))
            .child(
                div()
                    .text_color(palette.muted)
                    .child(SharedString::from(number)),
            )
            .child(
                ui::typography::page_title(ui::sanitize_line(&item.title).into_owned(), cx)
                    .min_w_0(),
            )
            .child(pill(
                SharedString::from(ui::sanitize_line(&status).into_owned()),
                palette.accent,
                palette,
            ))
            .when(!assignees.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(SharedString::from(format!(
                            "assigned to {}",
                            ui::sanitize_line(&assignees)
                        ))),
                )
            })
            .children(
                item.labels
                    .iter()
                    .map(|l| label_pill(l, palette).into_any_element()),
            )
            .child(div().flex_1())
            .when_some(item_web, |d, url| {
                d.child(
                    Button::new("board-detail-web")
                        .small()
                        .ghost()
                        .icon(IconName::ExternalLink)
                        .label(SharedString::from(format!("Open on {service}")))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            crate::links::open(&url, Some(window), cx)
                        }),
                )
            });
        let layout = if wide {
            h_flex()
                .w_full()
                .max_w(px(PROSE_WIDTH + META_WIDTH + 24.))
                .gap_6()
                .items_start()
                .child(description)
                .child(meta)
        } else {
            v_flex()
                .w_full()
                .max_w(px(PROSE_WIDTH))
                .gap_4()
                .child(meta)
                .child(description)
        };
        v_flex()
            .id("board-detail")
            .size_full()
            .bg(palette.panel)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("board-detail-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .justify_center()
                                    .p_4()
                                    .child(layout),
                            ),
                    )
                    .vertical_scrollbar(&self.detail_scroll),
            )
            .into_any_element()
    }
}

/// The laid-out children of `handle` span more than its viewport's height (an empty list never
/// does). Reads the last frame's layout, like every scroll handle query.
fn overflows_y(handle: &ScrollHandle) -> bool {
    let count = handle.children_count();
    let (Some(first), Some(last)) = (
        handle.bounds_for_item(0),
        count.checked_sub(1).and_then(|ix| handle.bounds_for_item(ix)),
    ) else {
        return false;
    };
    last.bottom() - first.top() > handle.bounds().size.height - px(12.)
}

/// [`overflows_y`] across: the columns span wider than the board's viewport.
fn overflows_x(handle: &ScrollHandle) -> bool {
    let count = handle.children_count();
    let (Some(first), Some(last)) = (
        handle.bounds_for_item(0),
        count.checked_sub(1).and_then(|ix| handle.bounds_for_item(ix)),
    ) else {
        return false;
    };
    last.right() - first.left() > handle.bounds().size.width - px(12.)
}
