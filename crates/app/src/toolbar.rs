//! Repository toolbar under the tabs: Fetch, Pull (mode menu), Push, Branch, Stash, Pop, with
//! behind / ahead badges for the current branch and the running operation on the right.

use git_core::lfs::LfsStatus;
use git_core::{PullMode, PushOptions, StashAction};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex};
use gpui_kit::{
    ClickEvent, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use ui::{Palette, zoomed};

use crate::dialogs::{self, Field};
use crate::repo_tab::RepoTab;

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

    fn pull(&mut self, mode: PullMode, cx: &mut Context<Self>) {
        self.op("Pulling".into(), cx, move |repo| async move {
            repo.pull(mode).await
        });
    }

    /// Pushes the current branch; with no upstream yet, to `origin` (or the first remote),
    /// setting the upstream.
    fn push(&mut self, cx: &mut Context<Self>) {
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

    fn new_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let this = cx.entity().downgrade();
        let pull_menu = Button::new("toolbar-pull-mode")
            .ghost()
            .small()
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
        h_flex()
            .flex_none()
            .h(zoomed(38., cx))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(
                Button::new("toolbar-fetch")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .label("Fetch")
                    .tooltip("Fetch all remotes")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.op("Fetching all remotes".into(), cx, |repo| async move {
                            repo.fetch(None, false).await
                        })
                    })),
            )
            .child(
                h_flex()
                    .child(
                        Button::new("toolbar-pull")
                            .ghost()
                            .small()
                            .icon(IconName::Download)
                            .label(pull_label)
                            .tooltip("Pull the current branch (per git config)")
                            .disabled(busy || !head.upstream)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.pull(PullMode::Config, cx)
                            })),
                    )
                    .child(pull_menu),
            )
            .child(
                Button::new("toolbar-push")
                    .ghost()
                    .small()
                    .icon(IconName::Upload)
                    .label(push_label)
                    .tooltip(if head.upstream {
                        "Push the current branch"
                    } else {
                        "Push and track on the remote"
                    })
                    .disabled(busy || head.branch.is_none())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.push(cx))),
            )
            .child(div().w_px().h(zoomed(18., cx)).mx_1().bg(palette.border))
            .child(
                Button::new("toolbar-branch")
                    .ghost()
                    .small()
                    .icon(IconName::GitBranch)
                    .label("Branch")
                    .disabled(busy)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.new_branch(window, cx)),
                    ),
            )
            .child(
                Button::new("toolbar-stash")
                    .ghost()
                    .small()
                    .icon(IconName::Archive)
                    .label("Stash")
                    .disabled(busy)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.stash(window, cx)),
                    ),
            )
            .child(
                Button::new("toolbar-pop")
                    .ghost()
                    .small()
                    .icon(IconName::ArchiveRestore)
                    .label("Pop")
                    .tooltip("Apply and drop the latest stash")
                    .disabled(busy || head.stashes == 0)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.op("Popping stash@{0}".into(), cx, |repo| async move {
                            repo.stash_action(StashAction::Pop, 0).await
                        })
                    })),
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
                    d.child(div().w_px().h(zoomed(18., cx)).mx_1().bg(palette.border))
                        .child(
                            Button::new("toolbar-lfs")
                                .ghost()
                                .small()
                                .icon(IconName::HardDriveDownload)
                                .label(label)
                                .disabled(busy)
                                .dropdown_menu(move |menu, _, _| {
                                    let item = |label: &'static str, action: LfsAction| {
                                        let this = this.clone();
                                        PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                            _ = this.update(cx, |view, cx| view.lfs(action, cx));
                                        })
                                    };
                                    let menu = menu
                                        .item(item(
                                            "Download LFS files (git lfs pull)",
                                            LfsAction::Pull,
                                        ))
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
                    Button::new("toolbar-lfs-missing")
                        .ghost()
                        .small()
                        .icon(IconName::TriangleAlert)
                        .label("Git LFS not installed")
                        .tooltip(
                            "This repository stores files in Git LFS, but git-lfs is not \
                             installed: you get small pointer files instead of the content. \
                             Click to open git-lfs.com, then reopen the repository.",
                        )
                        .on_click(|_: &ClickEvent, _, cx| cx.open_url("https://git-lfs.com")),
                )
            })
            .child(div().flex_1())
            .when_some(head.busy, |d, label| {
                d.child(badge(format!("{label}\u{2026}"), palette.accent))
            })
            .when_some(head.branch, |d, branch| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(SharedString::from(branch)),
                )
            })
            .into_any_element()
    }
}
