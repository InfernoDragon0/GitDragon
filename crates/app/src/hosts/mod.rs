//! The pull request (merge request) and issue (work item) panels. Everything goes through the
//! `GitHost` trait: a host that lacks a feature gets that feature hidden, and the words come
//! from its `HostVocab`. Each repository tab gets its own views, created on first visit.
//!
//! - [`prs`]: list, detail, checkout, CI dots.
//! - [`new_pr`]: the create dialog (branches, prefilled text, push first).
//! - [`issues`]: list, detail, comment, close / reopen.
//! - [`new_issue`]: the create dialog.
//! - [`logic`]: the pure parts (list state, filters, prefill, checkout plan), unit tested.
//! - [`page`]: the full-panel page an opened issue / pull request replaces the list with.
//! - [`conversation`]: its timeline, reviews and inline comments, each loading on its own.

pub mod changes;
pub mod conversation;
pub mod issues;
pub mod logic;
pub mod new_issue;
pub mod new_pr;
pub mod page;
pub mod prs;

use ::hosts::{Feature, Label, RepoId};
use gpui_kit::component::button::Button;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
    px, rgb,
};
use ui::Palette;

pub use issues::IssuesView;
pub use prs::PrsView;

use crate::hosted::{HostedRepo, Resolution, Session};
use crate::repo_tab::RepoTab;
use logic::ListError;

/// How the view stands with the repository's hosting service.
pub enum Link {
    Resolving,
    Ready(Session),
    Empty(Empty),
}

/// Why there is nothing to list, in words for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Empty {
    NoRemote,
    /// A hosted remote but no signed-in account. `service` is empty when the host is not a
    /// known service.
    NoAccount {
        service: &'static str,
        host: String,
    },
    /// A plain git server account: no API.
    NoApi {
        service: &'static str,
    },
    /// The host has no such feature.
    Unsupported {
        service: &'static str,
        what: &'static str,
    },
    Failed(String),
}

impl Empty {
    /// `(title, body)`; `noun` is the host's plural for the thing (`merge requests`).
    pub fn text(&self, title: &str, noun: &str) -> (String, String) {
        let body = match self {
            Empty::NoRemote => format!("Add a remote to this repository to see its {noun}."),
            Empty::NoAccount { service: "", host } => {
                format!("Add an account for {host} in Settings \u{203a} Accounts to see {noun}.")
            }
            Empty::NoAccount { service, .. } => {
                format!("Sign in to {service} in Settings \u{203a} Accounts to see {noun}.")
            }
            Empty::NoApi { .. } => format!(
                "This account is a plain git server without an API, so there are no {noun} to show."
            ),
            Empty::Unsupported { service, what } => {
                format!("{service} does not offer {what} through GitDragon.")
            }
            Empty::Failed(message) => message.clone(),
        };
        (title.to_string(), body)
    }

    /// Does Settings > Accounts help?
    pub fn wants_accounts(&self) -> bool {
        matches!(self, Empty::NoAccount { .. } | Empty::Failed(_))
    }
}

/// Resolves the tab's host in the background (the keychain can block) and hands the outcome to
/// `done`. `feature` is what the view needs; a host without it becomes [`Empty::Unsupported`].
pub fn resolve<V: 'static>(
    tab: &Entity<RepoTab>,
    feature: Feature,
    cx: &mut Context<V>,
    done: impl FnOnce(&mut V, Link, &mut Context<V>) + 'static,
) -> Task<()> {
    let (has_remotes, hosted) = {
        let t = tab.read(cx);
        (!t.remotes.is_empty(), HostedRepo::of_tab(t, cx))
    };
    let Some(hosted) = hosted else {
        let empty = if has_remotes {
            Empty::Failed("The saved accounts could not be loaded.".into())
        } else {
            Empty::NoRemote
        };
        // Not loaded yet (no remotes seen) or really none: either way show the empty state.
        return cx.spawn(async move |this, cx| {
            _ = this.update(cx, |view, cx| done(view, Link::Empty(empty), cx));
        });
    };
    cx.spawn(async move |this, cx| {
        let found = cx
            .background_executor()
            .spawn(async move { hosted.resolve() })
            .await;
        _ = this.update(cx, |view, cx| done(view, link_of(found, feature), cx));
    })
}

fn link_of(found: Resolution, feature: Feature) -> Link {
    match found {
        Resolution::Ready(session) if !session.host.supports(feature) => {
            Link::Empty(Empty::Unsupported {
                service: session.host.vocab().service,
                what: feature.label(),
            })
        }
        Resolution::Ready(session) => Link::Ready(session),
        Resolution::NoRemote => Link::Empty(Empty::NoRemote),
        Resolution::NoAccount(repo) => Link::Empty(no_account(&repo)),
        Resolution::NoApi(service) => Link::Empty(Empty::NoApi { service }),
        Resolution::Failed(message) => Link::Empty(Empty::Failed(message)),
    }
}

fn no_account(repo: &RepoId) -> Empty {
    let kind = repo.kind();
    let service = ::hosts::HostVocab::of(kind).service;
    Empty::NoAccount {
        service: if kind == accounts::HostKind::Generic {
            ""
        } else {
            service
        },
        host: repo.host.clone(),
    }
}

