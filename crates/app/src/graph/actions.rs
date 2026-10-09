//! Row context menu: checkout, branch / tag here, merge / rebase, cherry-pick, revert, reset,
//! copy SHA. Write
//! operations run on the background executor; the tab refresh that follows reloads the graph
//! when refs move.

use git_core::conflicts::MergeMode;
use git_core::{CliRepo, ResetMode};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Focusable as _, ParentElement as _, Styled as _,
    WeakEntity, Window, px,
};

use super::GraphView;

/// What the menu needs to know about the right-clicked row.
#[derive(Debug, Clone)]
pub struct RowInfo {
    pub id: String,
    /// Local branches pointing at the commit.
    pub branches: Vec<String>,
    pub merge: bool,
    pub head: bool,
    /// The WIP row: only "open Changes" applies.
    pub wip: bool,
}

#[derive(Debug, Clone)]
enum RowAction {
    /// A local branch, or `None` for a detached checkout.
    Checkout(Option<String>),
    CreateBranch,
    CreateTag,
    /// Merge this commit (or its branch) into the current branch.
    Merge(MergeMode),
    /// Rebase the current branch onto this commit (or its branch).
    Rebase,
    /// Interactive rebase of the commits after this one.
    InteractiveRebase,
    CherryPick,
    Revert,
    Reset(ResetMode),
    CopySha,
}

pub fn build_menu(menu: PopupMenu, info: &RowInfo, view: WeakEntity<GraphView>) -> PopupMenu {
    let item = |label: String, action: RowAction| {
        let view = view.clone();
        let info = info.clone();
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            _ = view.update(cx, |view, cx| view.row_action(&action, &info, window, cx));
        })
    };
    if info.wip {
        return menu
            .min_w(px(200.))
            .item(
                PopupMenuItem::new("Commit changes\u{2026}").on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::actions::ShowChanges), cx)
                }),
            );
    }
    let short = &info.id[..info.id.len().min(7)];
    let target = info
        .branches
        .first()
        .map_or(short, String::as_str)
        .to_string();
    let mut menu = menu.min_w(px(240.));
    for branch in info.branches.iter().take(4) {
        menu = menu.item(item(
            format!("Checkout {branch}"),
            RowAction::Checkout(Some(branch.clone())),
        ));
    }
    menu.item(item(
        format!("Checkout {short} (detached)"),
        RowAction::Checkout(None),
    ))
    .separator()
    .item(item(
        "Create branch here\u{2026}".into(),
        RowAction::CreateBranch,
    ))
    .item(item("Create tag here\u{2026}".into(), RowAction::CreateTag))
    .separator()
    .item(
        item(
            format!("Merge {target} into current branch"),
            RowAction::Merge(MergeMode::Default),
        )
        .disabled(info.head),
    )
    .item(
        item(
            format!("Merge {target} (always a merge commit)"),
            RowAction::Merge(MergeMode::NoFastForward),
        )
        .disabled(info.head),
    )
    .item(
        item(
            format!("Rebase current branch onto {target}"),
            RowAction::Rebase,
        )
        .disabled(info.head),
    )
    .item(
        item(
            "Interactive rebase of the commits after this\u{2026}".into(),
            RowAction::InteractiveRebase,
        )
        .disabled(info.head),
    )
    .separator()
    .item(item("Cherry-pick commit".into(), RowAction::CherryPick).disabled(info.head))
    .item(item("Revert commit".into(), RowAction::Revert))
    .separator()
    .item(item(
        "Reset current branch here: soft".into(),
        RowAction::Reset(ResetMode::Soft),
    ))
    .item(item(
        "Reset current branch here: mixed".into(),
        RowAction::Reset(ResetMode::Mixed),
    ))
    .item(item(
        "Reset current branch here: hard\u{2026}".into(),
        RowAction::Reset(ResetMode::Hard),
    ))
    .separator()
    .item(item("Copy SHA".into(), RowAction::CopySha))
}

impl GraphView {
    fn row_action(
        &mut self,
        action: &RowAction,
        info: &RowInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = info.id.clone();
        let merge = info.merge;
        match action.clone() {
            RowAction::Checkout(Some(branch)) => {
                self.run_op(
                    cx,
                    move |repo| async move { repo.switch_branch(&branch).await },
                )
            }
            RowAction::Checkout(None) => {
                self.run_op(
                    cx,
                    move |repo| async move { repo.checkout_detached(&id).await },
                )
            }
            RowAction::CreateBranch => self.prompt_name(NameKind::Branch, id, window, cx),
            RowAction::CreateTag => self.prompt_name(NameKind::Tag, id, window, cx),
            RowAction::Merge(mode) => {
                let rev = info.branches.first().cloned().unwrap_or(id);
                self.run_op_undoable(cx, format!("Merging {rev}"), move |repo| async move {
                    repo.merge(&rev, mode).await
                })
            }
            RowAction::Rebase => {
                let rev = info.branches.first().cloned().unwrap_or(id);
                self.run_op_undoable(cx, format!("Rebasing onto {rev}"), move |repo| async move {
                    repo.rebase(&rev).await
                })
            }
            RowAction::InteractiveRebase => {
                crate::conflicts::rebase_dialog::open(self.tab.clone(), id, window, cx)
            }
            RowAction::CherryPick => self.run_op_undoable(
                cx,
                format!("Cherry-picking {}", short_id(&id)),
                move |repo| async move { repo.cherry_pick(&id, merge).await },
            ),
            RowAction::Revert => self.run_op_undoable(
                cx,
                format!("Reverting {}", short_id(&id)),
                move |repo| async move { repo.revert(&id, merge).await },
            ),
            RowAction::Reset(ResetMode::Hard) => self.confirm_hard_reset(id, window, cx),
            RowAction::Reset(mode) => self.run_op_undoable(
                cx,
                format!("Resetting to {}", short_id(&id)),
                move |repo| async move { repo.reset(mode, &id).await },
            ),
            RowAction::CopySha => cx.write_to_clipboard(ClipboardItem::new_string(id)),
        }
    }

