//! The dialog that opens an issue (work item): title, description and comma separated labels.
//! Emits the created issue.

use ::hosts::{HostVocab, Issue, NewIssue};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _,
};
use ui::Palette;

use crate::hosted::Session;

pub enum NewIssueEvent {
    Created(Box<Issue>),
    Dismiss,
}

impl EventEmitter<NewIssueEvent> for NewIssueForm {}

pub struct NewIssueForm {
    session: Session,
    vocab: &'static HostVocab,
    title: Entity<InputState>,
    labels: Entity<InputState>,
    body: Entity<TextareaState>,
    busy: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// `bug, ui ,, ` as `["bug", "ui"]`.
pub fn split_labels(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in text.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        if !out.iter().any(|have| have.eq_ignore_ascii_case(label)) {
            out.push(label.to_string());
        }
    }
    out
}

impl NewIssueForm {
    pub fn new(
        session: Session,
        vocab: &'static HostVocab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Title (required)"));
        let labels =
            cx.new(|cx| InputState::new(window, cx).placeholder("Labels, comma separated"));
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
        title.update(cx, |state, cx| state.focus(window, cx));
        Self {
            session,
            vocab,
            title,
            labels,
            body,
            busy: false,
            error: None,
            task: None,
            _subs,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_string();
        if self.busy || title.is_empty() {
            return;
        }
        let body = self.body.read(cx).value().trim().to_string();
        let new = NewIssue {
            title,
            body: (!body.is_empty()).then_some(body),
            labels: split_labels(self.labels.read(cx).value().as_ref()),
        };
        self.busy = true;
        self.error = None;
        let (host, repo) = (self.session.host.clone(), self.session.repo.clone());
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = host.create_issue(&repo, &new).await;
            _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(issue) => cx.emit(NewIssueEvent::Created(Box::new(issue))),
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for NewIssueForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let busy = self.busy;
        let empty = self.title.read(cx).value().trim().is_empty();
        v_flex()
            .gap_3()
            .child(Input::new(&self.title).disabled(busy))
            .child(Textarea::new(&self.body).disabled(busy))
            .child(Input::new(&self.labels).disabled(busy))
            .when_some(self.error.clone(), |d, error| {
                d.child(div().text_sm().text_color(palette.error).child(error))
            })
            .child(
                h_flex()
                    .pt_2()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("newissue-cancel")
                            .ghost()
                            .label("Cancel")
                            .disabled(busy)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(NewIssueEvent::Dismiss)
                            })),
                    )
                    .child(
                        Button::new("newissue-create")
                            .primary()
                            .label(SharedString::from(format!(
                                "Create {}",
                                self.vocab.issue.to_lowercase()
                            )))
                            .loading(busy)
                            .disabled(busy || empty)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_split_trimmed_and_deduplicated() {
        assert_eq!(split_labels("bug, ui ,, Bug"), ["bug", "ui"]);
        assert!(split_labels("  ,").is_empty());
        assert_eq!(split_labels("good first issue"), ["good first issue"]);
    }
}
