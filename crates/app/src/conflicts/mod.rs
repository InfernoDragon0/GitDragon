//! The Conflicts view: conflicted files on the left; for the selected file, each conflict block
//! with ours / base / theirs side by side and Take ours / theirs / both, the resolved file in an
//! editable result pane, and Save & mark resolved. Files without markers (deleted on one side,
//! binary) take a whole side. The banner over every view carries Continue / Skip / Abort.

pub mod external;
pub mod rebase_dialog;

use bstr::ByteSlice as _;
use git_core::GitRepo as _;
use git_core::conflicts::{Choice, ConflictFile, ConflictKind, ConflictText, Side, Stages};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Editor as EditorInput, EditorState, InputEvent};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use ui::{Palette, zoomed};

use crate::dialogs;
use crate::file_editor::{Editors, HasEditors};
use crate::repo_tab::{OpStep, RepoTab};
use external::Editor;

const LIST_WIDTH: f32 = 280.;
const RESULT_HEIGHT: f32 = 260.;
/// Larger files are resolved by whole side or in an external editor.
const MAX_TEXT: usize = 4 * 1024 * 1024;

enum FileView {
    Empty,
    Loading,
    /// A text file with conflict markers.
    Text {
        original: Vec<u8>,
        parsed: ConflictText,
        choices: Vec<Option<Choice>>,
        /// The result pane can't hold non-UTF-8 text faithfully: choices only.
        utf8: bool,
    },
    /// Resolved by taking a whole side (deleted on one side, binary, no markers).
    Sides {
        reason: SharedString,
    },
    Message(SharedString),
}

pub struct ConflictsView {
    tab: Entity<RepoTab>,
    selected: Option<ConflictFile>,
    file: FileView,
    result: Entity<EditorState>,
    /// What the result pane was last set to (to tell hand edits apart).
    result_set: String,
    editors: Vec<(Editor, std::path::PathBuf)>,
    seen_generation: u64,
    load_task: Option<Task<()>>,
    error: Option<SharedString>,
    /// The in-app editor opened with "Edit file"; shown in place of the file's blocks.
    file_editors: Editors,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl HasEditors for ConflictsView {
    fn editors(&mut self) -> &mut Editors {
        &mut self.file_editors
    }
}

impl ConflictsView {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let result = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        let mut _subs = vec![
            cx.observe_in(&tab, window, |this, _, window, cx| {
                this.on_tab_changed(window, cx)
            }),
            cx.subscribe(&result, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        // The custom editor from Settings can change while this view is open.
        if let Some(prefs) = crate::prefs::handle(cx) {
            _subs.push(cx.observe(&prefs, |this, _, cx| {
                this.editors =
                    external::available(crate::prefs::get(cx).external_editor.as_deref());
                cx.notify();
            }));
        }
        let mut view = Self {
            tab,
            selected: None,
            file: FileView::Empty,
            result,
            result_set: String::new(),
            editors: external::available(crate::prefs::get(cx).external_editor.as_deref()),
            seen_generation: 0,
            load_task: None,
            error: None,
            file_editors: Editors::default(),
            focus: cx.focus_handle(),
            _subs,
        };
        view.on_tab_changed(window, cx);
        view
    }

    fn on_tab_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.tab.read(cx);
        if tab.generation == self.seen_generation {
            return;
        }
        self.seen_generation = tab.generation;
        let conflicts = tab.conflicts.clone();
        let keep = self
            .selected
            .as_ref()
            .and_then(|s| conflicts.iter().find(|c| c.path == s.path).cloned());
        match keep.or_else(|| conflicts.first().cloned()) {
            // Same file: reload only if it changed on disk (another editor saved it).
            Some(file) => self.load(file, false, window, cx),
            None => {
                self.selected = None;
                self.file = FileView::Empty;
                self.file_editors.follow(None, cx);
                cx.notify();
            }
        }
    }

    fn select(&mut self, file: ConflictFile, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.file_editors.follow(Some(&file.path), cx);
        self.load(file, true, window, cx);
    }