    /// Runs a write operation on the tab's shared runner; the refresh after it reloads the
    /// graph if refs moved.
    pub(super) fn run_op<F, Fut>(&mut self, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        self.tab
            .update(cx, |tab, cx| tab.run_op("Graph action", cx, op));
    }

    /// [`Self::run_op`] for operations that can be undone from the reflog (the toast and the
    /// operation strip then offer an Undo).
    pub(super) fn run_op_undoable<F, Fut>(&mut self, cx: &mut Context<Self>, label: String, op: F)
    where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        self.tab.update(cx, |tab, cx| {
            tab.run_op_undoable(label, crate::undo::UndoPlan::Head, cx, op)
        });
    }

    fn confirm_hard_reset(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let short = id[..id.len().min(7)].to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let id = id.clone();
            alert
                .confirm()
                .title("Hard reset?")
                .description(format!(
                    "The current branch moves to {short}. Uncommitted changes and commits after \
                     it are lost (commits stay in the reflog for a while)."
                ))
                .ok_text("Reset")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    _ = this.update(cx, |view, cx| {
                        view.run_op_undoable(
                            cx,
                            format!("Hard reset to {}", short_id(&id)),
                            move |repo| async move { repo.reset(ResetMode::Hard, &id).await },
                        )
                    });
                    true
                })
        });
    }

    fn prompt_name(
        &mut self,
        kind: NameKind,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(kind.placeholder()));
        let message = (kind == NameKind::Tag).then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Message (optional: makes an annotated tag)")
            })
        });
        let this = cx.entity().downgrade();
        let submit = {
            let (name, message, this, id) =
                (name.clone(), message.clone(), this.clone(), id.clone());
            move |checkout: bool, window: &mut Window, cx: &mut gpui_kit::App| {
                let text = name.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return;
                }
                let note = message.as_ref().map(|m| m.read(cx).value().to_string());
                let id = id.clone();
                _ = this.update(cx, |view, cx| match kind {
                    NameKind::Branch => view.run_op(cx, move |repo| async move {
                        repo.create_branch(&text, &id, checkout).await
                    }),
                    NameKind::Tag => view.run_op(cx, move |repo| async move {
                        repo.create_tag(&text, &id, note.as_deref()).await
                    }),
                });
                window.close_dialog(cx);
            }
        };
        self.dialog_sub = Some(cx.subscribe_in(&name, window, {
            let submit = submit.clone();
            move |_, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary, .. } = event {
                    submit(*secondary, window, cx);
                }
            }
        }));
        let short = id[..id.len().min(7)].to_string();
        let focus = name.focus_handle(cx);
        window.open_dialog(cx, move |dialog, _, _| {
            let mut fields = v_flex().gap_2().child(Input::new(&name));
            if let Some(message) = &message {
                fields = fields.child(Input::new(message));
            }
            let mut buttons = h_flex().justify_end().gap_2().child(
                Button::new("name-cancel")
                    .ghost()
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            );
            if kind == NameKind::Branch {
                let submit = submit.clone();
                buttons = buttons.child(
                    Button::new("name-create-checkout")
                        .outline()
                        .label("Create & checkout")
                        .tooltip("Ctrl+Enter")
                        .on_click(move |_, window, cx| submit(true, window, cx)),
                );
            }
            let submit = submit.clone();
            buttons = buttons.child(
                Button::new("name-create")
                    .primary()
                    .label("Create")
                    .on_click(move |_, window, cx| submit(false, window, cx)),
            );
            dialog
                .title(format!("{} at {short}", kind.title()))
                .w(px(440.))
                .child(fields)
                .footer(buttons)
        });
        window.focus(&focus, cx);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameKind {
    Branch,
    Tag,
}

impl NameKind {
    fn title(self) -> &'static str {
        match self {
            NameKind::Branch => "New branch",
            NameKind::Tag => "New tag",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            NameKind::Branch => "Branch name, e.g. feature/login",
            NameKind::Tag => "Tag name, e.g. v1.2.0",
        }
    }
}

/// First seven characters of a commit id, for operation labels.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(7)]
}
