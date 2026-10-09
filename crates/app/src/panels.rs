//! The centre dock panel: welcome screen, or the active repository's Graph / Conflicts / Settings
//! view. The working copy (commit box, staging) lives on the graph's WIP row.

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::v_flex;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    WeakEntity, Window, div,
};

use crate::boards::{BoardsEvent, BoardsView};
use crate::conflicts::{self, ConflictsView};
use crate::graph::GraphView;
use crate::graph::link::GraphLink;
use crate::hosted::HostedRepo;
use crate::hosts::{IssuesView, PrsView};
use crate::rail::View;
use crate::repo_tab::RepoTab;
use crate::settings::SettingsView;
use crate::welcome::Welcome;

/// The centre panel: the welcome screen when no repository tab is active, otherwise whichever
/// view the activity rail picked for the active repository.
pub struct CenterView {
    view: View,
    repo: Option<Entity<RepoTab>>,
    welcome: Entity<Welcome>,
    /// One Conflicts view per repository tab.
    conflicts: Vec<(WeakEntity<RepoTab>, Entity<ConflictsView>)>,
    /// Conflicted files the active tab had last time: the view switches to Conflicts when
    /// they first appear.
    seen_conflicts: usize,
    /// One Graph view per repository tab, so selection and scroll survive tab switches.
    graphs: Vec<(WeakEntity<RepoTab>, Entity<GraphView>)>,
    /// One pull request / merge request view per repository tab, created on first visit.
    prs: Vec<(WeakEntity<RepoTab>, Entity<PrsView>)>,
    /// One issue / work item view per repository tab, created on first visit.
    issues: Vec<(WeakEntity<RepoTab>, Entity<IssuesView>)>,
    /// One Boards view per repository tab, created on first visit.
    boards: Vec<(WeakEntity<RepoTab>, Entity<BoardsView>)>,
    /// Every open repository tab (the Boards picker lists their boards too).
    open_tabs: Vec<WeakEntity<RepoTab>>,
    _board_subs: Vec<Subscription>,
    /// One AI chat view per repository tab, created on first visit.
    chats: Vec<(WeakEntity<RepoTab>, Entity<crate::chat::view::ChatView>)>,
    /// Shared with the details panel: the active tab's selected commit.
    link: Entity<GraphLink>,
    /// Created on first visit; Settings works with no repository open.
    settings: Option<Entity<SettingsView>>,
    focus: FocusHandle,
    _observe_repo: Option<Subscription>,
}

impl CenterView {
    pub fn new(welcome: Entity<Welcome>, link: Entity<GraphLink>, cx: &mut Context<Self>) -> Self {
        Self {
            view: View::default(),
            repo: None,
            welcome,
            conflicts: Vec::new(),
            seen_conflicts: 0,
            graphs: Vec::new(),
            prs: Vec::new(),
            issues: Vec::new(),
            boards: Vec::new(),
            open_tabs: Vec::new(),
            _board_subs: Vec::new(),
            chats: Vec::new(),
            link,
            settings: None,
            focus: cx.focus_handle(),
            _observe_repo: None,
        }
    }

    pub fn set_repo(&mut self, repo: Option<Entity<RepoTab>>, cx: &mut Context<Self>) {
        self.seen_conflicts = repo.as_ref().map_or(0, |r| r.read(cx).conflicts.len());
        self._observe_repo = repo.as_ref().map(|repo| {
            cx.observe(repo, |this, repo, cx| {
                let now = repo.read(cx).conflicts.len();
                if now > 0 && this.seen_conflicts == 0 {
                    this.view = View::Conflicts;
                }
                this.seen_conflicts = now;
                cx.notify();
            })
        });
        self.repo = repo;
        // Closed tabs: drop their views (and any running operation).
        self.conflicts.retain(|(tab, _)| tab.upgrade().is_some());
        self.graphs.retain(|(tab, _)| tab.upgrade().is_some());
        self.prs.retain(|(tab, _)| tab.upgrade().is_some());
        self.issues.retain(|(tab, _)| tab.upgrade().is_some());
        self.boards.retain(|(tab, _)| tab.upgrade().is_some());
        self.chats.retain(|(tab, _)| tab.upgrade().is_some());
        self.sync_active(cx);
        // The details panel follows the newly active tab's selection.
        let weak = self.repo.as_ref().map(Entity::downgrade);
        let graph = self
            .graphs
            .iter()
            .find(|(tab, _)| Some(tab) == weak.as_ref())
            .map(|(_, graph)| graph.clone());
        match graph {
            Some(graph) => graph.update(cx, |graph, cx| graph.publish_selection(cx)),
            None => self
                .link
                .update(cx, |link, cx| link.set_selection(weak, None, cx)),
        }
        cx.notify();
    }

