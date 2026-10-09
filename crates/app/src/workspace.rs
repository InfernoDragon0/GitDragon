//! Main window: title bar with repo tabs, activity rail, and the dock area
//! (left sidebar, centre view picked by the rail, collapsible right details dock).
//! Owns the open repositories and what is remembered between runs.

use std::path::PathBuf;
use std::time::Duration;

use git_core::{GitRunner, setup};
use gpui_kit::assets::IconName;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{
    DockArea, DockEvent, DockLayout, DockPlacement, DockSkin, panel_handle,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity,
    Window, div, linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px,
};
use ui::{Appearance, Mode, Palette, theme};

use crate::actions::*;
use crate::clone_dialog::{CloneDialog, CloneEvent};
use crate::graph::DetailsPanel;
use crate::graph::link::GraphLink;
use crate::op_strip::{OpStrip, OpStripEvent};
use crate::palette::{self, Command};
use crate::panels::CenterView;
use crate::rail::{ActivityRail, View};
use crate::repo_tab::RepoTab;
use crate::sidebar::SidebarPanel;
use crate::state::{self, SavedState};
use crate::toolbar::RepoToolbar;
use crate::welcome::{Welcome, WelcomeEvent};

const DOCK_LAYOUT_VERSION: usize = 1;
const LEFT_DOCK_WIDTH: f32 = 248.;
const RIGHT_DOCK_WIDTH: f32 = 340.;
/// Layout drags fire many events; the state file is written once things settle.
const SAVE_DELAY: Duration = Duration::from_millis(400);

pub struct Workspace {
    dock: Entity<DockArea>,
    center: Entity<CenterView>,
    sidebar: Entity<SidebarPanel>,
    toolbar: Entity<RepoToolbar>,
    welcome: Entity<Welcome>,
    op_strip: Entity<OpStrip>,
    focus: FocusHandle,
    /// `None` until git is detected (or when it is missing / too old).
    runner: Option<GitRunner>,
    tabs: Vec<Entity<RepoTab>>,
    active: Option<usize>,
    state: SavedState,
    save_task: Option<Task<()>>,
    dialog_sub: Option<Subscription>,
    /// What each tab's toasts last showed, so one error or Undo toasts once.
    toasts: Vec<(WeakEntity<RepoTab>, ToastMemory)>,
    _subs: Vec<Subscription>,
}

/// The error text and Undo record id a tab's toasts last showed.
#[derive(Default)]
struct ToastMemory {
    error: Option<SharedString>,
    undo: Option<u64>,
}

