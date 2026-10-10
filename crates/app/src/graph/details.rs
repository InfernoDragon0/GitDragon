//! Details panel (right dock): the selected commit's message, people, ids, refs and changed
//! files. Clicking a file opens its diff in the centre; clicking a parent selects it.
//!
//! The graph's WIP row shows the working copy instead: the tab's commit box at the top, then
//! stash / pop, a filter, and the conflicted / staged / unstaged / untracked files with stage,
//! unstage, discard and LFS lock actions; clicking one opens its staging diff in the centre.
//! A stash row adds apply / pop / drop.

use std::sync::Arc;

use bstr::BString;
use bstr::ByteSlice as _;
use git_core::parse::diff::{FileChange, FileDiff, LineKind};
use git_core::parse::log::Commit;
use git_core::parse::status::{EntryKind, StatusEntry};
use git_core::{DiffTarget, GitRepo as _, LogQuery, StashAction};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use ui::{Palette, zoomed};

use super::format;
use super::link::{GraphLink, LinkEvent};
use crate::changes::commit_box::CommitBox;
use crate::changes::{self, Section};
use crate::repo_tab::RepoTab;

/// Commits kept loaded for going back to them.
const CACHE: usize = 32;
/// Opened file diffs kept (per commit and path).
const DIFF_CACHE: usize = 64;
/// Files listed before "Show all" (huge commits stay quick to render).
const FILE_LIMIT: usize = 300;

/// Path -> (added, removed) from `--numstat`; `None` counts for binary files.
type LineCounts = std::collections::HashMap<Vec<u8>, (Option<u32>, Option<u32>)>;

/// A changed file with its line counts.
struct FileEntry {
    /// A commit's file, whose diff is read when opened (`None` for the WIP lists, which open
    /// the staging diff instead).
    source: Option<Arc<git_core::ChangedFile>>,
    change: FileChange,
    path: String,
    added: usize,
    removed: usize,
    /// No line counts (binary).
    binary: bool,
    /// `added` / `removed` are known (a commit's counts arrive after its list).
    counted: bool,
    /// Stored in Git LFS: the size change replaces the pointer's line counts.
    lfs: Option<git_core::lfs::PointerChange>,
    /// Routed through LFS (`filter=lfs`), size unknown until opened.
    lfs_tracked: bool,
}

struct Loaded {
    commit: Commit,
    files: Vec<FileEntry>,
    /// False while only the header is in.
    files_loaded: bool,
}

/// The worktree behind the graph's WIP row.
struct WipLoaded {
    staged: Vec<FileEntry>,
    unstaged: Vec<FileEntry>,
    /// Untracked paths (no diff until opened).
    untracked: Vec<String>,
    /// Unmerged paths (resolved in the Conflicts view, marked resolved by staging).
    conflicted: Vec<String>,
}

enum State {
    Empty,
    Loading,
    Ready(Arc<Loaded>),
    Wip(Arc<WipLoaded>),
    Failed(SharedString),
}

/// The WIP row's id: all zeros (see `git_core::graph::History::with_pseudo`).
pub fn is_wip_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b == b'0')
}

