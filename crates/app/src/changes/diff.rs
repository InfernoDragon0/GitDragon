//! The staging diff: one working-copy file's diff (staged, unstaged or untracked), shown in place
//! of the graph when a file of the WIP row is clicked in the details panel. Hunks stage, unstage
//! or discard with one click; clicking changed lines picks them for line staging. A new status
//! generation reloads it; when the file leaves its section (fully staged, say) it follows the
//! file to the section it moved to, or asks to be closed when the change is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use bstr::{BString, ByteSlice as _};
use git_core::parse::diff::{FileDiff, LineKind};
use git_core::parse::status::StatusEntry;
use git_core::patch::{self, Direction, Pick};
use git_core::{ApplyTarget, DiffTarget, GitRepo as _};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle,
    InteractiveElement as _, IntoElement, ListHorizontalSizingBehavior, MouseButton,
    ParentElement as _, Render, ScrollStrategy, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder as _, uniform_list,
};
use ui::diff::copy::{self as diff_copy, RowSelection};
use ui::diff::{self as diff_ui, DiffModel, DiffRow, Layout};
use ui::{Palette, zoomed};

use super::Section;
use crate::diff_select::{self, DiffSelect};
use crate::file_editor::{Editors, HasEditors, editor_line};
use crate::repo_tab::RepoTab;

/// The file whose diff is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Selected {
    section: Section,
    path: BString,
}