    /// The Graph view with the WIP row selected: its details hold the commit box and the
    /// staged / unstaged files.
    pub fn show_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Graph, cx);
        if let Some(repo) = self.repo.clone() {
            let graph = self.graph_view(repo, window, cx);
            graph.update(cx, |graph, cx| graph.select_wip(cx));
        }
    }

    fn conflicts_view(
        &mut self,
        repo: Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConflictsView> {
        let weak = repo.downgrade();
        if let Some((_, view)) = self.conflicts.iter().find(|(tab, _)| *tab == weak) {
            return view.clone();
        }
        let view = cx.new(|cx| ConflictsView::new(repo, window, cx));
        self.conflicts.push((weak, view.clone()));
        view
    }

    fn graph_view(
        &mut self,
        repo: Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<GraphView> {
        let weak = repo.downgrade();
        if let Some((_, view)) = self.graphs.iter().find(|(tab, _)| *tab == weak) {
            return view.clone();
        }
        let link = self.link.clone();
        let view = cx.new(|cx| GraphView::new(repo, link, window, cx));
        self.graphs.push((weak, view.clone()));
        view
    }

    fn prs_view(
        &mut self,
        repo: Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PrsView> {
        let weak = repo.downgrade();
        if let Some((_, view)) = self.prs.iter().find(|(tab, _)| *tab == weak) {
            return view.clone();
        }
        let view = cx.new(|cx| PrsView::new(repo, window, cx));
        self.prs.push((weak, view.clone()));
        view
    }

    fn issues_view(
        &mut self,
        repo: Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<IssuesView> {
        let weak = repo.downgrade();
        if let Some((_, view)) = self.issues.iter().find(|(tab, _)| *tab == weak) {
            return view.clone();
        }
        let view = cx.new(|cx| IssuesView::new(repo, window, cx));
        self.issues.push((weak, view.clone()));
        view
    }

    fn boards_view(
        &mut self,
        repo: Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BoardsView> {
        let weak = repo.downgrade();
        if let Some((_, view)) = self.boards.iter().find(|(tab, _)| *tab == weak) {
            return view.clone();
        }
        let others = self.open_tabs.clone();
        let view = cx.new(|cx| BoardsView::new(repo.clone(), others, window, cx));
        self._board_subs.push(cx.subscribe_in(
            &view,
            window,
            move |this, _, event: &BoardsEvent, window, cx| match event {
                BoardsEvent::OpenPullRequest(number) => {
                    let number = *number;
                    this.set_view(View::PullRequests, cx);
                    let prs = this.prs_view(repo.clone(), window, cx);
                    prs.update(cx, |prs, cx| prs.reveal(number, cx));
                }
            },
        ));
        self.boards.push((weak, view.clone()));
        view
    }

    /// The open repository tabs, for the Boards picker.
    pub fn set_open_tabs(&mut self, tabs: Vec<WeakEntity<RepoTab>>, cx: &mut Context<Self>) {
        for (_, view) in &self.boards {
            let tabs = tabs.clone();
            view.update(cx, |view, _| view.set_other_tabs(tabs));
        }
        self.open_tabs = tabs;
    }

    /// Tells the pull request / issue views whether they are on screen, so only visible ones
    /// refresh on their timers.
    fn sync_active(&self, cx: &mut Context<Self>) {
        let current = self.repo.as_ref().map(Entity::downgrade);
        for (tab, view) in &self.prs {
            let on = self.view == View::PullRequests && Some(tab) == current.as_ref();
            view.update(cx, |view, cx| view.set_active(on, cx));
        }
        for (tab, view) in &self.issues {
            let on = self.view == View::Issues && Some(tab) == current.as_ref();
            view.update(cx, |view, cx| view.set_active(on, cx));
        }
        for (tab, view) in &self.boards {
            let on = self.view == View::Boards && Some(tab) == current.as_ref();
            view.update(cx, |view, cx| view.set_active(on, cx));
        }
    }

    /// The name of the current view for the panel tab: pull requests and issues in the active
    /// repository's host words (GitLab "Merge requests", Azure "Work items").
    fn view_title(&self, cx: &App) -> &'static str {
        let Some(repo) = &self.repo else {
            return self.view.label();
        };
        let weak = repo.downgrade();
        let vocab = || HostedRepo::of_tab(repo.read(cx), cx).and_then(|h| h.vocab());
        match self.view {
            View::PullRequests => self
                .prs
                .iter()
                .find(|(tab, _)| *tab == weak)
                .map(|(_, view)| view.read(cx).title())
                .or_else(|| vocab().map(|v| v.pull_requests))
                .unwrap_or(self.view.label()),
            View::Issues => self
                .issues
                .iter()
                .find(|(tab, _)| *tab == weak)
                .map(|(_, view)| view.read(cx).title())
                .or_else(|| vocab().map(|v| v.issues))
                .unwrap_or(self.view.label()),
            _ => self.view.label(),
        }
    }

    pub fn view(&self) -> View {
        self.view
    }

    /// Opens Settings scrolled to `section` (the command palette's "Settings: Git").
    pub fn show_settings_section(
        &mut self,
        section: crate::settings_find::Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_view(View::Settings, cx);
        let view = self
            .settings
            .get_or_insert_with(|| cx.new(|cx| SettingsView::new(window, cx)))
            .clone();
        view.update(cx, |settings, cx| settings.go_to(section, window, cx));
        cx.notify();
    }

    pub fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.sync_active(cx);
            cx.notify();
        }
    }
}

impl BasePanel for CenterView {
    fn panel_name(&self) -> &'static str {
        "center"
    }
}

