//! Dock panels. Placeholders until their cards land (branches sidebar, graph, details).

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::v_flex;
use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};
use ui::Palette;

use crate::rail::View;
use crate::repo_tab::RepoTab;
use crate::welcome::Welcome;

/// A titled panel that says what will live there.
pub struct Placeholder {
    name: &'static str,
    title: SharedString,
    hint: SharedString,
    focus: FocusHandle,
}

impl Placeholder {
    pub fn new(
        name: &'static str,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            name,
            title: title.into(),
            hint: hint.into(),
            focus: cx.focus_handle(),
        }
    }
}

impl BasePanel for Placeholder {
    fn panel_name(&self) -> &'static str {
        self.name
    }
}

impl Panel for Placeholder {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.title.clone()
    }
}

impl EventEmitter<PanelEvent> for Placeholder {}

impl Focusable for Placeholder {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Placeholder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        empty_state(&self.title, &self.hint, &Palette::of(cx)).track_focus(&self.focus)
    }
}

/// The centre panel: the welcome screen when no repository tab is active, otherwise whichever
/// view the activity rail picked for the active repository.
pub struct CenterView {
    view: View,
    repo: Option<Entity<RepoTab>>,
    welcome: Entity<Welcome>,
    focus: FocusHandle,
    _observe_repo: Option<Subscription>,
}

impl CenterView {
    pub fn new(welcome: Entity<Welcome>, cx: &mut Context<Self>) -> Self {
        Self {
            view: View::default(),
            repo: None,
            welcome,
            focus: cx.focus_handle(),
            _observe_repo: None,
        }
    }

    pub fn set_repo(&mut self, repo: Option<Entity<RepoTab>>, cx: &mut Context<Self>) {
        self._observe_repo = repo
            .as_ref()
            .map(|repo| cx.observe(repo, |_, _, cx| cx.notify()));
        self.repo = repo;
        cx.notify();
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
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
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match self.repo {
            Some(_) => self.view.label(),
            None => "Welcome",
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(repo) = &self.repo else {
            return div()
                .size_full()
                .track_focus(&self.focus)
                .child(self.welcome.clone())
                .into_any_element();
        };
        let repo = repo.read(cx);
        let hint = match self.view {
            View::Graph => "Commit graph and history.",
            View::Changes => "Working tree status, hunk staging and commit.",
            View::Conflicts => "Merge, rebase and cherry-pick conflicts.",
            View::PullRequests => "Pull requests from your connected hosts.",
            View::Issues => "Issues from your connected hosts.",
            View::Boards => "GitHub Projects, GitLab boards and Azure Boards.",
            View::Chat => "AI chat with Claude Code.",
            View::Settings => "Appearance, accounts, git and keyboard settings.",
        };
        let title = format!("{} · {}", repo.name, self.view.label());
        let body = format!(
            "{}
{hint}",
            repo.summary()
        );
        empty_state(&title, &body, &Palette::of(cx))
            .track_focus(&self.focus)
            .into_any_element()
    }
}

fn empty_state(title: &str, hint: &str, palette: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    v_flex()
        .id(SharedString::from(format!("empty-{title}")))
        .size_full()
        .items_center()
        .justify_center()
        .gap_1()
        .px_4()
        .text_center()
        .bg(palette.panel)
        .child(
            div()
                .text_lg()
                .text_color(palette.emph)
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .text_sm()
                .text_color(palette.muted)
                .child(SharedString::from(hint.to_string())),
        )
}