impl Workspace {
    pub fn new(state: SavedState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _span = tracing::info_span!("workspace_new").entered();

        let (dock, _skin) = DockSkin::dock_area("workspace", Some(DOCK_LAYOUT_VERSION), window, cx);
        let welcome = cx.new(|cx| {
            let mut welcome = Welcome::new(cx);
            welcome.set_recent(&state.recent, cx);
            welcome
        });
        let link = cx.new(|_| GraphLink::default());
        let center = cx.new(|cx| CenterView::new(welcome.clone(), link.clone(), cx));
        let op_strip = cx.new(|_| OpStrip::new());
        let sidebar = cx.new(|cx| SidebarPanel::new(link.clone(), window, cx));
        let details = cx.new(|cx| DetailsPanel::new(link, cx));

        let layout = state.dock.unwrap_or(state::DockLayout {
            left_open: true,
            right_open: true,
            left_width: LEFT_DOCK_WIDTH,
            right_width: RIGHT_DOCK_WIDTH,
        });
        dock.update(cx, |area, cx| {
            area.set_center(
                DockLayout::tabs().panel_view(panel_handle(center.clone()), cx),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Left,
                DockLayout::tabs().panel_view(panel_handle(sidebar.clone()), cx),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Right,
                DockLayout::tabs().panel_view(panel_handle(details), cx),
                window,
                cx,
            );
            for (placement, width, open) in [
                (DockPlacement::Left, layout.left_width, layout.left_open),
                (DockPlacement::Right, layout.right_width, layout.right_open),
            ] {
                area.set_dock_size(placement, px(width), window, cx);
                area.set_dock_collapsible(placement, true, window, cx);
                if !open {
                    area.toggle_dock(placement, window, cx);
                }
            }
        });

        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        let _subs = vec![
            cx.subscribe(&welcome, |this, _, event: &WelcomeEvent, cx| match event {
                WelcomeEvent::OpenRecent(path) => this.open_repo(path.clone(), cx),
                WelcomeEvent::Forget(path) => {
                    this.state.forget_recent(path);
                    this.sync_recent(cx);
                }
            }),
            cx.subscribe(&dock, |this, _, event: &DockEvent, cx| {
                if matches!(event, DockEvent::LayoutChanged) {
                    this.schedule_save(cx);
                }
            }),
            cx.observe_global::<Appearance>(|this, cx| this.schedule_save(cx)),
            cx.observe_global::<ui::Zoom>(|this, cx| this.schedule_save(cx)),
            cx.subscribe(
                &crate::avatars::handle(cx).expect("avatars::init runs before the workspace"),
                |this, _, _: &crate::avatars::SettingChanged, cx| this.schedule_save(cx),
            ),
            cx.subscribe(&op_strip, |this, _, event: &OpStripEvent, cx| {
                let Some(tab) = this.active.and_then(|ix| this.tabs.get(ix)).cloned() else {
                    return;
                };
                match event {
                    OpStripEvent::Cancel => tab.update(cx, |tab, cx| tab.cancel_op(cx)),
                    OpStripEvent::Undo => tab.update(cx, |tab, cx| tab.undo_last(cx)),
                }
            }),
            cx.subscribe(
                &crate::prefs::handle(cx).expect("prefs::init runs before the workspace"),
                |this, _, _: &crate::prefs::PrefsChanged, cx| {
                    this.schedule_save(cx);
                    cx.notify();
                },
            ),
            // Background fetches slow down while the window is in the background.
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                for tab in &this.tabs {
                    tab.update(cx, |tab, cx| tab.set_window_focused(active, cx));
                }
            }),
        ];

        let mut workspace = Self {
            dock,
            center,
            sidebar,
            toolbar: cx.new(|_| RepoToolbar::new()),
            welcome,
            op_strip,
            focus,
            runner: None,
            tabs: Vec::new(),
            active: None,
            state,
            save_task: None,
            dialog_sub: None,
            toasts: Vec::new(),
            _subs,
        };
        workspace.detect_git(cx);
        workspace
    }

    /// Finds git off the UI thread, then reopens the tabs from last time.
    fn detect_git(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { git_core::detect::git() })
                .await;
            _ = this.update(cx, |this, cx| match found {
                Ok(git) => {
                    tracing::info!(path = %git.path.display(), version = %git.version, "git");
                    let runner = GitRunner::new(git);
                    // Stored tokens reach git through our askpass helper (never argv / URLs).
                    runner.set_env(crate::accounts::askpass_env(cx));
                    this.op_strip
                        .update(cx, |strip, cx| strip.listen(&runner, cx));
                    this.runner = Some(runner);
                    let tabs = std::mem::take(&mut this.state.open_tabs);
                    let active = this.state.active_tab;
                    for path in tabs.into_iter().filter(|p| p.exists()) {
                        this.add_tab(path, cx);
                    }
                    this.activate(active.filter(|&ix| ix < this.tabs.len()), cx);
                }
                Err(err) => {
                    let message = match &err {
                        git_core::GitError::NotFound { .. } => {
                            "Git is not installed. Install it from git-scm.com, then restart \
                             GitDragon."
                                .to_string()
                        }
                        other => other.to_string(),
                    };
                    this.welcome
                        .update(cx, |w, cx| w.set_git_problem(Some(message.into()), cx));
                }
            });
        })
        .detach();
    }

    // ---- tabs -------------------------------------------------------------------------------

    /// Opens (or switches to) the repository at `path`.
    pub fn open_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.runner.is_none() {
            return;
        }
        let ix = match self.tabs.iter().position(|t| t.read(cx).path == path) {
            Some(ix) => ix,
            None => {
                self.add_tab(path.clone(), cx);
                self.tabs.len() - 1
            }
        };
        self.state.touch_recent(&path);
        self.sync_recent(cx);
        self.activate(Some(ix), cx);
    }

    fn add_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(runner) = self.runner.clone() else {
            return;
        };
        let tab = cx.new(|cx| RepoTab::new(path, runner, cx));
        self._subs.push(cx.observe(&tab, |this, _, cx| {
            this.sync_strip(cx);
            this.sync_toasts(cx);
            cx.notify()
        }));
        self.tabs.push(tab);
    }

    fn activate(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.active = ix.filter(|&ix| ix < self.tabs.len());
        let repo = self.active.map(|ix| self.tabs[ix].clone());
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_repo(repo.clone(), cx));
        self.toolbar
            .update(cx, |toolbar, cx| toolbar.set_repo(repo.clone(), cx));
        if repo.is_none() {
            self.welcome.update(cx, |welcome, cx| {
                welcome.focus_on_show();
                cx.notify();
            });
        }
        let open_tabs = self.tabs.iter().map(Entity::downgrade).collect();
        self.center.update(cx, |center, cx| {
            center.set_open_tabs(open_tabs, cx);
            center.set_repo(repo, cx)
        });
        self.sync_strip(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Branch and ahead / behind of the active tab, on the right of the operation strip.
    fn sync_strip(&mut self, cx: &mut Context<Self>) {
        let branch = self.active.and_then(|ix| {
            let status = self.tabs.get(ix)?.read(cx).status.as_ref()?;
            let mut text = status
                .branch
                .head
                .clone()
                .unwrap_or_else(|| "detached HEAD".into());
            if status.branch.upstream.is_some() {
                text.push_str(&format!(
                    " ↑{} ↓{}",
                    status.branch.ahead, status.branch.behind
                ));
            }
            Some(text)
        });
        self.op_strip
            .update(cx, |strip, cx| strip.set_branch(branch, cx));
        let undo = self
            .active
            .and_then(|ix| self.tabs.get(ix))
            .and_then(|tab| tab.read(cx).undo.as_ref().map(|u| u.describe()));
        self.op_strip
            .update(cx, |strip, cx| strip.set_undo(undo, cx));
    }

    /// Toasts for what happened in the tabs: a failed operation, an Undo on offer (and its
    /// removal once it is taken or has expired).
    fn sync_toasts(&mut self, cx: &mut Context<Self>) {
        self.toasts.retain(|(tab, _)| tab.upgrade().is_some());
        for tab in self.tabs.clone() {
            let weak = tab.downgrade();
            let at = match self.toasts.iter().position(|(t, _)| *t == weak) {
                Some(at) => at,
                None => {
                    self.toasts.push((weak.clone(), ToastMemory::default()));
                    self.toasts.len() - 1
                }
            };
            let (error, undo, name) = {
                let tab = tab.read(cx);
                (
                    tab.op_error.clone(),
                    tab.undo.as_ref().map(|u| (u.id, u.label.clone())),
                    tab.name.clone(),
                )
            };
            let memory = &mut self.toasts[at].1;
            let new_error = match &error {
                Some(err) if memory.error.as_ref() != Some(err) => Some(err.clone()),
                _ => None,
            };
            memory.error = error;
            let old_undo = memory.undo;
            memory.undo = undo.as_ref().map(|(id, _)| *id);
            let new_undo = memory.undo;
            if let Some(err) = new_error
                && err.as_ref() != crate::repo_tab::CANCELLED
            {
                crate::toast::error(cx, format!("{name}: operation failed"), &err);
            }
            if old_undo != new_undo {
                if let Some(old) = old_undo {
                    crate::toast::dismiss_undo(cx, old);
                }
                if let Some((id, label)) = undo {
                    let message = format!("{label} finished in {name}. Undo is on offer for 30 s.");
                    crate::toast::undo(cx, weak, id, message);
                }
            }
        }
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        // Dropping the entity stops its watcher and cancels its git processes.
        self.tabs.remove(ix);
        let next = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(active) if active > ix => Some(active - 1),
            Some(active) if active == ix => Some(ix.min(self.tabs.len() - 1)),
            other => other,
        };
        self.activate(next, cx);
    }

    fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let len = self.tabs.len() as isize;
        if len == 0 {
            return;
        }
        let current = self.active.map_or(-1, |ix| ix as isize);
        let next = (current + step).rem_euclid(len) as usize;
        self.activate(Some(next), cx);
    }

    // ---- open / new / clone -----------------------------------------------------------------

    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        self.pick_folder("Open repository", cx, |this, path, cx| {
            this.open_repo(path, cx)
        });
    }

    fn prompt_new(&mut self, cx: &mut Context<Self>) {
        self.pick_folder("Create repository in", cx, |this, path, cx| {
            let Some(runner) = this.runner.clone() else {
                return;
            };
            let branch = crate::prefs::get(cx).initial_branch().map(str::to_owned);
            cx.spawn(async move |this, cx| {
                let created = cx
                    .background_executor()
                    .spawn(async move {
                        setup::init_with_branch(&runner, &path, branch.as_deref()).await
                    })
                    .await;
                _ = this.update(cx, |this, cx| match created {
                    Ok(root) => this.open_repo(root, cx),
                    Err(err) => this.welcome.update(cx, |w, cx| {
                        w.set_git_problem(
                            Some(format!("Could not create repository: {err}").into()),
                            cx,
                        )
                    }),
                });
            })
            .detach();
        });
    }

    fn pick_folder(
        &mut self,
        prompt: &'static str,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Context<Self>) + 'static,
    ) {
        if self.runner.is_none() {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                _ = this.update(cx, |this, cx| then(this, path, cx));
            }
        })
        .detach();
    }

    fn show_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runner) = self.runner.clone() else {
            return;
        };
        let parent = self
            .state
            .clone_parent
            .clone()
            .or_else(|| etcetera::home_dir().ok())
            .unwrap_or_default();
        let dialog = cx.new(|cx| CloneDialog::new(runner, parent, window, cx));
        self.dialog_sub = Some(cx.subscribe_in(
            &dialog,
            window,
            |this, dialog, event: &CloneEvent, window, cx| {
                match event {
                    CloneEvent::Cloned(path) => {
                        this.state.clone_parent = Some(dialog.read(cx).parent_dir(cx));
                        this.open_repo(path.clone(), cx);
                    }
                    CloneEvent::Dismiss => {}
                }
                window.close_dialog(cx);
                this.dialog_sub = None;
            },
        ));
        window.open_dialog(cx, move |d, _, _| {
            d.title("Clone a repository")
                .w(px(560.))
                .child(dialog.clone())
        });
    }

    // ---- persistence ------------------------------------------------------------------------

    fn sync_recent(&mut self, cx: &mut Context<Self>) {
        let recent = self.state.recent.clone();
        self.welcome.update(cx, |w, cx| w.set_recent(&recent, cx));
        self.schedule_save(cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            _ = this.update(cx, |this, cx| this.save_now(cx));
        }));
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        let dock = self.dock.read(cx);
        let width = |placement| dock.dock_size(placement).map(f32::from);
        self.state.dock = Some(state::DockLayout {
            left_open: dock.is_dock_open(DockPlacement::Left),
            right_open: dock.is_dock_open(DockPlacement::Right),
            left_width: width(DockPlacement::Left).unwrap_or(LEFT_DOCK_WIDTH),
            right_width: width(DockPlacement::Right).unwrap_or(RIGHT_DOCK_WIDTH),
        });
        // Before git is found the saved tabs are still pending; keep them.
        if self.runner.is_some() {
            self.state.open_tabs = self.tabs.iter().map(|t| t.read(cx).path.clone()).collect();
            self.state.active_tab = self.active;
        }
        self.state.appearance = Some(theme::appearance(cx).key());
        let zoom = theme::zoom(cx);
        self.state.zoom = (zoom != ui::Zoom::default()).then_some(zoom.0);
        self.state.prefs = crate::prefs::get(cx);
        self.state.gravatar = crate::avatars::handle(cx)
            .map(|a| a.read(cx).gravatar())
            .filter(|on| *on);
        let snapshot = self.state.clone();
        cx.background_executor()
            .spawn(async move { snapshot.save() })
            .detach();
    }

    // ---- command palette ----------------------------------------------------------------------

    fn active_tab(&self) -> Option<Entity<RepoTab>> {
        self.active.and_then(|ix| self.tabs.get(ix)).cloned()
    }

    /// What the palette needs to know about the active tab and the settings.
    fn palette_ctx(&self, cx: &App) -> palette::Ctx {
        let prefs = crate::prefs::get(cx);
        let mut ctx = palette::Ctx {
            pull_default: Some(prefs.pull.mode()),
            prune: prefs.prune_on_fetch,
            ..palette::Ctx::default()
        };
        let Some(tab) = self.active_tab() else {
            return ctx;
        };
        let tab = tab.read(cx);
        let branch = tab.status.as_ref().map(|s| &s.branch);
        ctx.repo = true;
        ctx.branch = branch.and_then(|b| b.head.clone());
        ctx.upstream = branch.is_some_and(|b| b.upstream.is_some());
        ctx.remote = tab
            .remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(tab.remotes.first())
            .map(|r| r.name.clone());
        ctx.stashes = tab.stashes.len();
        ctx.vocab = crate::hosted::HostedRepo::of_tab(tab, cx).and_then(|h| h.vocab());
        ctx.undo = tab
            .undo
            .as_ref()
            .map(|u| (u.label.clone(), u.action.preview()));
        ctx
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.palette_ctx(cx);
        let this = cx.entity().downgrade();
        palette::open(
            &ctx,
            move |command, window, cx| {
                _ = this.update(cx, |this, cx| this.run_command(command, window, cx));
            },
            window,
            cx,
        );
    }

    /// Runs a palette command through the same code as the buttons, menus and shortcuts.
    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.active_tab();
        match command {
            Command::ShowView(view) => self.show(view, cx),
            Command::ShowChanges => self.show_changes(window, cx),
            Command::SettingsSection(section) => {
                self.center.update(cx, |center, cx| {
                    center.show_settings_section(section, window, cx)
                });
                cx.notify();
            }
            Command::Refresh => {
                if let Some(tab) = tab {
                    tab.update(cx, |tab, cx| tab.refresh(cx));
                }
            }
            Command::Fetch => self.toolbar.update(cx, |t, cx| t.fetch(cx)),
            Command::Pull(mode) => self.toolbar.update(cx, |t, cx| t.pull(mode, cx)),
            Command::PullDefault => {
                let mode = crate::prefs::get(cx).pull.mode();
                self.toolbar.update(cx, |t, cx| t.pull(mode, cx));
            }
            Command::Push => self.toolbar.update(cx, |t, cx| t.push(cx)),
            Command::Stash => self.toolbar.update(cx, |t, cx| t.stash(window, cx)),
            Command::PopStash => self.toolbar.update(cx, |t, cx| t.pop_stash(cx)),
            Command::NewBranch => self.toolbar.update(cx, |t, cx| t.new_branch(window, cx)),
            Command::MergeBranch => self.prompt_branch_op(true, window, cx),
            Command::RebaseOnto => self.prompt_branch_op(false, window, cx),
            Command::UndoLast => {
                if let Some(tab) = tab {
                    tab.update(cx, |tab, cx| tab.undo_last(cx));
                }
            }
            Command::OpenRepository => self.prompt_open(cx),
            Command::CloneRepository => self.show_clone(window, cx),
            Command::NewRepository => self.prompt_new(cx),
            Command::CloseTab => {
                if let Some(ix) = self.active {
                    self.close_tab(ix, cx);
                }
            }
            Command::ToggleLeftDock => self.toggle_dock(DockPlacement::Left, window, cx),
            Command::ToggleRightDock => self.toggle_dock(DockPlacement::Right, window, cx),
            Command::ToggleMode => {
                theme::toggle_mode(cx);
            }
            Command::NextSkin => {
                theme::cycle_skin(cx);
            }
            Command::ZoomIn => {
                theme::step_zoom(1, cx);
            }
            Command::ZoomOut => {
                theme::step_zoom(-1, cx);
            }
            Command::ZoomReset => {
                theme::set_zoom(1.0, cx);
            }
        }
    }

    /// "Merge a branch into the current one" / "Rebase onto": asks for the branch or ref.
    fn prompt_branch_op(&mut self, merge: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let current = tab
            .read(cx)
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .unwrap_or_else(|| "HEAD".into());
        let (title, button) = if merge {
            (format!("Merge into {current}"), "Merge")
        } else {
            (format!("Rebase {current} onto"), "Rebase")
        };
        let tab = tab.downgrade();
        self.dialog_sub = Some(crate::dialogs::prompt(
            window,
            cx,
            title,
            vec![crate::dialogs::Field::new(
                "Branch or ref, e.g. feature/login or origin/main",
            )],
            &[button],
            move |values, _, _, cx| {
                let rev = values[0].clone();
                _ = tab.update(cx, |tab, cx| {
                    if merge {
                        tab.merge_branch(rev, cx)
                    } else {
                        tab.rebase_onto(rev, cx)
                    }
                });
            },
        ));
    }

    // ---- rendering --------------------------------------------------------------------------

    fn show(&mut self, view: View, cx: &mut Context<Self>) {
        self.center
            .update(cx, |center, cx| center.set_view(view, cx));
        cx.notify();
    }

    /// The working copy: the graph's WIP row, with the details dock (commit box) open.
    fn show_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.center
            .update(cx, |center, cx| center.show_changes(window, cx));
        if !self.dock.read(cx).is_dock_open(DockPlacement::Right) {
            self.toggle_dock(DockPlacement::Right, window, cx);
        }
        cx.notify();
    }

    fn toggle_dock(
        &mut self,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dock
            .update(cx, |area, cx| area.toggle_dock(placement, window, cx));
    }

    fn render_tabs(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let tab = tab.read(cx);
            let active = self.active == Some(ix);
            let tip = format!("{}\n{}", tab.path.display(), tab.summary());
            let failed = tab.error.is_some();
            h_flex()
                .id(ElementId::NamedInteger("repo-tab".into(), ix as u64))
                .group("repo-tab")
                .h(px(24.))
                .pl_2()
                .pr_1()
                .gap_1()
                .rounded(px(5.))
                .cursor_pointer()
                .text_xs()
                .when(active, |this| {
                    this.bg(palette.panel)
                        .border_1()
                        .border_color(palette.border_strong)
                        .text_color(palette.emph)
                })
                .when(!active, |this| {
                    this.text_color(palette.muted)
                        .hover(|s| s.bg(palette.hover))
                })
                .when(failed, |this| this.text_color(palette.error))
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.activate(Some(ix), cx)),
                )
                .child(tab.name.clone())
                .child(
                    div()
                        .when(!active, |d| {
                            d.invisible().group_hover("repo-tab", |s| s.visible())
                        })
                        .child(
                            Button::new(ElementId::NamedInteger("close-tab".into(), ix as u64))
                                .ghost()
                                .xsmall()
                                .icon(IconName::X)
                                .tooltip_with_action("Close", &CloseTab, None)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(ix, cx)
                                })),
                        ),
                )
        });
        let welcome_active = self.active.is_none();
        // The title bar is a window-drag area; on Windows a click there goes to the OS (move the
        // window) unless something above it takes the mouse. `occlude` makes the tabs clickable.
        h_flex()
            .id("repo-tabs")
            .occlude()
            .gap_1()
            .children(tabs.collect::<Vec<_>>())
            .child(
                Button::new("open-repo")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .toggled(welcome_active && !self.tabs.is_empty())
                    .tooltip_with_action(
                        "Open, clone or create a repository",
                        &OpenRepository,
                        None,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.activate(None, cx))),
            )
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let appearance = theme::appearance(cx);
        let right_open = self.dock.read(cx).is_dock_open(DockPlacement::Right);
        let [g0, _, _, g3] = palette.gradient;

        let brand = h_flex()
            .gap_2()
            .child(div().size(px(14.)).rounded(px(4.)).bg(linear_gradient(
                135.,
                linear_color_stop(g0, 0.),
                linear_color_stop(g3, 1.),
            )))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(palette.emph)
                    .child("GitDragon"),
            );

        let skin_button = Button::new("skin")
            .ghost()
            .xsmall()
            .label(appearance.skin.label())
            .tooltip_with_action("Switch theme", &NextSkin, None)
            .on_click(|_, _, cx| {
                theme::cycle_skin(cx);
            });

        let mode_button = Button::new("mode")
            .ghost()
            .xsmall()
            .icon(match appearance.mode {
                Mode::Dark => IconName::Sun,
                Mode::Light => IconName::Moon,
            })
            .tooltip_with_action(
                match appearance.mode {
                    Mode::Dark => "Light mode",
                    Mode::Light => "Dark mode",
                },
                &ToggleDarkMode,
                None,
            )
            .on_click(|_, _, cx| {
                theme::toggle_mode(cx);
            });

        let right_dock_button = Button::new("toggle-right-dock")
            .ghost()
            .xsmall()
            .icon(IconName::PanelRight)
            .toggled(right_open)
            .tooltip_with_action("Toggle details", &ToggleRightDock, None)
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_dock(DockPlacement::Right, window, cx)
            }));

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .justify_between()
                .pr_2()
                .child(
                    h_flex()
                        .gap_4()
                        .min_w_0()
                        .child(brand)
                        .child(self.render_tabs(&palette, cx)),
                )
                .child(
                    h_flex()
                        .id("title-actions")
                        .occlude()
                        .gap_1()
                        .child(skin_button)
                        .child(mode_button)
                        .child(right_dock_button),
                ),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let active = self.center.read(cx).view();
        let vocab = self
            .active
            .and_then(|ix| self.tabs.get(ix))
            .and_then(|tab| crate::hosted::HostedRepo::of_tab(tab.read(cx), cx))
            .and_then(|hosted| hosted.vocab());
        let rail = ActivityRail::new(active, {
            let this = cx.entity().downgrade();
            move |view, _, cx| {
                _ = this.update(cx, |this, cx| this.show(view, cx));
            }
        })
        .vocab(vocab);

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(palette.bg)
            .text_color(palette.fg)
            .on_action(cx.listener(|this, _: &ShowGraph, _, cx| this.show(View::Graph, cx)))
            .on_action(
                cx.listener(|this, _: &ShowChanges, window, cx| this.show_changes(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowConflicts, _, cx| this.show(View::Conflicts, cx)))
            .on_action(
                cx.listener(|this, _: &ShowPullRequests, _, cx| this.show(View::PullRequests, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowIssues, _, cx| this.show(View::Issues, cx)))
            .on_action(cx.listener(|this, _: &ShowBoards, _, cx| this.show(View::Boards, cx)))
            .on_action(cx.listener(|this, _: &ShowChat, _, cx| this.show(View::Chat, cx)))
            .on_action(cx.listener(|this, _: &ShowSettings, _, cx| this.show(View::Settings, cx)))
            .on_action(
                cx.listener(|this, _: &CommandPalette, window, cx| this.open_palette(window, cx)),
            )
            .on_action(cx.listener(|this, _: &UndoLast, _, cx| {
                if let Some(ix) = this.active {
                    this.tabs[ix].update(cx, |tab, cx| tab.undo_last(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleLeftDock, window, cx| {
                this.toggle_dock(DockPlacement::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, window, cx| {
                this.toggle_dock(DockPlacement::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenRepository, _, cx| this.prompt_open(cx)))
            .on_action(cx.listener(|this, _: &NewRepository, _, cx| this.prompt_new(cx)))
            .on_action(
                cx.listener(|this, _: &CloneRepository, window, cx| this.show_clone(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(ix) = this.active {
                    this.close_tab(ix, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.cycle_tab(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| this.cycle_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                if let Some(ix) = this.active {
                    this.tabs[ix].update(cx, |tab, cx| tab.refresh(cx));
                }
            }))
            .child(self.render_title_bar(cx))
            .when(self.toolbar.read(cx).has_repo(), |d| {
                d.child(self.toolbar.clone())
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(rail)
                    .child(div().flex_1().h_full().min_w_0().child(self.dock.clone())),
            )
            .child(self.op_strip.clone())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Pending debounced save: write synchronously so nothing is lost on quit.
        if self.save_task.take().is_some() {
            self.state.save();
        }
    }
}
