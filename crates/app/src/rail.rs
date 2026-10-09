//! The 44 px activity rail: icon-only buttons that pick the centre view.

use gpui_kit::assets::IconName;
use gpui_kit::component::badge::Badge;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, v_flex};
use gpui_kit::{
    Action, App, ClickEvent, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use hosts::HostVocab;
use ui::Palette;

use crate::actions::*;

pub const RAIL_WIDTH: f32 = 44.;
const ITEM_HEIGHT: f32 = 40.;
const ACTIVE_BAR: f32 = 3.;

/// What the centre of the window is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Graph,
    Conflicts,
    PullRequests,
    Issues,
    Boards,
    Chat,
    Settings,
}

impl View {
    pub const TOP: [View; 5] = [
        View::Graph,
        View::Conflicts,
        View::PullRequests,
        View::Issues,
        View::Boards,
    ];
    pub const BOTTOM: [View; 2] = [View::Chat, View::Settings];

    pub fn label(self) -> &'static str {
        match self {
            View::Graph => "Graph",
            View::Conflicts => "Conflicts",
            View::PullRequests => "Pull requests",
            View::Issues => "Issues",
            View::Boards => "Boards",
            View::Chat => "Chat",
            View::Settings => "Settings",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            View::Graph => IconName::GitGraph,
            View::Conflicts => IconName::GitMergeConflict,
            View::PullRequests => IconName::GitPullRequest,
            View::Issues => IconName::CircleDot,
            View::Boards => IconName::SquareKanban,
            View::Chat => IconName::MessageSquare,
            View::Settings => IconName::Settings,
        }
    }

    /// The action that shows this view; the tooltip reads its key binding.
    /// The tooltip: the view's name, in the active repository's host words where it has them
    /// (GitLab "Merge requests", Azure "Work items").
    pub fn tooltip(self, vocab: Option<&HostVocab>) -> &'static str {
        match (self, vocab) {
            (View::PullRequests, Some(v)) => v.pull_requests,
            (View::Issues, Some(v)) => v.issues,
            _ => self.label(),
        }
    }

    pub fn action(self) -> Box<dyn Action> {
        match self {
            View::Graph => Box::new(ShowGraph),
            View::Conflicts => Box::new(ShowConflicts),
            View::PullRequests => Box::new(ShowPullRequests),
            View::Issues => Box::new(ShowIssues),
            View::Boards => Box::new(ShowBoards),
            View::Chat => Box::new(ShowChat),
            View::Settings => Box::new(ShowSettings),
        }
    }
}

type OnSelect = std::rc::Rc<dyn Fn(View, &mut Window, &mut App)>;

/// Rail element; the workspace owns which view is active and the badge counts.
#[derive(IntoElement)]
pub struct ActivityRail {
    active: View,
    badges: Vec<(View, usize)>,
    vocab: Option<&'static HostVocab>,
    on_select: OnSelect,
}

impl ActivityRail {
    pub fn new(active: View, on_select: impl Fn(View, &mut Window, &mut App) + 'static) -> Self {
        Self {
            active,
            badges: Vec::new(),
            vocab: None,
            on_select: std::rc::Rc::new(on_select),
        }
    }

    /// The active repository's host words for the tooltips.
    pub fn vocab(mut self, vocab: Option<&'static HostVocab>) -> Self {
        self.vocab = vocab;
        self
    }

    /// Count shown on a view's icon; 0 hides it.
    #[allow(dead_code, reason = "fed by the PRs / issues cards")]
    pub fn badge(mut self, view: View, count: usize) -> Self {
        self.badges.push((view, count));
        self
    }

    fn item(&self, view: View, palette: &Palette) -> impl IntoElement {
        let active = view == self.active;
        let count = self
            .badges
            .iter()
            .find(|(v, _)| *v == view)
            .map_or(0, |(_, n)| *n);
        let on_select = self.on_select.clone();
        let label = SharedString::from(view.tooltip(self.vocab));

        div()
            .id(ElementId::Name(SharedString::from(format!(
                "rail-{}",
                view.label()
            ))))
            .relative()
            .w(px(RAIL_WIDTH))
            .h(px(ITEM_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(if active {
                palette.accent
            } else {
                palette.muted
            })
            .hover(|s| s.bg(palette.hover).text_color(palette.emph))
            .when(active, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(8.))
                        .bottom(px(8.))
                        .w(px(ACTIVE_BAR))
                        .rounded_r(px(2.))
                        .bg(palette.accent),
                )
            })
            .child(
                Badge::new()
                    .count(count)
                    .color(palette.accent)
                    .small()
                    .child(Icon::new(view.icon()).size(px(20.))),
            )
            .tooltip(move |window, cx| {
                Tooltip::new(label.clone())
                    .action(view.action().as_ref(), None)
                    .build(window, cx)
            })
            .on_click(move |_: &ClickEvent, window, cx| on_select(view, window, cx))
    }
}

impl RenderOnce for ActivityRail {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = Palette::of(cx);
        v_flex()
            .id("activity-rail")
            .flex_shrink_0()
            .w(px(RAIL_WIDTH))
            .h_full()
            .py_1()
            .bg(palette.panel2)
            .border_r_1()
            .border_color(palette.border)
            .children(View::TOP.map(|v| self.item(v, &palette)))
            .child(div().flex_1())
            .children(View::BOTTOM.map(|v| self.item(v, &palette)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltips_use_the_host_vocabulary() {
        assert_eq!(View::PullRequests.tooltip(None), "Pull requests");
        assert_eq!(
            View::PullRequests.tooltip(Some(&HostVocab::GITLAB)),
            "Merge requests"
        );
        assert_eq!(View::Issues.tooltip(Some(&HostVocab::GITHUB)), "Issues");
        assert_eq!(View::Graph.tooltip(Some(&HostVocab::GITLAB)), "Graph");
        let azure = HostVocab::AZURE_DEVOPS;
        assert_eq!(View::Issues.tooltip(Some(&azure)), azure.issues);
    }
}