pub struct DetailsPanel {
    link: Entity<GraphLink>,
    /// (tab, commit) the state was loaded for.
    shown: Option<(WeakEntity<RepoTab>, String)>,
    state: State,
    load_task: Option<Task<()>>,
    /// Recently shown commits, newest last.
    cache: Vec<(String, Arc<Loaded>)>,
    /// Opened commit file diffs: ((commit, path), diff), newest last.
    diff_cache: Vec<((String, String), Arc<FileDiff>)>,
    diff_task: Option<Task<()>>,
    /// List every file of a huge commit, not just the first [`FILE_LIMIT`].
    show_all: bool,
    /// While the WIP row is shown: reloads it when the tab's status changes.
    tab_sub: Option<Subscription>,
    wip_generation: u64,
    /// One commit box per repository tab, so drafts survive switching rows and tabs.
    commit_boxes: Vec<(WeakEntity<RepoTab>, Entity<CommitBox>)>,
    /// Filters the WIP file lists; made on first render (it needs the window).
    filter: Option<(Entity<InputState>, Subscription)>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl DetailsPanel {
    pub fn new(link: Entity<GraphLink>, cx: &mut Context<Self>) -> Self {
        let _subs = vec![cx.observe(&link, |this, _, cx| this.on_link_changed(cx))];
        Self {
            link,
            shown: None,
            state: State::Empty,
            load_task: None,
            cache: Vec::new(),
            diff_cache: Vec::new(),
            diff_task: None,
            show_all: false,
            tab_sub: None,
            wip_generation: 0,
            commit_boxes: Vec::new(),
            filter: None,
            focus: cx.focus_handle(),
            _subs,
        }
    }

    fn on_link_changed(&mut self, cx: &mut Context<Self>) {
        let link = self.link.read(cx);
        let wanted = link.tab().cloned().zip(link.commit().map(str::to_owned));
        if wanted == self.shown {
            cx.notify();
            return;
        }
        self.shown = wanted.clone();
        self.tab_sub = None;
        let Some((tab, commit)) = wanted else {
            self.state = State::Empty;
            self.load_task = None;
            cx.notify();
            return;
        };
        if is_wip_id(&commit) {
            if let Some(tab) = tab.upgrade() {
                self.tab_sub = Some(cx.observe(&tab, |this, tab, cx| {
                    if tab.read(cx).generation != this.wip_generation {
                        this.load_wip(cx);
                    } else {
                        // Busy state changed: the WIP buttons enable / disable.
                        cx.notify();
                    }
                }));
            }
            self.state = State::Loading;
            self.load_wip(cx);
            return;
        }
        self.show_all = false;
        if let Some(ix) = self.cache.iter().position(|(id, _)| *id == commit) {
            let entry = self.cache.remove(ix);
            self.state = State::Ready(entry.1.clone());
            self.cache.push(entry);
            self.load_task = None;
            cx.notify();
            return;
        }
        let Some((repo, lfs)) = tab
            .upgrade()
            .and_then(|t| Some((t.read(cx).repo.clone()?, t.read(cx).uses_lfs())))
        else {
            self.state = State::Empty;
            cx.notify();
            return;
        };
        self.state = State::Loading;
        cx.notify();
        // Three steps, each shown as soon as it is in: the header (one `log -1`), the file
        // list (`diff --raw`), then line counts and LFS chips together (both read blobs or
        // attributes for every file: the slow part of big commits).
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let bg = cx.background_executor().clone();
            let fail = |err: git_core::GitError,
                        this: &gpui_kit::WeakEntity<Self>,
                        cx: &mut gpui_kit::AsyncApp| {
                _ = this.update(cx, |this, cx| {
                    this.state = State::Failed(err.to_string().into());
                    cx.notify();
                });
            };
            let show = |loaded: &Arc<Loaded>,
                        this: &gpui_kit::WeakEntity<Self>,
                        cx: &mut gpui_kit::AsyncApp| {
                let loaded = loaded.clone();
                _ = this.update(cx, |this, cx| {
                    this.state = State::Ready(loaded);
                    cx.notify();
                });
            };

            let header = {
                let repo = repo.clone();
                let id = commit.clone();
                bg.spawn(async move {
                    let query = LogQuery {
                        revs: vec![id.clone()],
                        limit: Some(1),
                        topo_order: false,
                        ..LogQuery::default()
                    };
                    repo.log(query).await?.into_iter().next().ok_or(
                        git_core::GitError::InvalidArgument {
                            what: "commit",
                            value: id,
                        },
                    )
                })
                .await
            };
            let header = match header {
                Ok(header) => header,
                Err(err) => return fail(err, &this, cx),
            };
            show(
                &Arc::new(Loaded {
                    commit: header.clone(),
                    files: Vec::new(),
                    files_loaded: false,
                }),
                &this,
                cx,
            );

            let listed = {
                let repo = repo.clone();
                let id = commit.clone();
                bg.spawn(async move { repo.commit_files(&id, false).await })
                    .await
            };
            let listed = match listed {
                Ok(listed) => listed,
                Err(err) => return fail(err, &this, cx),
            };
            let entries = |counts: Option<&LineCounts>,
                           tracked: &std::collections::HashMap<
                bstr::BString,
                git_core::lfs::LfsAttrs,
            >| {
                listed
                    .iter()
                    .map(|f| {
                        let (added, removed) = counts
                            .and_then(|c| c.get(f.path()).copied())
                            .unwrap_or_default();
                        FileEntry {
                            change: f.change,
                            path: f.path().to_str_lossy().into_owned(),
                            added: added.unwrap_or(0) as usize,
                            removed: removed.unwrap_or(0) as usize,
                            binary: counts.is_some() && added.is_none(),
                            counted: counts.is_some(),
                            lfs: None,
                            lfs_tracked: tracked.get(f.path()).is_some_and(|a| a.lfs),
                            source: Some(Arc::new(f.clone())),
                        }
                    })
                    .collect::<Vec<_>>()
            };
            show(
                &Arc::new(Loaded {
                    commit: header.clone(),
                    files: entries(None, &Default::default()),
                    files_loaded: true,
                }),
                &this,
                cx,
            );

            let paths: Vec<bstr::BString> = listed.iter().map(|f| f.path().into()).collect();
            let id = commit.clone();
            let (counted, tracked) = bg
                .spawn(async move {
                    let attrs = async {
                        if lfs {
                            repo.lfs_attrs(&paths).await.unwrap_or_default()
                        } else {
                            Default::default()
                        }
                    };
                    futures_lite::future::zip(repo.commit_files(&id, true), attrs).await
                })
                .await;
            let counts: Option<LineCounts> = counted.ok().map(|files| {
                files
                    .iter()
                    .map(|f| (f.path().to_vec(), (f.added, f.removed)))
                    .collect()
            });
            let full = Arc::new(Loaded {
                commit: header,
                files: entries(counts.as_ref(), &tracked),
                files_loaded: true,
            });
            _ = this.update(cx, |this, cx| {
                this.cache.push((commit, full.clone()));
                if this.cache.len() > CACHE {
                    this.cache.remove(0);
                }
                this.state = State::Ready(full);
                cx.notify();
            });
        }));
    }

    /// Opens a commit file: from the diff cache, or read now (just this file).
    fn open_commit_file(
        &mut self,
        file: Arc<git_core::ChangedFile>,
        path: String,
        cx: &mut Context<Self>,
    ) {
        let Some((tab, commit)) = self.shown.clone() else {
            return;
        };
        let key = (commit.clone(), path.clone());
        if let Some((_, diff)) = self.diff_cache.iter().find(|(k, _)| *k == key) {
            let diff = diff.clone();
            self.open_file(diff, path, cx);
            return;
        }
        let Some(repo) = tab.upgrade().and_then(|t| t.read(cx).repo.clone()) else {
            return;
        };
        // Highlight it now; the diff follows.
        self.link
            .update(cx, |link, cx| link.set_open_file(Some(path.clone()), cx));
        let job = cx
            .background_executor()
            .spawn(async move { repo.commit_file_diff(&commit, &file).await });
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Some(diff)) = job.await else { return };
            _ = this.update(cx, |this, cx| {
                let diff = Arc::new(diff);
                this.diff_cache.push((key, diff.clone()));
                if this.diff_cache.len() > DIFF_CACHE {
                    this.diff_cache.remove(0);
                }
                // Still the file the user wants?
                if this.link.read(cx).open_file() == Some(path.as_str()) {
                    this.open_file(diff, path, cx);
                }
            });
        }));
    }

    /// Reads the WIP row's staged and unstaged diffs (untracked paths come from the status).
    fn load_wip(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.shown.as_ref().and_then(|(t, _)| t.upgrade()) else {
            return;
        };
        let tab = tab.read(cx);
        let Some(repo) = tab.repo.clone() else {
            return;
        };
        self.wip_generation = tab.generation;
        let paths = |kind: EntryKind| -> Vec<String> {
            tab.status
                .iter()
                .flat_map(|s| &s.entries)
                .filter(|e| e.kind == kind)
                .map(|e| e.path.to_str_lossy().into_owned())
                .collect()
        };
        let untracked = paths(EntryKind::Untracked);
        let conflicted = paths(EntryKind::Unmerged);
        let lfs: std::collections::HashSet<String> = tab
            .lfs_attrs
            .iter()
            .filter(|(_, attrs)| attrs.lfs)
            .map(|(path, _)| path.to_str_lossy().into_owned())
            .collect();
        let job = cx.background_executor().spawn(async move {
            let staged = repo.diff(DiffTarget::Staged { paths: Vec::new() }).await?;
            let unstaged = repo
                .diff(DiffTarget::Unstaged { paths: Vec::new() })
                .await?;
            let entries = |files: Vec<FileDiff>| -> Vec<FileEntry> {
                files
                    .into_iter()
                    .map(file_entry)
                    // Conflicted files are listed on their own.
                    .filter(|f| !conflicted.contains(&f.path))
                    .map(|mut f| {
                        f.lfs_tracked = lfs.contains(&f.path);
                        f
                    })
                    .collect()
            };
            Ok::<_, git_core::GitError>(WipLoaded {
                staged: entries(staged),
                unstaged: entries(unstaged),
                untracked,
                conflicted,
            })
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(loaded) => State::Wip(Arc::new(loaded)),
                    Err(err) => State::Failed(err.to_string().into()),
                };
                cx.notify();
            });
        }));
    }

    /// `stash@{n}` for a commit id, if it is one of the tab's stashes.
    fn stash_index(&self, id: &str, cx: &App) -> Option<u32> {
        let tab = self.shown.as_ref()?.0.upgrade()?;
        tab.read(cx)
            .stashes
            .iter()
            .find(|s| s.oid == id)
            .map(|s| s.index)
    }

    fn stash_action(
        &mut self,
        action: StashAction,
        index: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.shown.as_ref().and_then(|(t, _)| t.upgrade()) else {
            return;
        };
        let run = move |tab: &Entity<RepoTab>, cx: &mut App| {
            let verb = match action {
                StashAction::Apply => "Applying",
                StashAction::Pop => "Popping",
                StashAction::Drop => "Dropping",
            };
            tab.update(cx, |tab, cx| {
                tab.run_op(
                    format!("{verb} stash@{{{index}}}"),
                    cx,
                    move |repo| async move { repo.stash_action(action, index).await },
                )
            });
        };
        if action == StashAction::Drop {
            let tab = tab.downgrade();
            crate::dialogs::confirm(
                window,
                cx,
                "Drop stash?",
                format!("stash@{{{index}}} is deleted. This cannot be undone from GitDragon."),
                "Drop",
                true,
                move |_, cx| {
                    if let Some(tab) = tab.upgrade() {
                        run(&tab, cx);
                    }
                },
            );
        } else {
            run(&tab, cx);
        }
    }

    /// One file line: change letter, name, folder, LFS chip, +/- counts. The caller adds what
    /// a click does and any buttons.
    fn file_row(
        &self,
        key: &'static str,
        i: usize,
        f: &FileEntry,
        selected: bool,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let (letter, color) = change_letter(&f.change, palette);
        let (dir, name) = match f.path.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_string()),
            None => (String::new(), f.path.clone()),
        };
        let lfs_size = f.lfs.as_ref().map(|c| {
            let size = |p: &Option<git_core::lfs::Pointer>| {
                p.as_ref().map(|p| git_core::lfs::human_size(p.size))
            };
            match (size(&c.old), size(&c.new)) {
                (Some(old), Some(new)) if old != new => format!("{old} \u{2192} {new}"),
                (_, Some(new)) => new,
                (Some(old), None) => old,
                (None, None) => String::new(),
            }
        });
        let lfs_size = lfs_size.or_else(|| f.lfs_tracked.then(String::new));
        let counts = f.counted && lfs_size.is_none() && !f.binary;
        name_row(key, i, letter, color, name, dir, selected, palette, cx)
            .when_some(lfs_size, |d, size| {
                d.child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .text_color(palette.accent)
                        .bg(palette.accent.opacity(0.14))
                        .child("LFS"),
                )
                .when(!size.is_empty(), |d| {
                    d.child(div().flex_none().text_color(palette.muted).child(size))
                })
            })
            .when(
                f.counted && f.binary && f.lfs.is_none() && !f.lfs_tracked,
                |d| d.child(div().flex_none().text_color(palette.muted).child("bin")),
            )
            .when(counts && f.added > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(palette.ok)
                        .child(format!("+{}", f.added)),
                )
            })
            .when(counts && f.removed > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(palette.error)
                        .child(format!("\u{2212}{}", f.removed)),
                )
            })
    }

    /// The repository tab whose row is shown.
    fn shown_tab(&self) -> Option<Entity<RepoTab>> {
        self.shown.as_ref().and_then(|(t, _)| t.upgrade())
    }

    /// The commit box of `tab`, made on first use.
    fn commit_box(
        &mut self,
        tab: &Entity<RepoTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<CommitBox> {
        let weak = tab.downgrade();
        if let Some((_, commit_box)) = self.commit_boxes.iter().find(|(t, _)| *t == weak) {
            return commit_box.clone();
        }
        // Closed tabs: drop their drafts.
        self.commit_boxes.retain(|(t, _)| t.upgrade().is_some());
        let commit_box = cx.new(|cx| CommitBox::new(tab.clone(), window, cx));
        self.commit_boxes.push((weak, commit_box.clone()));
        commit_box
    }

    fn filter_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some((input, _)) = &self.filter {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let sub = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        self.filter = Some((input.clone(), sub));
        input
    }

    /// The tab's status entries of `section`, only those at `paths` when given.
    fn wip_entries(
        &self,
        section: Section,
        paths: Option<&[String]>,
        cx: &App,
    ) -> Vec<StatusEntry> {
        let Some(tab) = self.shown_tab() else {
            return Vec::new();
        };
        let Some(status) = tab.read(cx).status.as_ref() else {
            return Vec::new();
        };
        section
            .entries(status)
            .into_iter()
            .filter(|e| paths.is_none_or(|p| p.iter().any(|p| e.path.as_slice() == p.as_bytes())))
            .collect()
    }

    /// Stages (unstages for [`Section::Staged`]; marks resolved for conflicts) the files of
    /// `section` at `paths`.
    fn wip_stage(&mut self, section: Section, paths: &[String], cx: &mut Context<Self>) {
        let Some(tab) = self.shown_tab() else {
            return;
        };
        let entries = self.wip_entries(section, Some(paths), cx);
        if !entries.is_empty() {
            changes::stage(&tab, &entries, section != Section::Staged, cx);
        }
    }

    /// Stages (or unstages) every change at once, new files included.
    fn wip_stage_everything(&mut self, stage: bool, cx: &mut Context<Self>) {
        if let Some(tab) = self.shown_tab() {
            changes::stage(&tab, &[], stage, cx);
        }
    }

    /// Discards (tracked) or deletes (untracked) the files of `section` at `paths`, after
    /// asking.
    fn wip_discard(
        &mut self,
        section: Section,
        paths: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.shown_tab() else {
            return;
        };
        let entries = self.wip_entries(section, Some(paths), cx);
        changes::confirm_discard(&tab, section, entries, window, cx);
    }

    /// Opens a working-copy file's staging diff in the centre.
    fn open_working(&mut self, section: Section, path: String, cx: &mut Context<Self>) {
        let Some((tab, _)) = self.shown.clone() else {
            return;
        };
        self.link.update(cx, |link, cx| {
            let bytes = BString::from(path.as_str());
            link.set_open_file(Some(path), cx);
            cx.emit(LinkEvent::ShowWorkingFile {
                tab,
                section,
                path: bytes,
            });
        });
    }

    /// Adds the WIP actions to a file row: lock chip, lock / discard on hover, the stage
    /// toggle. A click opens the staging diff; a double click toggles staging.
    #[allow(clippy::too_many_arguments)]
    fn wip_row(
        &self,
        row: gpui_kit::Stateful<gpui_kit::Div>,
        key: &'static str,
        i: usize,
        section: Section,
        path: &str,
        busy: bool,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let bytes = BString::from(path);
        let (attrs, lock) = self.shown_tab().map_or_else(Default::default, |tab| {
            let tab = tab.read(cx);
            let attrs = tab.lfs_attrs.get(&bytes).copied().unwrap_or_default();
            let lock = tab
                .lock_on(&bytes)
                .map(|l| (l.ours, SharedString::from(l.owner.clone())));
            (attrs, lock)
        });
        let id = |what: &str| ElementId::NamedInteger(format!("{key}-{what}").into(), i as u64);
        let button = |what: &str| Button::new(id(what)).ghost().xsmall().disabled(busy);
        let (toggle, toggle_tip) = match section {
            Section::Staged => ("\u{2212}", "Unstage"),
            Section::Conflicted => ("+", "Mark resolved (stage it)"),
            Section::Unstaged | Section::Untracked => ("+", "Stage"),
        };
        let discard = matches!(section, Section::Unstaged | Section::Untracked);
        // Lockable LFS files: lock before editing so nobody else does.
        let lockable = attrs.lockable && lock.as_ref().is_none_or(|(ours, _)| *ours);
        let locked = lock.is_some();
        let open = path.to_string();
        let one = vec![path.to_string()];
        let (stage_one, discard_one) = (one.clone(), one.clone());
        row.group("wip-row")
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.wip_stage(section, &one, cx)
                } else {
                    this.open_working(section, open.clone(), cx)
                }
            }))
            .when_some(lock.clone(), |d, (ours, owner)| {
                d.child(changes::lock_badge(id("lock-badge"), ours, owner, palette))
            })
            .child(
                h_flex()
                    .flex_none()
                    .invisible()
                    .group_hover("wip-row", |s| s.visible())
                    .when(lockable, |d| {
                        d.child(
                            button("lock")
                                .icon(if locked {
                                    IconName::LockOpen
                                } else {
                                    IconName::Lock
                                })
                                .tooltip(if locked {
                                    "Unlock (let others edit it)"
                                } else {
                                    "Lock (stop others editing it)"
                                })
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    let Some(tab) = this.shown_tab() else { return };
                                    let path = bytes.clone();
                                    tab.update(cx, |tab, cx| {
                                        tab.set_lock(path, !locked, false, cx)
                                    });
                                })),
                        )
                    })
                    .when(discard, |d| {
                        d.child(
                            button("discard")
                                .icon(IconName::Undo2)
                                .tooltip(if section == Section::Untracked {
                                    "Delete\u{2026}"
                                } else {
                                    "Discard\u{2026}"
                                })
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.wip_discard(section, &discard_one, window, cx)
                                })),
                        )
                    }),
            )
            .child(
                button("toggle")
                    .label(toggle)
                    .tooltip(toggle_tip)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.wip_stage(section, &stage_one, cx)
                    })),
            )
    }

    fn render_wip(
        &self,
        wip: &Arc<WipLoaded>,
        commit_box: Entity<CommitBox>,
        filter: Entity<InputState>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.link.read(cx).open_file().map(str::to_owned);
        let (busy, stash_count) = self.shown_tab().map_or((false, 0), |t| {
            let t = t.read(cx);
            (t.is_busy(), t.status.as_ref().map_or(0, |s| s.stash_count))
        });
        let needle = filter.read(cx).value().trim().to_lowercase();
        let filtering = !needle.is_empty();
        let shown = |path: &str| !filtering || path.to_lowercase().contains(&needle);
        let staged: Vec<&FileEntry> = wip.staged.iter().filter(|f| shown(&f.path)).collect();
        let unstaged: Vec<&FileEntry> = wip.unstaged.iter().filter(|f| shown(&f.path)).collect();
        let untracked: Vec<&String> = wip.untracked.iter().filter(|p| shown(p)).collect();
        let conflicted: Vec<&String> = wip.conflicted.iter().filter(|p| shown(p)).collect();
        let paths_of =
            |files: &[&FileEntry]| files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        let strings = |paths: &[&String]| paths.iter().map(|p| (*p).clone()).collect::<Vec<_>>();

        let display = ui::fonts(cx).display;
        let heading = |text: String| {
            div()
                .flex_1()
                .pt_1()
                .pb_0p5()
                .text_xs()
                .font_family(display.clone())
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(palette.muted)
                .child(text)
        };
        let section_button = |id: &'static str, label: &'static str, tip: &'static str| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(label)
                .tooltip(tip)
                .disabled(busy)
        };
        let discard_all = |id: &'static str, tip: &'static str| {
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(IconName::Undo2)
                .tooltip(tip)
                .disabled(busy)
        };
        let none =
            |text: &'static str| div().px_1().text_xs().text_color(palette.muted).child(text);
        let split = |p: &str| match p.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_string()),
            None => (String::new(), p.to_string()),
        };

        let mut body = v_flex().gap_0p5().p_3().child(
            h_flex()
                .gap_1()
                .pb_1()
                .child(ui::typography::item_title("Uncommitted changes", cx).flex_1())
                .child(
                    Button::new("wip-explain")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Bot)
                        .tooltip("Ask Claude to explain the uncommitted changes")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            if let Some(tab) = this.shown_tab() {
                                crate::chat::context::explain_wip(&tab, window, cx)
                            }
                        })),
                )
                .child(
                    Button::new("wip-stash")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Archive)
                        .label("Stash")
                        .disabled(busy)
                        .tooltip("Stash all changes, including untracked files")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(tab) = this.shown_tab() {
                                changes::stash(&tab, cx)
                            }
                        })),
                )
                .child(
                    Button::new("wip-stash-pop")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArchiveRestore)
                        .label(format!("Pop ({stash_count})"))
                        .disabled(busy || stash_count == 0)
                        .tooltip("Apply the latest stash and drop it")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if let Some(tab) = this.shown_tab() {
                                changes::stash_pop(&tab, cx)
                            }
                        })),
                ),
        );
        body = body.child(
            div()
                .pb_1()
                .child(Input::new(&filter).small().cleanable(true)),
        );

        if !conflicted.is_empty() {
            let paths = strings(&conflicted);
            body = body.child(
                h_flex()
                    .child(heading(format!("CONFLICTED ({})", conflicted.len())))
                    .child(
                        section_button(
                            "wip-resolve-all",
                            "Mark all resolved",
                            "Stage every conflicted file as resolved",
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.wip_stage(Section::Conflicted, &paths, cx)
                            },
                        )),
                    ),
            );
            for (i, p) in conflicted.iter().enumerate() {
                let (dir, name) = split(p);
                let row = name_row(
                    "wip-conflicted",
                    i,
                    "U",
                    palette.warn,
                    name,
                    dir,
                    open.as_deref() == Some(p.as_str()),
                    palette,
                    cx,
                );
                body = body.child(self.wip_row(
                    row,
                    "wip-conflicted",
                    i,
                    Section::Conflicted,
                    p,
                    busy,
                    palette,
                    cx,
                ));
            }
        }

        let staged_paths = paths_of(&staged);
        body = body.child(
            h_flex()
                .when(!conflicted.is_empty(), |d| d.pt_2())
                .child(heading(format!("STAGED ({})", staged.len())))
                .when(!staged.is_empty(), |d| {
                    d.child(
                        section_button(
                            "wip-unstage-all",
                            "Unstage all",
                            "Move every staged change back",
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                if filtering {
                                    this.wip_stage(Section::Staged, &staged_paths, cx)
                                } else {
                                    this.wip_stage_everything(false, cx)
                                }
                            },
                        )),
                    )
                }),
        );
        if staged.is_empty() {
            body = body.child(none("Nothing staged yet."));
        }
        for (i, f) in staged.iter().enumerate() {
            let selected = open.as_deref() == Some(f.path.as_str());
            let row = self.file_row("wip-staged", i, f, selected, palette, cx);
            body = body.child(self.wip_row(
                row,
                "wip-staged",
                i,
                Section::Staged,
                &f.path,
                busy,
                palette,
                cx,
            ));
        }

        let unstaged_paths = paths_of(&unstaged);
        let discard_paths = unstaged_paths.clone();
        body = body.child(
            h_flex()
                .pt_2()
                .child(heading(format!("UNSTAGED ({})", unstaged.len())))
                .when(!unstaged.is_empty(), |d| {
                    d.child(
                        section_button(
                            "wip-stage-all",
                            "Stage all",
                            if filtering {
                                "Stage the listed changes"
                            } else {
                                "Stage every change and new file"
                            },
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                if filtering {
                                    this.wip_stage(Section::Unstaged, &unstaged_paths, cx)
                                } else {
                                    this.wip_stage_everything(true, cx)
                                }
                            },
                        )),
                    )
                })
                .when(!unstaged.is_empty(), |d| {
                    d.child(
                        discard_all("wip-discard-all", "Discard all unstaged changes\u{2026}")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.wip_discard(Section::Unstaged, &discard_paths, window, cx)
                            })),
                    )
                }),
        );
        if unstaged.is_empty() {
            body = body.child(none("No unstaged changes."));
        }
        for (i, f) in unstaged.iter().enumerate() {
            let selected = open.as_deref() == Some(f.path.as_str());
            let row = self.file_row("wip-unstaged", i, f, selected, palette, cx);
            body = body.child(self.wip_row(
                row,
                "wip-unstaged",
                i,
                Section::Unstaged,
                &f.path,
                busy,
                palette,
                cx,
            ));
        }

        if !untracked.is_empty() {
            let stage_paths = strings(&untracked);
            let delete_paths = stage_paths.clone();
            body = body.child(
                h_flex()
                    .pt_2()
                    .child(heading(format!("UNTRACKED ({})", untracked.len())))
                    .child(
                        section_button("wip-stage-new", "Stage all", "Stage the new files")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.wip_stage(Section::Untracked, &stage_paths, cx)
                            })),
                    )
                    .child(
                        discard_all("wip-delete-all", "Delete all untracked files\u{2026}")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.wip_discard(Section::Untracked, &delete_paths, window, cx)
                            })),
                    ),
            );
            for (i, p) in untracked.iter().enumerate() {
                let (dir, name) = split(p);
                let lfs = self.shown_tab().is_some_and(|t| {
                    t.read(cx)
                        .lfs_attrs
                        .get(&BString::from(p.as_str()))
                        .is_some_and(|a| a.lfs)
                });
                let row = name_row(
                    "wip-untracked",
                    i,
                    "?",
                    palette.ok,
                    name,
                    dir,
                    open.as_deref() == Some(p.as_str()),
                    palette,
                    cx,
                )
                .when(lfs, |d| d.child(changes::lfs_badge(palette)));
                body = body.child(self.wip_row(
                    row,
                    "wip-untracked",
                    i,
                    Section::Untracked,
                    p,
                    busy,
                    palette,
                    cx,
                ));
            }
        }

        v_flex()
            .text_sm()
            .child(
                div()
                    .p_3()
                    .border_b_1()
                    .border_color(palette.border)
                    .child(commit_box),
            )
            .child(body)
            .into_any_element()
    }

    fn open_file(&mut self, file: Arc<FileDiff>, path: String, cx: &mut Context<Self>) {
        let Some((tab, commit)) = self.shown.clone() else {
            return;
        };
        self.link.update(cx, |link, cx| {
            link.set_open_file(Some(path), cx);
            cx.emit(LinkEvent::ShowFile { tab, commit, file });
        });
    }

    fn select_commit(&mut self, commit: String, cx: &mut Context<Self>) {
        let Some((tab, _)) = self.shown.clone() else {
            return;
        };
        self.link
            .update(cx, |_, cx| cx.emit(LinkEvent::Select { tab, commit }));
    }

    fn render_loaded(
        &self,
        loaded: &Arc<Loaded>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &loaded.commit;
        let mono = cx.theme().mono_font_family.clone();
        let label = |text: &'static str| {
            div()
                .w(zoomed(64., cx))
                .flex_none()
                .text_color(palette.muted)
                .child(text)
        };
        let person = |sig: &git_core::parse::log::Signature| {
            let name = sig.name.to_str_lossy().into_owned();
            let email = sig.email.to_str_lossy().into_owned();
            h_flex()
                .min_w_0()
                .gap_1p5()
                .child(
                    div()
                        .flex_none()
                        .size(zoomed(18., cx))
                        .rounded_full()
                        .bg(palette.lane(format::avatar_slot(&email)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(zoomed(9., cx))
                        .text_color(palette.white)
                        .child(SharedString::from(format::initials(&name))),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_color(palette.fg)
                                .child(SharedString::from(ui::sanitize_line(&name).into_owned())),
                        )
                        .child(div().text_xs().text_color(palette.muted).child(
                            SharedString::from(format::full_time(sig.time, sig.tz_offset_minutes)),
                        )),
                )
        };
        let same_committer =
            c.committer.name == c.author.name && c.committer.email == c.author.email;
        let id = c.id.clone();
        let (added, removed) = loaded
            .files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let counted = loaded.files_loaded && loaded.files.iter().all(|f| f.counted);
        let open = self.link.read(cx).open_file().map(str::to_owned);
        let stash = self.stash_index(&c.id, cx);

        v_flex()
            .gap_3()
            .p_3()
            .text_sm()
            .when_some(stash, |d, index| {
                let this = cx.entity().downgrade();
                // Pop is the usual way back; keeping the stash or deleting it are in the menu.
                let more = Button::new("stash-more")
                    .ghost()
                    .small()
                    .icon(IconName::Ellipsis)
                    .tooltip("More stash actions")
                    .dropdown_menu(move |menu, _, _| {
                        let item = |label: &'static str, action: StashAction| {
                            let this = this.clone();
                            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                _ = this.update(cx, |view, cx| {
                                    view.stash_action(action, index, window, cx)
                                });
                            })
                        };
                        menu.item(item("Apply (keep the stash)", StashAction::Apply))
                            .separator()
                            .item(item("Drop\u{2026}", StashAction::Drop))
                    });
                d.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(format!("stash@{{{index}}}"))),
                        )
                        .child(
                            Button::new("stash-pop")
                                .primary()
                                .small()
                                .label("Pop")
                                .tooltip("Bring the changes back and delete the stash")
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.stash_action(StashAction::Pop, index, window, cx)
                                })),
                        )
                        .child(more),
                )
            })
            .child(ui::typography::item_title(
                ui::sanitize_line(&c.subject.to_str_lossy()).into_owned(),
                cx,
            ))
            .when(!c.body.trim().is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.fg)
                        .child(SharedString::from(
                            ui::sanitize_display(c.body.to_str_lossy().trim()).into_owned(),
                        )),
                )
            })
            .child(
                v_flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Author"))
                            .child(person(&c.author)),
                    )
                    .when(!same_committer, |d| {
                        d.child(
                            h_flex()
                                .gap_2()
                                .child(label("Committer"))
                                .child(person(&c.committer)),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Commit"))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_color(palette.fg)
                                    .child(SharedString::from(
                                        c.id[..c.id.len().min(12)].to_string(),
                                    )),
                            )
                            .child(
                                Button::new("copy-sha")
                                    .ghost()
                                    .xsmall()
                                    .label("Copy")
                                    .tooltip("Copy the full SHA")
                                    .on_click({
                                        let id = id.clone();
                                        move |_: &ClickEvent, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                id.clone(),
                                            ))
                                        }
                                    }),
                            )
                            .child(
                                Button::new("explain-commit")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Bot)
                                    .label("Explain")
                                    .tooltip("Ask Claude to explain this commit's diff")
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        move |this, _: &ClickEvent, window, cx| {
                                            if let Some(tab) = this.shown_tab() {
                                                crate::chat::context::explain_commit(
                                                    &tab,
                                                    id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }
                                    })),
                            ),
                    )
                    .when(!c.parents.is_empty(), |d| {
                        d.child(
                            h_flex()
                                .gap_2()
                                .child(label(if c.parents.len() > 1 {
                                    "Parents"
                                } else {
                                    "Parent"
                                }))
                                .child(h_flex().flex_wrap().gap_1().children(
                                    c.parents.iter().enumerate().map(|(i, p)| {
                                        let parent = p.clone();
                                        div()
                                            .id(ElementId::NamedInteger("parent".into(), i as u64))
                                            .px_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .font_family(mono.clone())
                                            .text_color(palette.accent)
                                            .hover(|s| s.bg(palette.hover))
                                            .child(SharedString::from(
                                                p[..p.len().min(7)].to_string(),
                                            ))
                                            .on_click(cx.listener(
                                                move |this, _: &ClickEvent, _, cx| {
                                                    this.select_commit(parent.clone(), cx)
                                                },
                                            ))
                                    }),
                                )),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .pb_1()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(if loaded.files_loaded {
                                format!(
                                    "{} file{} changed",
                                    loaded.files.len(),
                                    if loaded.files.len() == 1 { "" } else { "s" }
                                )
                            } else {
                                "Loading files\u{2026}".to_string()
                            })
                            // Totals once the line counts are in.
                            .when(counted, |d| {
                                d.child(div().text_color(palette.ok).child(format!("+{added}")))
                                    .child(
                                        div()
                                            .text_color(palette.error)
                                            .child(format!("−{removed}")),
                                    )
                            }),
                    )
                    .children(
                        loaded
                            .files
                            .iter()
                            .take(if self.show_all {
                                usize::MAX
                            } else {
                                FILE_LIMIT
                            })
                            .enumerate()
                            .map(|(i, f)| {
                                let selected = open.as_deref() == Some(f.path.as_str());
                                let file = f.source.clone();
                                let path = f.path.clone();
                                self.file_row("details-file", i, f, selected, palette, cx)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        if let Some(file) = &file {
                                            this.open_commit_file(file.clone(), path.clone(), cx)
                                        }
                                    }))
                            }),
                    )
                    .when(!self.show_all && loaded.files.len() > FILE_LIMIT, |d| {
                        d.child(
                            Button::new("details-show-all")
                                .ghost()
                                .small()
                                .label(format!("Show all {} files", loaded.files.len()))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.show_all = true;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// A clickable file line without counts: change letter, file name, then its folder.
#[allow(clippy::too_many_arguments)]
fn name_row(
    key: &'static str,
    i: usize,
    letter: &'static str,
    color: gpui_kit::Hsla,
    name: String,
    dir: String,
    selected: bool,
    palette: &Palette,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(ElementId::NamedInteger(key.into(), i as u64))
        .h(zoomed(24., cx))
        .px_1()
        .gap_1p5()
        .rounded_sm()
        .text_xs()
        .cursor_pointer()
        .when(selected, |d| d.bg(palette.accent_soft))
        .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
        .child(
            div()
                .w(zoomed(12., cx))
                .flex_none()
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(color)
                .child(letter),
        )
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .text_color(palette.emph)
                        .child(SharedString::from(ui::sanitize_line(&name).into_owned())),
                )
                .child(
                    div()
                        .min_w_0()
                        .pl_1()
                        .truncate()
                        .text_color(palette.muted)
                        .child(SharedString::from(ui::sanitize_line(&dir).into_owned())),
                ),
        )
}

fn file_entry(diff: FileDiff) -> FileEntry {
    let path = diff
        .new_path
        .as_ref()
        .or(diff.old_path.as_ref())
        .map(|p| p.to_str_lossy().into_owned())
        .unwrap_or_default();
    let count = |kind: LineKind| {
        diff.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .count()
    };
    FileEntry {
        added: count(LineKind::Added),
        removed: count(LineKind::Deleted),
        binary: diff.binary,
        counted: true,
        lfs: git_core::lfs::pointer_change(&diff),
        lfs_tracked: false,
        change: diff.change,
        path,
        source: None,
    }
}

fn change_letter(change: &FileChange, palette: &Palette) -> (&'static str, gpui_kit::Hsla) {
    match change {
        FileChange::Added => ("A", palette.ok),
        FileChange::Deleted => ("D", palette.error),
        FileChange::Modified => ("M", palette.warn),
        FileChange::Renamed { .. } => ("R", palette.accent),
        FileChange::Copied { .. } => ("C", palette.accent),
        FileChange::ModeChanged => ("T", palette.muted),
    }
}

impl BasePanel for DetailsPanel {
    fn panel_name(&self) -> &'static str {
        "details"
    }
}

