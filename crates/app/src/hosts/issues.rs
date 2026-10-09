//! The issue (work item) view of one repository tab: a paged list with an Open / Closed / All
//! switch and a text filter over title and labels, the selected issue's details (description,
//! labels, assignees), a comment box, Close / Reopen and the create dialog. Only the
//! `GitHost` trait is used; buttons the host cannot back are hidden.

use std::ops::Range;
use std::time::{Duration, Instant};

use ::hosts::{
    Feature, HostVocab, Issue, IssueState, ListQuery, PollSchedule, StateFilter, WebTarget,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use ui::{Palette, zoomed};

use super::logic::{self, ListModel};
use super::new_issue::{NewIssueEvent, NewIssueForm};
use super::{Link, dot, empty_panel, error_banner, label_pill, pill, resolve};
use crate::hosted::HostedRepo;
use crate::repo_tab::RepoTab;

const ROW_HEIGHT: f32 = 50.;
const DETAIL_WIDTH: f32 = 440.;
const TICK: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 20_000;
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
    selected: Option<u64>,
    select_after: Option<u64>,
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
            select_after: None,
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
        self.load(1, false, cx);
    }

    /// The rows on screen: the loaded ones that match the text filter.
    fn visible(&self, cx: &Context<Self>) -> Vec<&Issue> {
        logic::filter_issues(&self.list.items, self.text.read(cx).value().as_ref())
    }

    // ---- actions ----------------------------------------------------------------------

    fn selected_issue(&self) -> Option<Issue> {
        let number = self.selected?;
        self.list.items.iter().find(|i| i.number == number).cloned()
    }

    fn open_in_browser(&self, issue: &Issue, cx: &mut Context<Self>) {
        let url = self
            .session()
            .and_then(|s| s.host.web_url(&s.repo, &WebTarget::Issue(issue.number)))
            .unwrap_or_else(|| issue.web_url.clone());
        if !url.is_empty() {
            cx.open_url(&url);
        }
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
                    Ok(_) => {
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
            uniform_list(
                "issue-rows",
                count,
                cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .into_any_element()
        };
        let detail = self
            .selected_issue()
            .map(|issue| self.render_detail(issue, palette, cx));
        v_flex()
            .id("issues-view")
            .size_full()
            .bg(palette.panel)
            .child(toolbar)
            .children(banner)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(v_flex().flex_1().min_w_0().h_full().child(list))
                    .children(detail),
            )
            .into_any_element()
    }

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
        h_flex()
            .id("issues-toolbar")
            .flex_none()
            .h(zoomed(38., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .px_1()
                    .text_sm()
                    .text_color(palette.emph)
                    .child(self.vocab.issues),
            )
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
            .child(div().flex_1())
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
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(palette.emph)
                                    .child(SharedString::from(issue.title.clone())),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(
                                div()
                                    .flex_none()
                                    .child(SharedString::from(author.to_string())),
                            )
                            .children(
                                issue
                                    .labels
                                    .iter()
                                    .take(3)
                                    .map(|l| label_pill(l, palette).into_any_element()),
                            )
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
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = Some(number);
                this.notice = None;
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_detail(
        &mut self,
        issue: Issue,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = logic::now_secs();
        let open = issue.state == IssueState::Open;
        let number = issue.number;
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
        let body = issue
            .body
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|b| b.chars().take(MAX_BODY).collect::<String>());
        let can_close = self.supports(Feature::CloseIssue);
        let can_comment = self.supports(Feature::IssueComments);
        let acting = self.acting;
        let has_comment = !self.comment.read(cx).value().trim().is_empty();
        let issue_for_open = issue.clone();
        let noun = self.vocab.issue.to_lowercase();

        let header =
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_base()
                        .text_color(palette.emph)
                        .child(SharedString::from(format!("{} #{number}", issue.title))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(pill(
                            if open { "Open" } else { "Closed" },
                            if open { palette.ok } else { palette.lanes[1] },
                            palette,
                        ))
                        .child(div().text_xs().text_color(palette.muted).child(
                            SharedString::from(format!(
                                "{author} \u{b7} updated {}",
                                logic::ago(&issue.updated_at, now)
                            )),
                        )),
                )
                .when(!assignees.is_empty(), |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(palette.fg)
                            .child(SharedString::from(format!("Assigned to {assignees}"))),
                    )
                })
                .when(!issue.labels.is_empty(), |d| {
                    d.child(
                        h_flex().gap_1().flex_wrap().children(
                            issue
                                .labels
                                .iter()
                                .map(|l| label_pill(l, palette).into_any_element()),
                        ),
                    )
                });

        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("issue-open-web")
                    .small()
                    .outline()
                    .label("Open in browser")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_in_browser(&issue_for_open, cx)
                    })),
            )
            .when(can_close, |d| {
                d.child(
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
                        })),
                )
            });

        let description = v_flex()
            .gap_1()
            .text_sm()
            .text_color(palette.fg)
            .children(match &body {
                Some(text) => text
                    .lines()
                    .map(|line| {
                        if line.trim().is_empty() {
                            div().h(px(6.)).into_any_element()
                        } else {
                            div()
                                .child(SharedString::from(line.trim_end().to_string()))
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>(),
                None => vec![
                    div()
                        .text_color(palette.muted)
                        .child("No description.")
                        .into_any_element(),
                ],
            });

        let comment_box = can_comment.then(|| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(SharedString::from(format!(
                            "{} comment{} \u{b7} add yours",
                            issue.comments,
                            if issue.comments == 1 { "" } else { "s" }
                        ))),
                )
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
        });

        let close = cx.listener(|this, _: &ClickEvent, _, cx| {
            this.selected = None;
            cx.notify();
        });
        v_flex()
            .id("issue-detail")
            .flex_none()
            .w(px(DETAIL_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(palette.border)
            .bg(palette.panel2)
            .child(
                h_flex().flex_none().justify_end().px_2().pt_1().child(
                    Button::new("issue-detail-close")
                        .xsmall()
                        .ghost()
                        .icon(IconName::X)
                        .tooltip("Close details")
                        .on_click(close),
                ),
            )
            .child(
                v_flex()
                    .id("issue-detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .pb_3()
                    .gap_3()
                    .child(header)
                    .child(actions)
                    .children(self.notice.clone().map(|(error, text)| {
                        div()
                            .text_xs()
                            .text_color(if error { palette.error } else { palette.ok })
                            .child(text)
                            .into_any_element()
                    }))
                    .child(description)
                    .children(comment_box),
            )
            .into_any_element()
    }
}
