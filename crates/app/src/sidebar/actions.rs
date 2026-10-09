//! Sidebar context menus and what they run. Destructive actions (branch / tag / remote delete,
//! force push, stash drop) confirm first; force pushes use `--force-with-lease` only.

use git_core::parse::refs::RefKind;
use git_core::{PullMode, PushOptions, StashAction};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{ClipboardItem, Context, Entity, WeakEntity, Window};

use super::SidebarPanel;
use super::rows::{Row, Section};
use crate::dialogs::{self, Field};
use crate::repo_tab::RepoTab;
use crate::undo::UndoPlan;

#[derive(Debug, Clone)]
enum Action {
    CheckoutLocal(String),
    CheckoutRemote {
        remote: String,
        branch: String,
    },
    NewBranchFrom(String),
    RenameBranch(String),
    DeleteBranch {
        name: String,
        force: bool,
    },
    SetUpstream {
        branch: String,
        upstream: Option<String>,
    },
    Push {
        branch: String,
        force: bool,
    },
    Pull(PullMode),
    /// Merge a branch (local or `remote/branch`) into the current one.
    Merge(String),
    /// Rebase the current branch onto a branch.
    Rebase(String),
    Fetch {
        remote: Option<String>,
        prune: bool,
    },
    AddRemote,
    EditRemote(String),
    RenameRemote(String),
    RemoveRemote(String),
    PushTag {
        remote: String,
        tag: String,
    },
    DeleteTag(String),
    DeleteRemoteTag {
        remote: String,
        tag: String,
    },
    Stash(StashAction, u32),
    Copy(String),
}

