//! Shown in the centre when no repository tab is active: open / clone / create, and the
//! recent repositories.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, ClickEvent, Context, ElementId, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px,
};
use ui::Palette;

use crate::actions::{CloneRepository, NewRepository, OpenRepository};
use crate::state::RecentRepo;

pub enum WelcomeEvent {
    OpenRecent(PathBuf),
    Forget(PathBuf),
}

struct Row {
    repo: RecentRepo,
    missing: bool,
}

pub struct Welcome {
    recent: Vec<Row>,
    /// Why git can't be used (not installed / too old); shown as a banner.
    git_problem: Option<SharedString>,
    /// Tracked on the screen's root so actions and shortcuts dispatched here reach the
    /// workspace.
    focus: FocusHandle,
    /// Take focus on the next render (set when the screen comes back after a repo).
    focus_on_show: bool,
}

impl EventEmitter<WelcomeEvent> for Welcome {}

impl Welcome {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            recent: Vec::new(),
            git_problem: None,
            focus: cx.focus_handle(),
            focus_on_show: true,
        }
    }

    /// The screen is shown again: the element that had focus (a repo's view) is gone, and
    /// GPUI would send actions and key bindings to the window root, where nothing handles them.
    pub fn focus_on_show(&mut self) {
        self.focus_on_show = true;
    }

    pub fn set_recent(&mut self, recent: &[RecentRepo], cx: &mut Context<Self>) {
        self.recent = recent
            .iter()
            .map(|repo| Row {
                missing: !repo.path.join(".git").exists(),
                repo: repo.clone(),
            })
            .collect();
        cx.notify();
    }

    pub fn set_git_problem(&mut self, problem: Option<SharedString>, cx: &mut Context<Self>) {
        self.git_problem = problem;
        cx.notify();
    }

    fn action_button(
        focus: FocusHandle,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        action: impl gpui_kit::Action + Clone,
    ) -> Button {
        let tooltip_action = action.clone();
        Button::new(id)
            .outline()
            .icon(icon)
            .label(label)
            .tooltip_with_action(label, &tooltip_action, None)
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                // From this screen's node: `window.dispatch_action` starts at the focused
                // element, which may be a repo view that is no longer on screen.
                window.focus(&focus, cx);
                focus.dispatch_action(&action, window, cx);
            })
    }

    fn render_row(
        &self,
        ix: usize,
        row: &Row,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let path = row.repo.path.clone();
        let forget = row.repo.path.clone();
        let missing = row.missing;
        h_flex()
            .id(ElementId::NamedInteger("recent".into(), ix as u64))
            .group("recent-row")
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .rounded(px(6.))
            .when(!missing, |this| {
                this.cursor_pointer()
                    .hover(|s| s.bg(palette.hover))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.emit(WelcomeEvent::OpenRecent(path.clone()));
                    }))
            })
            .child(
                Icon::new(IconName::FolderGit2)
                    .size(px(18.))
                    .text_color(if missing {
                        palette.muted
                    } else {
                        palette.accent
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(if missing { palette.muted } else { palette.emph })
                            .child(row.repo.name.clone()),
                    )
                    .child(div().text_xs().text_color(palette.muted).truncate().child(
                        if missing {
                            format!("Missing: {}", row.repo.path.display())
                        } else {
                            row.repo.path.display().to_string()
                        },
                    )),
            )
            .child(
                div()
                    .invisible()
                    .group_hover("recent-row", |s| s.visible())
                    .child(
                        Button::new(ElementId::NamedInteger("forget".into(), ix as u64))
                            .ghost()
                            .xsmall()
                            .icon(IconName::X)
                            .tooltip("Remove from recent")
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(WelcomeEvent::Forget(forget.clone()));
                            })),
                    ),
            )
    }
}

impl Render for Welcome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_on_show) {
            window.focus(&self.focus, cx);
        }
        let palette = Palette::of(cx);
        let [g0, g1, g2, g3] = palette.gradient;

        let header = h_flex()
            .gap_3()
            .child(div().size(px(36.)).rounded(px(9.)).bg(linear_gradient(
                135.,
                linear_color_stop(g0, 0.),
                linear_color_stop(g3, 1.),
            )))
            .child(
                v_flex()
                    .child(ui::typography::page_title("GitDragon", cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.muted)
                            .child("Open a repository to get started."),
                    ),
            );

        let banner = self.git_problem.clone().map(|problem| {
            h_flex()
                .gap_2()
                .p_3()
                .rounded(px(6.))
                .border_1()
                .border_color(palette.warn)
                .bg(palette.panel2)
                .child(Icon::new(IconName::TriangleAlert).text_color(palette.warn))
                .child(div().text_sm().text_color(palette.emph).child(problem))
        });

        let actions = h_flex()
            .gap_2()
            .child(Self::action_button(
                self.focus.clone(),
                "welcome-open",
                IconName::FolderOpen,
                "Open repository",
                OpenRepository,
            ))
            .child(Self::action_button(
                self.focus.clone(),
                "welcome-clone",
                IconName::Download,
                "Clone",
                CloneRepository,
            ))
            .child(Self::action_button(
                self.focus.clone(),
                "welcome-new",
                IconName::FolderPlus,
                "New repository",
                NewRepository,
            ));

        let recent = v_flex()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .pb_1()
                    .text_xs()
                    .text_color(palette.muted)
                    .child("RECENT"),
            )
            .when(self.recent.is_empty(), |this| {
                this.child(
                    div()
                        .px_3()
                        .text_sm()
                        .text_color(palette.muted)
                        .child("Repositories you open show up here."),
                )
            })
            .children(
                self.recent
                    .iter()
                    .enumerate()
                    .map(|(ix, row)| self.render_row(ix, row, &palette, cx)),
            );

        div()
            .id("welcome")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scrollbar()
            .bg(palette.panel)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(640.))
                    .mx_auto()
                    .px_6()
                    .py(px(56.))
                    .gap_6()
                    // A thin signature-gradient rule under the header.
                    .child(header)
                    .child(
                        div()
                            .h(px(2.))
                            .w(px(120.))
                            .rounded(px(1.))
                            .bg(linear_gradient(
                                90.,
                                linear_color_stop(g1, 0.),
                                linear_color_stop(g2, 1.),
                            )),
                    )
                    .children(banner)
                    .child(actions)
                    .child(recent),
            )
    }
}