impl Panel for CenterView {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match (&self.repo, self.view) {
            (_, View::Settings) => self.view.label(),
            (Some(_), _) => self.view_title(cx),
            (None, _) => "Welcome",
        }
    }
}

impl EventEmitter<PanelEvent> for CenterView {}

impl Focusable for CenterView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CenterView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_body(window, cx);
        let banner = match (&self.repo, self.view) {
            (_, View::Settings) | (None, _) => None,
            (Some(repo), _) => conflicts::banner(
                repo,
                |window, cx| window.dispatch_action(Box::new(crate::actions::ShowConflicts), cx),
                cx,
            ),
        };
        match banner {
            Some(banner) => v_flex()
                .size_full()
                .child(banner)
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
            None => body,
        }
    }
}

impl CenterView {
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        if self.view == View::Settings {
            let view = self
                .settings
                .get_or_insert_with(|| cx.new(|cx| SettingsView::new(window, cx)))
                .clone();
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::Conflicts, Some(repo)) = (self.view, self.repo.clone()) {
            let view = self.conflicts_view(repo, window, cx);
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::Graph, Some(repo)) = (self.view, self.repo.clone()) {
            let view = self.graph_view(repo, window, cx);
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::PullRequests, Some(repo)) = (self.view, self.repo.clone()) {
            let view = self.prs_view(repo, window, cx);
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::Issues, Some(repo)) = (self.view, self.repo.clone()) {
            let view = self.issues_view(repo, window, cx);
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::Boards, Some(repo)) = (self.view, self.repo.clone()) {
            let view = self.boards_view(repo, window, cx);
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        if let (View::Chat, Some(repo)) = (self.view, self.repo.clone()) {
            let weak = repo.downgrade();
            let view = match self.chats.iter().find(|(tab, _)| *tab == weak) {
                Some((_, view)) => view.clone(),
                None => {
                    let view = cx.new(|cx| crate::chat::view::ChatView::new(repo, window, cx));
                    self.chats.push((weak, view.clone()));
                    view
                }
            };
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(view)
                .into_any_element();
        }
        let Some(repo) = &self.repo else {
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(self.welcome.clone())
                .into_any_element();
        };
        let repo = repo.read(cx);
        let title = format!("{} · {}", repo.name, self.view.label());
        let hint = "Nothing to show here.";
        div()
            .size_full()
            .track_focus(&self.focus)
            .child(crate::empty::EmptyState::new(self.view.icon(), title, hint))
            .into_any_element()
    }
}
