//! Settings page (AgentSummary shape: a header with "Find a setting" and the section menu, then
//! one card per section, each with rows of a label, a short state line and the control). The
//! cards live in `settings_cards.rs`, what a query leaves visible in `settings_find.rs`.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use ui::theme::{self, Zoom};
use ui::{Appearance, Palette};

use crate::conflicts::external::{self, Editor};
use crate::prefs;
use crate::settings_find::{Filter, Section, active_section};

/// The page needs this much room beside the section menu to show it as a column; narrower, the
/// menu is a row of chips under the header.
const WIDE: f32 = 880.;
const NAV_WIDTH: f32 = 176.;

pub struct SettingsView {
    pub(crate) zoom_slider: Entity<SliderState>,
    accounts: Entity<crate::accounts::AccountsCard>,
    ai: Entity<crate::chat::settings::AiCard>,
    focus: FocusHandle,
    pub(crate) find: Entity<InputState>,
    pub(crate) scroll: ScrollHandle,
    /// The sections drawn last frame, in order (menu clicks and the scroll spy index into it).
    visible: Vec<Section>,
    wide: bool,
    seen_width: Pixels,
    pending: Option<Section>,
    pub(crate) git: Option<git_core::GitBinary>,
    pub(crate) signing: Option<String>,
    pub(crate) branch_input: Entity<InputState>,
    pub(crate) branch_error: Option<SharedString>,
    pub(crate) editor_input: Entity<InputState>,
    pub(crate) editor_error: Option<SharedString>,
    pub(crate) editors: Vec<(Editor, PathBuf)>,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let percent = theme::zoom(cx).percent() as f32;
        let zoom_slider = cx.new(|_| {
            SliderState::new()
                .min(Zoom::MIN * 100.)
                .max(Zoom::MAX * 100.)
                .step(5.)
                .default_value(percent)
        });
        let saved = prefs::get(cx);
        let find = cx.new(|cx| InputState::new(window, cx).placeholder("Find a setting\u{2026}"));
        let branch_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("main (empty: git's default)")
                .default_value(saved.default_branch.clone())
        });
        let editor_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Full path, e.g. C:\\Tools\\editor.exe")
                .default_value(
                    saved
                        .external_editor
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                )
        });
        let _subs = vec![
            cx.subscribe(&zoom_slider, |_, _, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                theme::set_zoom(value.start() / 100., cx);
            }),
            // Ctrl + = / - / 0 move the slider too.
            cx.observe_global_in::<Zoom>(window, |this, window, cx| {
                let percent = theme::zoom(cx).percent() as f32;
                this.zoom_slider.update(cx, |slider, cx| {
                    if slider.value().start() != percent {
                        slider.set_value(percent, window, cx);
                    }
                });
                cx.notify();
            }),
            cx.observe_global::<Appearance>(|_, cx| cx.notify()),
            cx.subscribe(&find, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&branch_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.apply_branch(cx);
                }
            }),
            cx.subscribe(&editor_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.apply_editor(cx);
                }
            }),
        ];
        let mut view = Self {
            zoom_slider,
            accounts: cx.new(|cx| crate::accounts::AccountsCard::new(window, cx)),
            ai: cx.new(|cx| crate::chat::settings::AiCard::new(window, cx)),
            focus: cx.focus_handle(),
            find,
            scroll: ScrollHandle::new(),
            visible: Vec::new(),
            wide: false,
            seen_width: px(0.),
            pending: None,
            git: None,
            signing: None,
            branch_input,
            branch_error: None,
            editor_input,
            editor_error: None,
            editors: external::available(None),
            _tasks: Vec::new(),
            _subs,
        };
        view.detect_git(cx);
        view
    }

    /// Finds git and its signing config off the UI thread (the Git card shows both).
    fn detect_git(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let git = git_core::detect::git().ok()?;
                    let runner = git_core::GitRunner::new(git.clone());
                    let home = etcetera::home_dir().unwrap_or_else(|_| std::env::temp_dir());
                    let signing = async_io_block(git_core::signing::signing_status(&runner, &home))
                        .map(|s| s.summary())
                        .unwrap_or_else(|err| format!("Could not read it: {err}"));
                    Some((git, signing))
                })
                .await;
            _ = this.update(cx, |this, cx| {
                match found {
                    Some((git, signing)) => {
                        this.git = Some(git);
                        this.signing = Some(signing);
                    }
                    None => this.signing = Some("Git was not found.".into()),
                }
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    /// The default-branch field changed: saved when valid, otherwise the problem is shown.
    pub(crate) fn apply_branch(&mut self, cx: &mut Context<Self>) {
        let text = self.branch_input.read(cx).value().trim().to_string();
        match prefs::validate_branch_name(&text) {
            Ok(()) => {
                self.branch_error = None;
                prefs::update(cx, |p| p.default_branch = text);
            }
            Err(err) => self.branch_error = Some(err.into()),
        }
        cx.notify();
    }

    /// The custom-editor field changed: saved when it is an existing absolute path (or empty).
    pub(crate) fn apply_editor(&mut self, cx: &mut Context<Self>) {
        let text = self.editor_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            self.editor_error = None;
            prefs::update(cx, |p| p.external_editor = None);
        } else {
            match prefs::validate_editor_path(&text) {
                Ok(path) => {
                    self.editor_error = None;
                    prefs::update(cx, |p| p.external_editor = Some(path));
                }
                Err(err) => self.editor_error = Some(err.into()),
            }
        }
        cx.notify();
    }

    /// Scrolls to `section` (clearing a query that hides it).
    pub fn go_to(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        let hidden = !Filter::new(self.find.read(cx).value().as_ref())
            .visible_sections(|_| Vec::new())
            .contains(&section);
        if hidden {
            self.find
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.pending = Some(section);
        cx.notify();
    }
}

/// Runs a git-core future to completion on the current (background) thread.
fn async_io_block<T>(future: impl std::future::Future<Output = T>) -> T {
    futures_lite::future::block_on(future)
}

/// A setting row: name + state line on the left, the control on the right.
pub(crate) fn row(
    name: &'static str,
    state: impl Into<SharedString>,
    control: impl IntoElement,
    palette: &Palette,
) -> AnyElement {
    h_flex()
        .w_full()
        .gap_4()
        .py_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().text_color(palette.emph).child(name))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(state.into()),
                ),
        )
        .child(control)
        .into_any_element()
}

