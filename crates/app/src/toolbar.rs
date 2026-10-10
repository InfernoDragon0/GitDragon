//! Repository toolbar under the tabs: Fetch, Pull (mode menu), Push, Branch, Stash, Pop, with
//! behind / ahead badges for the current branch and the running operation on the right.

use git_core::lfs::LfsStatus;
use git_core::{PullMode, PushOptions, StashAction};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, Entity, FontWeight, Hsla,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription,
    Window, div, ease_in_out, prelude::FluentBuilder as _,
};
use ui::{Palette, motion, zoomed};

use crate::dialogs::{self, Field};
use crate::repo_tab::{OpKind, RepoTab};
use crate::undo::UndoPlan;

pub struct RepoToolbar {
    repo: Option<Entity<RepoTab>>,
    dialog_sub: Option<Subscription>,
    _observe: Option<Subscription>,
}

/// What the buttons need from the active tab.
struct Head {
    branch: Option<String>,
    upstream: bool,
    ahead: u32,
    behind: u32,
    stashes: usize,
    remote: Option<String>,
    busy: Option<SharedString>,
    /// The running operation, `Other` when none.
    op: OpKind,
    lfs: Option<LfsStatus>,
    /// LFS locks held by the signed-in user.
    locks: usize,
}

#[derive(Debug, Clone, Copy)]
enum LfsAction {
    Pull,
    Prune,
    RefreshLocks,
}

impl RepoToolbar {
    pub fn new() -> Self {
        Self {
            repo: None,
            dialog_sub: None,
            _observe: None,
        }
    }

    pub fn set_repo(&mut self, repo: Option<Entity<RepoTab>>, cx: &mut Context<Self>) {
        self._observe = repo.as_ref().map(|r| cx.observe(r, |_, _, cx| cx.notify()));
        self.repo = repo;
        cx.notify();
    }

    pub fn has_repo(&self) -> bool {
        self.repo.is_some()
    }

    fn head(&self, cx: &Context<Self>) -> Option<Head> {
        let tab = self.repo.as_ref()?.read(cx);
        let branch = tab.status.as_ref().map(|s| &s.branch);
        let remote = tab
            .remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(tab.remotes.first())
            .map(|r| r.name.clone());
        Some(Head {
            branch: branch.and_then(|b| b.head.clone()),
            upstream: branch.is_some_and(|b| b.upstream.is_some()),
            ahead: branch.map_or(0, |b| b.ahead),
            behind: branch.map_or(0, |b| b.behind),
            stashes: tab.stashes.len(),
            remote,
            busy: tab.op_label.clone(),
            op: if tab.op_label.is_some() {
                tab.op_kind
            } else {
                OpKind::Other
            },
            lfs: tab.lfs.clone(),
            locks: tab.locks.iter().filter(|l| l.ours).count(),
        })
    }

    fn op<F, Fut>(&self, label: String, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(git_core::CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        if let Some(tab) = &self.repo {
            tab.update(cx, |tab, cx| tab.run_op(label, cx, op));
        }
    }

    fn lfs(&mut self, action: LfsAction, cx: &mut Context<Self>) {
        match action {
            LfsAction::Pull => self.op("Downloading LFS files".into(), cx, |repo| async move {
                repo.lfs_pull().await
            }),
            LfsAction::Prune => self.op("Pruning LFS files".into(), cx, |repo| async move {
                repo.lfs_prune().await
            }),
            LfsAction::RefreshLocks => {
                if let Some(tab) = &self.repo {
                    tab.update(cx, |tab, cx| tab.refresh_locks(cx));
                }
            }
        }
    }

    pub fn pull(&mut self, mode: PullMode, cx: &mut Context<Self>) {
        if let Some(tab) = &self.repo {
            tab.update(cx, |tab, cx| {
                tab.run_op_undoable("Pulling", UndoPlan::Head, cx, move |repo| async move {
                    repo.pull(mode).await
                })
            });
        }
    }

    /// Fetch all remotes (`--prune` per Settings).
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        let prune = crate::prefs::get(cx).prune_on_fetch;
        self.op("Fetching all remotes".into(), cx, move |repo| async move {
            repo.fetch(None, prune).await
        });
    }

    /// Pop the latest stash.
    pub fn pop_stash(&mut self, cx: &mut Context<Self>) {
        self.op("Popping stash@{0}".into(), cx, |repo| async move {
            repo.stash_action(StashAction::Pop, 0).await
        });
    }