    /// Opens the selected file, conflict markers and all, in the in-app editor.
    fn edit_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.selected.clone().filter(|f| f.kind.has_markers()) else {
            return;
        };
        let tab = self.tab.clone();
        self.open_editor(&tab, file.path, None, "\u{2190} Conflicts", window, cx);
    }

    /// Loads `file`; with `force` false an already loaded, unchanged file keeps its choices
    /// and hand edits.
    fn load(
        &mut self,
        file: ConflictFile,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.tab.read(cx).repo.clone() else {
            return;
        };
        let same = self.selected.as_ref() == Some(&file);
        self.selected = Some(file.clone());
        if !same || force {
            self.file = FileView::Loading;
        }
        let path = file.path.clone();
        let kind = file.kind;
        self.load_task = Some(cx.spawn_in(window, async move |this, cx| {
            let content = if kind.has_markers() {
                cx.background_executor()
                    .spawn(async move { repo.read_worktree_file(&path).await })
                    .await
                    .map(Some)
            } else {
                Ok(None)
            };
            _ = this.update_in(cx, |view, window, cx| {
                view.loaded(kind, content, force || !same, window, cx)
            });
        }));
        cx.notify();
    }

    fn loaded(
        &mut self,
        kind: ConflictKind,
        content: git_core::Result<Option<Vec<u8>>>,
        fresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = match content {
            Ok(Some(content)) => content,
            Ok(None) => {
                self.file = FileView::Sides {
                    reason: format!("This file was {}.", kind.label()).into(),
                };
                cx.notify();
                return;
            }
            Err(err) => {
                self.file = FileView::Message(err.to_string().into());
                cx.notify();
                return;
            }
        };
        if let FileView::Text { original, .. } = &self.file
            && !fresh
            && *original == content
        {
            return;
        }
        let binary = content.iter().take(8000).any(|&b| b == 0);
        if binary || content.len() > MAX_TEXT {
            self.file = FileView::Sides {
                reason: if binary {
                    "Binary file: take one side.".into()
                } else {
                    "Large file: take one side or use an external editor.".into()
                },
            };
            cx.notify();
            return;
        }
        let parsed = ConflictText::parse(&content);
        if parsed.conflict_count() == 0 {
            self.file = FileView::Sides {
                reason:
                    "No conflict markers left in this file: mark it resolved, or take one side."
                        .into(),
            };
            cx.notify();
            return;
        }
        let choices = vec![None; parsed.conflict_count()];
        let utf8 = content.to_str().is_ok();
        self.file = FileView::Text {
            original: content,
            parsed,
            choices,
            utf8,
        };
        self.refresh_result(window, cx);
    }