enum DiffState {
    Empty,
    Loading,
    Ready(Arc<DiffModel>),
    Message(SharedString),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartAction {
    Stage,
    Unstage,
    Discard,
}

pub enum StagingDiffEvent {
    /// Back to the graph: the user asked, or the file has no changes left.
    Close,
}

pub struct StagingDiff {
    tab: Entity<RepoTab>,
    selected: Option<Selected>,
    diff: DiffState,
    layout: Layout,
    show_large: bool,
    /// Line staging: picked line indices per hunk.
    picked: BTreeMap<usize, BTreeSet<usize>>,
    /// Whole rows picked for copying; in a stageable section dragging or shift-clicking also
    /// picks the changed lines in the range for line staging.
    sel: RowSelection,
    /// `picked` when the current drag / shift-click began; the range's lines are added to it.
    picked_base: BTreeMap<usize, BTreeSet<usize>>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    seen_generation: u64,
    diff_task: Option<Task<()>>,
    /// The in-app editor opened from a diff row or the header; shown instead of the diff.
    editors: Editors,
    _subs: Vec<Subscription>,
}

impl EventEmitter<StagingDiffEvent> for StagingDiff {}

impl HasEditors for StagingDiff {
    fn editors(&mut self) -> &mut Editors {
        &mut self.editors
    }
}

impl StagingDiff {
    pub fn new(tab: Entity<RepoTab>, cx: &mut Context<Self>) -> Self {
        let _subs = vec![cx.observe(&tab, |this, _, cx| this.on_tab_changed(cx))];
        Self {
            tab,
            selected: None,
            diff: DiffState::Empty,
            layout: Layout::Unified,
            show_large: false,
            picked: BTreeMap::new(),
            sel: RowSelection::default(),
            picked_base: BTreeMap::new(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            seen_generation: 0,
            diff_task: None,
            editors: Editors::default(),
            _subs,
        }
    }

    /// Shows `path` as it is in `section`.
    pub fn open(&mut self, section: Section, path: BString, cx: &mut Context<Self>) {
        self.seen_generation = self.tab.read(cx).generation;
        let sel = Selected { section, path };
        self.editors.follow(Some(&sel.path), cx);
        if self.selected.as_ref() != Some(&sel) {
            self.selected = Some(sel);
            self.show_large = false;
            self.sel.clear();
            self.diff = DiffState::Loading;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.load_diff(cx);
    }

    /// Forgets the file (and stops following status changes for it).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.editors.follow(None, cx);
        self.selected = None;
        self.diff = DiffState::Empty;
        self.diff_task = None;
        self.picked.clear();
        self.sel.clear();
        cx.notify();
    }

    fn busy(&self, cx: &App) -> bool {
        self.tab.read(cx).is_busy()
    }

    fn on_tab_changed(&mut self, cx: &mut Context<Self>) {
        let generation = self.tab.read(cx).generation;
        // Same status: only the busy state may have changed.
        let Some(sel) = self
            .selected
            .clone()
            .filter(|_| generation != self.seen_generation)
        else {
            cx.notify();
            return;
        };
        self.seen_generation = generation;
        if self.entry(&sel, cx).is_some() {
            self.load_diff(cx);
            return;
        }
        // Gone from its section: follow it, or close when nothing is left to show.
        let moved = [
            Section::Unstaged,
            Section::Staged,
            Section::Untracked,
            Section::Conflicted,
        ]
        .into_iter()
        .find(|&section| {
            let probe = Selected {
                section,
                path: sel.path.clone(),
            };
            self.entry(&probe, cx).is_some()
        });
        match moved {
            Some(section) => self.open(section, sel.path, cx),
            None => {
                self.close(cx);
                cx.emit(StagingDiffEvent::Close);
            }
        }
    }

    fn entry<'a>(&self, sel: &Selected, cx: &'a App) -> Option<&'a StatusEntry> {
        self.tab
            .read(cx)
            .status
            .as_ref()?
            .entries
            .iter()
            .find(|e| e.path == sel.path && sel.section.contains(e))
    }

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.selected.clone() else {
            return;
        };
        let Some(repo) = self.tab.read(cx).repo.clone() else {
            return;
        };
        let orig = self.entry(&sel, cx).and_then(|e| e.orig_path.clone());
        let theme = cx.theme().highlight_theme.clone();
        self.picked.clear();
        self.sel.clear();
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let mut paths = vec![sel.path.clone()];
                    paths.extend(orig);
                    let files = match sel.section {
                        Section::Untracked => repo.untracked_diff(&sel.path).await?,
                        Section::Staged => repo.diff(DiffTarget::Staged { paths }).await?,
                        Section::Unstaged | Section::Conflicted => {
                            repo.diff(DiffTarget::Unstaged { paths }).await?
                        }
                    };
                    let model = pick_file(files, &sel.path).map(|file| {
                        let mut model = DiffModel::new(file);
                        diff_ui::syntax::highlight(&mut model, &sel.path, &theme);
                        model
                    });
                    Ok::<_, git_core::GitError>(model)
                })
                .await;
            _ = this.update(cx, |this, cx| {
                let conflicted =
                    this.selected.as_ref().map(|s| s.section) == Some(Section::Conflicted);
                this.diff = match loaded {
                    Ok(Some(model)) if model.file.binary => {
                        DiffState::Message("Binary file: no text diff.".into())
                    }
                    Ok(Some(model)) if model.file.hunks.is_empty() => {
                        DiffState::Message("Only the file mode changed.".into())
                    }
                    Ok(Some(model)) => DiffState::Ready(Arc::new(model)),
                    Ok(None) if conflicted => DiffState::Message(
                        "Conflicted: resolve it in the Conflicts view, or stage it to mark it \
                         resolved."
                            .into(),
                    ),
                    Ok(None) => DiffState::Message("No changes to show.".into()),
                    Err(err) => DiffState::Message(err.to_string().into()),
                };
                cx.notify();
            });
        }));
    }

    /// Opens the selected file in the in-app editor, at the new-file line of diff line
    /// `at = (hunk, line)` when given.
    fn edit_file(
        &mut self,
        at: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.selected.as_ref().map(|s| s.path.clone()) else {
            return;
        };
        let line = match (&self.diff, at) {
            (DiffState::Ready(model), Some((hunk, line))) => {
                model.file.hunks.get(hunk).and_then(|h| {
                    let numbers: Vec<Option<u32>> = h.lines.iter().map(|l| l.new_lineno).collect();
                    editor_line(&numbers, line)
                })
            }
            _ => None,
        };
        let tab = self.tab.clone();
        self.open_editor(&tab, path, line, "\u{2190} Diff", window, cx);
    }

    /// A click on diff line `(hunk, line)`: picks it for line staging when it is a changed line
    /// of a stageable section; a double click opens the file there.
    fn line_clicked(
        &mut self,
        ix: usize,
        hunk: usize,
        line: usize,
        pickable: bool,
        clicks: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if pickable {
            // The second click of a double click puts back what the first one picked.
            self.toggle_line(hunk, line, cx);
            // Unpicked again: a lone row selection would still look picked.
            let still = self.picked.get(&hunk).is_some_and(|s| s.contains(&line));
            if !still && self.sel.range() == Some(ix..ix + 1) {
                self.sel.clear();
            }
        }
        if clicks >= 2 {
            self.edit_file(Some((hunk, line)), window, cx);
        }
    }

    fn toggle_line(&mut self, hunk: usize, line: usize, cx: &mut Context<Self>) {
        let lines = self.picked.entry(hunk).or_default();
        if !lines.remove(&line) {
            lines.insert(line);
        }
        if lines.is_empty() {
            self.picked.remove(&hunk);
        }
        cx.notify();
    }

    fn picked_count(&self) -> usize {
        self.picked.values().map(BTreeSet::len).sum()
    }

    /// Stage / unstage / discard part of the shown diff. `hunk: None` = the picked lines.
    fn apply_part(&mut self, action: PartAction, hunk: Option<usize>, cx: &mut Context<Self>) {
        let DiffState::Ready(model) = &self.diff else {
            return;
        };
        let picks: Vec<(usize, Pick)> = match hunk {
            Some(h) => vec![(h, Pick::All)],
            None => self
                .picked
                .iter()
                .map(|(h, lines)| (*h, Pick::Lines(lines.iter().copied().collect())))
                .collect(),
        };
        let (direction, target, verb) = match action {
            PartAction::Stage => (Direction::Forward, ApplyTarget::Index, "Staging"),
            PartAction::Unstage => (Direction::Reverse, ApplyTarget::Index, "Unstaging"),
            PartAction::Discard => (Direction::Reverse, ApplyTarget::Worktree, "Discarding"),
        };
        let Some(patch) = patch::build(&model.file, &picks, direction) else {
            return;
        };
        let what = if hunk.is_some() { "hunk" } else { "lines" };
        self.picked.clear();
        self.tab.update(cx, |tab, cx| {
            tab.run_op(format!("{verb} {what}"), cx, move |repo| async move {
                repo.apply_patch(patch, target, direction).await
            })
        });
        cx.notify();
    }

    fn confirm_discard_part(
        &mut self,
        hunk: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let what = match hunk {
            Some(_) => "this hunk".to_string(),
            None => format!("{} selected lines", self.picked_count()),
        };
        let this = cx.entity().downgrade();
        crate::dialogs::confirm(
            window,
            cx,
            "Discard changes?",
            format!("The changes in {what} will be lost. This cannot be undone."),
            "Discard",
            true,
            move |_, cx| {
                _ = this.update(cx, |view, cx| {
                    view.apply_part(PartAction::Discard, hunk, cx)
                });
            },
        );
    }
}