    /// Pushes the current branch; with no upstream yet, to `origin` (or the first remote),
    /// setting the upstream.
    pub fn push(&mut self, cx: &mut Context<Self>) {
        let Some(head) = self.head(cx) else {
            return;
        };
        let Some(branch) = head.branch else {
            return;
        };
        if head.upstream {
            self.op(format!("Pushing {branch}"), cx, move |repo| async move {
                repo.push(None).await
            });
            return;
        }
        let Some(remote) = head.remote else {
            if let Some(tab) = &self.repo {
                tab.update(cx, |tab, cx| {
                    tab.op_error =
                        Some("No remote to push to: add one in the sidebar under REMOTES.".into());
                    cx.notify();
                });
            }
            return;
        };
        let opts = PushOptions {
            set_upstream: true,
            force_with_lease: false,
        };
        self.op(
            format!("Pushing {branch} to {remote}"),
            cx,
            move |repo| async move { repo.push_branch(&remote, &branch, opts).await },
        );
    }

    pub fn new_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.dialog_sub = Some(dialogs::prompt(
            window,
            cx,
            "New branch from HEAD",
            vec![Field::new("Branch name, e.g. feature/login")],
            &["Create", "Create & checkout"],
            move |values, button, _, cx| {
                let (name, checkout) = (values[0].clone(), button == 1);
                _ = this.update(cx, |view, cx| {
                    view.op(format!("Creating {name}"), cx, move |repo| async move {
                        repo.create_branch(&name, "HEAD", checkout).await
                    })
                });
            },
        ));
    }

    pub fn stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.dialog_sub = Some(dialogs::prompt(
            window,
            cx,
            "Stash all changes",
            vec![Field::new("Message (optional)").value("WIP")],
            &["Stash"],
            move |values, _, _, cx| {
                let message = values[0].clone();
                _ = this.update(cx, |view, cx| {
                    view.op("Stashing".into(), cx, move |repo| async move {
                        repo.stash_push(&message, true, false, &[]).await
                    })
                });
            },
        ));
    }
}

/// A toolbar button: icon over label, no border, transparent until hovered, all the same width
/// so the group reads as evenly spaced. With a `tone` (a semantic colour, not the accent) the
/// icon and label glow: they take the colour, the label turns semibold and a soft light sits
/// behind the icon and slowly pulses (GPUI has no text shadow, and the backdrop stays clear).
fn tool(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    palette: &Palette,
    tone: Option<Hsla>,
    cx: &App,
) -> Button {
    tool_anim(id, icon, label, palette, tone, OpKind::Other, cx)
}

/// The icon of a toolbar button while its operation runs: Fetch spins, Pull drops the arrow into
/// a tray, Push lifts it out. `Other` is the plain static icon.
fn op_icon(
    id: &'static str,
    icon: IconName,
    anim: OpKind,
    color: Hsla,
    size: Pixels,
    reduce: bool,
) -> AnyElement {
    // Lucide draws on a 24 grid with a 2 unit stroke; `unit` is one grid unit at this size.
    let unit = size / 24.;
    match anim {
        OpKind::Other => Icon::new(icon).size(size).into_any_element(),
        OpKind::Fetch => motion::spin(
            Icon::new(icon).size(size),
            SharedString::from(format!("{id}-spin")),
            motion::CYCLE,
            reduce,
        ),
        OpKind::Pull | OpKind::Push => {
            let pull = anim == OpKind::Pull;
            // Lucide's download tray: x 3..21, y 15..21 on the 24 grid, 2 unit stroke, open at
            // the top, corners rounded (outer radius 3).
            let tray = div()
                .absolute()
                .top(unit * 15.)
                .left(unit * 2.)
                .w(unit * 20.)
                .h(unit * 7.)
                .border(unit * 2.)
                .border_t_0()
                .border_color(color)
                .rounded_b(unit * 3.);
            // The arrow at full icon size so its stroke matches the idle icon; the outer box clips
            // it where it sits above or below the tray.
            let arrow = div().absolute().left_0().size(size).child(
                // Pull's arrow points into the tray, Push's out of it.
                Icon::new(if pull {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                })
                .size(size),
            );
            let arrow = arrow.with_animation(
                SharedString::from(format!("{id}-move")),
                Animation::new(motion::CYCLE)
                    .repeat()
                    .with_easing(ease_in_out)
                    .with_max_fps(motion::FPS_CAP),
                move |d, t| {
                    // Pull: from above the tray down into it, fading in. Push: the reverse,
                    // fading out at the top.
                    let (from, to) = if pull { (-12., 0.) } else { (0., -12.) };
                    let y = from + (to - from) * t;
                    let fade = if pull { t * 3. } else { (1. - t) * 3. };
                    d.top(unit * y).opacity(fade.min(1.))
                },
            );
            div()
                .relative()
                .size(size)
                .overflow_hidden()
                .child(tray)
                .child(arrow)
                .into_any_element()
        }
    }
}