impl Panel for DetailsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Details"
    }
}

impl EventEmitter<PanelEvent> for DetailsPanel {}

impl Focusable for DetailsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DetailsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let hint = |text: SharedString| {
            div()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child(text)
                .into_any_element()
        };
        let body = match &self.state {
            State::Empty => hint("Select a commit in the graph to see its details.".into()),
            State::Loading => hint("Loading…".into()),
            State::Failed(err) => hint(err.clone()),
            State::Ready(loaded) => {
                let loaded = loaded.clone();
                self.render_loaded(&loaded, &palette, cx)
            }
            State::Wip(wip) => {
                let wip = wip.clone();
                match self.shown_tab() {
                    Some(tab) => {
                        let commit_box = self.commit_box(&tab, window, cx);
                        let filter = self.filter_input(window, cx);
                        self.render_wip(&wip, commit_box, filter, &palette, cx)
                    }
                    None => hint("This tab was closed.".into()),
                }
            }
        };
        div()
            .id("details-scroll")
            .size_full()
            .overflow_y_scrollbar()
            .track_focus(&self.focus)
            .bg(palette.panel)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_entry_counts_lines_and_picks_the_new_path() {
        let patch = "diff --git a/old.txt b/new.txt\nsimilarity index 50%\nrename from old.txt\nrename to new.txt\n--- a/old.txt\n+++ b/new.txt\n@@ -1,2 +1,3 @@\n a\n-b\n+B\n+C\n";
        let diff = git_core::parse::diff::parse(patch.as_bytes())
            .unwrap()
            .remove(0);
        let entry = file_entry(diff);
        assert_eq!(entry.path, "new.txt");
        assert_eq!((entry.added, entry.removed), (2, 1));
        assert!(matches!(entry.change, FileChange::Renamed { .. }));
    }
}