/// The diff for `path` out of what git returned (a rename lists the new path).
fn pick_file(files: Vec<FileDiff>, path: &BString) -> Option<FileDiff> {
    let at = files
        .iter()
        .position(|f| f.new_path.as_ref() == Some(path) || f.old_path.as_ref() == Some(path))
        .unwrap_or(0);
    files.into_iter().nth(at)
}

// ---- rendering ------------------------------------------------------------------------------

impl StagingDiff {
    fn render_header(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy(cx);
        let picked = self.picked_count();
        let sel = self.selected.clone();
        let section = sel.as_ref().map(|s| s.section);
        let model = match &self.diff {
            DiffState::Ready(model) => Some(model.clone()),
            _ => None,
        };
        let line_staging = matches!(section, Some(Section::Staged | Section::Unstaged));
        h_flex()
            .flex_none()
            .h(zoomed(36., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .text_xs()
            .child(
                Button::new("staging-back")
                    .ghost()
                    .small()
                    .label("\u{2190} Graph")
                    .tooltip("Back to the commit graph")
                    .on_click(
                        cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(StagingDiffEvent::Close)),
                    ),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(palette.emph)
                    .child(SharedString::from(
                        sel.as_ref()
                            .map(|s| ui::sanitize_line(&s.path.to_str_lossy()).into_owned())
                            .unwrap_or_default(),
                    )),
            )
            .when_some(section, |d, section| {
                let label = match section {
                    Section::Conflicted => "conflicted",
                    Section::Staged => "staged",
                    Section::Unstaged => "unstaged",
                    Section::Untracked => "untracked",
                };
                d.child(div().flex_none().text_color(palette.muted).child(label))
            })
            .when_some(model.filter(|m| m.lfs.is_none()), |d, model| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(palette.muted)
                        .child(SharedString::from(format!(
                            "{} hunk{} \u{b7}",
                            model.file.hunks.len(),
                            if model.file.hunks.len() == 1 { "" } else { "s" },
                        ))),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(palette.ok)
                        .child(format!("+{}", model.added)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(palette.error)
                        .child(format!("\u{2212}{}", model.removed)),
                )
            })
            .child(div().flex_1())
            .when(picked > 0 && line_staging, |d| {
                let (label, action) = match section {
                    Some(Section::Staged) => {
                        (format!("Unstage {picked} lines"), PartAction::Unstage)
                    }
                    _ => (format!("Stage {picked} lines"), PartAction::Stage),
                };
                d.child(
                    Button::new("lines-apply")
                        .primary()
                        .xsmall()
                        .label(label)
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.apply_part(action, None, cx)
                        })),
                )
                .when(section == Some(Section::Unstaged), |d| {
                    d.child(
                        Button::new("lines-discard")
                            .ghost()
                            .xsmall()
                            .label("Discard lines\u{2026}")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.confirm_discard_part(None, window, cx)
                            })),
                    )
                })
                .child(
                    Button::new("lines-clear")
                        .ghost()
                        .xsmall()
                        .label("Clear")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.picked.clear();
                            cx.notify();
                        })),
                )
            })
            .when_some(sel.clone().filter(|_| line_staging), |d, sel| {
                d.child(
                    Button::new("staging-explain")
                        .ghost()
                        .small()
                        .label("Explain")
                        .tooltip("Ask Claude to explain this file's changes")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            crate::chat::context::explain_file(
                                &this.tab,
                                sel.path.clone(),
                                sel.section == Section::Staged,
                                window,
                                cx,
                            )
                        })),
                )
            })
            .child(
                Button::new("staging-edit")
                    .ghost()
                    .small()
                    .label("Edit file")
                    .tooltip("Edit this file in the built-in editor (or double-click a line)")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.edit_file(None, window, cx)
                    })),
            )
            .child(
                Button::new("staging-layout")
                    .ghost()
                    .small()
                    .label(match self.layout {
                        Layout::Unified => "Split",
                        Layout::Split => "Unified",
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.layout = match this.layout {
                            Layout::Unified => Layout::Split,
                            Layout::Split => Layout::Unified,
                        };
                        // Row indices differ between the layouts.
                        this.sel.clear();
                        cx.notify();
                    })),
            )
    }

    fn render_body(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: SharedString| {
            div()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child(SharedString::from(ui::sanitize_display(&text).into_owned()))
                .into_any_element()
        };
        let model = match &self.diff {
            DiffState::Empty => return message("Select a file to see its changes.".into()),
            DiffState::Loading => return message("Loading diff\u{2026}".into()),
            DiffState::Message(text) => return message(text.clone()),
            DiffState::Ready(model) => model.clone(),
        };
        // An LFS file's text diff is its pointer: show the size change instead.
        if let Some(change) = &model.lfs {
            return diff_ui::lfs_summary(change, palette, cx);
        }
        if model.is_large() && !self.show_large {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(palette.muted)
                .child(format!("Large diff ({} lines).", model.line_count()))
                .child(
                    Button::new("show-large")
                        .outline()
                        .small()
                        .label("Show anyway")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.show_large = true;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let count = model.rows(self.layout).len();
        let widest = diff_ui::widest_row(&model, self.layout);
        let view = cx.entity().downgrade();
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "staging-diff",
                    count,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|ix| {
                                let row = this.render_row(&model, ix, cx)?;
                                Some(diff_select::row(ix, row, cx))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                // Long lines scroll sideways: the list is as wide as its widest row.
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(widest)
                // Keeps the overlay scrollbars off the rows' right-hand and bottom edges.
                .pr_3()
                .pb_3()
                .flex_1(),
            )
            .scrollbar(&self.scroll, ScrollbarAxis::Both)
            .context_menu(move |menu, _, _| diff_select::menu(menu, view.clone()))
            .into_any_element()
    }

    fn render_row(
        &self,
        model: &DiffModel,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let palette = Palette::of(cx);
        let row = *model.rows(self.layout).get(ix)?;
        let section = self.selected.as_ref()?.section;
        let busy = self.busy(cx);
        let selectable = matches!(section, Section::Staged | Section::Unstaged);
        let copy_sel = self.sel.contains(ix);
        Some(match row {
            DiffRow::Hunk { hunk } => {
                let h = model.file.hunks.get(hunk)?;
                // The buttons are not part of the row selection.
                let mut actions = h_flex()
                    .gap_1()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
                let button = |id: &'static str, label: &'static str| {
                    Button::new(ElementId::NamedInteger(id.into(), hunk as u64))
                        .ghost()
                        .xsmall()
                        .label(label)
                        .disabled(busy)
                };
                match section {
                    Section::Unstaged => {
                        actions =
                            actions
                                .child(button("hunk-stage", "Stage hunk").on_click(cx.listener(
                                    move |this, _: &ClickEvent, _, cx| {
                                        this.apply_part(PartAction::Stage, Some(hunk), cx)
                                    },
                                )))
                                .child(button("hunk-discard", "Discard\u{2026}").on_click(
                                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                                        this.confirm_discard_part(Some(hunk), window, cx)
                                    }),
                                ))
                    }
                    Section::Staged => {
                        actions = actions.child(button("hunk-unstage", "Unstage hunk").on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.apply_part(PartAction::Unstage, Some(hunk), cx)
                            }),
                        ))
                    }
                    _ => {}
                }
                h_flex()
                    .h(zoomed(diff_ui::ROW_HEIGHT, cx))
                    .w_full()
                    .px_2()
                    .gap_2()
                    .bg(if copy_sel {
                        palette.accent_soft
                    } else {
                        palette.panel2
                    })
                    .text_xs()
                    .text_color(palette.muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(
                                ui::sanitize_line(&h.header.to_str_lossy()).into_owned(),
                            )),
                    )
                    .child(actions)
                    .into_any_element()
            }
            DiffRow::Line { hunk, line } => {
                let l = model.file.hunks.get(hunk)?.lines.get(line)?;
                let view = model.lines.get(hunk)?.get(line)?;
                let picked = copy_sel || self.picked.get(&hunk).is_some_and(|s| s.contains(&line));
                let changed = matches!(l.kind, LineKind::Added | LineKind::Deleted);
                div()
                    .id(ElementId::NamedInteger("diff-line".into(), ix as u64))
                    .w_full()
                    .when(selectable && changed, |d| d.cursor_pointer())
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.line_clicked(
                            ix,
                            hunk,
                            line,
                            selectable && changed,
                            event.click_count(),
                            window,
                            cx,
                        )
                    }))
                    .child(diff_ui::line(
                        view,
                        l.kind,
                        &[l.old_lineno, l.new_lineno],
                        picked,
                        &palette,
                        cx,
                    ))
                    .into_any_element()
            }
            DiffRow::Pair { hunk, left, right } => {
                let lines = &model.file.hunks.get(hunk)?.lines;
                let side = |slot: Option<usize>, old: bool, cx: &mut Context<Self>| -> AnyElement {
                    let Some(line) = slot else {
                        return diff_ui::filler(&palette, cx).into_any_element();
                    };
                    let (Some(l), Some(view)) = (lines.get(line), model.lines[hunk].get(line))
                    else {
                        return diff_ui::filler(&palette, cx).into_any_element();
                    };
                    let picked =
                        copy_sel || self.picked.get(&hunk).is_some_and(|s| s.contains(&line));
                    let changed = matches!(l.kind, LineKind::Added | LineKind::Deleted);
                    let number = if old { l.old_lineno } else { l.new_lineno };
                    div()
                        .id(ElementId::NamedInteger(
                            if old { "diff-left" } else { "diff-right" }.into(),
                            ix as u64,
                        ))
                        .w_full()
                        .when(selectable && changed, |d| d.cursor_pointer())
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.line_clicked(
                                ix,
                                hunk,
                                line,
                                selectable && changed,
                                event.click_count(),
                                window,
                                cx,
                            )
                        }))
                        .child(diff_ui::line(view, l.kind, &[number], picked, &palette, cx))
                        .into_any_element()
                };
                let left = side(left, true, cx);
                let right = side(right, false, cx);
                h_flex()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .border_r_1()
                            .border_color(palette.border)
                            .child(left),
                    )
                    .child(div().flex_1().min_w_0().child(right))
                    .into_any_element()
            }
        })
    }
}

