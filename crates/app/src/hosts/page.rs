//! The full-panel page of one issue or pull request, shared by both views: a header row
//! (Back, number and title, state, author, labels, Open on web) over one scrolling page with
//! the reading column (description, conversation, comment box) and the metadata column
//! (CI, branches, actions). The metadata sits to the right when the panel is wide, above the
//! reading column when it is narrow. The reading column is capped for line length.

use std::cell::Cell;
use std::rc::Rc;

use ::hosts::{Person, Thread};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, canvas,
    div, prelude::FluentBuilder as _, px,
};
use ui::{Palette, zoomed};

/// Widest reading column.
const READ_MAX: f32 = 960.;
/// The metadata column beside it.
const SIDE_WIDTH: f32 = 300.;
/// Page width from which the metadata goes beside the reading column (a 580 px reading column
/// plus the side): low enough that the default 1280 px window gets both with the side list open.
pub const WIDE_PAGE: f32 = 880.;
/// Panel width below which the side list of an open page folds away by itself.
pub const WIDE_PANEL: f32 = 900.;

/// An element's last laid-out width, so the next frame can pick a layout: wide or not.
#[derive(Clone)]
pub struct WidthProbe {
    width: Rc<Cell<f32>>,
    at: f32,
}

impl WidthProbe {
    /// Wide from `at` px on; wide until the first layout says otherwise.
    pub fn new(at: f32) -> Self {
        Self {
            width: Rc::new(Cell::new(f32::MAX)),
            at,
        }
    }

    pub fn wide(&self) -> bool {
        self.width.get() >= self.at
    }