/// Segmented choice: one button per option, the current one filled.
pub(crate) fn choice<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    on_pick: impl Fn(T, &mut gpui_kit::App) + Clone + 'static,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .children(options.iter().enumerate().map(|(ix, (value, label))| {
            let value = *value;
            let on_pick = on_pick.clone();
            let button = Button::new((id, ix)).small().label(*label);
            let button = if value == current {
                button.primary()
            } else {
                button.outline()
            };
            button.on_click(move |_: &ClickEvent, _, cx| on_pick(value, cx))
        }))
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let query = self.find.read(cx).value().to_string();
        let filter = Filter::new(&query);
        let shortcut_labels: Vec<&'static str> = crate::actions::shortcut_actions()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        let visible = filter.visible_sections(|section| {
            if section == Section::Shortcuts {
                shortcut_labels.clone()
            } else {
                Vec::new()
            }
        });

        // Menu as a column when the page is wide enough (with some slack so it does not flip
        // back and forth as the column takes its own width), else as chips.
        let page_width =
            self.scroll.bounds().size.width + if self.wide { px(NAV_WIDTH) } else { px(0.) };
        let wide = if self.wide {
            page_width >= px(WIDE - 40.)
        } else {
            page_width >= px(WIDE)
        };
        if wide != self.wide || page_width != self.seen_width {
            self.wide = wide;
            self.seen_width = page_width;
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                _ = this.update(cx, |_, cx| cx.notify());
            });
        }

        if let Some(section) = self.pending.take() {
            if let Some(ix) = visible.iter().position(|s| *s == section) {
                self.scroll.scroll_to_top_of_item(ix);
            }
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                _ = this.update(cx, |_, cx| cx.notify());
            });
        }
        let max = self.scroll.max_offset().y;
        let at_bottom = max > px(0.) && -self.scroll.offset().y >= max - px(2.);
        let active = active_section(&visible, self.scroll.top_item(), at_bottom);
        self.visible = visible.clone();

        let cards: Vec<AnyElement> = visible
            .iter()
            .map(|section| {
                let card = match section {
                    Section::Appearance => self.appearance_card(&filter, &palette, cx),
                    Section::Accounts => self.accounts.clone().into_any_element(),
                    Section::Git => self.git_card(&filter, &palette, cx),
                    Section::Editor => self.editor_card(&filter, &palette, cx),
                    Section::Diff => self.diff_card(&filter, &palette, cx),
                    Section::PullPush => self.pull_card(&filter, &palette, cx),
                    Section::Refresh => self.refresh_card(&filter, &palette, cx),
                    Section::Ai => self.ai.clone().into_any_element(),
                    Section::Shortcuts => self.shortcuts_card(&filter, &palette, window),
                    Section::Privacy => self.privacy_card(&filter, &palette, cx),
                };
                div()
                    .w_full()
                    .max_w(px(760.))
                    .mx_auto()
                    .child(card)
                    .into_any_element()
            })
            .collect();

        let nothing_found = visible.is_empty().then(|| {
            div()
                .w_full()
                .max_w(px(760.))
                .mx_auto()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child(format!("No setting matches \"{}\".", query.trim()))
        });

        let menu_click = |section: Section| {
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.pending = Some(section);
                cx.notify();
            })
        };

        let header = v_flex()
            .flex_none()
            .px_6()
            .pt_5()
            .pb_3()
            .gap_3()
            .border_b_1()
            .border_color(palette.border)
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().text_xl().text_color(palette.emph).child("Settings"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(palette.muted)
                                    .child("Changes save as you go."),
                            ),
                    )
                    .child(
                        div()
                            .w(px(280.))
                            .child(Input::new(&self.find).cleanable(true)),
                    ),
            )
            .children((!wide && !visible.is_empty()).then(|| {
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(visible.iter().enumerate().map(|(ix, section)| {
                        let button = Button::new(("settings-chip", ix))
                            .small()
                            .label(section.title())
                            .on_click(menu_click(*section));
                        if Some(*section) == active {
                            button.primary()
                        } else {
                            button.ghost()
                        }
                    }))
            }));

        let nav = (wide && !visible.is_empty()).then(|| {
            v_flex()
                .flex_none()
                .w(px(NAV_WIDTH))
                .py_4()
                .pl_4()
                .pr_2()
                .gap_0p5()
                .border_r_1()
                .border_color(palette.border)
                .children(visible.iter().enumerate().map(|(ix, section)| {
                    let current = Some(*section) == active;
                    div()
                        .id(("settings-nav", ix))
                        .px_2()
                        .py_1()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_sm()
                        .text_color(if current {
                            palette.accent
                        } else {
                            palette.muted
                        })
                        .when(current, |d| d.bg(palette.hover))
                        .hover(|s| s.bg(palette.hover).text_color(palette.emph))
                        .on_click(menu_click(*section))
                        .child(section.title())
                }))
        });

        v_flex()
            .id("settings")
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.bg)
            .child(header)
            .child(
                h_flex().flex_1().min_h_0().w_full().children(nav).child(
                    div()
                        .id("settings-scroll")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .p_6()
                        .gap_4()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                        .children(cards)
                        .children(nothing_found),
                ),
            )
    }
}