impl Render for StagingDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        if let Some(editor) = self.editors.shown(cx) {
            return v_flex()
                .size_full()
                .bg(palette.panel)
                .child(editor)
                .into_any_element();
        }
        diff_select::container(v_flex().size_full(), &self.focus, cx)
            .bg(palette.panel)
            .child(self.render_header(&palette, cx))
            .child(self.render_body(&palette, cx))
            .into_any_element()
    }
}

impl DiffSelect for StagingDiff {
    fn selection(&mut self) -> &mut RowSelection {
        &mut self.sel
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// In a stageable section the changed lines of the range join the picked lines, so a
    /// dragged range can be staged as well as copied.
    fn press_started(&mut self) {
        self.picked_base = self.picked.clone();
    }

    fn range_extended(&mut self, cx: &mut Context<Self>) {
        let (Some(rows), DiffState::Ready(model), Some(sel)) =
            (self.sel.range(), &self.diff, &self.selected)
        else {
            return;
        };
        if !matches!(sel.section, Section::Staged | Section::Unstaged) {
            return;
        }
        // Derived from the snapshot, so dragging back un-picks the rows left behind.
        let mut picked = self.picked_base.clone();
        for (hunk, lines) in diff_copy::changed_lines(model, self.layout, rows) {
            picked.entry(hunk).or_default().extend(lines);
        }
        self.picked = picked;
        cx.notify();
    }

    fn copy_selection(&mut self, patch: bool, cx: &mut Context<Self>) {
        let (Some(rows), DiffState::Ready(model)) = (self.sel.range(), &self.diff) else {
            return;
        };
        let text = if patch {
            diff_copy::copy_patch(model, self.layout, rows)
        } else {
            diff_copy::copy_lines(model, self.layout, rows)
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
    }
}
