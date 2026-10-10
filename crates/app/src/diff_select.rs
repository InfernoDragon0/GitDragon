//! Row selection and copy for the diff views (commit file diff, staging diff): click, shift-click
//! and drag select whole rows; Ctrl/Cmd+C or the right-click menu copies them (see
//! `ui::diff::copy` for what each mode produces); Escape clears.

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{
    AnyElement, App, Context, Div, ElementId, FocusHandle, InteractiveElement as _,
    IntoElement as _, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Styled as _, WeakEntity, div, px,
};
use ui::diff::copy::RowSelection;

use crate::actions::{ClearDiffSelection, CopyDiffLines, CopyDiffPatch};

/// A view that shows a diff list with a [`RowSelection`].
pub trait DiffSelect: Sized + 'static {
    fn selection(&mut self) -> &mut RowSelection;
    fn focus_handle(&self) -> &FocusHandle;
    /// Left mouse down on a row, before the selection changes (line staging snapshots its
    /// picks here).
    fn press_started(&mut self) {}
    /// The user dragged or shift-clicked the range to a new extent (line staging picks the
    /// changed lines in it).
    fn range_extended(&mut self, _cx: &mut Context<Self>) {}
    /// Copies the selected rows to the clipboard: the lines' code, or as a patch.
    fn copy_selection(&mut self, patch: bool, cx: &mut Context<Self>);
}

/// Makes `root` the focusable diff view: the copy shortcuts and Escape work once a row was
/// clicked.
pub fn container<V: DiffSelect>(root: Div, focus: &FocusHandle, cx: &mut Context<V>) -> Div {
    root.key_context("DiffView")
        .track_focus(focus)
        .on_action(
            cx.listener(|this: &mut V, _: &CopyDiffLines, _, cx| this.copy_selection(false, cx)),
        )
        .on_action(
            cx.listener(|this: &mut V, _: &CopyDiffPatch, _, cx| this.copy_selection(true, cx)),
        )
        .on_action(cx.listener(|this: &mut V, _: &ClearDiffSelection, _, cx| {
            if this.selection().is_empty() {
                // Nothing to clear: Escape belongs to whatever holds the view (a page's Back).
                cx.propagate();
                return;
            }
            this.selection().clear();
            cx.notify();
        }))
}

/// Wraps list row `ix` with the selection mouse handling.
pub fn row<V: DiffSelect>(ix: usize, row: AnyElement, cx: &mut Context<V>) -> AnyElement {
    div()
        .id(ElementId::NamedInteger("diff-select".into(), ix as u64))
        .w_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this: &mut V, event: &MouseDownEvent, window, cx| {
                window.focus(this.focus_handle(), cx);
                let extend = event.modifiers.shift;
                this.press_started();
                this.selection().press(ix, extend);
                if extend {
                    this.range_extended(cx);
                }
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this: &mut V, _: &MouseDownEvent, window, cx| {
                window.focus(this.focus_handle(), cx);
                this.selection().ensure(ix);
                cx.notify();
            }),
        )
        .on_mouse_move(
            cx.listener(move |this: &mut V, event: &MouseMoveEvent, _, cx| {
                if !event.dragging() {
                    this.selection().release();
                } else if this.selection().drag_to(ix) {
                    this.range_extended(cx);
                    cx.notify();
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this: &mut V, _: &MouseUpEvent, _, _| this.selection().release()),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this: &mut V, _: &MouseUpEvent, _, _| this.selection().release()),
        )
        .child(row)
        .into_any_element()
}

/// The right-click menu of a diff list.
pub fn menu<V: DiffSelect>(menu: PopupMenu, view: WeakEntity<V>) -> PopupMenu {
    let item = |label: &'static str, patch: bool| {
        let view = view.clone();
        PopupMenuItem::new(label).on_click(move |_, _, cx: &mut App| {
            _ = view.update(cx, |view, cx| view.copy_selection(patch, cx));
        })
    };
    menu.min_w(px(180.))
        .item(item("Copy lines", false))
        .item(item("Copy as patch", true))
}
