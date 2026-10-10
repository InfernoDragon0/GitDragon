//! The pull request (merge request) view of one repository tab: a filterable, paged list with
//! state chips, labels and CI dots. Opening one (click, Enter) replaces the list with its page
//! (description, conversation, reviews, inline comments, checks, branches, check out) beside a
//! compact copy of the list; Back (Esc, mouse back) returns to the full list as it was. Plus
//! the create dialog. Data comes only from the `GitHost` trait; the words from its
//! `HostVocab`.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use ::hosts::{
    CiState, CiStatus, Feature, HostVocab, ListQuery, PollSchedule, PullRequest, StateFilter,
    Thread, WebTarget,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::Escape as InputEscape;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    NavigationDirection, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use ui::{Palette, zoomed};

use super::changes::{ChangesRequest, ChangesView};
use super::conversation::ConversationView;
use super::logic::{self, CheckoutPlan, Chip, ListModel};
use super::new_pr::{NewPrEvent, NewPrForm};
use super::page::{self, DetailPage, SideRow, WidthProbe};
use super::{Link, dot, empty_panel, error_banner, label_pill, pill, resolve};
use crate::actions::{ClearDiffSelection, PageBack};
use crate::hosted::HostedRepo;
use crate::repo_tab::RepoTab;

const ROW_HEIGHT: f32 = 50.;
/// The refresh timer wakes this often to see whether a refresh is due.
const TICK: Duration = Duration::from_secs(15);
/// CI lookups running at once.
const MAX_CI: usize = 4;
const PAGE_SIZE: u8 = 30;
/// Fetched pull requests (`refs/gitdragon/pr/*`) whose head is older than this are dropped
/// when the view first sees the repository.
const PR_REFS_MAX_AGE: Duration = Duration::from_secs(14 * 86_400);

/// The reading column of an open page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageTab {
    Conversation,
    Changes,
}

/// What is known of one commit's CI.
struct CiEntry {
    status: Option<CiStatus>,
    /// The list generation it was asked for; older ones are asked again when seen.
    epoch: u64,
    loading: bool,
}

pub struct PrsView {
    tab: Entity<RepoTab>,
    link: Link,
    vocab: &'static HostVocab,
    /// The tab's `(name, url)` remotes the link was made for; a change reconnects.
    remotes_seen: Option<Vec<(String, Option<String>)>>,
    resolve_task: Option<Task<()>>,
    filter: StateFilter,
    mine: bool,
    /// The signed-in user's login, fetched when "Mine" is first used.
    me: Option<String>,
    me_task: Option<Task<()>>,
    list: ListModel<PullRequest>,
    list_task: Option<Task<()>>,
    /// The highlighted row (kept while a page is open, so Back lands on it).
    selected: Option<u64>,
    /// The pull request whose page replaces the list.
    open: Option<u64>,
    /// Open this one when the next load brings it (just created, revealed).
    select_after: Option<u64>,
    conversation: Entity<ConversationView>,
    /// The Changes tab (kept across pages, like the tab choice).
    changes: Entity<ChangesView>,
    page_tab: PageTab,
    /// Old fetched pull requests were pruned for this view's repository.
    pruned: bool,
    focus: FocusHandle,
    /// The compact list beside an open page.
    side_scroll: UniformListScrollHandle,
    /// The user folded the side list (kept for the session).
    side_folded: bool,
    /// The user unfolded it on a narrow panel.
    side_pinned: bool,
    panel_probe: WidthProbe,
    page_probe: WidthProbe,
    ci: HashMap<String, CiEntry>,
    ci_epoch: u64,
    ci_inflight: usize,
    schedule: PollSchedule,
    active: bool,
    /// A message about the last action (checkout refused, ...).
    notice: Option<SharedString>,
    scroll: UniformListScrollHandle,
    dialog_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
    _timer: Option<Task<()>>,
}