/// [`tool`] whose icon animates while `anim` (the kind of the running operation) matches.
fn tool_anim(
    id: &'static str,
    icon: IconName,
    label: impl Into<SharedString>,
    palette: &Palette,
    tone: Option<Hsla>,
    anim: OpKind,
    cx: &App,
) -> Button {
    // The system "reduce motion" setting keeps the icon still.
    let reduce = cx.reduce_motion();
    let anim = if reduce { OpKind::Other } else { anim };
    let size = zoomed(18., cx);
    // The light breathes slowly; capped at 30 fps since it repaints for as long as it shows.
    let halo = tone.map(|t| {
        // Stacked translucent discs with exact radii: a blurred shadow on an element this small
        // renders boxy, these stay round. Densest in the middle, fading outwards.
        let disc = |scale: f32, alpha: f32| {
            let d = size * scale;
            div()
                .absolute()
                .size(d)
                .rounded(d / 2.)
                .bg(t.opacity(alpha))
        };
        let halo = div()
            .absolute()
            .size(size * 1.1)
            .flex()
            .items_center()
            .justify_center()
            .child(disc(1.1, 0.08))
            .child(disc(0.85, 0.12))
            .child(disc(0.6, 0.18));
        motion::pulse(
            halo,
            SharedString::from(format!("{id}-glow")),
            0.35,
            1.,
            motion::BREATHE,
            reduce,
        )
    });
    tone_button(Button::new(id), palette, tone, cx)
        .min_w(zoomed(64., cx))
        .px_2()
        .child(
            v_flex()
                .items_center()
                .gap_0p5()
                .child(
                    div()
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(size)
                        .children(halo)
                        .child(op_icon(
                            id,
                            icon,
                            anim,
                            tone.unwrap_or(palette.fg),
                            size,
                            reduce,
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .whitespace_nowrap()
                        .when(tone.is_some(), |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(label.into()),
                ),
        )
}

/// The look [`tool`] shares with the pull-mode chevron: borderless, clear backdrop, full
/// toolbar height; a `tone` only colours the content.
fn tone_button(button: Button, palette: &Palette, tone: Option<Hsla>, cx: &App) -> Button {
    let clear = palette.panel.opacity(0.);
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .foreground(tone.unwrap_or(palette.fg))
                .color(clear)
                .hover(palette.hover)
                .active(palette.hover),
        )
        .bg(clear)
        .h(zoomed(44., cx))
}

impl Default for RepoToolbar {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for RepoToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let Some(head) = self.head(cx) else {
            return div().into_any_element();
        };
        let busy = head.busy.is_some();
        let badge = |text: String, color: gpui_kit::Hsla| {
            div()
                .px_1()
                .rounded_sm()
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.14))
                .child(SharedString::from(text))
        };
        let pull_label = if head.behind > 0 {
            format!("Pull \u{2193}{}", head.behind)
        } else {
            "Pull".into()
        };
        let push_label = if head.ahead > 0 {
            format!("Push \u{2191}{}", head.ahead)
        } else {
            "Push".into()
        };
        // Glow when there is something to pull / push; semantic colours, never the accent.
        let pull_tone = (head.behind > 0 && head.upstream && !busy).then_some(palette.warn);
        let push_tone = (head.ahead > 0 && !busy).then_some(palette.ok);
        let pull_pref = crate::prefs::get(cx).pull;
        let this = cx.entity().downgrade();
        // The pull-mode chevron sits flush against Pull, as tall as it, and glows with it.
        let pull_menu = tone_button(Button::new("toolbar-pull-mode"), &palette, pull_tone, cx)
            .px_0p5()
            .xsmall()
            .icon(IconName::ChevronDown)
            .disabled(busy || head.branch.is_none())
            .dropdown_menu(move |menu, _, _| {
                let item = |label: &'static str, mode: PullMode| {
                    let this = this.clone();
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        _ = this.update(cx, |view, cx| view.pull(mode, cx));
                    })
                };
                menu.item(item("Pull (per git config)", PullMode::Config))
                    .item(item("Pull, merge", PullMode::Merge))
                    .item(item("Pull, rebase", PullMode::Rebase))
                    .item(item("Pull, fast-forward only", PullMode::FastForwardOnly))
            });
        let divider = div().w_px().h(zoomed(30., cx)).mx_2().bg(palette.border);
        // Local actions on the left of the group, the remote ones (fetch, pull, push) on the right.
        let local = h_flex()
            .gap_1()
            .child(
                tool(
                    "toolbar-branch",
                    IconName::GitBranch,
                    "Branch",
                    &palette,
                    None,
                    cx,
                )
                .disabled(busy)
                .on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.new_branch(window, cx)),
                ),
            )
            .child(
                tool(
                    "toolbar-stash",
                    IconName::Archive,
                    "Stash",
                    &palette,
                    None,
                    cx,
                )
                .disabled(busy)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.stash(window, cx))),
            )
            .child(
                tool(
                    "toolbar-pop",
                    IconName::ArchiveRestore,
                    "Pop",
                    &palette,
                    None,
                    cx,
                )
                .tooltip("Apply and drop the latest stash")
                .disabled(busy || head.stashes == 0)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.pop_stash(cx))),
            )
            .when(
                head.lfs.as_ref().is_some_and(|l| l.used && !l.missing()),
                |d| {
                    let this = cx.entity().downgrade();
                    let lockable = head.lfs.as_ref().is_some_and(|l| l.lockable);
                    let label = if head.locks > 0 {
                        format!("LFS \u{b7} {} locked", head.locks)
                    } else {
                        "LFS".into()
                    };
                    d.child(
                        tool(
                            "toolbar-lfs",
                            IconName::HardDriveDownload,
                            label,
                            &palette,
                            None,
                            cx,
                        )
                        .disabled(busy)
                        .dropdown_menu(move |menu, _, _| {
                            let item = |label: &'static str, action: LfsAction| {
                                let this = this.clone();
                                PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                    _ = this.update(cx, |view, cx| view.lfs(action, cx));
                                })
                            };
                            let menu = menu
                                .item(item("Download LFS files (git lfs pull)", LfsAction::Pull))
                                .item(item("Prune old LFS files", LfsAction::Prune));
                            if lockable {
                                menu.separator()
                                    .item(item("Refresh locks", LfsAction::RefreshLocks))
                            } else {
                                menu
                            }
                        }),
                    )
                },
            )
            .when(head.lfs.as_ref().is_some_and(LfsStatus::missing), |d| {
                d.child(
                    tool(
                        "toolbar-lfs-missing",
                        IconName::TriangleAlert,
                        "Git LFS missing",
                        &palette,
                        None,
                        cx,
                    )
                    .tooltip(
                        "This repository stores files in Git LFS, but git-lfs is not \
                         installed: you get small pointer files instead of the content. \
                         Click to open git-lfs.com, then reopen the repository.",
                    )
                    .on_click(|_: &ClickEvent, window, cx| {
                        crate::links::open("https://git-lfs.com", Some(window), cx)
                    }),
                )
            });
        let remote = h_flex()
            .gap_1()
            .child(
                tool_anim(
                    "toolbar-fetch",
                    IconName::RefreshCw,
                    "Fetch",
                    &palette,
                    None,
                    if head.op == OpKind::Fetch {
                        OpKind::Fetch
                    } else {
                        OpKind::Other
                    },
                    cx,
                )
                .tooltip("Fetch all remotes")
                .disabled(busy && head.op != OpKind::Fetch)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.fetch(cx))),
            )
            .child(
                h_flex()
                    .child(
                        tool_anim(
                            "toolbar-pull",
                            IconName::Download,
                            pull_label,
                            &palette,
                            pull_tone,
                            if head.op == OpKind::Pull {
                                OpKind::Pull
                            } else {
                                OpKind::Other
                            },
                            cx,
                        )
                        .tooltip(format!(
                            "Pull the current branch ({}; change under Settings > Pull & push)",
                            pull_pref.label().to_lowercase()
                        ))
                        .disabled((busy && head.op != OpKind::Pull) || !head.upstream)
                        .on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                let mode = crate::prefs::get(cx).pull.mode();
                                this.pull(mode, cx)
                            },
                        )),
                    )
                    .child(pull_menu),
            )
            .child(
                tool_anim(
                    "toolbar-push",
                    IconName::Upload,
                    push_label,
                    &palette,
                    push_tone,
                    if head.op == OpKind::Push {
                        OpKind::Push
                    } else {
                        OpKind::Other
                    },
                    cx,
                )
                .tooltip(if head.upstream {
                    "Push the current branch"
                } else {
                    "Push and track on the remote"
                })
                .disabled((busy && head.op != OpKind::Push) || head.branch.is_none())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.push(cx))),
            );
        // Equal flexible sides keep the button group centred whatever the right side shows.
        h_flex()
            .flex_none()
            .h(zoomed(52., cx))
            .px_2()
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(div().flex_1().min_w_0())
            .child(
                h_flex()
                    .flex_none()
                    .child(local)
                    .child(divider)
                    .child(remote),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_end()
                    .gap_2()
                    .when_some(head.busy, |d, label| {
                        d.child(badge(format!("{label}\u{2026}"), palette.accent))
                    })
                    .when_some(head.branch, |d, branch| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(palette.muted)
                                .truncate()
                                .child(SharedString::from(branch)),
                        )
                    }),
            )
            .into_any_element()
    }
}
