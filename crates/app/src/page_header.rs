//! The page header every full-width view shares (Settings, Pull requests, Issues, Boards,
//! Conflicts, Chat): a large title with a muted subtitle line, controls on the right, and a
//! bottom border. The Graph keeps the dock's own title bar instead.

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{AnyElement, App, ParentElement as _, SharedString, Styled as _, div};
use ui::Palette;

/// The header row: `title` over a muted `subtitle`, with `right` (filters, buttons, a search
/// box) at the end. The returned `Div` takes more children (a chip bar) under the row.
pub fn page_header(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    right: Option<AnyElement>,
    cx: &App,
) -> gpui_kit::Div {
    let palette = Palette::of(cx);
    v_flex()
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
                        .min_w_0()
                        .gap_1()
                        .child(ui::typography::page_title(title, cx))
                        .child(
                            div()
                                .text_sm()
                                .text_color(palette.muted)
                                .child(subtitle.into()),
                        ),
                )
                .children(right),
        )
}