pub(super) fn menu(
    menu: PopupMenu,
    row: &Row,
    busy: bool,
    remotes: &[String],
    view: WeakEntity<SidebarPanel>,
) -> PopupMenu {
    let item = |label: String, action: Action| {
        let view = view.clone();
        // Copy works while git is busy; everything else waits.
        let disabled = busy && !matches!(action, Action::Copy(_));
        PopupMenuItem::new(label)
            .disabled(disabled)
            .on_click(move |_, window, cx| {
                _ = view.update(cx, |view, cx| view.run(action.clone(), window, cx));
            })
    };
    let mut menu = menu.min_w(gpui_kit::px(230.));
    match row {
        Row::Header {
            section: Section::Remotes,
            ..
        } => {
            menu = menu
                .item(item("Add remote\u{2026}".into(), Action::AddRemote))
                .item(item(
                    "Fetch all remotes".into(),
                    Action::Fetch {
                        remote: None,
                        prune: false,
                    },
                ))
                .item(item(
                    "Fetch all and prune".into(),
                    Action::Fetch {
                        remote: None,
                        prune: true,
                    },
                ));
        }
        Row::Header { .. } | Row::Empty(_) | Row::Submodule { .. } => {}
        Row::Local {
            name,
            oid,
            current,
            upstream,
            ..
        } => {
            if !current {
                menu = menu
                    .item(item(
                        format!("Checkout {name}"),
                        Action::CheckoutLocal(name.clone()),
                    ))
                    .item(item(
                        format!("Merge {name} into current branch"),
                        Action::Merge(name.clone()),
                    ))
                    .item(item(
                        format!("Rebase current branch onto {name}"),
                        Action::Rebase(name.clone()),
                    ));
            } else {
                menu = menu
                    .item(item(
                        "Pull (per config)".into(),
                        Action::Pull(PullMode::Config),
                    ))
                    .item(item("Pull, merge".into(), Action::Pull(PullMode::Merge)))
                    .item(item("Pull, rebase".into(), Action::Pull(PullMode::Rebase)))
                    .item(item(
                        "Pull, fast-forward only".into(),
                        Action::Pull(PullMode::FastForwardOnly),
                    ));
            }
            menu = menu
                .item(item(
                    format!("Push {name}"),
                    Action::Push {
                        branch: name.clone(),
                        force: false,
                    },
                ))
                .item(item(
                    "Force push (with lease)\u{2026}".into(),
                    Action::Push {
                        branch: name.clone(),
                        force: true,
                    },
                ))
                .separator()
                .item(item(
                    format!("New branch from {name}\u{2026}"),
                    Action::NewBranchFrom(oid.clone()),
                ))
                .item(item(
                    "Rename\u{2026}".into(),
                    Action::RenameBranch(name.clone()),
                ));
            for remote in remotes.iter().take(3) {
                let target = format!("{remote}/{name}");
                if upstream.as_deref() != Some(target.as_str()) {
                    menu = menu.item(item(
                        format!("Track {target}"),
                        Action::SetUpstream {
                            branch: name.clone(),
                            upstream: Some(target),
                        },
                    ));
                }
            }
            if upstream.is_some() {
                menu = menu.item(item(
                    "Stop tracking upstream".into(),
                    Action::SetUpstream {
                        branch: name.clone(),
                        upstream: None,
                    },
                ));
            }
            menu = menu.separator();
            if !current {
                menu = menu
                    .item(item(
                        "Delete\u{2026}".into(),
                        Action::DeleteBranch {
                            name: name.clone(),
                            force: false,
                        },
                    ))
                    .item(item(
                        "Force delete\u{2026}".into(),
                        Action::DeleteBranch {
                            name: name.clone(),
                            force: true,
                        },
                    ));
            }
            menu = menu.item(item("Copy name".into(), Action::Copy(name.clone())));
        }
        Row::Remote { name, url, .. } => {
            menu = menu
                .item(item(
                    format!("Fetch {name}"),
                    Action::Fetch {
                        remote: Some(name.clone()),
                        prune: false,
                    },
                ))
                .item(item(
                    format!("Fetch {name} and prune"),
                    Action::Fetch {
                        remote: Some(name.clone()),
                        prune: true,
                    },
                ))
                .separator()
                .item(item(
                    "Edit URL\u{2026}".into(),
                    Action::EditRemote(name.clone()),
                ))
                .item(item(
                    "Rename\u{2026}".into(),
                    Action::RenameRemote(name.clone()),
                ))
                .item(item(
                    "Remove\u{2026}".into(),
                    Action::RemoveRemote(name.clone()),
                ));
            if let Some(url) = url {
                menu = menu.item(item("Copy URL".into(), Action::Copy(url.clone())));
            }
        }
        Row::RemoteBranch {
            remote,
            branch,
            oid,
        } => {
            menu = menu
                .item(item(
                    format!("Checkout as local {branch}"),
                    Action::CheckoutRemote {
                        remote: remote.clone(),
                        branch: branch.clone(),
                    },
                ))
                .item(item(
                    "New branch from here\u{2026}".into(),
                    Action::NewBranchFrom(oid.clone()),
                ))
                .item(item(
                    format!("Merge {remote}/{branch} into current branch"),
                    Action::Merge(format!("{remote}/{branch}")),
                ))
                .item(item(
                    format!("Rebase current branch onto {remote}/{branch}"),
                    Action::Rebase(format!("{remote}/{branch}")),
                ))
                .item(item(
                    "Copy name".into(),
                    Action::Copy(format!("{remote}/{branch}")),
                ));
        }
        Row::Tag { name, .. } => {
            for remote in remotes.iter().take(3) {
                menu = menu.item(item(
                    format!("Push to {remote}"),
                    Action::PushTag {
                        remote: remote.clone(),
                        tag: name.clone(),
                    },
                ));
            }
            menu = menu.separator().item(item(
                "Delete local tag\u{2026}".into(),
                Action::DeleteTag(name.clone()),
            ));
            for remote in remotes.iter().take(3) {
                menu = menu.item(item(
                    format!("Delete from {remote}\u{2026}"),
                    Action::DeleteRemoteTag {
                        remote: remote.clone(),
                        tag: name.clone(),
                    },
                ));
            }
            menu = menu.item(item("Copy name".into(), Action::Copy(name.clone())));
        }
        Row::Stash { index, .. } => {
            menu = menu
                .item(item(
                    "Apply".into(),
                    Action::Stash(StashAction::Apply, *index),
                ))
                .item(item(
                    "Pop (apply and drop)".into(),
                    Action::Stash(StashAction::Pop, *index),
                ))
                .separator()
                .item(item(
                    "Drop\u{2026}".into(),
                    Action::Stash(StashAction::Drop, *index),
                ));
        }
    }
    menu
}