    /// Rewrites the result pane from the current choices.
    fn refresh_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let FileView::Text {
            parsed,
            choices,
            utf8,
            ..
        } = &self.file
        else {
            return;
        };
        let text = if *utf8 {
            String::from_utf8_lossy(&parsed.resolve(choices)).into_owned()
        } else {
            "This file is not UTF-8: pick a side per block; the result is written as bytes.".into()
        };
        self.result_set = text.clone();
        self.result
            .update(cx, |state, cx| state.set_value(text, window, cx));
        cx.notify();
    }

    fn choose(
        &mut self,
        block: usize,
        choice: Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let FileView::Text { choices, .. } = &mut self.file
            && let Some(slot) = choices.get_mut(block)
        {
            *slot = Some(choice);
        }
        self.refresh_result(window, cx);
    }

    fn choose_all(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        if let FileView::Text { choices, .. } = &mut self.file {
            choices.iter_mut().for_each(|c| *c = Some(choice.clone()));
        }
        self.refresh_result(window, cx);
    }

    /// The bytes Save writes: the hand-edited result pane, else the choices applied.
    fn resolved_bytes(&self, cx: &App) -> Option<Vec<u8>> {
        let FileView::Text {
            original,
            parsed,
            choices,
            utf8,
        } = &self.file
        else {
            return None;
        };
        let edited = self.result.read(cx).value().to_string();
        if !*utf8 || edited == self.result_set {
            return Some(parsed.resolve(choices));
        }
        // The editor works in `\n`; keep the file's CRLF endings.
        let crlf = original.find(b"\r\n").is_some() && !edited.contains('\r');
        Some(if crlf {
            edited.replace('\n', "\r\n").into_bytes()
        } else {
            edited.into_bytes()
        })
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(file), Some(bytes)) = (self.selected.clone(), self.resolved_bytes(cx)) else {
            return;
        };
        let name = file.path.to_str_lossy().into_owned();
        let write = {
            let tab = self.tab.clone();
            move |cx: &mut App| {
                let (path, bytes) = (file.path.clone(), bytes.clone());
                tab.update(cx, |tab, cx| {
                    tab.run_op(format!("Resolving {name}"), cx, move |repo| async move {
                        repo.resolve_with(&path, &bytes).await
                    })
                });
            }
        };
        let bytes_now = self.resolved_bytes(cx).unwrap_or_default();
        if git_core::conflicts::has_markers(&bytes_now) {
            dialogs::confirm(
                window,
                cx,
                "Conflict markers left",
                "The result still contains conflict markers. Mark it resolved anyway?",
                "Mark resolved",
                false,
                move |_, cx| write(cx),
            );
        } else {
            write(cx);
        }
    }

    fn take_side(&mut self, side: Side, cx: &mut Context<Self>) {
        let Some(file) = self.selected.clone() else {
            return;
        };
        let name = file.path.to_str_lossy().into_owned();
        self.tab.update(cx, |tab, cx| {
            tab.run_op(format!("Resolving {name}"), cx, move |repo| async move {
                repo.take_side(&file.path, side).await
            })
        });
    }

    fn mark_resolved(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.selected.clone() else {
            return;
        };
        self.tab.update(cx, |tab, cx| {
            tab.run_op("Marking resolved", cx, move |repo| async move {
                repo.mark_resolved(&[file.path]).await
            })
        });
    }

    fn open_external(&mut self, editor: Editor, cx: &mut Context<Self>) {
        let Some(program) = self
            .editors
            .iter()
            .find(|(e, _)| *e == editor)
            .map(|(_, p)| p.clone())
        else {
            return;
        };
        let (Some(file), Some(repo)) = (self.selected.clone(), self.tab.read(cx).repo.clone())
        else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    git_core::ops::validate_repo_path(&file.path)?;
                    let target = repo.root().join(file.path.to_path_lossy());
                    let stages = match editor {
                        Editor::VsCode if file.kind.has_markers() => {
                            let stages: Stages = repo.conflict_stages(&file.path).await?;
                            Some(external::write_stages(&target, &stages)?)
                        }
                        _ => None,
                    };
                    let args =
                        external::args(editor, &target, stages.as_ref()).ok_or_else(|| {
                            git_core::GitError::Io(std::io::Error::other("path is not absolute"))
                        })?;
                    external::launch(&program, &args)?;
                    Ok::<_, git_core::GitError>(())
                })
                .await;
            _ = this.update(cx, |view, cx| {
                view.error = result.err().map(|e| e.to_string().into());
                cx.notify();
            });
        });
        task.detach();
    }
}

/// Continue / Skip / Abort the tab's operation; Abort confirms first.
pub fn finish_operation(tab: &Entity<RepoTab>, step: OpStep, window: &mut Window, cx: &mut App) {
    let Some(op) = tab.read(cx).in_progress.as_ref().map(|p| p.op) else {
        return;
    };
    if step != OpStep::Abort {
        tab.update(cx, |tab, cx| tab.finish_operation(step, cx));
        return;
    }
    let tab = tab.clone();
    dialogs::confirm(
        window,
        cx,
        format!("Abort {}?", op.label().to_lowercase()),
        "Everything goes back to how it was before it started; resolutions so far are lost.",
        "Abort",
        true,
        move |_, cx| tab.update(cx, |tab, cx| tab.finish_operation(OpStep::Abort, cx)),
    );
}