    /// An invisible full-size child (of a `relative` parent) that records the width;
    /// crossing the threshold redraws.
    pub fn element(&self) -> impl IntoElement {
        let (cell, at) = (self.width.clone(), self.at);
        canvas(
            move |bounds, window, _| {
                let width = bounds.size.width / px(1.);
                let was = cell.replace(width);
                if (was >= at) != (width >= at) {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }
}

/// What the page shows; the views fill it in.
pub struct DetailPage {
    /// Unique per item, so each opens scrolled to the top.
    pub id: ElementId,
    pub title: String,
    pub number: u64,
    /// State pill, author line, labels.
    pub chips: Vec<AnyElement>,
    pub web_url: Option<String>,
    /// Description, conversation, comment box.
    pub main: Vec<AnyElement>,
    /// CI, branches, assignees, actions.
    pub side: Vec<AnyElement>,
    /// Before Back (the side list's unfold button).
    pub leading: Vec<AnyElement>,
}

pub fn render(
    page: DetailPage,
    probe: &WidthProbe,
    on_back: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    palette: &Palette,
    cx: &App,
) -> AnyElement {
    let wide = probe.wide();
    let mut side_blocks = page.side;
    side_blocks.extend(participants_block(&page.id, palette, cx));
    let header = h_flex()
        .flex_none()
        .w_full()
        .px_3()
        .py_2()
        .gap_3()
        .items_start()
        .border_b_1()
        .border_color(palette.border)
        .children(page.leading)
        .child(
            Button::new("detail-back")
                .small()
                .ghost()
                .icon(IconName::ArrowLeft)
                .label("Back")
                .tooltip("Back to the list (Esc)")
                .on_click(on_back),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(ui::typography::page_title(
                    format!("#{} {}", page.number, ui::sanitize_line(&page.title)),
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .items_center()
                        .children(page.chips),
                ),
        )
        .children(page.web_url.map(|url| {
            Button::new("detail-open-web")
                .small()
                .outline()
                .icon(IconName::ExternalLink)
                .label("Open on web")
                .on_click(move |_: &ClickEvent, window, cx| {
                    crate::links::open(&url, Some(window), cx)
                })
        }));
    let main = v_flex()
        .flex_1()
        .min_w_0()
        .max_w(zoomed(READ_MAX, cx))
        .gap_4()
        .children(page.main);
    let side = v_flex()
        .gap_3()
        .when(wide, |d| {
            d.flex_none()
                .w(zoomed(SIDE_WIDTH, cx))
                .pl_4()
                .border_l_1()
                .border_color(palette.border)
        })
        .when(!wide, |d| {
            d.w_full()
                .max_w(zoomed(READ_MAX, cx))
                .pb_3()
                .border_b_1()
                .border_color(palette.border)
        })
        .children(side_blocks);
    let body = if wide {
        h_flex()
            .w_full()
            .items_start()
            .justify_center()
            .gap_4()
            .child(main)
            .child(side)
            .into_any_element()
    } else {
        v_flex()
            .w_full()
            .items_center()
            .gap_4()
            .child(side)
            .child(main.w_full())
            .into_any_element()
    };
    v_flex()
        .relative()
        .size_full()
        .bg(palette.panel)
        .child(probe.element())
        .child(header)
        .child(
            div()
                .id(page.id)
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(div().w_full().px_4().py_3().child(body)),
        )
        .into_any_element()
}

/// A labelled block of the metadata column.
pub fn side_block(title: &str, body: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .gap_1()
        .child(ui::typography::section_title(
            SharedString::from(title.to_uppercase()),
            cx,
        ))
        .child(body)
        .into_any_element()
}

/// Faces shown before the rest fold into "+N".
const FACES: usize = super::conversation::FACES;
/// Face diameter.
const FACE: f32 = 24.;
/// Names listed in the "+N" tooltip.
const FOLDED_NAMES: usize = 40;

/// The thread a page shows, from its element id (`pr-page` / `issue-page` plus the number).
fn page_thread(id: &ElementId) -> Option<Thread> {
    match id {
        ElementId::NamedInteger(name, n) if name.as_ref() == "pr-page" => {
            Some(Thread::PullRequest(*n))
        }
        ElementId::NamedInteger(name, n) if name.as_ref() == "issue-page" => {
            Some(Thread::Issue(*n))
        }
        _ => None,
    }
}

/// "Participants . N": the people the conversation loaded so far has seen, as round faces
/// (their picture when the avatar pipeline has it, else coloured initials) with the login in a
/// tooltip, and a "+N" chip for the overflow. Nothing until someone is known.
fn participants_block(id: &ElementId, palette: &Palette, cx: &App) -> Option<AnyElement> {
    let people = super::conversation::thread_people(page_thread(id)?, cx);
    if people.is_empty() {
        return None;
    }
    let total = people.len();
    let folded: Vec<String> = people
        .iter()
        .skip(FACES)
        .map(|p| ui::sanitize_line(&p.login).into_owned())
        .collect();
    let faces = people
        .iter()
        .take(FACES)
        .enumerate()
        .map(|(ix, p)| face(ix, p, palette, cx));
    let more = (!folded.is_empty()).then(|| {
        let mut names = folded
            .iter()
            .take(FOLDED_NAMES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if folded.len() > FOLDED_NAMES {
            names.push_str(", \u{2026}");
        }
        let tip = SharedString::from(names);
        div()
            .id("participants-more")
            .flex_none()
            .h(zoomed(FACE, cx))
            .min_w(zoomed(FACE, cx))
            .px_1p5()
            .rounded_full()
            .bg(palette.panel2)
            .border_1()
            .border_color(palette.border)
            .flex()
            .items_center()
            .justify_center()
            .text_size(zoomed(10., cx))
            .text_color(palette.muted)
            .child(SharedString::from(format!("+{}", folded.len())))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
    });
    Some(side_block(
        &format!("Participants \u{b7} {total}"),
        h_flex().flex_wrap().gap_1().children(faces).children(more),
        cx,
    ))
}

/// One round face; a click opens the profile on the host when it has one.
fn face(ix: usize, person: &Person, palette: &Palette, cx: &App) -> AnyElement {
    let login = SharedString::from(ui::sanitize_line(&person.login).into_owned());
    let size = zoomed(FACE, cx);
    let picture = super::conversation::avatar_key(person)
        .or_else(|| {
            person
                .avatar_url
                .as_deref()
                .and_then(crate::avatars::url_key)
        })
        .and_then(|key| crate::avatars::image(&key, cx));
    let base = div()
        .id(ElementId::NamedInteger("participant".into(), ix as u64))
        .flex_none()
        .size(size)
        .rounded_full()
        .overflow_hidden();
    let base = match picture {
        Some(image) => base.child(gpui_kit::img(image).size_full().rounded_full()),
        None => base
            .bg(palette.lane(crate::graph::format::avatar_slot(&person.login)))
            .flex()
            .items_center()
            .justify_center()
            .text_size(zoomed(10., cx))
            .font_weight(gpui_kit::FontWeight::BOLD)
            .text_color(palette.white)
            .child(SharedString::from(crate::graph::format::initials(
                &person.login,
            ))),
    };
    let profile = person.web_url.clone().filter(|u| u.starts_with("https://"));
    let tip = login;
    base.tooltip(move |window, cx| {
        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
    })
    .when_some(profile, |d, url| {
        d.cursor_pointer()
            .hover(|s| s.opacity(0.8))
            .on_click(move |_: &ClickEvent, window, cx| crate::links::open(&url, Some(window), cx))
    })
    .into_any_element()
}

/// The description as Markdown, or a muted "No description.".
pub fn description(body: Option<&str>, id: ElementId, palette: &Palette) -> AnyElement {
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(text) => super::conversation::body_view(id, text, ::hosts::BodyFormat::Markdown),
        None => div()
            .text_sm()
            .text_color(palette.muted)
            .child("No description.")
            .into_any_element(),
    }
}

// ---- the side list of an open page ------------------------------------------------------

/// Width of the compact list beside an open page.
pub const SIDE_LIST_WIDTH: f32 = 280.;
/// Height of one compact row.
pub const SIDE_ROW_HEIGHT: f32 = 44.;

/// One compact row: what the views know of an item.
pub struct SideRow {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub state: SharedString,
    pub color: gpui_kit::Hsla,
}

/// A compact row: state dot, `#n`, the title (full in a tooltip), author and state word.
pub fn side_row(
    ix: usize,
    row: SideRow,
    open: bool,
    palette: &Palette,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let full = SharedString::from(ui::sanitize_line(&row.title).into_owned());
    let tip = full.clone();
    h_flex()
        .id(ElementId::NamedInteger("side-row".into(), ix as u64))
        .h(zoomed(SIDE_ROW_HEIGHT, cx))
        .w_full()
        .px_2()
        .gap_2()
        .cursor_pointer()
        .border_l_2()
        .border_color(if open {
            palette.accent
        } else {
            palette.accent.opacity(0.)
        })
        .when(open, |d| d.bg(palette.accent_soft))
        .when(!open, |d| d.hover(|s| s.bg(palette.hover)))
        .child(super::dot(row.color, 7.))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .text_xs()
                        .child(
                            div()
                                .flex_none()
                                .text_color(palette.muted)
                                .child(SharedString::from(format!("#{}", row.number))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(palette.emph)
                                .child(full),
                        ),
                )
                .child(div().truncate().text_xs().text_color(palette.muted).child(
                    SharedString::from(format!(
                        "{} \u{b7} {}",
                        ui::sanitize_line(&row.author),
                        row.state
                    )),
                )),
        )
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .on_click(on_click)
        .into_any_element()
}

/// The side column around a compact list: its title and the fold button.
pub fn side_column(
    title: &str,
    list: AnyElement,
    palette: &Palette,
    cx: &App,
    on_fold: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    v_flex()
        .flex_none()
        .w(zoomed(SIDE_LIST_WIDTH, cx))
        .h_full()
        .border_r_1()
        .border_color(palette.border)
        .bg(palette.panel2)
        .child(
            h_flex()
                .flex_none()
                .h(zoomed(38., cx))
                .px_2()
                .gap_1()
                .border_b_1()
                .border_color(palette.border)
                .child(ui::typography::section_title(
                    SharedString::from(title.to_uppercase()),
                    cx,
                ))
                .child(div().flex_1())
                .child(
                    Button::new("side-list-fold")
                        .xsmall()
                        .ghost()
                        .icon(IconName::PanelLeft)
                        .tooltip("Hide the list")
                        .on_click(on_fold),
                ),
        )
        .child(list)
        .into_any_element()
}

/// The button that brings a folded side list back.
pub fn unfold_button(
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    Button::new("side-list-unfold")
        .small()
        .ghost()
        .icon(IconName::PanelLeft)
        .tooltip("Show the list")
        .on_click(on_click)
        .into_any_element()
}
