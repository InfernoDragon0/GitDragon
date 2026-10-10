//! Text roles, after the Maomi board: a display face for titles, small caps-style eyebrows for
//! sections, semibold item titles, body, caption and mono. Each helper returns a [`Div`] sized
//! with [`zoomed`] so it follows the UI zoom, coloured from the [`Palette`], and set in the
//! families from [`fonts`]. Chain more styling (padding, `truncate`, `child`) as usual.
//!
//! | role | size | weight | colour |
//! |------|------|--------|--------|
//! | [`page_title`] | 18 | semibold, display | emphasis |
//! | [`section_title`] | 11.5 | semibold, display | muted |
//! | [`item_title`] | 13 | semibold | emphasis |
//! | [`body`] | 13 | normal | text |
//! | [`caption`] | 11.5 | normal | muted |
//! | [`mono`] | 12 | normal, mono | text |

use gpui_kit::{Div, FontWeight, ParentElement as _, SharedString, Styled as _, div};

use crate::theme::{Palette, fonts, zoomed};
use gpui_kit::App;

fn text(size: f32, cx: &App) -> Div {
    div()
        .text_size(zoomed(size, cx))
        .line_height(zoomed(size * 1.4, cx))
}

/// Panel / page heading (PRs, Issues, Boards, Settings, Welcome).
pub fn page_title(label: impl Into<SharedString>, cx: &App) -> Div {
    text(18., cx)
        .font_family(fonts(cx).display)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(Palette::of(cx).emph)
        .child(label.into())
}

/// Larger dialog / commit-subject heading (15px).
pub fn subtitle(label: impl Into<SharedString>, cx: &App) -> Div {
    text(15., cx)
        .font_family(fonts(cx).display)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(Palette::of(cx).emph)
        .child(label.into())
}

/// Section eyebrow (sidebar groups, settings sections). Pass the text already upper-cased if it
/// should be.
pub fn section_title(label: impl Into<SharedString>, cx: &App) -> Div {
    text(11.5, cx)
        .font_family(fonts(cx).display)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(Palette::of(cx).muted)
        .child(label.into())
}

/// Card / list item title.
pub fn item_title(label: impl Into<SharedString>, cx: &App) -> Div {
    text(13., cx)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(Palette::of(cx).emph)
        .child(label.into())
}

/// Plain body text.
pub fn body(label: impl Into<SharedString>, cx: &App) -> Div {
    text(13., cx)
        .text_color(Palette::of(cx).fg)
        .child(label.into())
}

/// Small muted text (hints, timestamps).
pub fn caption(label: impl Into<SharedString>, cx: &App) -> Div {
    text(11.5, cx)
        .text_color(Palette::of(cx).muted)
        .child(label.into())
}

/// Code, hashes, paths.
pub fn mono(label: impl Into<SharedString>, cx: &App) -> Div {
    text(12., cx)
        .font_family(fonts(cx).mono)
        .text_color(Palette::of(cx).fg)
        .child(label.into())
}