/// The banner shown over every view while an operation is in progress.
pub fn banner(
    tab: &Entity<RepoTab>,
    on_resolve: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    let t = tab.read(cx);
    let state = t.in_progress.as_ref()?;
    let palette = Palette::of(cx);
    let conflicts = t.conflicts.len();
    let busy = t.is_busy();
    let mut title = match (state.op, &state.branch) {
        (git_core::conflicts::Operation::Rebase, Some(branch)) => format!("Rebasing {branch}"),
        (op, _) => format!("{} in progress", op.label()),
    };
    if let Some((step, total)) = state.step {
        title.push_str(&format!(" · step {step} of {total}"));
    }
    if let Some(message) = &state.message {
        title.push_str(&format!(" · {message}"));
    }
    let detail = match conflicts {
        0 => "No conflicts left: continue when ready.".to_string(),
        1 => "1 conflicted file".to_string(),
        n => format!("{n} conflicted files"),
    };
    let button = |id: &'static str, label: &'static str, step: OpStep| {
        let tab = tab.clone();
        Button::new(id)
            .xsmall()
            .label(label)
            .disabled(busy)
            .on_click(move |_, window, cx| finish_operation(&tab, step, window, cx))
    };
    Some(
        h_flex()
            .id("op-banner")
            .w_full()
            .h(zoomed(34., cx))
            .px_3()
            .gap_2()
            .flex_none()
            .bg(palette.warn.opacity(0.14))
            .border_b_1()
            .border_color(palette.warn.opacity(0.5))
            .child(
                div()
                    .text_sm()
                    .text_color(palette.emph)
                    .truncate()
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(if conflicts > 0 {
                        palette.warn
                    } else {
                        palette.ok
                    })
                    .child(SharedString::from(detail)),
            )
            .child(div().flex_1())
            .when(conflicts > 0, |d| {
                d.child(
                    Button::new("banner-resolve")
                        .xsmall()
                        .outline()
                        .label("Resolve conflicts")
                        .on_click(move |_, window, cx| on_resolve(window, cx)),
                )
            })
            .child(
                button("banner-continue", "Continue", OpStep::Continue)
                    .primary()
                    .disabled(busy || conflicts > 0),
            )
            .when(state.op.can_skip(), |d| {
                d.child(button("banner-skip", "Skip commit", OpStep::Skip).ghost())
            })
            .child(button("banner-abort", "Abort", OpStep::Abort).ghost())
            .into_any_element(),
    )
}

// ---- rendering ------------------------------------------------------------------------------

