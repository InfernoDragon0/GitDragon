//! The dialog that opens a pull request (merge request): base and compare branches, a title and
//! description prefilled from the commits between them, a Draft switch and, when the branch is
//! not on the remote yet, an offer to push it first. Emits the created pull request.

use ::hosts::{HostVocab, NewPullRequest, PullRequest};
use bstr::ByteSlice as _;
use git_core::{GitRepo as _, LogQuery, PushOptions};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _,
};
use ui::Palette;

use super::logic::{self, PushOffer};
use crate::hosted::Session;
use crate::repo_tab::RepoTab;

/// Most commits read for the prefill.
const PREFILL_COMMITS: usize = 100;

pub enum NewPrEvent {
    Created(Box<PullRequest>),
    Dismiss,
}

impl EventEmitter<NewPrEvent> for NewPrForm {}

pub struct NewPrForm {
    tab: Entity<RepoTab>,
    session: Session,
    vocab: &'static HostVocab,
    base: Option<String>,
    compare: Option<String>,
    /// Branches of the remote (base choices) and local branches (compare choices).
    bases: Vec<String>,
    locals: Vec<String>,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    draft: bool,
    offer: PushOffer,
    push: bool,
    /// What the prefill last wrote; text the user changed is not overwritten.
    last_prefill: (String, String),
    /// What the form is waiting for (`Pushing feat/x`, `Creating`); inputs lock meanwhile.
    busy: Option<SharedString>,
    error: Option<SharedString>,
    prefill_task: Option<Task<()>>,
    create_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl NewPrForm {
    pub fn new(
        tab: Entity<RepoTab>,
        session: Session,
        vocab: &'static HostVocab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (refs, current) = {
            let t = tab.read(cx);
            (
                t.refs.clone(),
                t.status.as_ref().and_then(|s| s.branch.head.clone()),
            )
        };
        let bases = logic::remote_branches(&refs, &session.remote);
        let locals = logic::local_branches(&refs);
        let base = logic::default_base(&refs, &session.remote, current.as_deref())
            .or_else(|| bases.first().cloned());
        let compare = current.or_else(|| locals.first().cloned());
        let offer = compare
            .as_deref()
            .map_or(PushOffer::No, |c| logic::push_offer(&refs, c));
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Title (required)"));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(5, 12)
                .placeholder("Description")
        });
        let _subs = vec![cx.subscribe(&title, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        let mut form = Self {
            tab,
            session,
            vocab,
            base,
            compare,
            bases,
            locals,
            title,
            body,
            draft: false,
            offer,
            push: offer != PushOffer::No,
            last_prefill: Default::default(),
            busy: None,
            error: None,
            prefill_task: None,
            create_task: None,
            _subs,
        };
        form.reload_prefill(window, cx);
        form
    }

    fn set_base(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.base = Some(name);
        self.reload_prefill(window, cx);
        cx.notify();
    }

    fn set_compare(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let refs = self.tab.read(cx).refs.clone();
        self.offer = logic::push_offer(&refs, &name);
        self.push = self.offer != PushOffer::No;
        self.compare = Some(name);
        self.reload_prefill(window, cx);
        cx.notify();
    }

    /// Reads the commits `base..compare` and, where the user has not typed their own text,
    /// fills in the title and description.
    fn reload_prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(base), Some(compare)) = (self.base.clone(), self.compare.clone()) else {
            return;
        };
        let Some(repo) = self.tab.read(cx).repo.clone() else {
            return;
        };
        let refs = self.tab.read(cx).refs.clone();
        let range = format!(
            "{}..{compare}",
            logic::base_rev(&refs, &self.session.remote, &base)
        );
        self.prefill_task = Some(cx.spawn_in(window, async move |this, cx| {
            let commits = cx
                .background_executor()
                .spawn(async move {
                    repo.log(LogQuery {
                        revs: vec![range],
                        topo_order: false,
                        limit: Some(PREFILL_COMMITS),
                        ..LogQuery::default()
                    })
                    .await
                })
                .await;
            let Ok(mut commits) = commits else { return };
            // Newest first from git; merges only add noise unless that is all there is.
            if commits.iter().any(|c| c.parents.len() < 2) {
                commits.retain(|c| c.parents.len() < 2);
            }
            commits.reverse();
            let pairs: Vec<(String, String)> = commits
                .iter()
                .map(|c| {
                    (
                        c.subject.to_str_lossy().into_owned(),
                        c.body.to_str_lossy().into_owned(),
                    )
                })
                .collect();
            let (title, body) = logic::prefill(&pairs);
            _ = this.update_in(cx, |this, window, cx| {
                this.apply_prefill(title, body, window, cx)
            });
        }));
    }

    fn apply_prefill(
        &mut self,
        title: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let untouched = |current: &str, last: &str| current.trim().is_empty() || current == last;
        let (cur_title, cur_body) = (
            self.title.read(cx).value().to_string(),
            self.body.read(cx).value().to_string(),
        );
        if untouched(&cur_title, &self.last_prefill.0) {
            self.title
                .update(cx, |s, cx| s.set_value(title.clone(), window, cx));
        }
        if untouched(&cur_body, &self.last_prefill.1) {
            self.body
                .update(cx, |s, cx| s.set_value(body.clone(), window, cx));
        }
        self.last_prefill = (title, body);
        cx.notify();
    }

    fn valid(&self, cx: &Context<Self>) -> Result<(), &'static str> {
        let (Some(base), Some(compare)) = (&self.base, &self.compare) else {
            return Err("Pick the base and compare branches.");
        };
        if base == compare {
            return Err("Base and compare are the same branch.");
        }
        if self.title.read(cx).value().trim().is_empty() {
            return Err("A title is required.");
        }
        Ok(())
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        if let Err(message) = self.valid(cx) {
            self.error = Some(message.into());
            cx.notify();
            return;
        }
        let (Some(base), Some(compare)) = (self.base.clone(), self.compare.clone()) else {
            return;
        };
        let body = self.body.read(cx).value().trim().to_string();
        let new = NewPullRequest {
            title: self.title.read(cx).value().trim().to_string(),
            body: (!body.is_empty()).then_some(body),
            source_branch: compare.clone(),
            target_branch: base,
            draft: self.draft,
        };
        self.error = None;
        if !(self.push && self.offer != PushOffer::No) {
            return self.create(new, cx);
        }
        self.busy = Some(format!("Pushing {compare}\u{2026}").into());
        let remote = self.session.remote.clone();
        let opts = PushOptions {
            set_upstream: self.offer == PushOffer::NoUpstream,
            force_with_lease: false,
        };
        let form = cx.entity().downgrade();
        let started = self.tab.update(cx, |tab, cx| {
            tab.run_op_with(
                format!("Pushing {compare}"),
                cx,
                move |repo| async move { repo.push_branch(&remote, &compare, opts).await },
                move |_, result, cx| {
                    _ = form.update(cx, |form, cx| match result {
                        Ok(()) => form.create(new, cx),
                        Err(err) => {
                            form.busy = None;
                            form.error = Some(format!("Push failed: {err}").into());
                            cx.notify();
                        }
                    });
                },
            )
        });
        if !started {
            self.busy = None;
            self.error = Some("Another operation is running; try again in a moment.".into());
        }
        cx.notify();
    }

    fn create(&mut self, new: NewPullRequest, cx: &mut Context<Self>) {
        self.busy = Some(
            format!(
                "Creating the {}\u{2026}",
                self.vocab.pull_request.to_lowercase()
            )
            .into(),
        );
        let (host, repo) = (self.session.host.clone(), self.session.repo.clone());
        self.create_task = Some(cx.spawn(async move |this, cx| {
            let result = host.create_pull_request(&repo, &new).await;
            _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(pr) => cx.emit(NewPrEvent::Created(Box::new(pr))),
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn branch_picker(
        &self,
        id: &'static str,
        current: &Option<String>,
        names: Vec<String>,
        pick: fn(&mut Self, String, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        Button::new(id)
            .small()
            .outline()
            .icon(gpui_kit::assets::IconName::GitBranch)
            .label(SharedString::from(
                current.clone().unwrap_or_else(|| "Choose\u{2026}".into()),
            ))
            .disabled(self.busy.is_some() || names.is_empty())
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for name in &names {
                    let (this, picked) = (this.clone(), name.clone());
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(name.clone())).on_click(
                            move |_, window, cx| {
                                let picked = picked.clone();
                                _ = this.update(cx, |form, cx| pick(form, picked, window, cx));
                            },
                        ),
                    );
                }
                menu
            })
    }
}

