//! Left dock: local branches (current, upstream, ahead / behind), remotes with their branches,
//! tags, stashes and submodules. Click a ref to find it in the graph, double-click a branch to
//! check it out, right-click for everything else (see `actions`).

mod actions;
mod rows;

use std::collections::HashSet;
use std::ops::Range;

use git_core::SubmoduleState;
use gpui_kit::assets::IconName;
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, uniform_list,
};
use ui::{Palette, zoomed};

use crate::graph::link::{GraphLink, LinkEvent};
use crate::repo_tab::RepoTab;
use rows::{Row, Section};

const ROW_HEIGHT: f32 = 26.;

pub struct SidebarPanel {
    repo: Option<Entity<RepoTab>>,
    link: Entity<GraphLink>,
    filter: Entity<InputState>,
    collapsed: HashSet<Section>,
    collapsed_remotes: HashSet<String>,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    /// Keeps an open prompt's Enter handler alive.
    dialog_sub: Option<Subscription>,
    focus: FocusHandle,
    _observe_repo: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl SidebarPanel {
    pub fn new(link: Entity<GraphLink>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter refs"));
        let _subs = vec![cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.rebuild(cx);
            }
        })];
        Self {
            repo: None,
            link,
            filter,
            collapsed: HashSet::new(),
            collapsed_remotes: HashSet::new(),
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            dialog_sub: None,
            focus: cx.focus_handle(),
            _observe_repo: None,
            _subs,
        }
    }

    /// The active repository tab changed.
    pub fn set_repo(&mut self, repo: Option<Entity<RepoTab>>, cx: &mut Context<Self>) {
        self._observe_repo = repo
            .as_ref()
            .map(|repo| cx.observe(repo, |this, _, cx| this.rebuild(cx)));
        self.repo = repo;
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        self.rows = match &self.repo {
            None => Vec::new(),
            Some(repo) => {
                let tab = repo.read(cx);
                let head = tab.status.as_ref().and_then(|s| s.branch.head.as_deref());
                rows::build(
                    &rows::Data {
                        refs: &tab.refs,
                        remotes: &tab.remotes,
                        stashes: &tab.stashes,
                        submodules: &tab.submodules,
                        head,
                    },
                    &rows::View {
                        filter: &filter,
                        collapsed: &self.collapsed,
                        collapsed_remotes: &self.collapsed_remotes,
                    },
                )
            }
        };
        cx.notify();
    }

    /// Selects `oid` in the graph (when the graph has it).
    fn reveal(&mut self, oid: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.repo.as_ref().map(Entity::downgrade) else {
            return;
        };
        let commit = oid.to_string();
        self.link
            .update(cx, |_, cx| cx.emit(LinkEvent::Select { tab, commit }));
    }

    fn on_row_click(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        let double = event.click_count() >= 2;
        match row {
            Row::Header { section, .. } => {
                if !self.collapsed.remove(&section) {
                    self.collapsed.insert(section);
                }
                self.rebuild(cx);
            }
            Row::Remote { name, .. } => {
                if !self.collapsed_remotes.remove(&name) {
                    self.collapsed_remotes.insert(name);
                }
                self.rebuild(cx);
            }
            Row::Local {
                name, oid, current, ..
            } => {
                if double && !current {
                    self.checkout_local(name, cx);
                } else {
                    self.reveal(&oid, cx);
                }
            }
            Row::RemoteBranch {
                remote,
                branch,
                oid,
            } => {
                if double {
                    self.checkout_remote(remote, branch, window, cx);
                } else {
                    self.reveal(&oid, cx);
                }
            }
            Row::Tag { oid, .. } | Row::Stash { oid, .. } => self.reveal(&oid, cx),
            Row::Submodule { .. } | Row::Empty(_) => {}
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let palette = Palette::of(cx);
        let row = self.rows.get(ix)?.clone();
        let (busy, remotes) = match &self.repo {
            Some(repo) => {
                let tab = repo.read(cx);
                let names: Vec<String> = tab.remotes.iter().map(|r| r.name.clone()).collect();
                (tab.is_busy(), names)
            }
            None => (false, Vec::new()),
        };
        let indent = |level: f32| zoomed(8. + level * 14., cx);
        let icon = |name: IconName, color: Hsla| Icon::new(name).xsmall().text_color(color);
        let base = h_flex()
            .id(ElementId::NamedInteger("sidebar-row".into(), ix as u64))
            .h(zoomed(ROW_HEIGHT, cx))
            .w_full()
            .pr_2()
            .gap_1p5()
            .text_sm()
            .cursor_pointer()
            .hover(|s| s.bg(palette.hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_row_click(ix, event, window, cx)
            }));
        let label = |text: String, color: Hsla| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(color)
                .child(SharedString::from(text))
        };
        let chip = |text: String, color: Hsla| {
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.12))
                .child(SharedString::from(text))
        };
        let view = cx.entity().downgrade();
        let element = match row.clone() {
            Row::Header {
                section,
                count,
                collapsed,
            } => base
                .pl(indent(0.))
                .mt_1()
                .text_xs()
                .text_color(palette.muted)
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .xsmall(),
                )
                .child(div().text_color(palette.emph).child(section.label()))
                .child(div().flex_1().child(SharedString::from(count.to_string())))
                .into_any_element(),
            Row::Local {
                name,
                current,
                upstream,
                ahead,
                behind,
                gone,
                ..
            } => base
                .pl(indent(1.))
                .when(current, |d| d.bg(palette.accent_soft))
                .child(icon(
                    if current {
                        IconName::Check
                    } else {
                        IconName::GitBranch
                    },
                    if current {
                        palette.accent
                    } else {
                        palette.muted
                    },
                ))
                .child(
                    label(name, if current { palette.emph } else { palette.fg })
                        .when(current, |d| d.font_weight(gpui_kit::FontWeight::SEMIBOLD)),
                )
                .when(ahead > 0, |d| {
                    d.child(chip(format!("\u{2191}{ahead}"), palette.ok))
                })
                .when(behind > 0, |d| {
                    d.child(chip(format!("\u{2193}{behind}"), palette.warn))
                })
                .when(gone, |d| d.child(chip("gone".into(), palette.muted)))
                .when(upstream.is_none(), |d| {
                    d.child(chip("local".into(), palette.muted))
                })
                .into_any_element(),
            Row::Remote {
                name,
                url,
                count,
                collapsed,
            } => base
                .pl(indent(1.))
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .xsmall(),
                )
                .child(icon(IconName::Cloud, palette.lane(0)))
                .child(label(name, palette.fg))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(SharedString::from(count.to_string())),
                )
                .when_some(url, |d, url| {
                    d.tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(url.clone()).build(window, cx)
                    })
                })
                .into_any_element(),
            Row::RemoteBranch { branch, .. } => base
                .pl(indent(2.))
                .child(icon(IconName::GitBranch, palette.lane(0)))
                .child(label(branch, palette.fg))
                .into_any_element(),
            Row::Tag { name, .. } => base
                .pl(indent(1.))
                .child(icon(IconName::Tag, palette.lane(3)))
                .child(label(name, palette.fg))
                .into_any_element(),
            Row::Stash { index, message, .. } => base
                .pl(indent(1.))
                .child(icon(IconName::Archive, palette.muted))
                .child(label(message, palette.fg))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(SharedString::from(format!("{{{index}}}"))),
                )
                .into_any_element(),
            Row::Submodule { path, state } => {
                let (text, color) = match state {
                    SubmoduleState::UpToDate => (None, palette.muted),
                    SubmoduleState::Uninitialized => (Some("not initialised"), palette.muted),
                    SubmoduleState::Modified => (Some("modified"), palette.warn),
                    SubmoduleState::Conflicted => (Some("conflict"), palette.error),
                };
                base.pl(indent(1.))
                    .child(icon(IconName::Package, palette.muted))
                    .child(label(path, palette.fg))
                    .when_some(text, |d, text| d.child(chip(text.into(), color)))
                    .into_any_element()
            }
            Row::Empty(section) => {
                let text = match section {
                    Section::Local => "No branches yet.",
                    Section::Remotes => "No remotes. Right-click REMOTES to add one.",
                    Section::Tags => "No tags.",
                    Section::Stashes => "No stashes.",
                    Section::Submodules => "No submodules.",
                };
                h_flex()
                    .h(zoomed(ROW_HEIGHT, cx))
                    .pl(indent(1.))
                    .text_xs()
                    .text_color(palette.muted)
                    .child(text)
                    .into_any_element()
            }
        };
        // Rows with actions get a context menu (disabled items while an operation runs).
        if matches!(row, Row::Empty(_) | Row::Submodule { .. }) {
            return Some(element);
        }
        Some(
            div()
                .id(ElementId::NamedInteger("sidebar-menu".into(), ix as u64))
                .child(element)
                .context_menu(move |menu, _, _| {
                    actions::menu(menu, &row, busy, &remotes, view.clone())
                })
                .into_any_element(),
        )
    }
}

impl BasePanel for SidebarPanel {
    fn panel_name(&self) -> &'static str {
        "sidebar"
    }
}

impl Panel for SidebarPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Branches"
    }
}

impl EventEmitter<PanelEvent> for SidebarPanel {}

impl Focusable for SidebarPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let body = if self.repo.is_none() {
            div()
                .p_4()
                .text_sm()
                .text_color(palette.muted)
                .child("Open a repository to see its branches.")
                .into_any_element()
        } else {
            uniform_list(
                "sidebar-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range
                        .filter_map(|ix| this.render_row(ix, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.panel)
            .child(
                div()
                    .p_2()
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(body)
    }
}
