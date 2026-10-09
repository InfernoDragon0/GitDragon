//! Diff view building blocks shared by the Changes panel and commit details: the row model and
//! one rendered line (gutter, sign, text with word-level emphasis). The panels own the virtual
//! list and the hunk / line actions.

pub mod model;
pub mod syntax;
pub mod words;

use git_core::parse::diff::LineKind;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::{
    App, Div, Hsla, ParentElement as _, Styled as _, StyledText, div, prelude::FluentBuilder as _,
};

pub use model::{DiffModel, DiffRow, Layout, LineView};

use crate::{Palette, zoomed};

/// Height of every diff row at 100% zoom (the list is uniform; use [`zoomed`]).
pub const ROW_HEIGHT: f32 = 22.;
/// Diff text size: a touch larger than the 12 px UI mono size, for reading code.
pub const FONT_SIZE: f32 = 13.5;
const GUTTER: f32 = 48.;

/// Background tints for a line kind: (line, emphasised words).
pub fn tints(kind: LineKind, palette: &Palette) -> Option<(Hsla, Hsla)> {
    match kind {
        LineKind::Added => Some((palette.ok.opacity(0.12), palette.ok.opacity(0.32))),
        LineKind::Deleted => Some((palette.error.opacity(0.12), palette.error.opacity(0.32))),
        _ => None,
    }
}

/// One side of a diff line. `numbers` are shown in order (one column for split, two for
/// unified). `selected` marks a line picked for line staging.
pub fn line(
    view: &LineView,
    kind: LineKind,
    numbers: &[Option<u32>],
    selected: bool,
    palette: &Palette,
    cx: &App,
) -> Div {
    let tint = tints(kind, palette);
    let sign = match kind {
        LineKind::Added => "+",
        LineKind::Deleted => "-",
        _ => " ",
    };
    let strong = tint.map_or(palette.accent_soft, |(_, strong)| strong);
    let emphasis: &[std::ops::Range<usize>] = if tint.is_some() { &view.emphasis } else { &[] };
    let text = StyledText::new(view.text.clone()).with_highlights(syntax::merge(
        &view.syntax,
        emphasis,
        strong,
        view.text.len(),
    ));
    let mono = cx.theme().mono_font_family.clone();
    h_flex()
        .h(zoomed(ROW_HEIGHT, cx))
        .w_full()
        .min_w_0()
        .font_family(mono)
        .text_size(zoomed(FONT_SIZE, cx))
        .when_some(tint, |d, (bg, _)| d.bg(bg))
        .when(selected, |d| d.bg(palette.accent_soft))
        .children(numbers.iter().map(|n| {
            div()
                .w(zoomed(GUTTER, cx))
                .flex_none()
                .pr_2()
                .text_right()
                .text_color(palette.muted)
                .child(n.map(|n| n.to_string()).unwrap_or_default())
        }))
        .child(
            div()
                .w(zoomed(14., cx))
                .flex_none()
                .text_color(match kind {
                    LineKind::Added => palette.ok,
                    LineKind::Deleted => palette.error,
                    _ => palette.muted,
                })
                .child(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(palette.fg)
                .child(text),
        )
        .when(view.no_eol, |d| {
            d.child(
                div()
                    .flex_none()
                    .px_1()
                    .text_color(palette.warn)
                    .child("no newline"),
            )
        })
}

/// An empty filler cell for the side of a split row with no line.
pub fn filler(palette: &Palette, cx: &App) -> Div {
    div().h(zoomed(ROW_HEIGHT, cx)).w_full().bg(palette.panel2)
}