impl ConflictsView {
    fn render_list(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab.read(cx);
        let rows = tab.conflicts.iter().enumerate().map(|(ix, file)| {
            let selected = self.selected.as_ref().is_some_and(|s| s.path == file.path);
            let name = file.path.to_str_lossy().into_owned();
            let (dir, base) = match name.rsplit_once('/') {
                Some((dir, base)) => (Some(dir.to_string()), base.to_string()),
                None => (None, name.clone()),
            };
            let file = file.clone();
            h_flex()
                .id(ElementId::NamedInteger("conflict-row".into(), ix as u64))
                .h(zoomed(28., cx))
                .px_2()
                .gap_2()
                .cursor_pointer()
                .when(selected, |d| {
                    d.bg(palette.accent_soft).text_color(palette.emph)
                })
                .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
                .child(
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(palette.warn)
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(SharedString::from(base))
                        .when_some(dir, |d, dir| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .truncate()
                                    .child(SharedString::from(dir)),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(file.kind.label()),
                )
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.select(file.clone(), window, cx);
                    if event.click_count() >= 2 {
                        this.edit_file(window, cx);
                    }
                }))
        });
        v_flex()
            .id("conflict-list")
            .w(px(LIST_WIDTH))
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .children(rows)
    }

    fn render_block(
        &self,
        ix: usize,
        total: usize,
        block: &git_core::conflicts::Block,
        choice: Option<&Choice>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = Palette::of(cx);
        let mono = cx.theme().mono_font_family.clone();
        let busy = self.tab.read(cx).is_busy();
        let pick = |id: &'static str, label: &'static str, value: Choice| {
            let active = choice == Some(&value);
            Button::new(ElementId::NamedInteger(id.into(), ix as u64))
                .xsmall()
                .label(label)
                .disabled(busy)
                .with_variant(if active {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.choose(ix, value.clone(), window, cx)
                }))
        };
        let column = |title: String, text: &[u8], tint: gpui_kit::Hsla| {
            v_flex()
                .flex_1()
                .min_w_0()
                .border_1()
                .border_color(palette.border)
                .rounded_md()
                .overflow_hidden()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .text_color(palette.muted)
                        .bg(tint.opacity(0.12))
                        .truncate()
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .font_family(mono.clone())
                        .text_size(cx.theme().mono_font_size)
                        .text_color(palette.fg)
                        .whitespace_nowrap()
                        .overflow_x_hidden()
                        .children(text.lines().map(|l| {
                            div().child(SharedString::from(l.to_str_lossy().into_owned()))
                        }))
                        .when(text.is_empty(), |d| {
                            d.text_color(palette.muted).child("(nothing)")
                        }),
                )
        };
        let mut columns = h_flex().gap_2().items_start().child(column(
            format!("Ours · {}", block.ours_label),
            &block.ours,
            palette.ok,
        ));
        if let Some(base) = &block.base {
            columns = columns.child(column(
                format!("Base · {}", block.base_label.clone().unwrap_or_default()),
                base,
                palette.muted,
            ));
        }
        columns = columns.child(column(
            format!("Theirs · {}", block.theirs_label),
            &block.theirs,
            palette.accent,
        ));
        v_flex()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(if choice.is_some() {
                                palette.ok
                            } else {
                                palette.warn
                            })
                            .child(SharedString::from(format!(
                                "Conflict {} of {total}{}",
                                ix + 1,
                                if choice.is_some() { " · resolved" } else { "" }
                            ))),
                    )
                    .child(div().flex_1())
                    .child(pick("pick-ours", "Take ours", Choice::Ours))
                    .child(pick("pick-theirs", "Take theirs", Choice::Theirs))
                    .child(pick("pick-both", "Both (ours first)", Choice::Both))
                    .child(pick(
                        "pick-both-rev",
                        "Both (theirs first)",
                        Choice::BothReversed,
                    ))
                    .when(block.base.is_some(), |d| {
                        d.child(pick("pick-base", "Base", Choice::Base))
                    }),
            )
            .child(columns)
            .into_any_element()
    }

    fn render_external(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editable = self.selected.as_ref().is_some_and(|f| f.kind.has_markers());
        h_flex()
            .gap_1()
            .when(editable, |d| {
                d.child(
                    Button::new("edit-file")
                        .xsmall()
                        .ghost()
                        .label("Edit file")
                        .tooltip("Edit the file with its conflict markers in the built-in editor")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.edit_file(window, cx)
                        })),
                )
            })
            .children(self.editors.iter().map(|(editor, program)| {
                let editor = *editor;
                Button::new(SharedString::from(format!("open-{editor:?}")))
                    .xsmall()
                    .ghost()
                    .label(external::label(editor, program))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_external(editor, cx)
                    }))
            }))
    }

    fn render_sides(
        &self,
        reason: &SharedString,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.tab.read(cx).is_busy();
        let kind = self.selected.as_ref().map(|s| s.kind);
        let (ours, theirs) = match kind {
            Some(ConflictKind::DeletedByThem) => ("Keep the file (ours)", "Delete it (theirs)"),
            Some(ConflictKind::DeletedByUs) => {
                ("Keep it deleted (ours)", "Restore the file (theirs)")
            }
            Some(ConflictKind::BothDeleted) => ("Delete it", "Delete it"),
            _ => ("Take ours", "Take theirs"),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(div().text_sm().text_color(palette.fg).child(reason.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("side-ours")
                            .outline()
                            .label(ours)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.take_side(Side::Ours, cx)
                            })),
                    )
                    .when(kind != Some(ConflictKind::BothDeleted), |d| {
                        d.child(
                            Button::new("side-theirs")
                                .outline()
                                .label(theirs)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.take_side(Side::Theirs, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("side-resolved")
                            .ghost()
                            .label("Mark resolved as is")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.mark_resolved(cx)),
                            ),
                    ),
            )
            .child(self.render_external(cx))
            .into_any_element()
    }

    fn render_file(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: SharedString| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(palette.muted)
                .child(text)
                .into_any_element()
        };
        if let Some(editor) = self.file_editors.shown(cx) {
            return editor.into_any_element();
        }
        let tab = self.tab.read(cx);
        let (in_progress, no_conflicts, busy) = (
            tab.in_progress.clone(),
            tab.conflicts.is_empty(),
            tab.is_busy(),
        );
        match &self.file {
            FileView::Empty if in_progress.is_none() => crate::empty::EmptyState::new(
                gpui_kit::assets::IconName::GitMergeConflict,
                "No conflicts",
                "Nothing is being merged, rebased or cherry-picked. When one stops on a conflict, the files show up here.",
            )
            .into_any_element(),
            FileView::Empty => message(match (&in_progress, no_conflicts) {
                (Some(state), true) => format!(
                    "All conflicts are resolved. Continue the {} from the banner.",
                    state.op.label().to_lowercase()
                )
                .into(),
                (None, _) => "No merge, rebase or cherry-pick in progress.".into(),
                _ => "Pick a file.".into(),
            }),
            FileView::Loading => message("Loading…".into()),
            FileView::Message(text) => message(text.clone()),
            FileView::Sides { reason } => self.render_sides(reason, palette, cx),
            FileView::Text {
                parsed,
                choices,
                utf8,
                ..
            } => {
                let total = choices.len();
                let open = choices.iter().filter(|c| c.is_none()).count();
                let blocks: Vec<AnyElement> = parsed
                    .blocks()
                    .enumerate()
                    .map(|(ix, block)| {
                        self.render_block(ix, total, block, choices[ix].as_ref(), cx)
                    })
                    .collect();
                let edited = self.result.read(cx).value().as_ref() != self.result_set;
                let status = if edited {
                    "Result edited by hand".to_string()
                } else if open == 0 {
                    "All blocks resolved".to_string()
                } else {
                    format!("{open} of {total} blocks open")
                };
                v_flex()
                    .size_full()
                    .child(
                        h_flex()
                            .px_2()
                            .h(zoomed(32., cx))
                            .gap_1()
                            .flex_none()
                            .border_b_1()
                            .border_color(palette.border)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .child(SharedString::from(status)),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("all-ours")
                                    .xsmall()
                                    .ghost()
                                    .label("All ours")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.choose_all(Choice::Ours, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("all-theirs")
                                    .xsmall()
                                    .ghost()
                                    .label("All theirs")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.choose_all(Choice::Theirs, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("ask-claude")
                                    .xsmall()
                                    .ghost()
                                    .label("Ask Claude")
                                    .tooltip(
                                        "Ask Claude for a resolution of this file; you review \
                                         and save it",
                                    )
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        let Some(file) = this.selected.clone() else {
                                            return;
                                        };
                                        let op = this
                                            .tab
                                            .read(cx)
                                            .in_progress
                                            .as_ref()
                                            .map_or("merge", |p| p.op.label())
                                            .to_lowercase();
                                        crate::chat::context::resolve_conflict(
                                            &this.tab, file.path, op, window, cx,
                                        )
                                    })),
                            )
                            .child(self.render_external(cx)),
                    )
                    .child(
                        div()
                            .id("conflict-blocks")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(blocks),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .h(px(RESULT_HEIGHT))
                            .border_t_1()
                            .border_color(palette.border)
                            .child(
                                h_flex()
                                    .px_2()
                                    .h(zoomed(32., cx))
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(palette.muted)
                                            .child("Result (editable)"),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("save-resolved")
                                            .xsmall()
                                            .primary()
                                            .label("Save & mark resolved")
                                            .disabled(busy)
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.save(window, cx)
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                div().flex_1().min_h_0().px_1().child(
                                    EditorInput::new(&self.result)
                                        .h_full()
                                        .bordered(false)
                                        .disabled(!*utf8),
                                ),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Focusable for ConflictsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ConflictsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let error = self
            .error
            .clone()
            .or_else(|| self.tab.read(cx).op_error.clone());
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.bg)
            .when_some(error, |d, error| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(palette.error)
                        .bg(palette.error.opacity(0.1))
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_list(&palette, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .bg(palette.panel)
                            .child(self.render_file(&palette, cx)),
                    ),
            )
    }
}