/// `owner/repo` for a page header's subtitle (Azure DevOps adds its project).
pub fn repo_slug(repo: &RepoId) -> String {
    match &repo.project {
        Some(project) => format!("{}/{}/{}", repo.owner, project, repo.repo),
        None => format!("{}/{}", repo.owner, repo.repo),
    }
}

/// The friendly empty state: what is missing, and a way to Settings when that helps.
pub fn empty_panel(
    empty: &Empty,
    title: &str,
    noun: &str,
    palette: &Palette,
    id: &'static str,
    cx: &App,
) -> AnyElement {
    let (title, body) = empty.text(title, noun);
    v_flex()
        .id(id)
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4()
        .text_center()
        .bg(palette.panel)
        .child(ui::typography::page_title(title, cx))
        .child(
            div()
                .text_sm()
                .text_color(palette.muted)
                .child(SharedString::from(body)),
        )
        .when(empty.wants_accounts(), |d| {
            d.child(
                Button::new(SharedString::from(format!("{id}-accounts")))
                    .small()
                    .outline()
                    .label("Open Settings")
                    .on_click(|_: &ClickEvent, window, cx| {
                        window.dispatch_action(Box::new(crate::actions::ShowSettings), cx)
                    }),
            )
        })
        .into_any_element()
}

/// The inline error of a failed load, with Retry (or Settings for sign-in problems). The
/// list below keeps showing the last good data.
pub fn error_banner(
    error: &ListError,
    palette: &Palette,
    id: &'static str,
    on_retry: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    h_flex()
        .id(id)
        .flex_none()
        .w_full()
        .px_3()
        .py_1()
        .gap_2()
        .bg(palette.error.opacity(0.12))
        .border_b_1()
        .border_color(palette.error.opacity(0.5))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(palette.error)
                .child(SharedString::from(
                    ui::sanitize_display(&error.message).into_owned(),
                )),
        )
        .child(if error.auth {
            Button::new(SharedString::from(format!("{id}-settings")))
                .xsmall()
                .outline()
                .label("Accounts")
                .on_click(|_: &ClickEvent, window, cx| {
                    window.dispatch_action(Box::new(crate::actions::ShowSettings), cx)
                })
        } else {
            Button::new(SharedString::from(format!("{id}-retry")))
                .xsmall()
                .outline()
                .label("Retry")
                .on_click(move |_: &ClickEvent, window, cx| on_retry(window, cx))
        })
        .into_any_element()
}

/// A small filled dot.
pub fn dot(color: Hsla, size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(size))
        .h(px(size))
        .rounded_full()
        .bg(color)
}

/// A small rounded word on a tinted fill (state chips, labels).
pub fn pill(text: impl Into<SharedString>, color: Hsla, palette: &Palette) -> impl IntoElement {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.6))
        .bg(color.opacity(0.16))
        .text_xs()
        .text_color(palette.fg)
        .whitespace_nowrap()
        .child(text.into())
}

/// A host label as a pill in its own colour.
pub fn label_pill(label: &Label, palette: &Palette) -> impl IntoElement {
    pill(
        SharedString::from(ui::sanitize_line(&label.name).into_owned()),
        label_color(label.color.as_deref()).unwrap_or(palette.muted),
        palette,
    )
}

/// `d73a4a` (no `#`) as a colour.
pub fn label_color(hex: Option<&str>) -> Option<Hsla> {
    let hex = hex?.trim().trim_start_matches('#');
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
        .map(|value| rgb(value).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_states_name_the_service_and_the_thing() {
        let no_account = Empty::NoAccount {
            service: "GitHub",
            host: "github.com".into(),
        };
        let (title, body) = no_account.text("Pull requests", "pull requests");
        assert_eq!(title, "Pull requests");
        assert_eq!(
            body,
            "Sign in to GitHub in Settings \u{203a} Accounts to see pull requests."
        );
        let (_, body) = Empty::NoAccount {
            service: "",
            host: "git.corp".into(),
        }
        .text("x", "issues");
        assert!(body.starts_with("Add an account for git.corp"));
        let (_, body) = Empty::Unsupported {
            service: "Azure DevOps",
            what: "creating issues",
        }
        .text("x", "work items");
        assert!(body.contains("Azure DevOps") && body.contains("creating issues"));
        assert!(no_account.wants_accounts());
        assert!(!Empty::NoRemote.wants_accounts());
        assert!(
            Empty::NoRemote
                .text("x", "merge requests")
                .1
                .contains("merge requests")
        );
    }

    #[test]
    fn missing_accounts_name_the_guessed_service() {
        let repo = RepoId::parse("git@gitlab.com:group/proj.git").unwrap();
        assert!(matches!(
            no_account(&repo),
            Empty::NoAccount {
                service: "GitLab",
                ..
            }
        ));
        let repo = RepoId::parse("git@git.corp.example:team/proj.git").unwrap();
        assert!(matches!(
            no_account(&repo),
            Empty::NoAccount { service: "", .. }
        ));
    }

    #[test]
    fn label_colours() {
        assert!(label_color(Some("d73a4a")).is_some());
        assert!(label_color(Some("#d73a4a")).is_some());
        assert!(label_color(Some("nope")).is_none());
        assert!(label_color(None).is_none());
    }

    #[test]
    fn host_errors_become_inline_messages() {
        let err = ::hosts::HostError::RateLimited {
            host: "github.com".into(),
            until: None,
        };
        assert!(ListError::of(&err).message.contains("rate limit"));
    }
}
