//! The Changes view (GitHub Desktop style): file list grouped by state on the left with the
//! commit box under it, the selected file's diff on the right with hunk / line staging.
//! Every git call runs on the background executor; the file watcher refreshes the tab, and a
//! new status generation reloads the open diff.

mod commit_box;
mod list;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use bstr::{BString, ByteSlice};
use git_core::parse::diff::{FileDiff, LineKind};
use git_core::parse::status::StatusEntry;
use git_core::patch::{self, Direction, Pick};
use git_core::{ApplyTarget, CliRepo, DiffTarget, GitRepo as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use ui::diff::{self as diff_ui, DiffModel, DiffRow, Layout};
use ui::{Palette, zoomed};

use crate::repo_tab::RepoTab;
use commit_box::{CommitBox, CommitBoxEvent};
use list::{ListRow, Section};

const LIST_ROW_HEIGHT: f32 = 24.;
const LIST_WIDTH: f32 = 300.;

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

pub struct ChangesView {
    tab: Entity<RepoTab>,
    tree: bool,
    filter: Entity<InputState>,
    rows: Vec<ListRow>,
    selected: Option<Selected>,
    diff: DiffState,
    layout: Layout,
    show_large: bool,
    /// Line staging: picked line indices per hunk.
    picked: BTreeMap<usize, BTreeSet<usize>>,
    list_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    commit_box: Entity<CommitBox>,
    seen_generation: u64,
    diff_task: Option<Task<()>>,
    /// The running stage / commit / stash operation; dropping it kills git.
    op_task: Option<Task<()>>,
    op_error: Option<SharedString>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl ChangesView {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let commit_box = cx.new(|cx| CommitBox::new(window, cx));
        let _subs = vec![
            cx.observe_in(&tab, window, |this, _, window, cx| {
                this.on_tab_changed(window, cx)
            }),
            cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.rebuild_rows(cx);
                }
            }),
            cx.subscribe_in(
                &commit_box,
                window,
                |this, _, event: &CommitBoxEvent, window, cx| match event {
                    CommitBoxEvent::Commit { request, push } => {
                        this.commit(request.clone(), *push, window, cx)
                    }
                    CommitBoxEvent::WantsHeadMessage => this.load_head_message(window, cx),
                },
            ),
        ];
        let mut view = Self {
            tab,
            tree: false,
            filter,
            rows: Vec::new(),
            selected: None,
            diff: DiffState::Empty,
            layout: Layout::Unified,
            show_large: false,
            picked: BTreeMap::new(),
            list_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            commit_box,
            seen_generation: 0,
            diff_task: None,
            op_task: None,
            op_error: None,
            focus: cx.focus_handle(),
            _subs,
        };
        view.on_tab_changed(window, cx);
        view
    }

    fn repo(&self, cx: &App) -> Option<CliRepo> {
        self.tab.read(cx).repo.clone()
    }

    pub fn is_busy(&self) -> bool {
        self.op_task.is_some()
    }

    /// Stops the running operation (its git process is killed).
    pub fn cancel_op(&mut self, cx: &mut Context<Self>) {
        if self.op_task.take().is_some() {
            self.op_error = Some("Cancelled.".into());
            self.commit_box
                .update(cx, |b, cx| b.set_busy(false, None, cx));
            cx.notify();
        }
    }

    // ---- status ---------------------------------------------------------------------------

    fn on_tab_changed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let generation = self.tab.read(cx).generation;
        if generation == self.seen_generation {
            cx.notify();
            return;
        }
        self.seen_generation = generation;
        self.rebuild_rows(cx);
        let tab = self.tab.read(cx);
        let staged = tab.status.as_ref().map_or(0, |s| {
            s.entries
                .iter()
                .filter(|e| Section::Staged.contains(e))
                .count()
        });
        let branch = tab
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .map(SharedString::from);
        self.commit_box
            .update(cx, |b, cx| b.set_context(branch, staged, cx));
        // The selected file may have moved section or gone away; with none, show the first.
        match self.selected.clone() {
            Some(sel) if self.entry(&sel, cx).is_some() => self.load_diff(cx),
            Some(sel) => self.select_first_in(sel.section, cx),
            None => {
                let first = self.rows.iter().find_map(|row| match row {
                    ListRow::File { section, .. } => Some(*section),
                    _ => None,
                });
                if let Some(section) = first {
                    self.select_first_in(section, cx);
                }
            }
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        self.rows = match &self.tab.read(cx).status {
            Some(status) => list::build(status, self.tree, &filter),
            None => Vec::new(),
        };
        cx.notify();
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

    fn entries_in(&self, section: Section, cx: &App) -> Vec<StatusEntry> {
        self.tab
            .read(cx)
            .status
            .as_ref()
            .map(|s| {
                s.entries
                    .iter()
                    .filter(|e| section.contains(e))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn select_first_in(&mut self, section: Section, cx: &mut Context<Self>) {
        let first = self.rows.iter().find_map(|row| match row {
            ListRow::File {
                section: s, entry, ..
            } if *s == section => Some(*entry),
            _ => None,
        });
        match first.and_then(|ix| self.tab.read(cx).status.as_ref()?.entries.get(ix).cloned()) {
            Some(entry) => self.select(section, entry.path, cx),
            None => {
                self.selected = None;
                self.diff = DiffState::Empty;
                self.diff_task = None;
                cx.notify();
            }
        }
    }

    fn select(&mut self, section: Section, path: BString, cx: &mut Context<Self>) {
        let sel = Selected { section, path };
        if self.selected.as_ref() != Some(&sel) {
            self.selected = Some(sel);
            self.show_large = false;
            self.diff = DiffState::Loading;
            self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.load_diff(cx);
    }

    // ---- diff -----------------------------------------------------------------------------

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let (Some(sel), Some(repo)) = (self.selected.clone(), self.repo(cx)) else {
            return;
        };
        let orig = self.entry(&sel, cx).and_then(|e| e.orig_path.clone());
        let theme = cx.theme().highlight_theme.clone();
        self.picked.clear();
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
                this.diff = match loaded {
                    Ok(Some(model)) if model.file.binary => {
                        DiffState::Message("Binary file: no text diff.".into())
                    }
                    Ok(Some(model)) if model.file.hunks.is_empty() => {
                        DiffState::Message("Only the file mode changed.".into())
                    }
                    Ok(Some(model)) => DiffState::Ready(Arc::new(model)),
                    Ok(None) if this.selected.as_ref().map(|s| s.section) == Some(Section::Conflicted) => {
                        DiffState::Message("Conflicted: resolve it in the Conflicts view, or stage it to mark it resolved.".into())
                    }
                    Ok(None) => DiffState::Message("No changes to show.".into()),
                    Err(err) => DiffState::Message(err.to_string().into()),
                };
                cx.notify();
            });
        }));
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

    // ---- operations -----------------------------------------------------------------------

    /// Runs `op` on the background executor, then refreshes the tab. One at a time.
    fn run_op<F, Fut>(&mut self, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        if self.op_task.is_some() {
            return;
        }
        let Some(repo) = self.repo(cx) else {
            return;
        };
        self.op_error = None;
        let job = cx.background_executor().spawn(op(repo));
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                this.op_task = None;
                if let Err(err) = result {
                    this.op_error = Some(err.to_string().into());
                }
                this.tab.update(cx, |tab, cx| tab.refresh(cx));
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Paths an operation on `entry` must name (both sides of a rename).
    fn op_paths(entry: &StatusEntry) -> Vec<BString> {
        let mut paths = vec![entry.path.clone()];
        paths.extend(entry.orig_path.clone());
        paths
    }

    fn stage_entries(&mut self, entries: Vec<StatusEntry>, cx: &mut Context<Self>) {
        let paths: Vec<BString> = entries.iter().flat_map(Self::op_paths).collect();
        if paths.is_empty() {
            return;
        }
        self.run_op(cx, move |repo| async move { repo.stage(&paths).await });
    }

    fn unstage_entries(&mut self, entries: Vec<StatusEntry>, cx: &mut Context<Self>) {
        let paths: Vec<BString> = entries.iter().flat_map(Self::op_paths).collect();
        if paths.is_empty() {
            return;
        }
        self.run_op(cx, move |repo| async move { repo.unstage(&paths).await });
    }

    /// Asks first, then throws away unstaged changes (tracked) or deletes (untracked).
    fn confirm_discard(
        &mut self,
        section: Section,
        entries: Vec<StatusEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entries.is_empty() || !matches!(section, Section::Unstaged | Section::Untracked) {
            return;
        }
        let what = match entries.as_slice() {
            [one] => format!("“{}”", one.path.to_str_lossy()),
            many => format!("{} files", many.len()),
        };
        let (title, body) = match section {
            Section::Untracked => (
                "Delete untracked files?",
                format!("{what} will be deleted from disk. This cannot be undone."),
            ),
            _ => (
                "Discard changes?",
                format!("Unstaged changes to {what} will be lost. This cannot be undone."),
            ),
        };
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entries = entries.clone();
            let this = this.clone();
            alert
                .confirm()
                .title(title)
                .description(body.clone())
                .ok_text(if section == Section::Untracked {
                    "Delete"
                } else {
                    "Discard"
                })
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let entries = entries.clone();
                    _ = this.update(cx, |view, cx| view.discard_entries(section, entries, cx));
                    true
                })
        });
    }

    fn discard_entries(
        &mut self,
        section: Section,
        entries: Vec<StatusEntry>,
        cx: &mut Context<Self>,
    ) {
        let paths: Vec<BString> = entries.into_iter().map(|e| e.path).collect();
        match section {
            Section::Untracked => self.run_op(cx, move |repo| async move {
                repo.delete_untracked(&paths).await
            }),
            _ => self.run_op(cx, move |repo| async move { repo.discard(&paths).await }),
        }
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
        let (direction, target) = match action {
            PartAction::Stage => (Direction::Forward, ApplyTarget::Index),
            PartAction::Unstage => (Direction::Reverse, ApplyTarget::Index),
            PartAction::Discard => (Direction::Reverse, ApplyTarget::Worktree),
        };
        let Some(patch) = patch::build(&model.file, &picks, direction) else {
            return;
        };
        self.picked.clear();
        self.run_op(cx, move |repo| async move {
            repo.apply_patch(patch, target, direction).await
        });
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
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title("Discard changes?")
                .description(format!(
                    "The changes in {what} will be lost. This cannot be undone."
                ))
                .ok_text("Discard")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    _ = this.update(cx, |view, cx| {
                        view.apply_part(PartAction::Discard, hunk, cx)
                    });
                    true
                })
        });
    }

    fn commit(
        &mut self,
        request: git_core::CommitRequest,
        push: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.op_task.is_some() {
            return;
        }
        let Some(repo) = self.repo(cx) else {
            return;
        };
        let upstream = self
            .tab
            .read(cx)
            .status
            .as_ref()
            .and_then(|s| s.branch.upstream.clone());
        self.commit_box
            .update(cx, |b, cx| b.set_busy(true, None, cx));
        let job = cx.background_executor().spawn(async move {
            repo.commit(&request).await.map_err(|err| (false, err))?;
            if push {
                let set_upstream = match upstream {
                    Some(_) => None,
                    None => {
                        let remotes = repo.remotes().await.map_err(|err| (true, err))?;
                        let name = remotes
                            .iter()
                            .find(|r| r.name == "origin")
                            .or(remotes.first())
                            .map(|r| r.name.clone());
                        Some(name.ok_or_else(|| {
                            (
                                true,
                                git_core::GitError::InvalidArgument {
                                    what: "remote (none configured)",
                                    value: String::new(),
                                },
                            )
                        })?)
                    }
                };
                repo.push(set_upstream.as_deref())
                    .await
                    .map_err(|err| (true, err))?;
            }
            Ok::<_, (bool, git_core::GitError)>(())
        });
        self.op_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.op_task = None;
                match result {
                    Ok(()) => this.commit_box.update(cx, |b, cx| b.committed(window, cx)),
                    // Committed, but the push failed: clear the box, report the push.
                    Err((true, err)) => {
                        this.commit_box.update(cx, |b, cx| b.committed(window, cx));
                        this.op_error = Some(format!("Committed, but push failed: {err}").into());
                    }
                    Err((false, err)) => this.commit_box.update(cx, |b, cx| {
                        b.set_busy(false, Some(err.to_string().into()), cx)
                    }),
                }
                this.tab.update(cx, |tab, cx| tab.refresh(cx));
                cx.notify();
            });
        }));
    }

    fn load_head_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else {
            return;
        };
        let job = cx
            .background_executor()
            .spawn(async move { repo.head_message().await });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Some(message)) = job.await {
                _ = this.update_in(cx, |this, window, cx| {
                    this.commit_box
                        .update(cx, |b, cx| b.prefill(&message, window, cx))
                });
            }
        })
        .detach();
    }

    fn stash(&mut self, cx: &mut Context<Self>) {
        let branch = self
            .tab
            .read(cx)
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .unwrap_or_else(|| "detached HEAD".into());
        let message = format!("WIP on {branch}");
        self.run_op(cx, move |repo| async move {
            repo.stash_push(&message, true, false, &[]).await
        });
    }

    fn stash_pop(&mut self, cx: &mut Context<Self>) {
        self.run_op(cx, |repo| async move {
            repo.stash_action(git_core::StashAction::Pop, 0).await
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartAction {
    Stage,
    Unstage,
    Discard,
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

impl ChangesView {
    fn render_toolbar(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab.read(cx);
        let stash_count = tab.status.as_ref().map_or(0, |s| s.stash_count);
        let has_changes = tab.status.as_ref().is_some_and(|s| !s.entries.is_empty());
        let busy = self.is_busy();
        h_flex()
            .h(zoomed(36., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .when(tab.loading, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child("refreshing…"),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("stash")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Archive)
                    .label("Stash")
                    .disabled(busy || !has_changes)
                    .tooltip("Stash all changes, including untracked files")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stash(cx))),
            )
            .child(
                Button::new("stash-pop")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArchiveRestore)
                    .label(format!("Pop ({stash_count})"))
                    .disabled(busy || stash_count == 0)
                    .tooltip("Apply the latest stash and drop it")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stash_pop(cx))),
            )
            .child(div().w(px(8.)))
            .child(
                Button::new("layout-tree")
                    .ghost()
                    .xsmall()
                    .icon(if self.tree {
                        IconName::ListTree
                    } else {
                        IconName::List
                    })
                    .tooltip(if self.tree {
                        "Show as list"
                    } else {
                        "Show as tree"
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.tree = !this.tree;
                        this.rebuild_rows(cx);
                    })),
            )
            .child(
                Button::new("layout-split")
                    .ghost()
                    .xsmall()
                    .icon(match self.layout {
                        Layout::Unified => IconName::Rows2,
                        Layout::Split => IconName::Columns2,
                    })
                    .tooltip(match self.layout {
                        Layout::Unified => "Side-by-side diff",
                        Layout::Split => "Unified diff",
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.layout = match this.layout {
                            Layout::Unified => Layout::Split,
                            Layout::Split => Layout::Unified,
                        };
                        cx.notify();
                    })),
            )
    }

    fn render_list(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        let body = if count == 0 {
            let text = if self.tab.read(cx).status.is_none() {
                "Loading…"
            } else if self.filter.read(cx).value().is_empty() {
                "No local changes."
            } else {
                "No files match."
            };
            div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child(text)
                .into_any_element()
        } else {
            uniform_list(
                "changes-files",
                count,
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range
                        .filter_map(|ix| this.render_list_row(ix, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.list_scroll)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .w(zoomed(LIST_WIDTH, cx))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(
                div()
                    .p_2()
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(body)
            .when_some(self.op_error.clone(), |d, err| {
                d.child(
                    div()
                        .id("op-error")
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(palette.error)
                        .cursor_pointer()
                        .child(err)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.op_error = None;
                            cx.notify();
                        })),
                )
            })
            .child(self.commit_box.clone())
    }

    fn render_list_row(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = Palette::of(cx);
        let row = self.rows.get(ix)?.clone();
        let busy = self.is_busy();
        let id = ElementId::NamedInteger("change-row".into(), ix as u64);
        let base = h_flex()
            .id(id)
            .group("change-row")
            .h(zoomed(LIST_ROW_HEIGHT, cx))
            .w_full()
            .px_2()
            .gap_1()
            .text_xs();
        Some(match row {
            ListRow::Header { section, count } => {
                let mut actions = h_flex().gap_0p5();
                let stage_label = match section {
                    Section::Staged => Some(("Unstage all", false)),
                    Section::Unstaged | Section::Untracked => Some(("Stage all", true)),
                    Section::Conflicted => Some(("Mark all resolved", true)),
                };
                if let Some((label, stage)) = stage_label {
                    actions = actions.child(
                        Button::new(ElementId::NamedInteger("section-stage".into(), ix as u64))
                            .ghost()
                            .xsmall()
                            .label(label)
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let entries = this.entries_in(section, cx);
                                if stage {
                                    this.stage_entries(entries, cx)
                                } else {
                                    this.unstage_entries(entries, cx)
                                }
                            })),
                    );
                }
                if matches!(section, Section::Unstaged | Section::Untracked) {
                    actions = actions.child(
                        Button::new(ElementId::NamedInteger("section-discard".into(), ix as u64))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Undo2)
                            .tooltip(if section == Section::Untracked {
                                "Delete all untracked files…"
                            } else {
                                "Discard all unstaged changes…"
                            })
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                let entries = this.entries_in(section, cx);
                                this.confirm_discard(section, entries, window, cx)
                            })),
                    );
                }
                base.mt_1()
                    .text_color(palette.muted)
                    .child(
                        div()
                            .text_color(palette.emph)
                            .child(SharedString::from(section.label())),
                    )
                    .child(SharedString::from(count.to_string()))
                    .child(div().flex_1())
                    .child(actions)
                    .into_any_element()
            }
            ListRow::Dir { path, depth, .. } => base
                .pl(px(8. + depth as f32 * 12.))
                .text_color(palette.muted)
                .child(SharedString::from(format!(
                    "{}/",
                    String::from_utf8_lossy(list::file_name(&path))
                )))
                .into_any_element(),
            ListRow::File {
                section,
                entry,
                depth,
            } => {
                let status = self.tab.read(cx).status.as_ref()?;
                let entry = status.entries.get(entry)?.clone();
                let state = section.state(&entry);
                let selected = self
                    .selected
                    .as_ref()
                    .is_some_and(|s| s.section == section && s.path == entry.path);
                let name = if self.tree {
                    String::from_utf8_lossy(list::file_name(&entry.path)).into_owned()
                } else {
                    match &entry.orig_path {
                        Some(orig) => {
                            format!("{} → {}", orig.to_str_lossy(), entry.path.to_str_lossy())
                        }
                        None => entry.path.to_str_lossy().into_owned(),
                    }
                };
                let color = match state {
                    git_core::parse::status::FileState::Added => palette.ok,
                    git_core::parse::status::FileState::Deleted => palette.error,
                    git_core::parse::status::FileState::Unmerged => palette.warn,
                    _ => palette.accent,
                };
                let stage = section != Section::Staged;
                let path = entry.path.clone();
                let toggle_entry = entry.clone();
                let discard_entry = entry.clone();
                base.pl(px(8. + depth as f32 * 12.))
                    .cursor_pointer()
                    .rounded(px(4.))
                    .when(selected, |d| {
                        d.bg(palette.accent_soft).text_color(palette.emph)
                    })
                    .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        let double = matches!(event, ClickEvent::Mouse(m) if m.up.click_count >= 2);
                        if double {
                            let entries = vec![toggle_entry.clone()];
                            if stage {
                                this.stage_entries(entries, cx)
                            } else {
                                this.unstage_entries(entries, cx)
                            }
                        } else {
                            this.select(section, path.clone(), cx)
                        }
                    }))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .text_color(color)
                            .child(list::state_letter(state)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(name)),
                    )
                    .child(
                        h_flex()
                            .invisible()
                            .group_hover("change-row", |s| s.visible())
                            .when(
                                matches!(section, Section::Unstaged | Section::Untracked),
                                |d| {
                                    d.child(
                                        Button::new(ElementId::NamedInteger(
                                            "row-discard".into(),
                                            ix as u64,
                                        ))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Undo2)
                                        .tooltip(if section == Section::Untracked {
                                            "Delete…"
                                        } else {
                                            "Discard…"
                                        })
                                        .disabled(busy)
                                        .on_click(
                                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                                cx.stop_propagation();
                                                this.confirm_discard(
                                                    section,
                                                    vec![discard_entry.clone()],
                                                    window,
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                },
                            )
                            .child(
                                Button::new(ElementId::NamedInteger("row-stage".into(), ix as u64))
                                    .ghost()
                                    .xsmall()
                                    .icon(if stage {
                                        IconName::Plus
                                    } else {
                                        IconName::Minus
                                    })
                                    .tooltip(if stage { "Stage" } else { "Unstage" })
                                    .disabled(busy)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        let entries = vec![entry.clone()];
                                        if stage {
                                            this.stage_entries(entries, cx)
                                        } else {
                                            this.unstage_entries(entries, cx)
                                        }
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        })
    }

    fn render_diff(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let message = |text: SharedString| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(palette.muted)
                .child(text)
                .into_any_element()
        };
        let model = match &self.diff {
            DiffState::Empty => return message("Select a file to see its changes.".into()),
            DiffState::Loading => return message("Loading diff…".into()),
            DiffState::Message(text) => return message(text.clone()),
            DiffState::Ready(model) => model.clone(),
        };
        let Some(sel) = self.selected.clone() else {
            return message("Select a file to see its changes.".into());
        };
        let busy = self.is_busy();
        let picked = self.picked_count();
        let line_staging = matches!(sel.section, Section::Staged | Section::Unstaged);

        let header = h_flex()
            .h(zoomed(32., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .text_xs()
            .child(
                div()
                    .text_sm()
                    .text_color(palette.emph)
                    .truncate()
                    .child(SharedString::from(sel.path.to_str_lossy().into_owned())),
            )
            .child(
                div()
                    .text_color(palette.muted)
                    .child(SharedString::from(format!(
                        "{} hunk{} · +{} −{}",
                        model.file.hunks.len(),
                        if model.file.hunks.len() == 1 { "" } else { "s" },
                        model.added,
                        model.removed
                    ))),
            )
            .child(div().flex_1())
            .when(picked > 0 && line_staging, |d| {
                let (label, action) = match sel.section {
                    Section::Staged => (format!("Unstage {picked} lines"), PartAction::Unstage),
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
                .when(sel.section == Section::Unstaged, |d| {
                    d.child(
                        Button::new("lines-discard")
                            .ghost()
                            .xsmall()
                            .label("Discard lines…")
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
            });

        if model.is_large() && !self.show_large {
            return v_flex()
                .size_full()
                .child(header)
                .child(
                    v_flex()
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
                        ),
                )
                .into_any_element();
        }

        let count = model.rows(self.layout).len();
        v_flex()
            .size_full()
            .child(header)
            .child(
                uniform_list(
                    "changes-diff",
                    count,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|ix| this.render_diff_row(&model, ix, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.diff_scroll)
                .flex_1(),
            )
            .into_any_element()
    }

    fn render_diff_row(
        &self,
        model: &DiffModel,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let palette = Palette::of(cx);
        let row = *model.rows(self.layout).get(ix)?;
        let section = self.selected.as_ref()?.section;
        let busy = self.is_busy();
        let selectable = matches!(section, Section::Staged | Section::Unstaged);
        Some(match row {
            DiffRow::Hunk { hunk } => {
                let h = model.file.hunks.get(hunk)?;
                let mut actions = h_flex().gap_1();
                let button = |id: &'static str, label: &'static str| {
                    Button::new(ElementId::NamedInteger(id.into(), hunk as u64))
                        .ghost()
                        .xsmall()
                        .label(label)
                        .disabled(busy)
                };
                match section {
                    Section::Unstaged => {
                        actions = actions
                            .child(button("hunk-stage", "Stage hunk").on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    this.apply_part(PartAction::Stage, Some(hunk), cx)
                                },
                            )))
                            .child(button("hunk-discard", "Discard…").on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.confirm_discard_part(Some(hunk), window, cx)
                                },
                            )))
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
                    .bg(palette.panel2)
                    .text_xs()
                    .text_color(palette.muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(h.header.to_str_lossy().into_owned())),
                    )
                    .child(actions)
                    .into_any_element()
            }
            DiffRow::Line { hunk, line } => {
                let l = model.file.hunks.get(hunk)?.lines.get(line)?;
                let view = model.lines.get(hunk)?.get(line)?;
                let picked = self.picked.get(&hunk).is_some_and(|s| s.contains(&line));
                let changed = matches!(l.kind, LineKind::Added | LineKind::Deleted);
                div()
                    .id(ElementId::NamedInteger("diff-line".into(), ix as u64))
                    .w_full()
                    .when(selectable && changed, |d| {
                        d.cursor_pointer().on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.toggle_line(hunk, line, cx),
                        ))
                    })
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
                    let picked = self.picked.get(&hunk).is_some_and(|s| s.contains(&line));
                    let changed = matches!(l.kind, LineKind::Added | LineKind::Deleted);
                    let number = if old { l.old_lineno } else { l.new_lineno };
                    div()
                        .id(ElementId::NamedInteger(
                            if old { "diff-left" } else { "diff-right" }.into(),
                            ix as u64,
                        ))
                        .w_full()
                        .when(selectable && changed, |d| {
                            d.cursor_pointer().on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| this.toggle_line(hunk, line, cx),
                            ))
                        })
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

impl Focusable for ChangesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChangesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.bg)
            .child(self.render_toolbar(&palette, cx))
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
                            .child(self.render_diff(&palette, cx)),
                    ),
            )
    }
}