impl Render for NewPrForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let busy = self.busy.is_some();
        let label = |text: &'static str| div().text_xs().text_color(palette.muted).child(text);
        let remote = self.session.remote.clone();
        let compare_name = self.compare.clone().unwrap_or_default();
        let push_text = match self.offer {
            PushOffer::NoUpstream => Some(format!(
                "Push {compare_name} to {remote} first (it is not there yet)"
            )),
            PushOffer::Ahead(n) => Some(format!(
                "Push {n} unpushed commit{} to {remote} first",
                if n == 1 { "" } else { "s" }
            )),
            PushOffer::No => None,
        };
        let create_label = format!("Create {}", self.vocab.pull_request.to_lowercase());
        let bases = self.bases.clone();
        let locals = self.locals.clone();
        let base = self.base.clone();
        let compare = self.compare.clone();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(label("Base"))
                    .child(self.branch_picker("newpr-base", &base, bases, Self::set_base, cx))
                    .child(label("\u{2190} compare"))
                    .child(self.branch_picker(
                        "newpr-compare",
                        &compare,
                        locals,
                        Self::set_compare,
                        cx,
                    )),
            )
            .child(Input::new(&self.title).disabled(busy))
            .child(Textarea::new(&self.body).disabled(busy))
            .child(
                Checkbox::new("newpr-draft")
                    .label("Create as draft")
                    .checked(self.draft)
                    .disabled(busy)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.draft = *checked;
                        cx.notify();
                    })),
            )
            .when_some(push_text, |d, text| {
                d.child(
                    Checkbox::new("newpr-push")
                        .label(SharedString::from(text))
                        .checked(self.push)
                        .disabled(busy)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.push = *checked;
                            cx.notify();
                        })),
                )
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(div().text_sm().text_color(palette.error).child(error))
            })
            .when_some(self.busy.clone(), |d, text| {
                d.child(div().text_sm().text_color(palette.muted).child(text))
            })
            .child(
                h_flex()
                    .pt_2()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("newpr-cancel")
                            .ghost()
                            .label("Cancel")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.emit(NewPrEvent::Dismiss)
                                }),
                            ),
                    )
                    .child(
                        Button::new("newpr-create")
                            .primary()
                            .label(SharedString::from(create_label))
                            .loading(busy)
                            .disabled(busy || self.valid(cx).is_err())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit(cx))),
                    ),
            )
    }
}
