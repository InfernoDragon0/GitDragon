//! Interactive rebase editor: the commits after a chosen base, oldest first, each with
//! Pick / Reword / Squash / Fixup / Drop and move up / down. Start hands the plan to
//! `CliRepo::interactive_rebase` (no editor opens); conflicts continue in the Conflicts view.

use git_core::conflicts::{RebaseCommit, TodoAction, TodoItem};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use ui::Palette;

use crate::repo_tab::RepoTab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}

impl Kind {
    const ALL: [Kind; 5] = [
        Kind::Pick,
        Kind::Reword,
        Kind::Squash,
        Kind::Fixup,
        Kind::Drop,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Pick => "Pick",
            Kind::Reword => "Reword",
            Kind::Squash => "Squash",
            Kind::Fixup => "Fixup",
            Kind::Drop => "Drop",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Kind::Pick => "Keep as is",
            Kind::Reword => "Keep the changes, edit the message",
            Kind::Squash => "Meld into the commit above, keeping both messages",
            Kind::Fixup => "Meld into the commit above, dropping this message",
            Kind::Drop => "Remove the commit and its changes",
        }
    }
}

struct Row {
    commit: RebaseCommit,
    kind: Kind,
    /// The new message's first line while rewording.
    subject: Entity<InputState>,
}

pub struct RebaseDialog {
    tab: Entity<RepoTab>,
    base: String,
    rows: Vec<Row>,
    merges: usize,
    error: Option<SharedString>,
}

/// Loads the plan for commits after `base` and opens the editor.
pub fn open(tab: Entity<RepoTab>, base: String, window: &mut Window, cx: &mut App) {
    let Some(repo) = tab.read(cx).repo.clone() else {
        return;
    };
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let range_base = base.clone();
        let plan = cx
            .background_executor()
            .spawn(async move { repo.rebase_plan(Some(&range_base)).await })
            .await;
        _ = window_handle.update(cx, |_, window, cx| match plan {
            Ok(plan) if plan.commits.is_empty() => {
                crate::dialogs::confirm(
                    window,
                    cx,
                    "Nothing to rebase",
                    "There are no commits between this one and HEAD.",
                    "OK",
                    false,
                    |_, _| {},
                );
            }
            Ok(plan) => {
                let dialog = cx.new(|cx| {
                    let rows = plan
                        .commits
                        .into_iter()
                        .map(|commit| Row {
                            subject: cx.new(|cx| {
                                InputState::new(window, cx).default_value(commit.subject.clone())
                            }),
                            commit,
                            kind: Kind::Pick,
                        })
                        .collect();
                    RebaseDialog {
                        tab,
                        base,
                        rows,
                        merges: plan.merges,
                        error: None,
                    }
                });
                window.open_dialog(cx, move |d, _, _| {
                    d.title("Interactive rebase")
                        .w(px(760.))
                        .child(dialog.clone())
                });
            }
            Err(err) => {
                crate::dialogs::confirm(
                    window,
                    cx,
                    "Could not list the commits",
                    err.to_string(),
                    "OK",
                    false,
                    |_, _| {},
                );
            }
        });
    })
    .detach();
}