impl SidebarPanel {
    fn tab(&self) -> Option<Entity<RepoTab>> {
        self.repo.clone()
    }

    fn op<F, Fut>(&self, label: String, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(git_core::CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        if let Some(tab) = self.tab() {
            tab.update(cx, |tab, cx| tab.run_op(label, cx, op));
        }
    }

    /// [`Self::op`] for operations the reflog can take back (offers an Undo afterwards).
    fn op_undoable<F, Fut>(&self, label: String, plan: UndoPlan, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(git_core::CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        if let Some(tab) = self.tab() {
            tab.update(cx, |tab, cx| tab.run_op_undoable(label, plan, cx, op));
        }
    }

    /// The remote a push of `branch` goes to: its upstream's remote, else `origin`, else the
    /// first remote. `None` when the repo has no remotes.
    fn push_remote(&self, branch: &str, cx: &Context<Self>) -> Option<(String, bool)> {
        let tab = self.tab()?;
        let tab = tab.read(cx);
        let upstream = tab
            .refs
            .iter()
            .find(|r| r.kind == RefKind::LocalBranch && r.short_name == branch)
            .and_then(|r| r.upstream.as_ref())
            .and_then(|u| u.name.strip_prefix("refs/remotes/"))
            .and_then(|rest| {
                tab.remotes
                    .iter()
                    .map(|r| r.name.as_str())
                    .find(|name| rest.starts_with(&format!("{name}/")))
            });
        if let Some(remote) = upstream {
            return Some((remote.to_string(), false));
        }
        let remote = tab
            .remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(tab.remotes.first())?;
        Some((remote.name.clone(), true))
    }

    pub(super) fn checkout_local(&mut self, name: String, cx: &mut Context<Self>) {
        self.op(format!("Checking out {name}"), cx, move |repo| async move {
            repo.switch_branch(&name).await
        });
    }

    /// Checks out a local branch for `remote/branch`, creating it (tracking) if needed.
    pub(super) fn checkout_remote(
        &mut self,
        remote: String,
        branch: String,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let exists = self.tab().is_some_and(|tab| {
            tab.read(cx)
                .refs
                .iter()
                .any(|r| r.kind == RefKind::LocalBranch && r.short_name == branch)
        });
        self.op(
            format!("Checking out {branch}"),
            cx,
            move |repo| async move {
                if exists {
                    return repo.switch_branch(&branch).await;
                }
                let upstream = format!("{remote}/{branch}");
                repo.create_branch(&branch, &upstream, true).await?;
                repo.set_upstream(&branch, Some(&upstream)).await
            },
        );
    }

    fn run(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        match action {
            Action::CheckoutLocal(name) => self.checkout_local(name, cx),
            Action::CheckoutRemote { remote, branch } => {
                self.checkout_remote(remote, branch, window, cx)
            }
            Action::NewBranchFrom(start) => {
                self.dialog_sub = Some(dialogs::prompt(
                    window,
                    cx,
                    format!("New branch at {}", &start[..start.len().min(7)]),
                    vec![Field::new("Branch name, e.g. feature/login")],
                    &["Create", "Create & checkout"],
                    move |values, button, _, cx| {
                        let (name, start, checkout) =
                            (values[0].clone(), start.clone(), button == 1);
                        _ = this.update(cx, |view, cx| {
                            view.op(format!("Creating {name}"), cx, move |repo| async move {
                                repo.create_branch(&name, &start, checkout).await
                            })
                        });
                    },
                ));
            }
            Action::RenameBranch(old) => {
                self.dialog_sub = Some(dialogs::prompt(
                    window,
                    cx,
                    format!("Rename {old}"),
                    vec![Field::new("New name").value(old.clone())],
                    &["Rename"],
                    move |values, _, _, cx| {
                        let (old, new) = (old.clone(), values[0].clone());
                        _ = this.update(cx, |view, cx| {
                            view.op(format!("Renaming {old}"), cx, move |repo| async move {
                                repo.rename_branch(&old, &new).await
                            })
                        });
                    },
                ));
            }
            Action::DeleteBranch { name, force } => {
                let body = if force {
                    format!(
                        "{name} is deleted even if it has commits no other branch has. They stay in the reflog for a while."
                    )
                } else {
                    format!("{name} is deleted. Git refuses if it has unmerged commits.")
                };
                dialogs::confirm(
                    window,
                    cx,
                    if force {
                        "Force delete branch?"
                    } else {
                        "Delete branch?"
                    },
                    body,
                    "Delete",
                    true,
                    move |_, cx| {
                        let name = name.clone();
                        _ = this.update(cx, |view, cx| {
                            view.op_undoable(
                                format!("Deleting {name}"),
                                UndoPlan::Branch(name.clone()),
                                cx,
                                move |repo| async move { repo.delete_branch(&name, force).await },
                            )
                        });
                    },
                );
            }
            Action::SetUpstream { branch, upstream } => self.op(
                format!("Updating upstream of {branch}"),
                cx,
                move |repo| async move { repo.set_upstream(&branch, upstream.as_deref()).await },
            ),
            Action::Push { branch, force } => {
                let Some((remote, set_upstream)) = self.push_remote(&branch, cx) else {
                    if let Some(tab) = self.tab() {
                        tab.update(cx, |tab, cx| {
                            tab.op_error =
                                Some("No remote to push to: add one under REMOTES.".into());
                            cx.notify();
                        });
                    }
                    return;
                };
                let opts = PushOptions {
                    set_upstream,
                    force_with_lease: force,
                };
                let label = format!("Pushing {branch} to {remote}");
                if !force {
                    self.op(label, cx, move |repo| async move {
                        repo.push_branch(&remote, &branch, opts).await
                    });
                    return;
                }
                dialogs::confirm(
                    window,
                    cx,
                    "Force push?",
                    format!(
                        "{remote}/{branch} is overwritten with your {branch}. With lease: git refuses if someone pushed there since your last fetch."
                    ),
                    "Force push",
                    true,
                    move |_, cx| {
                        let (remote, branch, label) =
                            (remote.clone(), branch.clone(), label.clone());
                        _ = this.update(cx, |view, cx| {
                            view.op(label, cx, move |repo| async move {
                                repo.push_branch(&remote, &branch, opts).await
                            })
                        });
                    },
                );
            }
            Action::Pull(mode) => self.op_undoable(
                "Pulling".into(),
                UndoPlan::Head,
                cx,
                move |repo| async move { repo.pull(mode).await },
            ),
            Action::Merge(rev) => {
                if let Some(tab) = self.tab() {
                    tab.update(cx, |tab, cx| tab.merge_branch(rev, cx));
                }
            }
            Action::Rebase(rev) => {
                if let Some(tab) = self.tab() {
                    tab.update(cx, |tab, cx| tab.rebase_onto(rev, cx));
                }
            }
            Action::Fetch { remote, prune } => {
                let label = match &remote {
                    Some(remote) => format!("Fetching {remote}"),
                    None => "Fetching all remotes".into(),
                };
                self.op(label, cx, move |repo| async move {
                    repo.fetch(remote.as_deref(), prune).await
                })
            }
            Action::AddRemote => {
                self.dialog_sub = Some(dialogs::prompt(
                    window,
                    cx,
                    "Add remote",
                    vec![
                        Field::new("Name, e.g. origin"),
                        Field::new("URL, e.g. https://github.com/owner/repo.git"),
                    ],
                    &["Add"],
                    move |values, _, _, cx| {
                        let (name, url) = (values[0].clone(), values[1].clone());
                        _ = this.update(cx, |view, cx| {
                            view.op(
                                format!("Adding remote {name}"),
                                cx,
                                move |repo| async move {
                                    repo.add_remote(&name, &url).await?;
                                    repo.fetch(Some(&name), false).await
                                },
                            )
                        });
                    },
                ));
            }
            Action::EditRemote(name) => {
                let current = self
                    .tab()
                    .and_then(|tab| {
                        tab.read(cx)
                            .remotes
                            .iter()
                            .find(|r| r.name == name)
                            .and_then(|r| r.fetch_url.clone())
                    })
                    .unwrap_or_default();
                self.dialog_sub = Some(dialogs::prompt(
                    window,
                    cx,
                    format!("Edit {name}"),
                    vec![Field::new("URL").value(current)],
                    &["Save"],
                    move |values, _, _, cx| {
                        let (name, url) = (name.clone(), values[0].clone());
                        _ = this.update(cx, |view, cx| {
                            view.op(format!("Updating {name}"), cx, move |repo| async move {
                                repo.set_remote_url(&name, &url).await
                            })
                        });
                    },
                ));
            }
            Action::RenameRemote(old) => {
                self.dialog_sub = Some(dialogs::prompt(
                    window,
                    cx,
                    format!("Rename remote {old}"),
                    vec![Field::new("New name").value(old.clone())],
                    &["Rename"],
                    move |values, _, _, cx| {
                        let (old, new) = (old.clone(), values[0].clone());
                        _ = this.update(cx, |view, cx| {
                            view.op(format!("Renaming {old}"), cx, move |repo| async move {
                                repo.rename_remote(&old, &new).await
                            })
                        });
                    },
                ));
            }
            Action::RemoveRemote(name) => dialogs::confirm(
                window,
                cx,
                "Remove remote?",
                format!(
                    "{name} and its remote-tracking branches are removed from this repository. Nothing is deleted on the server."
                ),
                "Remove",
                true,
                move |_, cx| {
                    let name = name.clone();
                    _ = this.update(cx, |view, cx| {
                        view.op(format!("Removing {name}"), cx, move |repo| async move {
                            repo.remove_remote(&name).await
                        })
                    });
                },
            ),
            Action::PushTag { remote, tag } => self.op(
                format!("Pushing tag {tag} to {remote}"),
                cx,
                move |repo| async move { repo.push_tag(&remote, &tag).await },
            ),
            Action::DeleteTag(tag) => dialogs::confirm(
                window,
                cx,
                "Delete tag?",
                format!("The local tag {tag} is deleted. Copies on remotes stay."),
                "Delete",
                true,
                move |_, cx| {
                    let tag = tag.clone();
                    _ = this.update(cx, |view, cx| {
                        view.op(format!("Deleting tag {tag}"), cx, move |repo| async move {
                            repo.delete_tag(&tag).await
                        })
                    });
                },
            ),
            Action::DeleteRemoteTag { remote, tag } => dialogs::confirm(
                window,
                cx,
                "Delete tag on remote?",
                format!("The tag {tag} is deleted on {remote} for everyone."),
                "Delete",
                true,
                move |_, cx| {
                    let (remote, tag) = (remote.clone(), tag.clone());
                    _ = this.update(cx, |view, cx| {
                        view.op(
                            format!("Deleting {tag} on {remote}"),
                            cx,
                            move |repo| async move { repo.delete_remote_tag(&remote, &tag).await },
                        )
                    });
                },
            ),
            Action::Stash(StashAction::Drop, index) => dialogs::confirm(
                window,
                cx,
                "Drop stash?",
                format!("stash@{{{index}}} is deleted. This cannot be undone from GitDragon."),
                "Drop",
                true,
                move |_, cx| {
                    _ = this.update(cx, |view, cx| {
                        view.op(format!("Dropping stash@{{{index}}}"), cx, move |repo| async move {
                            repo.stash_action(StashAction::Drop, index).await
                        })
                    });
                },
            ),
            Action::Stash(action, index) => {
                let verb = if action == StashAction::Pop {
                    "Popping"
                } else {
                    "Applying"
                };
                self.op(
                    format!("{verb} stash@{{{index}}}"),
                    cx,
                    move |repo| async move { repo.stash_action(action, index).await },
                )
            }
            Action::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
        }
    }
}
