//! Settings page (AgentSummary shape: page header, one card per section, rows with a label, a
//! short state line and the control). Appearance only for now; Accounts, Git and Keyboard join
//! with their cards.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use ui::theme::{self, Zoom};
use ui::{Appearance, Mode, Palette, Skin};

pub struct SettingsView {
    zoom_slider: Entity<SliderState>,
    focus: FocusHandle,
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
        ];
        Self {
            zoom_slider,
            focus: cx.focus_handle(),
            _subs,
        }
    }
}

/// A setting row: name + state line on the left, the control on the right.
fn row(
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
fn choice<T: Copy + PartialEq + 'static>(
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let appearance = theme::appearance(cx);
        let zoom = theme::zoom(cx);

        let appearance_card = v_flex()
            .w_full()
            .p_4()
            .gap_1()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(div().pb_2().text_color(palette.emph).child("Appearance"))
            .child(row(
                "Theme",
                "Colour family for the whole app.",
                choice(
                    "skin",
                    &[(Skin::Aurora, "Aurora"), (Skin::Onyx, "Onyx")],
                    appearance.skin,
                    |skin, cx| {
                        let mut next = theme::appearance(cx);
                        next.skin = skin;
                        theme::set_appearance(next, cx);
                    },
                ),
                &palette,
            ))
            .child(row(
                "Mode",
                "Ctrl+Shift+L switches anywhere.",
                choice(
                    "mode",
                    &[(Mode::Dark, "Dark"), (Mode::Light, "Light")],
                    appearance.mode,
                    |mode, cx| {
                        let mut next = theme::appearance(cx);
                        next.mode = mode;
                        theme::set_appearance(next, cx);
                    },
                ),
                &palette,
            ))
            .child(row(
                "Zoom",
                format!(
                    "{}% · Ctrl+= bigger, Ctrl+- smaller, Ctrl+0 reset. Applies to text and lists.",
                    zoom.percent()
                ),
                h_flex()
                    .gap_2()
                    .w(px(280.))
                    .child(div().flex_1().child(Slider::new(&self.zoom_slider)))
                    .child(
                        div()
                            .w(px(40.))
                            .text_sm()
                            .text_right()
                            .text_color(palette.emph)
                            .child(format!("{}%", zoom.percent())),
                    )
                    .child(
                        Button::new("zoom-reset")
                            .ghost()
                            .small()
                            .label("Reset")
                            .on_click(|_: &ClickEvent, _, cx| {
                                theme::set_zoom(1.0, cx);
                            }),
                    ),
                &palette,
            ));

        div()
            .id("settings")
            .size_full()
            .track_focus(&self.focus)
            .overflow_y_scroll()
            .bg(palette.bg)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(760.))
                    .mx_auto()
                    .p_6()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xl().text_color(palette.emph).child("Settings"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(palette.muted)
                                    .child("Changes save as you go."),
                            ),
                    )
                    .child(appearance_card),
            )
    }
}
