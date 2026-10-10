//! The issue (work item) view of one repository tab: a paged list with an Open / Closed / All
//! switch and a text filter over title and labels. Opening an issue (click, Enter) replaces the
//! list with its page (description, labels, assignees, conversation, a comment box, Close /
//! Reopen) beside a compact copy of the list; Back (Esc, mouse back) returns to the full list
//! as it was. Plus the create dialog. Only the `GitHost` trait is used; buttons the host
//! cannot back are hidden.

use std::ops::Range;
use std::time::{Duration, Instant};

use ::hosts::{
    Feature, HostVocab, Issue, IssueState, ListQuery, PollSchedule, StateFilter, Thread, WebTarget,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{
    Escape as InputEscape, Input, InputEvent, InputState, Textarea, TextareaState,
};
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

use super::conversation::ConversationView;
use super::logic::{self, ListModel};
use super::new_issue::{NewIssueEvent, NewIssueForm};
use super::page::{self, DetailPage, SideRow, WidthProbe};
use super::{Link, dot, empty_panel, error_banner, label_pill, pill, resolve};
use crate::actions::PageBack;
use crate::hosted::HostedRepo;
use crate::repo_tab::RepoTab;

const ROW_HEIGHT: f32 = 50.;
const TICK: Duration = Duration::from_secs(15);
const PAGE_SIZE: u8 = 30;

pub struct IssuesView {
    tab: Entity<RepoTab>,
    link: Link,
    vocab: &'static HostVocab,
    remotes_seen: Option<Vec<(String, Option<String>)>>,
    resolve_task: Option<Task<()>>,
    filter: StateFilter,
    text: Entity<InputState>,
    list: ListModel<Issue>,
    list_task: Option<Task<()>>,
    /// The highlighted row (kept while a page is open, so Back lands on it).
    selected: Option<u64>,
    /// The issue whose page replaces the list.
    open: Option<u64>,
    select_after: Option<u64>,
    conversation: Entity<ConversationView>,
    focus: FocusHandle,
    /// The compact list beside an open page.
    side_scroll: UniformListScrollHandle,
    /// The user folded the side list (kept for the session).
    side_folded: bool,
    /// The user unfolded it on a narrow panel.
    side_pinned: bool,
    panel_probe: WidthProbe,
    page_probe: WidthProbe,
    comment: Entity<TextareaState>,
    /// A comment or state change is on its way.
    acting: bool,
    act_task: Option<Task<()>>,
    /// `(is_error, text)` about the last action.
    notice: Option<(bool, SharedString)>,
    schedule: PollSchedule,
    active: bool,
    scroll: UniformListScrollHandle,
    dialog_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
    _timer: Option<Task<()>>,
}

impl IssuesView {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut schedule = PollSchedule::host_refresh(&crate::prefs::get(cx).poll);
        schedule.set_focused(window.is_window_active(), Instant::now());
        let text = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by title or label"));
        let comment = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("Write a comment")
        });
        let subs = vec![
            cx.observe(&tab, |this, _, cx| this.on_tab_changed(cx)),
            cx.subscribe(&text, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&comment, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
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
            text,
            list: ListModel::default(),
            list_task: None,
            selected: None,
            open: None,
            select_after: None,
            conversation: cx.new(|_| ConversationView::new()),
            focus: cx.focus_handle(),
            side_scroll: UniformListScrollHandle::new(),
            side_folded: false,
            side_pinned: false,
            panel_probe: WidthProbe::new(page::WIDE_PANEL),
            page_probe: WidthProbe::new(page::WIDE_PAGE),
            comment,
            acting: false,
            act_task: None,
            notice: None,
            schedule,
            active: true,
            scroll: UniformListScrollHandle::new(),
            dialog_sub: None,
            _subs: subs,
            _timer: Some(timer),
        };
        view.on_tab_changed(cx);
        view
    }

    /// The panel title in the host's words (`Work items`).
    pub fn title(&self) -> &'static str {
        self.vocab.issues
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
        self.list.reset();
        self.list_task = None;
        self.selected = None;
        self.open = None;
        self.notice = None;
        self.acting = false;
        self.act_task = None;
        self.resolve_task = Some(resolve(&self.tab, Feature::Issues, cx, |this, link, cx| {
            this.resolve_task = None;
            if let Link::Ready(session) = &link {
                this.vocab = session.host.vocab();
            }
            this.link = link;
            if matches!(this.link, Link::Ready(_)) {
                this.refresh(false, cx);
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

    fn session(&self) -> Option<&crate::hosted::Session> {
        match &self.link {
            Link::Ready(session) => Some(session),
            _ => None,
        }
    }

    fn supports(&self, feature: Feature) -> bool {
        self.session().is_some_and(|s| s.host.supports(feature))
    }

    // ---- loading ----------------------------------------------------------------------

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
            }
        }
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
            let result = host.issues(&repo, &query).await;
            _ = this.update(cx, |this, cx| {
                if let Err(err) = &result
                    && this
                        .session()
                        .is_some_and(|s| s.retry_with_other_account(err))
                {
                    this.reconnect(cx);
                    return;
                }
                let ok = result.is_ok();
                if this
                    .list
                    .finish(req, result, append, |i| i.number, logic::now_secs())
                {
                    this.schedule.record(Instant::now(), ok);
                    if ok
                        && let Some(number) = this.select_after.take()
                        && this.list.items.iter().any(|i| i.number == number)
                    {
                        this.selected = Some(number);
                        this.open = Some(number);
                    }
                    cx.notify();
                }
            });
        }));
        cx.notify();
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

    /// The rows on screen: the loaded ones that match the text filter.
    fn visible(&self, cx: &Context<Self>) -> Vec<&Issue> {
        logic::filter_issues(&self.list.items, self.text.read(cx).value().as_ref())
    }

    // ---- actions ----------------------------------------------------------------------

    fn open_issue(&self) -> Option<Issue> {
        let number = self.open?;
        self.list.items.iter().find(|i| i.number == number).cloned()
    }

    /// Opens `number`'s page (from the list, the side list or a key). Both lists follow, so
    /// Back lands on the last one opened.
    fn open_page(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.open != Some(number) {
            self.notice = None;
        }
        self.selected = Some(number);
        self.open = Some(number);
        if let Some(ix) = self.visible(cx).iter().position(|i| i.number == number) {
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
        self.notice = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// On the list: Up / Down move the selection, Enter opens it. On a page: Alt+Up / Alt+Down open the previous / next item.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let rows: Vec<u64> = self.visible(cx).iter().map(|i| i.number).collect();
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

    fn post_comment(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.comment.read(cx).value().trim().to_string();
        let Some(session) = self.session() else {
            return;
        };
        if text.is_empty() || self.acting {
            return;
        }
        let (host, repo) = (session.host.clone(), session.repo.clone());
        self.acting = true;
        self.notice = None;
        self.act_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = host.comment_on_issue(&repo, number, &text).await;
            host.invalidate(Some(&repo));
            _ = this.update_in(cx, |this, window, cx| {
                this.acting = false;
                match result {
                    Ok(posted) => {
                        this.conversation.update(cx, |c, cx| {
                            c.posted(Thread::Issue(number), posted.into(), cx)
                        });
                        if let Some(issue) = this.list.items.iter_mut().find(|i| i.number == number)
                        {
                            issue.comments += 1;
                        }
                        this.notice = Some((false, "Comment posted.".into()));
                        this.comment
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    Err(err) => this.notice = Some((true, err.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_state(&mut self, number: u64, open: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        if self.acting {
            return;
        }
        let (host, repo) = (session.host.clone(), session.repo.clone());
        self.acting = true;
        self.notice = None;
        self.act_task = Some(cx.spawn(async move |this, cx| {
            let result = host.set_issue_state(&repo, number, open).await;
            host.invalidate(Some(&repo));
            _ = this.update(cx, |this, cx| {
                this.acting = false;
                match result {
                    // Updated in place so the detail stays; the next refresh re-sorts the list.
                    Ok(updated) => {
                        if let Some(issue) = this.list.items.iter_mut().find(|i| i.number == number)
                        {
                            issue.state = updated.state;
                            issue.updated_at = updated.updated_at;
                        }
                    }
                    Err(err) => this.notice = Some((true, err.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn open_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let vocab = self.vocab;
        let form = cx.new(|cx| NewIssueForm::new(session, vocab, window, cx));
        self.dialog_sub = Some(cx.subscribe_in(
            &form,
            window,
            |this, _, event: &NewIssueEvent, window, cx| {
                if let NewIssueEvent::Created(issue) = event {
                    this.created(issue, cx);
                }
                window.close_dialog(cx);
                this.dialog_sub = None;
            },
        ));
        let title = format!("New {}", vocab.issue.to_lowercase());
        window.open_dialog(cx, move |d, _, _| {
            d.title(title.clone()).w(px(560.)).child(form.clone())
        });
    }

    fn created(&mut self, issue: &Issue, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.host.invalidate(Some(&session.repo));
        }
        self.select_after = Some(issue.number);
        self.filter = StateFilter::Open;
        self.list.reset();
        self.refresh(false, cx);
    }
}

// ---- rendering ------------------------------------------------------------------------------

impl Render for IssuesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        match &self.link {
            Link::Resolving => div()
                .id("issues-connecting")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(palette.panel)
                .text_sm()
                .text_color(palette.muted)
                .child("Connecting\u{2026}")
                .into_any_element(),
            Link::Empty(empty) => empty_panel(
                empty,
                self.vocab.issues,
                &self.vocab.issues.to_lowercase(),
                &palette,
                "issues-empty",
                cx,
            ),
            Link::Ready(_) => self.render_ready(&palette, window, cx),
        }
    }
}

impl IssuesView {
    fn render_ready(
        &mut self,
        palette: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match self.open_issue() {
            Some(issue) => self.render_open(issue, palette, cx),
            None => self.render_list(palette, cx),
        };
        div()
            .id("issues-root")
            .key_context("IssuesView")
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
        let header = self.render_header(palette, cx);
        let toolbar = self.render_toolbar(palette, cx);
        let banner = self.list.error.clone().map(|error| {
            error_banner(&error, palette, "issues-error", {
                let this = cx.entity().downgrade();
                move |_, cx| {
                    _ = this.update(cx, |view, cx| view.refresh(true, cx));
                }
            })
        });
        let shown = self.visible(cx).len();
        let count = shown + usize::from(self.list.next.is_some());
        let list: AnyElement = if shown == 0 && self.list.next.is_none() {
            let noun = self.vocab.issues.to_lowercase();
            let text = if self.list.loading || !self.list.loaded {
                if self.list.error.is_some() {
                    "Could not load.".to_string()
                } else {
                    "Loading\u{2026}".to_string()
                }
            } else if !self.list.items.is_empty() {
                format!("No {noun} match the filter.")
            } else {
                format!("No {noun} here.")
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
                        "issue-rows",
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
            .id("issues-view")
            .size_full()
            .bg(palette.panel)
            .child(header)
            .child(toolbar)
            .children(banner)
            .child(v_flex().flex_1().min_h_0().w_full().child(list))
            .into_any_element()
    }

    /// An open issue: the compact list on the left (unless folded or the panel is narrow),
    /// the issue's page beside it.
    fn render_open(
        &mut self,
        issue: Issue,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_side = !self.side_folded && (self.panel_probe.wide() || self.side_pinned);
        let side = show_side.then(|| {
            let count = self.visible(cx).len();
            let list = v_flex()
                .relative()
                .w_full()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "issue-side-rows",
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
                self.vocab.issues,
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
        let page = self.render_detail(issue, unfold, palette, cx);
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
        let rows: Vec<Issue> = self.visible(cx).into_iter().cloned().collect();
        range
            .filter_map(|ix| {
                let issue = rows.get(ix)?;
                let open = issue.state == IssueState::Open;
                let number = issue.number;
                let row = SideRow {
                    number,
                    title: issue.title.clone(),
                    author: issue
                        .author
                        .as_ref()
                        .map_or("unknown".into(), |a| a.login.clone()),
                    state: if open { "Open" } else { "Closed" }.into(),
                    color: if open { palette.ok } else { palette.lanes[1] },
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
    fn render_header(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
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
        let new_label = format!("New {}", self.vocab.issue.to_lowercase());
        let can_create = self.supports(Feature::CreateIssue);
        let subtitle = match self.session() {
            Some(session) => format!(
                "Track and discuss {} for {}.",
                self.vocab.issues.to_lowercase(),
                super::repo_slug(&session.repo)
            ),
            None => format!("Sign in to see {}.", self.vocab.issues.to_lowercase()),
        };
        let right = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(status)),
            )
            .child(
                Button::new("issues-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh")
                    .loading(self.list.loading)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(true, cx))),
            )
            .when(can_create, |d| {
                d.child(
                    Button::new("issue-new")
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
        crate::page_header::page_header(self.vocab.issues, subtitle, Some(right), cx)
            .into_any_element()
    }

    /// The filter and search row under the header.
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
            .id("issues-toolbar")
            .flex_none()
            .h(zoomed(38., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .child(filter_button(
                "issue-filter-open",
                "Open",
                StateFilter::Open,
            ))
            .child(filter_button(
                "issue-filter-closed",
                "Closed",
                StateFilter::Closed,
            ))
            .child(filter_button("issue-filter-all", "All", StateFilter::All))
            .child(
                div()
                    .w(px(200.))
                    .child(Input::new(&self.text).small().cleanable(true)),
            )
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let now = logic::now_secs();
        let rows: Vec<Issue> = self.visible(cx).into_iter().cloned().collect();
        let height = zoomed(ROW_HEIGHT, cx);
        range
            .map(|ix| match rows.get(ix) {
                Some(issue) => self.render_row(ix, issue, height, now, &palette, cx),
                None => {
                    let next = self.list.next;
                    h_flex()
                        .id("issue-more")
                        .h(height)
                        .justify_center()
                        .child(
                            Button::new("issue-load-more")
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
                        .into_any_element()
                }
            })
            .collect()
    }

    fn render_row(
        &self,
        ix: usize,
        issue: &Issue,
        height: gpui_kit::Pixels,
        now: i64,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(issue.number);
        let open = issue.state == IssueState::Open;
        let number = issue.number;
        let author = issue
            .author
            .as_ref()
            .map_or("unknown", |a| a.login.as_str());
        h_flex()
            .id(ElementId::NamedInteger("issue-row".into(), ix as u64))
            .h(height)
            .w_full()
            .px_3()
            .gap_2()
            .cursor_pointer()
            .border_b_1()
            .border_color(palette.border.opacity(0.5))
            .when(selected, |d| d.bg(palette.accent_soft))
            .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
            .child(dot(if open { palette.ok } else { palette.lanes[1] }, 8.))
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
                                        ui::sanitize_line(&issue.title).into_owned(),
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
                            .children(
                                issue
                                    .labels
                                    .iter()
                                    .take(3)
                                    .map(|l| label_pill(l, palette).into_any_element()),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_none()
                                    .child(SharedString::from(logic::ago(&issue.updated_at, now))),
                            )
                            .when(issue.comments > 0, |d| {
                                d.child(div().flex_none().child(SharedString::from(format!(
                                    "{} comment{}",
                                    issue.comments,
                                    if issue.comments == 1 { "" } else { "s" }
                                ))))
                            }),
                    ),
            )
            .child(pill(
                if open { "Open" } else { "Closed" },
                if open { palette.ok } else { palette.lanes[1] },
                palette,
            ))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_page(number, window, cx)
            }))
            .into_any_element()
    }

    fn render_detail(
        &mut self,
        issue: Issue,
        unfold: Option<AnyElement>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = logic::now_secs();
        let open = issue.state == IssueState::Open;
        let number = issue.number;
        if let Some(session) = self.session().cloned() {
            super::conversation::seed_head(
                Thread::Issue(number),
                issue
                    .author
                    .iter()
                    .chain(&issue.assignees)
                    .cloned()
                    .collect(),
                cx,
            );
            self.conversation
                .update(cx, |c, cx| c.show(&session, Thread::Issue(number), cx));
        }
        let author = issue
            .author
            .as_ref()
            .map_or("unknown".to_string(), |a| a.login.clone());
        let assignees = issue
            .assignees
            .iter()
            .map(|a| a.login.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let can_close = self.supports(Feature::CloseIssue);
        let can_comment = self.supports(Feature::IssueComments);
        let acting = self.acting;
        let has_comment = !self.comment.read(cx).value().trim().is_empty();
        let noun = self.vocab.issue.to_lowercase();
        let web_url = self
            .session()
            .and_then(|s| s.host.web_url(&s.repo, &WebTarget::Issue(number)))
            .or_else(|| Some(issue.web_url.clone()).filter(|u| !u.is_empty()));

        let mut chips = vec![
            pill(
                if open { "Open" } else { "Closed" },
                if open { palette.ok } else { palette.lanes[1] },
                palette,
            )
            .into_any_element(),
            div()
                .text_xs()
                .text_color(palette.muted)
                .child(SharedString::from(format!(
                    "{} opened {} \u{b7} updated {}",
                    ui::sanitize_line(&author),
                    logic::ago(&issue.created_at, now),
                    logic::ago(&issue.updated_at, now)
                )))
                .into_any_element(),
        ];
        chips.extend(
            issue
                .labels
                .iter()
                .map(|l| label_pill(l, palette).into_any_element()),
        );

        let mut side = Vec::new();
        side.push(page::side_block(
            "Assignees",
            div()
                .text_xs()
                .text_color(if assignees.is_empty() {
                    palette.muted
                } else {
                    palette.fg
                })
                .child(SharedString::from(if assignees.is_empty() {
                    "Nobody".to_string()
                } else {
                    ui::sanitize_line(&assignees).into_owned()
                })),
            cx,
        ));
        if can_close {
            side.push(
                Button::new("issue-toggle-state")
                    .small()
                    .outline()
                    .label(if open {
                        SharedString::from(format!("Close {noun}"))
                    } else {
                        SharedString::from(format!("Reopen {noun}"))
                    })
                    .loading(acting)
                    .disabled(acting)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_state(number, !open, cx)
                    }))
                    .into_any_element(),
            );
        }
        side.extend(self.notice.clone().map(|(error, text)| {
            div()
                .text_xs()
                .text_color(if error { palette.error } else { palette.ok })
                .child(SharedString::from(ui::sanitize_display(&text).into_owned()))
                .into_any_element()
        }));

        let comment_box = can_comment.then(|| {
            v_flex()
                .gap_1()
                .child(ui::typography::section_title("ADD A COMMENT", cx))
                .child(Textarea::new(&self.comment).disabled(acting))
                .child(
                    h_flex().justify_end().child(
                        Button::new("issue-comment")
                            .small()
                            .primary()
                            .label("Comment")
                            .loading(acting)
                            .disabled(acting || !has_comment)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.post_comment(number, window, cx)
                            })),
                    ),
                )
                .into_any_element()
        });
        let mut main = vec![
            page::description(
                issue.body.as_deref(),
                ElementId::NamedInteger("issue-body".into(), number),
                palette,
            ),
            self.conversation.clone().into_any_element(),
        ];
        main.extend(comment_box);

        page::render(
            DetailPage {
                id: ElementId::NamedInteger("issue-page".into(), number),
                title: issue.title.clone(),
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
