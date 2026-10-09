//! Commit box under the file list: summary, description, amend / sign-off, optional author and
//! co-authors, Commit and Commit & push. Emits [`CommitBoxEvent::Commit`]; the Changes view runs
//! it and calls [`CommitBox::committed`] on success.

use git_core::CommitRequest;
use git_core::ops::validate_identity;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use ui::Palette;

pub enum CommitBoxEvent {
    Commit {
        request: CommitRequest,
        push: bool,
    },
    /// Amend was switched on: the view loads HEAD's message into the box.
    WantsHeadMessage,
}

pub struct CommitBox {
    summary: Entity<InputState>,
    body: Entity<TextareaState>,
    author: Entity<InputState>,
    co_authors: Entity<InputState>,
    amend: bool,
    signoff: bool,
    more: bool,
    /// Set by the view: what the button says and whether there is anything to commit.
    branch: Option<SharedString>,
    staged: usize,
    busy: bool,
    error: Option<SharedString>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<CommitBoxEvent> for CommitBox {}

impl CommitBox {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let mut _subs = vec![cx.subscribe_in(
            &summary,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.submit(false, window, cx),
                _ => {}
            },
        )];
        _subs.push(
            cx.subscribe_in(&body, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    this.submit(false, window, cx)
                }
            }),
        );
        Self {
            summary,
            body,
            author,
            co_authors,
            amend: false,
            signoff: false,
            more: false,
            branch: None,
            staged: 0,
            busy: false,
            error: None,
            _subs,
        }
    }

    pub fn set_context(
        &mut self,
        branch: Option<SharedString>,
        staged: usize,
        cx: &mut Context<Self>,
    ) {
        if self.branch != branch || self.staged != staged {
            self.branch = branch;
            self.staged = staged;
            cx.notify();
        }
    }

    pub fn set_busy(&mut self, busy: bool, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.busy = busy;
        self.error = error;
        cx.notify();
    }

    /// Fills summary + body from HEAD's message, unless the user already typed one.
    pub fn prefill(&mut self, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.amend || !self.summary.read(cx).value().trim().is_empty() {
            return;
        }
        let (summary, body) = message.split_once('\n').unwrap_or((message, ""));
        self.summary.update(cx, |s, cx| {
            s.set_value(summary.trim().to_string(), window, cx)
        });
        self.body
            .update(cx, |s, cx| s.set_value(body.trim().to_string(), window, cx));
        cx.notify();
    }

    /// Clears the box after a successful commit.
    pub fn committed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.summary.update(cx, |s, cx| s.set_value("", window, cx));
        self.body.update(cx, |s, cx| s.set_value("", window, cx));
        self.amend = false;
        self.busy = false;
        self.error = None;
        cx.notify();
    }

    fn can_commit(&self, cx: &Context<Self>) -> bool {
        !self.busy
            && !self.summary.read(cx).value().trim().is_empty()
            && (self.staged > 0 || self.amend)
    }

    fn submit(&mut self, push: bool, _: &mut Window, cx: &mut Context<Self>) {
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
        self.error = None;
        cx.emit(CommitBoxEvent::Commit { request, push });
    }
}

impl Render for CommitBox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let can_commit = self.can_commit(cx);
        let target = self
            .branch
            .clone()
            .unwrap_or_else(|| "detached HEAD".into());
        let label = if self.amend {
            format!("Amend {target}")
        } else {
            format!("Commit to {target}")
        };
        v_flex()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(palette.border)
            .bg(palette.panel)
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
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.amend = *checked;
                                if *checked {
                                    cx.emit(CommitBoxEvent::WantsHeadMessage);
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
                        Button::new("commit-more")
                            .ghost()
                            .xsmall()
                            .label(if self.more { "Less" } else { "Author…" })
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
                            .loading(self.busy)
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
