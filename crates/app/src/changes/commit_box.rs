//! Commit box at the top of the details panel while the WIP row is selected: summary,
//! description, amend / sign-off, optional author and co-authors, Commit and Commit & push.
//! One per repository tab (the details panel keeps them), so a draft survives switching rows
//! and tabs. The commit runs as the tab's operation; the box clears once it lands.

use git_core::ops::validate_identity;
use git_core::{CliRepo, CommitRequest, GitError, GitRepo as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _,
};
use ui::Palette;

use super::Section;
use crate::repo_tab::RepoTab;

/// Where it failed: `(pushing, error)`; a failed push still made the commit.
type Outcome = Result<(), (bool, GitError)>;

pub struct CommitBox {
    tab: Entity<RepoTab>,
    summary: Entity<InputState>,
    body: Entity<TextareaState>,
    author: Entity<InputState>,
    co_authors: Entity<InputState>,
    amend: bool,
    signoff: bool,
    more: bool,
    /// Our commit is the tab's running operation.
    committing: bool,
    error: Option<SharedString>,
    /// Waits for the commit's outcome; ends early when the operation is cancelled.
    commit_task: Option<Task<()>>,
    /// "Write message" in flight (AI); dropping it abandons the request.
    ai_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl CommitBox {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let summary = cx.new(|cx| InputState::new(window, cx).placeholder("Summary (required)"));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("Description")
        });
        let author =
            cx.new(|cx| InputState::new(window, cx).placeholder("Author override: Name <email>"));
        let co_authors = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Co-authors: Name <email>; Name <email>")
        });
        let _subs = vec![
            // Branch name, staged count and busy state come from the tab.
            cx.observe(&tab, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &summary,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.submit(false, window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&body, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    this.submit(false, window, cx)
                }
            }),
        ];
        Self {
            tab,
            summary,
            body,
            author,
            co_authors,
            amend: false,
            signoff: false,
            more: false,
            committing: false,
            error: None,
            commit_task: None,
            ai_task: None,
            _subs,
        }
    }

    /// Fills summary + body with an AI-written message for the staged changes (never commits).
    fn write_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_task.is_some() {
            self.ai_task = None;
            cx.notify();
            return;
        }
        let job = crate::chat::context::write_commit_message(&self.tab, cx);
        self.error = None;
        self.ai_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.ai_task = None;
                match result {
                    Ok((summary, body)) => {
                        this.summary
                            .update(cx, |s, cx| s.set_value(summary, window, cx));
                        this.body.update(cx, |s, cx| s.set_value(body, window, cx));
                    }
                    Err(err) => {
                        this.error = Some(git_core::redact::redact(&err).into_owned().into())
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn staged(&self, cx: &Context<Self>) -> usize {
        self.tab.read(cx).status.as_ref().map_or(0, |s| {
            s.entries
                .iter()
                .filter(|e| Section::Staged.contains(e))
                .count()
        })
    }

    fn can_commit(&self, cx: &Context<Self>) -> bool {
        !self.tab.read(cx).is_busy()
            && !self.summary.read(cx).value().trim().is_empty()
            && (self.staged(cx) > 0 || self.amend)
    }

    /// Clears the box after a commit.
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.summary.update(cx, |s, cx| s.set_value("", window, cx));
        self.body.update(cx, |s, cx| s.set_value("", window, cx));
        self.amend = false;
        self.error = None;
        cx.notify();
    }

    /// Amend was switched on: fills summary + body from HEAD's message, unless the user
    /// already typed one.
    fn load_head_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.tab.read(cx).repo.clone() else {
            return;
        };
        let job = cx
            .background_executor()
            .spawn(async move { repo.head_message().await });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Some(message)) = job.await else { return };
            _ = this.update_in(cx, |this, window, cx| {
                if !this.amend || !this.summary.read(cx).value().trim().is_empty() {
                    return;
                }
                let (summary, body) = message.split_once('\n').unwrap_or((&message, ""));
                this.summary.update(cx, |s, cx| {
                    s.set_value(summary.trim().to_string(), window, cx)
                });
                this.body
                    .update(cx, |s, cx| s.set_value(body.trim().to_string(), window, cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn submit(&mut self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let author = self.author.read(cx).value().trim().to_string();
        let co_authors: Vec<String> = self
            .co_authors
            .read(cx)
            .value()
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(bad) = (!author.is_empty())
            .then_some(&author)
            .into_iter()
            .chain(&co_authors)
            .find(|who| validate_identity(who).is_err())
        {
            self.error = Some(format!("Use Name <email>: {bad}").into());
            cx.notify();
            return;
        }
        let request = CommitRequest {
            summary: self.summary.read(cx).value().to_string(),
            body: self.body.read(cx).value().to_string(),
            amend: self.amend,
            signoff: self.signoff,
            author: (!author.is_empty()).then_some(author),
            co_authors,
        };
        let has_upstream = self
            .tab
            .read(cx)
            .status
            .as_ref()
            .is_some_and(|s| s.branch.upstream.is_some());
        let label = match (self.amend, push) {
            (true, false) => "Amending",
            (true, true) => "Amending and pushing",
            (false, false) => "Committing",
            (false, true) => "Committing and pushing",
        };
        // The outcome comes back through `done`; a cancelled operation drops it unsent.
        let (tx, rx) = async_channel::bounded::<Outcome>(1);
        let started = self.tab.update(cx, |tab, cx| {
            tab.run_op_with(
                label,
                cx,
                move |repo| commit_and_push(repo, request, push, has_upstream),
                move |tab, outcome: Outcome, _| {
                    if let Err((true, err)) = &outcome {
                        tab.op_error = Some(format!("Committed, but push failed: {err}").into());
                    }
                    _ = tx.try_send(outcome);
                },
            )
        });
        if !started {
            return;
        }
        self.committing = true;
        self.error = None;
        self.commit_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = rx.recv().await;
            _ = this.update_in(cx, |this, window, cx| {
                this.committing = false;
                match outcome {
                    // Committed (a failed push is reported by the tab): clear the box.
                    Ok(Ok(())) | Ok(Err((true, _))) => this.clear(window, cx),
                    Ok(Err((false, err))) => this.error = Some(err.to_string().into()),
                    // Cancelled: the tab says so.
                    Err(_) => {}
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// Commits, then (for Commit & push) pushes, setting an upstream on `origin` (or the first
/// remote) when the branch has none.
async fn commit_and_push(
    repo: CliRepo,
    request: CommitRequest,
    push: bool,
    has_upstream: bool,
) -> Outcome {
    repo.commit(&request).await.map_err(|err| (false, err))?;
    if !push {
        return Ok(());
    }
    let set_upstream = if has_upstream {
        None
    } else {
        let remotes = repo.remotes().await.map_err(|err| (true, err))?;
        let name = remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(remotes.first())
            .map(|r| r.name.clone());
        Some(name.ok_or_else(|| {
            (
                true,
                GitError::InvalidArgument {
                    what: "remote (none configured)",
                    value: String::new(),
                },
            )
        })?)
    };
    repo.push(set_upstream.as_deref())
        .await
        .map_err(|err| (true, err))
}

impl Render for CommitBox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let can_commit = self.can_commit(cx);
        let target = self
            .tab
            .read(cx)
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .unwrap_or_else(|| "detached HEAD".into());
        let label = if self.amend {
            format!("Amend {target}")
        } else {
            format!("Commit to {target}")
        };
        v_flex()
            .gap_2()
            .child(Input::new(&self.summary).small())
            .child(Textarea::new(&self.body).small())
            .when(self.more, |d| {
                d.child(Input::new(&self.author).small())
                    .child(Input::new(&self.co_authors).small())
            })
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .child(
                        Checkbox::new("commit-amend")
                            .label("Amend")
                            .checked(self.amend)
                            .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                this.amend = *checked;
                                if *checked {
                                    this.load_head_message(window, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("commit-signoff")
                            .label("Sign-off")
                            .checked(self.signoff)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.signoff = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("commit-ai")
                            .ghost()
                            .xsmall()
                            .icon(gpui_kit::component::IconName::Bot)
                            .label(if self.ai_task.is_some() {
                                "Writing\u{2026} (stop)"
                            } else {
                                "Write message"
                            })
                            .disabled(self.staged(cx) == 0 && self.ai_task.is_none())
                            .tooltip("Ask Claude to write a message for the staged changes")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.write_message(window, cx)
                            })),
                    )
                    .child(
                        Button::new("commit-more")
                            .ghost()
                            .xsmall()
                            .label(if self.more { "Less" } else { "Author\u{2026}" })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.more = !this.more;
                                cx.notify();
                            })),
                    ),
            )
            .when_some(self.error.clone(), |d, err| {
                d.child(div().text_xs().text_color(palette.error).child(err))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("commit")
                            .primary()
                            .small()
                            .flex_1()
                            .label(label)
                            .loading(self.committing)
                            .disabled(!can_commit)
                            .tooltip("Commit (Ctrl+Enter)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("commit-push")
                            .outline()
                            .small()
                            .label("& Push")
                            .disabled(!can_commit)
                            .tooltip("Commit, then push")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit(true, window, cx)
                            })),
                    ),
            )
    }
}
