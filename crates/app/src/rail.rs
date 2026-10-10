//! The 58 px activity rail: icon-over-label buttons that pick the centre view.

use gpui_kit::assets::IconName;
use gpui_kit::component::badge::Badge;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, v_flex};
use gpui_kit::{
    Action, AnyElement, App, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use hosts::HostVocab;
use ui::{Palette, motion, zoomed};

use crate::actions::*;

pub const RAIL_WIDTH: f32 = 58.;
const ITEM_HEIGHT: f32 = 56.;
const ICON_SIZE: f32 = 16.;
const LABEL_SIZE: f32 = 10.5;
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

    /// The caption under the icon: one short word, "MRs" on GitLab.
    pub fn caption(self, vocab: Option<&HostVocab>) -> SharedString {
        match (self, vocab) {
            (View::PullRequests, Some(v)) => format!("{}s", v.pr_short).into(),
            (View::PullRequests, None) => "PRs".into(),
            _ => self.label().into(),
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

/// The view switch in flight, handed to the newly active item so it can fade in and slide its bar.
#[derive(Clone, Copy)]
struct Switch {
    /// Counts switches; part of the animation ids so every switch restarts them.
    n: u32,
    /// How many item heights the bar travels (positive: from below), `None` when the previous
    /// item is in the other group or hidden, where the bar fades in instead.
    slide: Option<f32>,
}

/// Remembered across frames by the rail: which view was active, to animate the change.
struct RailMotion {
    current: View,
    prev: Option<View>,
    n: u32,
}

type OnSelect = std::rc::Rc<dyn Fn(View, &mut Window, &mut App)>;

/// Rail element; the workspace owns which view is active and the badge counts.
#[derive(IntoElement)]
pub struct ActivityRail {
    active: View,
    badges: Vec<(View, usize)>,
    conflicts: usize,
    vocab: Option<&'static HostVocab>,
    on_select: OnSelect,
}

impl ActivityRail {
    pub fn new(active: View, on_select: impl Fn(View, &mut Window, &mut App) + 'static) -> Self {
        Self {
            active,
            badges: Vec::new(),
            conflicts: 0,
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

    /// Conflicted files in the active repository. The Conflicts tab shows only while above 0.
    pub fn conflicts(mut self, count: usize) -> Self {
        self.conflicts = count;
        self
    }

    fn item(&self, view: View, switch: Option<Switch>, palette: &Palette, cx: &App) -> AnyElement {
        let reduce = cx.reduce_motion();
        let active = view == self.active;
        let conflicted = view == View::Conflicts;
        let count = if conflicted {
            self.conflicts
        } else {
            self.badges
                .iter()
                .find(|(v, _)| *v == view)
                .map_or(0, |(_, n)| *n)
        };
        // Conflicts glow in the warning colour rather than the accent.
        let tone = conflicted.then_some(palette.warn);
        let on_select = self.on_select.clone();
        let tip = SharedString::from(view.tooltip(self.vocab));
        let caption = view.caption(self.vocab);
        let icon_size = zoomed(ICON_SIZE, cx);

        // Stacked translucent discs behind the icon, as the toolbar's glowing buttons. Conflicts
        // pulse slowly in the warning colour; the active item has a still accent glow that fades
        // in when it becomes active.
        let discs = |t: Hsla, size: f32, levels: [(f32, f32); 3]| {
            let disc = |scale: f32, alpha: f32| {
                let d = icon_size * scale;
                div()
                    .absolute()
                    .size(d)
                    .rounded(d / 2.)
                    .bg(t.opacity(alpha))
            };
            div()
                .absolute()
                .size(icon_size * size)
                .flex()
                .items_center()
                .justify_center()
                .child(disc(levels[0].0, levels[0].1))
                .child(disc(levels[1].0, levels[1].1))
                .child(disc(levels[2].0, levels[2].1))
        };
        let halo: Option<AnyElement> = match tone {
            Some(t) => Some(motion::pulse(
                discs(t, 1.4, [(1.4, 0.08), (1.1, 0.12), (0.8, 0.18)]),
                "rail-conflicts-glow",
                0.35,
                1.,
                motion::BREATHE,
                reduce,
            )),
            None if active => {
                let glow = discs(palette.accent, 1.5, [(1.5, 0.04), (1.15, 0.07), (0.8, 0.1)]);
                Some(match switch {
                    Some(sw) => motion::fade_in(
                        glow,
                        SharedString::from(format!("rail-glow-{}", sw.n)),
                        motion::FAST,
                        reduce,
                    ),
                    None => glow.into_any_element(),
                })
            }
            None => None,
        };

        // The label: emphasised while active; fades from muted on a switch.
        let label = div()
            .max_w_full()
            .px_0p5()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(zoomed(LABEL_SIZE, cx))
            .when(tone.is_some(), |d| d.font_weight(FontWeight::SEMIBOLD))
            .child(caption);
        let label = match (active && tone.is_none(), switch) {
            (true, Some(sw)) => {
                let (from, to) = (palette.muted, palette.emph);
                motion::transition(
                    label,
                    SharedString::from(format!("rail-label-{}", sw.n)),
                    motion::FAST,
                    reduce,
                    move |d, t| d.text_color(motion::lerp_color(from, to, t)),
                )
            }
            (true, None) => label.text_color(palette.emph).into_any_element(),
            _ => label.into_any_element(),
        };

        // The active bar, in the item's own space. On a switch within a group it starts where the
        // previous item's bar was and slides over; otherwise it fades in.
        let bar_colour = tone.unwrap_or(palette.accent);
        let bar_top = zoomed(10., cx);
        let bar = active.then(|| {
            let bar = div()
                .absolute()
                .left_0()
                .h(zoomed(ITEM_HEIGHT - 20., cx))
                .w(px(ACTIVE_BAR))
                .rounded_r(px(2.))
                .bg(bar_colour);
            match switch {
                Some(Switch {
                    n,
                    slide: Some(items),
                }) => motion::slide_y(
                    bar,
                    SharedString::from(format!("rail-bar-{n}")),
                    bar_top + zoomed(ITEM_HEIGHT, cx) * items,
                    bar_top,
                    motion::SLIDE,
                    reduce,
                ),
                Some(Switch { n, slide: None }) => motion::fade_in(
                    bar.top(bar_top),
                    SharedString::from(format!("rail-bar-{n}")),
                    motion::SLIDE,
                    reduce,
                ),
                None => bar.top(bar_top).into_any_element(),
            }
        });

        let base = div()
            .id(ElementId::Name(SharedString::from(format!(
                "rail-{}",
                view.label()
            ))))
            .relative()
            .w(zoomed(RAIL_WIDTH, cx))
            .h(zoomed(ITEM_HEIGHT, cx))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(zoomed(2., cx))
            .cursor_pointer()
            .text_color(match tone {
                Some(t) => t,
                None if active => palette.accent,
                None => palette.muted,
            })
            // One hover style only: GPUI asserts it isn't set twice.
            .hover(|s| {
                let s = s.bg(palette.hover);
                if tone.is_none() {
                    s.text_color(palette.emph)
                } else {
                    s
                }
            })
            .children(bar)
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(halo)
                    .child(
                        Badge::new()
                            .count(count)
                            .color(tone.unwrap_or(palette.accent))
                            .small()
                            .child(Icon::new(view.icon()).size(icon_size)),
                    ),
            )
            .child(label)
            .tooltip(move |window, cx| {
                Tooltip::new(tip.clone())
                    .action(view.action().as_ref(), None)
                    .build(window, cx)
            })
            .on_click(move |_: &ClickEvent, window, cx| on_select(view, window, cx));
        // The icon fades from muted to the accent on a switch (the label does so on its own).
        match switch {
            Some(sw) if active && tone.is_none() => {
                let (from, to) = (palette.muted, palette.accent);
                motion::transition(
                    base,
                    SharedString::from(format!("rail-item-{}", sw.n)),
                    motion::FAST,
                    reduce,
                    move |d, t| d.text_color(motion::lerp_color(from, to, t)),
                )
            }
            _ => base.into_any_element(),
        }
    }
}

impl RenderOnce for ActivityRail {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = Palette::of(cx);
        let top: Vec<View> = View::TOP
            .into_iter()
            .filter(|v| *v != View::Conflicts || self.conflicts > 0)
            .collect();

        // Note the change of active view; the new item animates for as long as this switch is
        // the latest one (a finished animation is inert and schedules no frames).
        let state = window.use_keyed_state("rail-motion", cx, |_, _| RailMotion {
            current: self.active,
            prev: None,
            n: 0,
        });
        state.update(cx, |s, _| {
            if s.current != self.active {
                s.prev = Some(s.current);
                s.current = self.active;
                s.n += 1;
            }
        });
        let (prev, n) = {
            let s = state.read(cx);
            (s.prev, s.n)
        };
        let switch = prev.map(|prev| {
            // Items share a group when both are in the same list; the bar then slides by the
            // difference of their positions. Across groups (or from a hidden Conflicts) the
            // distance isn't a whole number of items, so it fades instead.
            let pos = |list: &[View], v: View| list.iter().position(|x| *x == v);
            let slide = [top.as_slice(), &View::BOTTOM]
                .into_iter()
                .find_map(|list| {
                    Some((pos(list, prev)? as f32) - (pos(list, self.active)? as f32))
                });
            Switch { n, slide }
        });
        let switch_for = |v: View| switch.filter(|_| v == self.active);

        v_flex()
            .id("activity-rail")
            .flex_shrink_0()
            .w(zoomed(RAIL_WIDTH, cx))
            .h_full()
            .py_1()
            .bg(palette.panel2)
            .border_r_1()
            .border_color(palette.border)
            .children(
                top.iter()
                    .map(|v| self.item(*v, switch_for(*v), &palette, cx)),
            )
            .child(div().flex_1())
            .children(View::BOTTOM.map(|v| self.item(v, switch_for(v), &palette, cx)))
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

    #[test]
    fn captions_are_short() {
        assert_eq!(View::PullRequests.caption(None), "PRs");
        assert_eq!(View::PullRequests.caption(Some(&HostVocab::GITLAB)), "MRs");
        assert_eq!(
            View::Issues.caption(Some(&HostVocab::AZURE_DEVOPS)),
            "Issues"
        );
    }
}