impl PrsView {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut schedule = PollSchedule::host_refresh(&crate::prefs::get(cx).poll);
        schedule.set_focused(window.is_window_active(), Instant::now());
        let subs = vec![
            cx.observe(&tab, |this, _, cx| this.on_tab_changed(cx)),
            // Settings > Refresh: a new interval takes effect at once.
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
            filter: StateFilter::Open,
            mine: false,
            me: None,
            me_task: None,
            list: ListModel::default(),
            list_task: None,
            selected: None,
            open: None,
            select_after: None,
            conversation: cx.new(|_| ConversationView::new()),
            changes: cx.new(ChangesView::new),
            page_tab: PageTab::Conversation,
            pruned: false,
            focus: cx.focus_handle(),
            side_scroll: UniformListScrollHandle::new(),
            side_folded: false,
            side_pinned: false,
            panel_probe: WidthProbe::new(page::WIDE_PANEL),
            page_probe: WidthProbe::new(page::WIDE_PAGE),
            ci: HashMap::new(),
            ci_epoch: 0,
            ci_inflight: 0,
            schedule,
            active: true,
            notice: None,
            scroll: UniformListScrollHandle::new(),
            dialog_sub: None,
            _subs: subs,
            _timer: Some(timer),
        };
        view.on_tab_changed(cx);
        view
    }

    /// The panel title in the host's words (`Merge requests`).
    pub fn title(&self) -> &'static str {
        self.vocab.pull_requests
    }

    // ---- connection -------------------------------------------------------------------

    fn on_tab_changed(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab.read(cx);
        // Wait for the first refresh: until then the remotes are simply not known yet.
        if tab.generation == 0 {
            return;
        }
        if !self.pruned
            && let Some(repo) = tab.repo.clone()
        {
            self.pruned = true;
            // Best effort housekeeping, off the UI thread.
            cx.background_executor()
                .spawn(async move { _ = repo.prune_pr_refs(PR_REFS_MAX_AGE).await })
                .detach();
        }
        let tab = self.tab.read(cx);
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
        self.list.reset();
        self.list_task = None;
        self.ci.clear();
        self.ci_inflight = 0;
        self.selected = None;
        self.open = None;
        self.changes.update(cx, |c, _| c.hide());
        self.me = None;
        self.me_task = None;
        self.notice = None;
        self.resolve_task = Some(resolve(
            &self.tab,
            Feature::PullRequests,
            cx,
            |this, link, cx| {
                this.resolve_task = None;
                if let Link::Ready(session) = &link {
                    this.vocab = session.host.vocab();
                }
                this.link = link;
                if matches!(this.link, Link::Ready(_)) {
                    this.refresh(false, cx);
                }
                cx.notify();
            },
        ));
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
            // The user may have signed in meanwhile.
            Link::Empty(_) => self.reconnect(cx),
            Link::Ready(_) => self.tick(cx),
            Link::Resolving => {}
        }
    }

    fn session(&self) -> Option<&crate::hosted::Session> {
        match &self.link {
            Link::Ready(session) => Some(session),
            _ => None,
        }
    }

    // ---- loading ----------------------------------------------------------------------

    /// The timer: refresh when due, only while visible.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.list.loading {
            return;
        }
        let Some(session) = self.session() else {
            return;
        };
        let blocked = session.host.rate_limit().blocked_until;
        if self.schedule.is_due(Instant::now(), blocked) {
            self.refresh(false, cx);
        }
    }

    /// Reloads the first page. `manual` also drops the host's cached answers.
    pub fn refresh(&mut self, manual: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        if manual {
            session.host.invalidate(Some(&session.repo));
            if self.open.is_some() {
                self.conversation.update(cx, |c, cx| c.reload(cx));
                self.changes.update(cx, |c, cx| c.reload(cx));
            }
        }
        self.ci_epoch += 1;
        self.load(1, false, cx);
    }

    fn load(&mut self, page: u32, append: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let (host, repo) = (session.host.clone(), session.repo.clone());
        let query = ListQuery {
            state: self.filter,
            page,
            per_page: PAGE_SIZE,
        };
        let req = self.list.begin();
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let result = host.pull_requests(&repo, &query).await;
            _ = this.update(cx, |this, cx| this.loaded(req, result, append, cx));
        }));
        cx.notify();
    }

    fn loaded(
        &mut self,
        req: u64,
        result: Result<::hosts::Page<PullRequest>, ::hosts::HostError>,
        append: bool,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = &result
            && self
                .session()
                .is_some_and(|s| s.retry_with_other_account(err))
        {
            self.reconnect(cx);
            return;
        }
        let ok = result.is_ok();
        if !self
            .list
            .finish(req, result, append, |p| p.number, logic::now_secs())
        {
            return;
        }
        self.schedule.record(Instant::now(), ok);
        if ok
            && let Some(number) = self.select_after.take()
            && self.list.items.iter().any(|p| p.number == number)
        {
            self.selected = Some(number);
            self.open = Some(number);
        }
        cx.notify();
    }

    /// Selects pull request `number` (the Boards tab's "Open PR"), now when it is listed, else
    /// when the next load brings it.
    pub fn reveal(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.list.items.iter().any(|p| p.number == number) {
            self.selected = Some(number);
            self.open = Some(number);
            cx.notify();
        } else {
            self.select_after = Some(number);
            self.refresh(false, cx);
        }
    }

    fn set_filter(&mut self, filter: StateFilter, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.list.reset();
        self.selected = None;
        self.open = None;
        self.load(1, false, cx);
    }

    fn toggle_mine(&mut self, cx: &mut Context<Self>) {
        self.mine = !self.mine;
        if self.mine && self.me.is_none() && self.me_task.is_none() {
            let Some(session) = self.session() else {
                return;
            };
            let host = session.host.clone();
            self.me_task = Some(cx.spawn(async move |this, cx| {
                let who = host.whoami().await;
                _ = this.update(cx, |this, cx| {
                    this.me_task = None;
                    match who {
                        Ok(person) => this.me = Some(person.login),
                        Err(err) => {
                            this.mine = false;
                            this.list.error = Some(logic::ListError::of(&err));
                        }
                    }
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    /// The rows on screen: the loaded ones, narrowed to the user's own with "Mine".
    fn visible(&self) -> Vec<&PullRequest> {
        let me = self.mine.then_some(self.me.as_deref()).flatten();
        logic::visible_prs(&self.list.items, me)
    }

    // ---- CI ---------------------------------------------------------------------------

    /// Asks for the CI state of `sha` unless it is known for this generation of the list or
    /// being asked right now.
    fn request_ci(&mut self, sha: &str, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        if sha.is_empty()
            || !session.host.supports(Feature::CiStatus)
            || self.ci_inflight >= MAX_CI
            || session
                .host
                .rate_limit()
                .blocked(std::time::SystemTime::now())
                .is_some()
        {
            return;
        }
        if let Some(entry) = self.ci.get(sha)
            && (entry.loading || entry.epoch == self.ci_epoch)
        {
            return;
        }
        let (host, repo) = (session.host.clone(), session.repo.clone());
        let (key, epoch) = (sha.to_string(), self.ci_epoch);
        self.ci_inflight += 1;
        self.ci
            .entry(key.clone())
            .and_modify(|e| e.loading = true)
            .or_insert(CiEntry {
                status: None,
                epoch,
                loading: true,
            });
        cx.spawn(async move |this, cx| {
            let result = host.ci_status(&repo, &key).await;
            _ = this.update(cx, |this, cx| {
                this.ci_inflight = this.ci_inflight.saturating_sub(1);
                if let Some(entry) = this.ci.get_mut(&key) {
                    entry.loading = false;
                    entry.epoch = epoch;
                    // A failed lookup keeps the old answer and is not retried this round.
                    if let Ok(status) = result {
                        entry.status = Some(status);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn ci_state(&self, sha: &str) -> Option<CiState> {
        self.ci.get(sha)?.status.as_ref().map(|s| s.state)
    }

    // ---- actions ----------------------------------------------------------------------

    fn open_pr(&self) -> Option<PullRequest> {
        let number = self.open?;
        self.list.items.iter().find(|p| p.number == number).cloned()
    }

    /// Opens `number`'s page (from the list, the side list or a key). Both lists follow, so
    /// Back lands on the last one opened.
    fn open_page(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.open != Some(number) {
            self.notice = None;
        }
        self.selected = Some(number);
        self.open = Some(number);
        if let Some(ix) = self.visible().iter().position(|p| p.number == number) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            self.side_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Back to the list: the selection and the list's scroll offset are as they were (the
    /// scroll handle outlives the page).
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.take().is_none() {
            return;
        }
        self.conversation.update(cx, |c, _| c.hide());
        self.changes.update(cx, |c, _| c.hide());
        self.notice = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// On the list: Up / Down move the selection, Enter opens it. On a page: Alt+Up / Alt+Down open the previous / next item.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let rows: Vec<u64> = self.visible().iter().map(|p| p.number).collect();
        let at = self
            .selected
            .and_then(|n| rows.iter().position(|r| *r == n));
        if self.open.is_some() {
            match key {
                "up" | "down" if event.keystroke.modifiers.alt => {
                    if let Some(to) = logic::step(at, rows.len(), key == "down") {
                        self.open_page(rows[to], window, cx);
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        match key {
            "up" | "down" => {
                if let Some(to) = logic::step(at, rows.len(), key == "down") {
                    self.selected = Some(rows[to]);
                    self.scroll.scroll_to_item(to, ScrollStrategy::Nearest);
                    cx.notify();
                }
            }
            "enter" => match at {
                Some(ix) => self.open_page(rows[ix], window, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Fetches the pull request's branch and switches to it (a tab operation, so it shows in
    /// the operation strip and refreshes the graph).
    fn checkout(&mut self, pr: &PullRequest, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let remote = session.remote.clone();
        let plan = logic::checkout_plan(pr, session.host.pull_head_ref(pr.number));
        if let CheckoutPlan::Unavailable(message) = &plan {
            self.notice = Some(message.clone().into());
            cx.notify();
            return;
        }
        self.notice = None;
        let number = pr.number;
        let label = format!("Checking out {}{number}", self.vocab.pr_short);
        self.tab.update(cx, |tab, cx| {
            tab.run_op(label, cx, move |repo| async move {
                match plan {
                    CheckoutPlan::Branch { branch, fallback } => {
                        match repo.checkout_remote_branch(&remote, &branch).await {
                            Ok(_) => Ok(()),
                            // Deleted source branch, or a fork the host did not report.
                            Err(err) => match fallback {
                                Some(head_ref) => repo
                                    .checkout_pull_ref(&remote, &head_ref, number)
                                    .await
                                    .map(drop)
                                    .map_err(|_| err),
                                None => Err(err),
                            },
                        }
                    }
                    CheckoutPlan::HeadRef { head_ref } => repo
                        .checkout_pull_ref(&remote, &head_ref, number)
                        .await
                        .map(drop),
                    CheckoutPlan::Unavailable(_) => Ok(()),
                }
            })
        });
    }

    fn open_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let (tab, vocab) = (self.tab.clone(), self.vocab);
        let form = cx.new(|cx| NewPrForm::new(tab, session, vocab, window, cx));
        self.dialog_sub =
            Some(
                cx.subscribe_in(&form, window, |this, _, event: &NewPrEvent, window, cx| {
                    if let NewPrEvent::Created(pr) = event {
                        this.created(pr, cx);
                    }
                    window.close_dialog(cx);
                    this.dialog_sub = None;
                }),
            );
        let title = format!("New {}", vocab.pull_request.to_lowercase());
        window.open_dialog(cx, move |d, _, _| {
            d.title(title.clone()).w(px(560.)).child(form.clone())
        });
    }

    /// A pull request was created: show the Open list and select it once it arrives.
    fn created(&mut self, pr: &PullRequest, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.host.invalidate(Some(&session.repo));
        }
        self.select_after = Some(pr.number);
        self.filter = StateFilter::Open;
        self.mine = false;
        self.list.reset();
        self.refresh(false, cx);
    }
}

// ---- rendering ------------------------------------------------------------------------------

impl Render for PrsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        match &self.link {
            Link::Resolving => centered("pr-connecting", "Connecting\u{2026}", &palette),
            Link::Empty(empty) => empty_panel(
                empty,
                self.vocab.pull_requests,
                &self.vocab.pull_requests.to_lowercase(),
                &palette,
                "pr-empty",
                cx,
            ),
            Link::Ready(_) => self.render_ready(&palette, cx),
        }
    }
}

fn centered(id: &'static str, text: &'static str, palette: &Palette) -> AnyElement {
    div()
        .id(id)
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(palette.panel)
        .text_sm()
        .text_color(palette.muted)
        .child(text)
        .into_any_element()
}

fn ci_color(state: Option<CiState>, palette: &Palette) -> gpui_kit::Hsla {
    match state {
        Some(CiState::Success) => palette.ok,
        Some(CiState::Pending) => palette.warn,
        Some(CiState::Failure) => palette.error,
        Some(CiState::None) | None => palette.border_strong,
    }
}

fn chip_color(chip: Chip, palette: &Palette) -> gpui_kit::Hsla {
    match chip {
        Chip::Open => palette.ok,
        Chip::Draft => palette.muted,
        Chip::Merged => palette.lanes[1],
        Chip::Closed => palette.error,
    }
}

fn ci_word(state: CiState) -> &'static str {
    match state {
        CiState::None => "No checks",
        CiState::Success => "Passing",
        CiState::Pending => "Running",
        CiState::Failure => "Failing",
    }
}

impl PrsView {
    fn render_ready(&mut self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let content = match self.open_pr() {
            Some(pr) => self.render_open(pr, palette, cx),
            None => self.render_list(palette, cx),
        };
        div()
            .id("prs-root")
            .key_context("PrsView")
            .track_focus(&self.focus)
            .size_full()
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.on_key(event, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &PageBack, window, cx| {
                if this.open.is_some() {
                    this.back(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            // A diff with nothing selected lets Escape through (see `diff_select`).
            .on_action(cx.listener(|this, _: &ClearDiffSelection, window, cx| {
                if this.open.is_some() {
                    this.back(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            // A text input that has nothing to dismiss passes Escape on: it blurs.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                if this.open.is_some() {
                    this.focus.focus(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.back(window, cx)),
            )
            .child(content)
            .into_any_element()
    }

    fn render_list(&mut self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let supports_create = self
            .session()
            .is_some_and(|s| s.host.supports(Feature::CreatePullRequest));
        let header = self.render_header(palette, supports_create, cx);
        let toolbar = self.render_toolbar(palette, cx);
        let banner = self.list.error.clone().map(|error| {
            error_banner(&error, palette, "pr-error", {
                let this = cx.entity().downgrade();
                move |_, cx| {
                    _ = this.update(cx, |view, cx| view.refresh(true, cx));
                }
            })
        });
        let count = self.visible().len() + usize::from(self.list.next.is_some());
        let list: AnyElement = if self.visible().is_empty() && self.list.next.is_none() {
            let text = if self.list.loading || !self.list.loaded {
                if self.list.error.is_some() {
                    "Could not load.".to_string()
                } else {
                    "Loading\u{2026}".to_string()
                }
            } else if self.mine && self.me.is_some() {
                format!(
                    "None of your own {} here.",
                    self.vocab.pull_requests.to_lowercase()
                )
            } else {
                format!("No {} here.", self.vocab.pull_requests.to_lowercase())
            };
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(palette.muted)
                .child(SharedString::from(text))
                .into_any_element()
        } else {
            v_flex()
                .relative()
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "pr-rows",
                        count,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.render_rows(range, cx)
                        }),
                    )
                    .track_scroll(&self.scroll)
                    // Keeps the overlay scrollbar off the rows' right-hand badges.
                    .pr_3()
                    .w_full()
                    .flex_1(),
                )
                .vertical_scrollbar(&self.scroll)
                .into_any_element()
        };
        v_flex()
            .id("prs-view")
            .size_full()
            .bg(palette.panel)
            .child(header)
            .child(toolbar)
            .children(banner)
            .child(v_flex().flex_1().min_h_0().w_full().child(list))
            .into_any_element()
    }

    /// An open pull request: the compact list on the left (unless folded or the panel is
    /// narrow), its page beside it.
    fn render_open(
        &mut self,
        pr: PullRequest,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_side = !self.side_folded && (self.panel_probe.wide() || self.side_pinned);
        let side = show_side.then(|| {
            let count = self.visible().len();
            let list = v_flex()
                .relative()
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "pr-side-rows",
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
                self.vocab.pull_requests,
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
        let page = self.render_detail(pr, unfold, palette, cx);
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
        let rows: Vec<PullRequest> = self.visible().into_iter().cloned().collect();
        range
            .filter_map(|ix| {
                let pr = rows.get(ix)?;
                let chip = logic::chip(pr);
                let number = pr.number;
                let row = SideRow {
                    number,
                    title: pr.title.clone(),
                    author: pr
                        .author
                        .as_ref()
                        .map_or("unknown".into(), |a| a.login.clone()),
                    state: chip.label().into(),
                    color: chip_color(chip, &palette),
                };
                Some(page::side_row(
                    ix,
                    row,
                    self.open == Some(number),
                    &palette,
                    cx,
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_page(number, window, cx)
                    }),
                ))
            })
            .collect()
    }

    /// The page header: title and subtitle, with the status, Refresh and New buttons at the end.
    fn render_header(
        &self,
        palette: &Palette,
        supports_create: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = if self.list.loading {
            "Loading\u{2026}".to_string()
        } else {
            self.list
                .updated
                .map(|t| {
                    let ago = logic::relative(t, logic::now_secs());
                    if self.list.error.is_some() {
                        format!("Showing data from {ago}")
                    } else {
                        format!("Updated {ago}")
                    }
                })
                .unwrap_or_default()
        };
        let shown = self.visible().len();
        let new_label = format!("New {}", self.vocab.pull_request.to_lowercase());
        let subtitle = match self.session() {
            Some(session) => format!(
                "Review and check out {} for {}.",
                self.vocab.pull_requests.to_lowercase(),
                super::repo_slug(&session.repo)
            ),
            None => format!("Sign in to see {}.", self.vocab.pull_requests.to_lowercase()),
        };
        let right = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(if self.list.loaded {
                        format!("{shown} shown \u{b7} {status}")
                    } else {
                        status
                    })),
            )
            .child(
                Button::new("pr-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh")
                    .loading(self.list.loading)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(true, cx))),
            )
            .when(supports_create, |d| {
                d.child(
                    Button::new("pr-new")
                        .small()
                        .primary()
                        .icon(IconName::Plus)
                        .label(SharedString::from(new_label))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_new_dialog(window, cx)
                        })),
                )
            })
            .into_any_element();
        crate::page_header::page_header(self.vocab.pull_requests, subtitle, Some(right), cx)
            .into_any_element()
    }

    /// The filter row under the header.
    fn render_toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let filter_button = |id: &'static str, label: &'static str, value: StateFilter| {
            Button::new(id)
                .xsmall()
                .label(label)
                .with_variant(if self.filter == value {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                })
                .on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.set_filter(value, cx)),
                )
        };
        h_flex()
            .id("pr-toolbar")
            .flex_none()
            .h(zoomed(38., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .child(filter_button("pr-filter-open", "Open", StateFilter::Open))
            .child(filter_button(
                "pr-filter-closed",
                "Closed",
                StateFilter::Closed,
            ))
            .child(filter_button("pr-filter-all", "All", StateFilter::All))
            .child(div().w_px().h(zoomed(18., cx)).mx_1().bg(palette.border))
            .child(
                Button::new("pr-filter-mine")
                    .xsmall()
                    .label("Mine")
                    .with_variant(if self.mine {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .loading(self.mine && self.me.is_none() && self.me_task.is_some())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_mine(cx))),
            )
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let now = logic::now_secs();
        let rows: Vec<PullRequest> = self.visible().into_iter().cloned().collect();
        for pr in rows.iter().skip(range.start).take(range.len()) {
            if pr.state == ::hosts::PrState::Open {
                self.request_ci(&pr.head_sha, cx);
            }
        }
        let height = zoomed(ROW_HEIGHT, cx);
        range
            .map(|ix| match rows.get(ix) {
                Some(pr) => self.render_row(ix, pr, height, now, &palette, cx),
                None => self.render_more(height, &palette, cx),
            })
            .collect()
    }

    fn render_more(
        &self,
        height: gpui_kit::Pixels,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let next = self.list.next;
        h_flex()
            .id("pr-more")
            .h(height)
            .justify_center()
            .child(
                Button::new("pr-load-more")
                    .small()
                    .outline()
                    .label("Load more")
                    .loading(self.list.loading)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(page) = next {
                            this.load(page, true, cx);
                        }
                    })),
            )
            .text_color(palette.muted)
            .into_any_element()
    }

    fn render_row(
        &self,
        ix: usize,
        pr: &PullRequest,
        height: gpui_kit::Pixels,
        now: i64,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(pr.number);
        let chip = logic::chip(pr);
        let author = pr.author.as_ref().map_or("unknown", |a| a.login.as_str());
        let branches = if pr.source_repo.is_some() {
            format!("{} (fork) \u{2192} {}", pr.source_branch, pr.target_branch)
        } else {
            format!("{} \u{2192} {}", pr.source_branch, pr.target_branch)
        };
        let number = pr.number;
        let ci = if pr.state == ::hosts::PrState::Open {
            self.ci_state(&pr.head_sha)
        } else {
            None
        };
        h_flex()
            .id(ElementId::NamedInteger("pr-row".into(), ix as u64))
            .h(height)
            .w_full()
            .px_3()
            .gap_2()
            .cursor_pointer()
            .border_b_1()
            .border_color(palette.border.opacity(0.5))
            .when(selected, |d| d.bg(palette.accent_soft))
            .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
            .child(dot(ci_color(ci, palette), 8.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .child(SharedString::from(format!("#{number}"))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(palette.emph)
                                    .child(SharedString::from(
                                        ui::sanitize_line(&pr.title).into_owned(),
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .min_w_0()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(
                                div().flex_none().child(SharedString::from(
                                    ui::sanitize_line(author).into_owned(),
                                )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(
                                        ui::sanitize_line(&branches).into_owned(),
                                    )),
                            )
                            .children(
                                pr.labels
                                    .iter()
                                    .take(3)
                                    .map(|l| label_pill(l, palette).into_any_element()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .child(SharedString::from(logic::ago(&pr.updated_at, now))),
                            ),
                    ),
            )
            .child(pill(chip.label(), chip_color(chip, palette), palette))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_page(number, window, cx)
            }))
            .into_any_element()
    }

    /// "Conversation | Changes (N files)" over the reading column.
    fn render_page_tabs(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let changes = match self.changes.read(cx).file_count() {
            Some(1) => "Changes (1 file)".to_string(),
            Some(n) => format!("Changes ({n} files)"),
            None => "Changes".to_string(),
        };
        let tab = |id: &'static str, label: String, value: PageTab| {
            let on = self.page_tab == value;
            div()
                .id(id)
                .pb_1p5()
                .text_sm()
                .cursor_pointer()
                .border_b_2()
                .when(on, |d| {
                    d.border_color(palette.accent).text_color(palette.emph)
                })
                .when(!on, |d| {
                    d.border_color(gpui_kit::transparent_black())
                        .text_color(palette.muted)
                        .hover(|s| s.text_color(palette.fg))
                })
                .child(SharedString::from(label))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.page_tab = value;
                    cx.notify();
                }))
        };
        h_flex()
            .w_full()
            .gap_4()
            .border_b_1()
            .border_color(palette.border)
            .child(tab(
                "pr-tab-conversation",
                "Conversation".to_string(),
                PageTab::Conversation,
            ))
            .child(tab("pr-tab-changes", changes, PageTab::Changes))
            .into_any_element()
    }

    fn render_detail(
        &mut self,
        pr: PullRequest,
        unfold: Option<AnyElement>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.request_ci(&pr.head_sha, cx);
        let number = pr.number;
        if let Some(session) = self.session().cloned() {
            super::conversation::seed_head(
                Thread::PullRequest(number),
                pr.author.iter().cloned().collect(),
                cx,
            );
            self.conversation.update(cx, |c, cx| {
                c.show(&session, Thread::PullRequest(number), cx)
            });
            // Fetched only once the Changes tab is shown; another pull request stops the old fetch.
            if self.page_tab == PageTab::Changes {
                let repo = self.tab.read(cx).repo.clone();
                let short = self.vocab.pr_short;
                self.changes.update(cx, |c, cx| {
                    c.show(
                        ChangesRequest {
                            repo,
                            session: &session,
                            pr: &pr,
                            short,
                        },
                        cx,
                    )
                });
            } else {
                self.changes
                    .update(cx, |c, _| c.retarget(number, &pr.head_sha));
            }
        }
        let now = logic::now_secs();
        let chip = logic::chip(&pr);
        let busy = self.tab.read(cx).is_busy();
        let author = pr
            .author
            .as_ref()
            .map_or("unknown".to_string(), |a| a.login.clone());
        let branches = match &pr.source_repo {
            Some(fork) => format!("{fork}:{} \u{2192} {}", pr.source_branch, pr.target_branch),
            None => format!("{} \u{2192} {}", pr.source_branch, pr.target_branch),
        };
        let can_checkout =
            pr.state == ::hosts::PrState::Open || pr.state == ::hosts::PrState::Merged;
        let entry = self.ci.get(&pr.head_sha);
        let ci_overall = entry.and_then(|e| e.status.as_ref()).map(|s| s.state);
        let checks: Vec<::hosts::Check> = entry
            .and_then(|e| e.status.as_ref())
            .map(|s| s.checks.clone())
            .unwrap_or_default();
        let ci_loading = entry.is_some_and(|e| e.loading && e.status.is_none());
        let supports_ci = self
            .session()
            .is_some_and(|s| s.host.supports(Feature::CiStatus));
        let web_url = self
            .session()
            .and_then(|s| s.host.web_url(&s.repo, &WebTarget::PullRequest(number)))
            .or_else(|| Some(pr.web_url.clone()).filter(|u| !u.is_empty()));
        let pr_for_checkout = pr.clone();

        let mut chips = vec![
            pill(chip.label(), chip_color(chip, palette), palette).into_any_element(),
            div()
                .text_xs()
                .text_color(palette.muted)
                .child(SharedString::from(format!(
                    "{} opened {} \u{b7} updated {}",
                    ui::sanitize_line(&author),
                    logic::ago(&pr.created_at, now),
                    logic::ago(&pr.updated_at, now)
                )))
                .into_any_element(),
        ];
        chips.extend(
            pr.labels
                .iter()
                .map(|l| label_pill(l, palette).into_any_element()),
        );

        let checks_block = supports_ci.then(|| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(self.vocab.ci),
                        )
                        .child(dot(ci_color(ci_overall, palette), 8.))
                        .child(div().text_xs().text_color(palette.muted).child(
                            SharedString::from(match (ci_overall, ci_loading) {
                                (Some(state), _) => ci_word(state).to_string(),
                                (None, true) => "Loading\u{2026}".to_string(),
                                (None, false) => "Unknown".to_string(),
                            }),
                        )),
                )
                .children(checks.into_iter().enumerate().map(|(ix, check)| {
                    let url = check.url.clone();
                    h_flex()
                        .id(ElementId::NamedInteger("pr-check".into(), ix as u64))
                        .gap_2()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .when(url.is_some(), |d| {
                            d.cursor_pointer().hover(|s| s.bg(palette.hover))
                        })
                        .child(dot(ci_color(Some(check.state), palette), 7.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(palette.fg)
                                .child(SharedString::from(
                                    ui::sanitize_line(&match &check.description {
                                        Some(text) if !text.is_empty() => {
                                            format!("{} \u{b7} {text}", check.name)
                                        }
                                        _ => check.name.clone(),
                                    })
                                    .into_owned(),
                                )),
                        )
                        .on_click(move |_: &ClickEvent, window, cx| {
                            if let Some(url) = &url {
                                crate::links::open(url, Some(window), cx);
                            }
                        })
                }))
        });

        let mut side = vec![page::side_block(
            "Branches",
            div()
                .text_xs()
                .text_color(palette.fg)
                .child(SharedString::from(
                    ui::sanitize_line(&branches).into_owned(),
                )),
            cx,
        )];
        if can_checkout {
            side.push(
                Button::new("pr-checkout")
                    .small()
                    .primary()
                    .icon(IconName::GitBranch)
                    .label("Check out branch")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.checkout(&pr_for_checkout, cx)
                    }))
                    .into_any_element(),
            );
        }
        side.extend(self.notice.clone().map(|text| {
            div()
                .text_xs()
                .text_color(palette.warn)
                .child(SharedString::from(ui::sanitize_display(&text).into_owned()))
                .into_any_element()
        }));
        side.extend(checks_block.map(IntoElement::into_any_element));

        let mut main = vec![self.render_page_tabs(palette, cx)];
        match self.page_tab {
            PageTab::Conversation => {
                main.push(page::description(
                    pr.body.as_deref(),
                    ElementId::NamedInteger("pr-body".into(), number),
                    palette,
                ));
                main.push(self.conversation.clone().into_any_element());
            }
            PageTab::Changes => main.push(self.changes.clone().into_any_element()),
        }
        page::render(
            DetailPage {
                id: ElementId::NamedInteger("pr-page".into(), number),
                title: pr.title.clone(),
                number,
                chips,
                web_url,
                main,
                side,
                leading: unfold.into_iter().collect(),
            },
            &self.page_probe,
            cx.listener(|this, _: &ClickEvent, window, cx| this.back(window, cx)),
            palette,
            cx,
        )
    }
}
