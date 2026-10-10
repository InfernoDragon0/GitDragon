//! Diff view building blocks shared by the Changes panel and commit details: the row model and
//! one rendered line (gutter, sign, text with word-level emphasis). The panels own the virtual
//! list and the hunk / line actions.

pub mod copy;
pub mod model;
pub mod syntax;
pub mod words;

use git_core::parse::diff::LineKind;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::{
    AnyElement, App, Div, Hsla, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    StyledText, div, prelude::FluentBuilder as _,
};

pub use model::{DiffModel, DiffRow, Layout, LineView};

use crate::{Palette, zoomed};

/// Height of every diff row at 100% zoom (the list is uniform; use [`zoomed`]).
pub const ROW_HEIGHT: f32 = 22.;
const GUTTER: f32 = 48.;

/// Replaces the pointer text diff of a Git LFS file: "LFS object · 12.0 MB → 14.2 MB" and the
/// short object ids.
pub fn lfs_summary(
    change: &git_core::lfs::PointerChange,
    palette: &Palette,
    cx: &App,
) -> AnyElement {
    use git_core::lfs::human_size;
    let size = |p: &Option<git_core::lfs::Pointer>| p.as_ref().map(|p| human_size(p.size));
    let (title, color) = match (size(&change.old), size(&change.new)) {
        (Some(old), Some(new)) => (format!("{old} \u{2192} {new}"), palette.warn),
        (None, Some(new)) => (format!("Added \u{b7} {new}"), palette.ok),
        (Some(old), None) => (format!("Deleted \u{b7} {old}"), palette.error),
        (None, None) => (String::new(), palette.muted),
    };
    let short = |p: &Option<git_core::lfs::Pointer>| {
        p.as_ref().map_or("\u{2014}".to_string(), |p| {
            let hex = p.oid.strip_prefix("sha256:").unwrap_or(&p.oid);
            hex[..hex.len().min(12)].to_string()
        })
    };
    let mono = cx.theme().mono_font_family.clone();
    div()
        .size_full()
        .p_4()
        .child(
            gpui_kit::component::v_flex()
                .gap_1p5()
                .p_3()
                .rounded(zoomed(8., cx))
                .border_1()
                .border_color(palette.border)
                .bg(palette.panel2)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .text_xs()
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .text_color(palette.accent)
                                .bg(palette.accent.opacity(0.14))
                                .child("LFS"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(palette.emph)
                                .child(SharedString::from(format!("LFS object \u{b7} {title}"))),
                        )
                        .child(div().size(zoomed(8., cx)).rounded_full().bg(color)),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family(mono)
                        .text_color(palette.muted)
                        .child(SharedString::from(format!(
                            "{} \u{2192} {}",
                            short(&change.old),
                            short(&change.new)
                        ))),
                )
                .child(
                    div().text_xs().text_color(palette.muted).child(
                        "The content lives in Git LFS; git tracks this small pointer to it.",
                    ),
                ),
        )
        .into_any_element()
}

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
    // The UI's mono size, which already follows the app zoom.
    let size = cx.theme().mono_font_size;
    h_flex()
        .h(zoomed(ROW_HEIGHT, cx))
        .w_full()
        .min_w_0()
        .font_family(mono)
        .text_size(size)
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

/// The row with the longest line (the longer side of a split pair), for a horizontally
/// scrolling list to measure its width by (`UniformList::with_width_from_item`).
pub fn widest_row(model: &DiffModel, layout: Layout) -> Option<usize> {
    let len = |hunk: usize, line: Option<usize>| {
        line.and_then(|l| model.lines.get(hunk)?.get(l))
            .map_or(0, |v| v.text.chars().count())
    };
    model
        .rows(layout)
        .iter()
        .enumerate()
        .max_by_key(|(_, row)| match **row {
            DiffRow::Hunk { .. } => 0,
            DiffRow::Line { hunk, line } => len(hunk, Some(line)),
            DiffRow::Pair { hunk, left, right } => len(hunk, left).max(len(hunk, right)),
        })
        .map(|(ix, _)| ix)
}

/// One row of a read-only diff (commit details): hunk header, unified line or split pair.
/// `selected` marks a row of the copy selection.
pub fn read_only_row(
    model: &DiffModel,
    layout: Layout,
    ix: usize,
    selected: bool,
    palette: &Palette,
    cx: &App,
) -> Option<AnyElement> {
    Some(match *model.rows(layout).get(ix)? {
        DiffRow::Hunk { hunk } => {
            let header = &model.file.hunks.get(hunk)?.header;
            h_flex()
                .h(zoomed(ROW_HEIGHT, cx))
                .w_full()
                .px_2()
                .bg(if selected {
                    palette.accent_soft
                } else {
                    palette.panel2
                })
                .text_xs()
                .text_color(palette.muted)
                .child(div().min_w_0().truncate().child(SharedString::from(
                    String::from_utf8_lossy(header).into_owned(),
                )))
                .into_any_element()
        }
        DiffRow::Line { hunk, line } => {
            let l = model.file.hunks.get(hunk)?.lines.get(line)?;
            let view = model.lines.get(hunk)?.get(line)?;
            self::line(
                view,
                l.kind,
                &[l.old_lineno, l.new_lineno],
                selected,
                palette,
                cx,
            )
            .into_any_element()
        }
        DiffRow::Pair { hunk, left, right } => {
            let side = |slot: Option<usize>, old: bool| -> AnyElement {
                let found = slot.and_then(|i| {
                    Some((
                        model.file.hunks.get(hunk)?.lines.get(i)?,
                        model.lines.get(hunk)?.get(i)?,
                    ))
                });
                match found {
                    Some((l, view)) => {
                        let number = if old { l.old_lineno } else { l.new_lineno };
                        self::line(view, l.kind, &[number], selected, palette, cx)
                            .into_any_element()
                    }
                    None => filler(palette, cx).into_any_element(),
                }
            };
            h_flex()
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .border_r_1()
                        .border_color(palette.border)
                        .child(side(left, true)),
                )
                .child(div().flex_1().min_w_0().child(side(right, false)))
                .into_any_element()
        }
    })
}
