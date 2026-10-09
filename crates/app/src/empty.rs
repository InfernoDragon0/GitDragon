//! One look for "nothing here yet": an icon, a title, a line saying what to do, and optionally
//! the button that does it. Used where a view would otherwise show a bare sentence.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, Sizable as _, v_flex};
use gpui_kit::{
    App, ClickEvent, IntoElement, ParentElement as _, RenderOnce, SharedString, Styled as _,
    Window, div, px,
};
use ui::Palette;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct EmptyState {
    icon: IconName,
    title: SharedString,
    hint: SharedString,
    action: Option<(SharedString, OnClick)>,
}

impl EmptyState {
    pub fn new(
        icon: IconName,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
    ) -> Self {
        Self {
            icon,
            title: title.into(),
            hint: hint.into(),
            action: None,
        }
    }

    /// A button under the hint.
    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.action = Some((label.into(), Box::new(on_click)));
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let palette = Palette::of(cx);
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .text_center()
            .bg(palette.panel)
            .child(Icon::new(self.icon).size(px(28.)).text_color(palette.muted))
            .child(div().text_lg().text_color(palette.emph).child(self.title))
            .child(
                div()
                    .max_w(px(420.))
                    .text_sm()
                    .text_color(palette.muted)
                    .child(self.hint),
            )
            .children(self.action.map(|(label, on_click)| {
                Button::new("empty-action")
                    .primary()
                    .small()
                    .label(label)
                    .on_click(on_click)
            }))
    }
}