impl RebaseDialog {
    fn set_kind(&mut self, ix: usize, kind: Kind, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.kind = kind;
        }
        self.error = None;
        cx.notify();
    }

    fn shift(&mut self, ix: usize, up: bool, cx: &mut Context<Self>) {
        let other = if up { ix.checked_sub(1) } else { Some(ix + 1) };
        if let Some(other) = other.filter(|o| *o < self.rows.len()) {
            self.rows.swap(ix, other);
            cx.notify();
        }
    }

    fn items(&self, cx: &App) -> Vec<TodoItem> {
        self.rows
            .iter()
            .map(|row| TodoItem {
                commit: row.commit.id.clone(),
                action: match row.kind {
                    Kind::Pick => TodoAction::Pick,
                    Kind::Squash => TodoAction::Squash,
                    Kind::Fixup => TodoAction::Fixup,
                    Kind::Drop => TodoAction::Drop,
                    Kind::Reword => {
                        let subject = row.subject.read(cx).value().trim().to_string();
                        // Keep the original body under the new first line.
                        let body = row
                            .commit
                            .message
                            .split_once('\n')
                            .map(|(_, rest)| rest.trim_start_matches(['\r', '\n']))
                            .unwrap_or_default();
                        TodoAction::Reword(if body.is_empty() {
                            subject
                        } else {
                            format!("{subject}\n\n{body}")
                        })
                    }
                },
            })
            .collect()
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.items(cx);
        let first_kept = items
            .iter()
            .find(|i| !matches!(i.action, TodoAction::Drop))
            .map(|i| &i.action);
        if matches!(first_kept, Some(TodoAction::Squash | TodoAction::Fixup)) {
            self.error = Some("The first kept commit can't be squashed: nothing above it.".into());
            cx.notify();
            return;
        }
        if first_kept.is_none() {
            self.error = Some("Every commit is dropped.".into());
            cx.notify();
            return;
        }
        let base = self.base.clone();
        self.tab.update(cx, |tab, cx| {
            tab.run_op("Interactive rebase", cx, move |repo| async move {
                repo.interactive_rebase(Some(&base), &items).await
            })
        });
        window.close_dialog(cx);
    }
}

impl Render for RebaseDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let count = self.rows.len();
        let rows = self.rows.iter().enumerate().map(|(ix, row)| {
            let short = row.commit.id[..row.commit.id.len().min(7)].to_string();
            let mut kinds = h_flex().gap_0p5();
            for kind in Kind::ALL {
                kinds = kinds.child(
                    Button::new(ElementId::NamedInteger(
                        SharedString::from(format!("kind-{}", kind.label())),
                        ix as u64,
                    ))
                    .xsmall()
                    .label(kind.label())
                    .tooltip(kind.tooltip())
                    .with_variant(if row.kind == kind {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.set_kind(ix, kind, cx)),
                    ),
                );
            }
            let dropped = row.kind == Kind::Drop;
            v_flex()
                .id(ElementId::NamedInteger("rebase-row".into(), ix as u64))
                .gap_1()
                .py_1()
                .border_b_1()
                .border_color(palette.border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(ElementId::NamedInteger("rebase-up".into(), ix as u64))
                                .xsmall()
                                .ghost()
                                .icon(IconName::ChevronUp)
                                .disabled(ix == 0)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.shift(ix, true, cx)
                                })),
                        )
                        .child(
                            Button::new(ElementId::NamedInteger("rebase-down".into(), ix as u64))
                                .xsmall()
                                .ghost()
                                .icon(IconName::ChevronDown)
                                .disabled(ix + 1 == count)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.shift(ix, false, cx)
                                })),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(short)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(if dropped { palette.muted } else { palette.fg })
                                .when(dropped, |d| d.line_through())
                                .child(SharedString::from(
                                    ui::sanitize_line(&row.commit.subject).into_owned(),
                                )),
                        )
                        .child(kinds),
                )
                .when(row.kind == Kind::Reword, |d| {
                    d.child(div().pl(px(64.)).child(Input::new(&row.subject).small()))
                })
        });
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child("Oldest first. Squash and fixup meld a commit into the one above it."),
            )
            .when(self.merges > 0, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.warn)
                        .child(SharedString::from(format!(
                            "{} merge commit(s) in this range will be flattened.",
                            self.merges
                        ))),
                )
            })
            .child(
                v_flex()
                    .id("rebase-rows")
                    .max_h(px(420.))
                    .overflow_y_scrollbar()
                    .children(rows),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(div().text_xs().text_color(palette.error).child(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("rebase-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("rebase-start")
                            .primary()
                            .label("Start rebase")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.start(window, cx)
                            })),
                    ),
            )
    }
}
