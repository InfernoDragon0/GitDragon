//! Clone dialog: URL + destination folder, live progress, cancel (kills git and removes the
//! half-cloned folder).

use std::path::PathBuf;

use git_core::setup;
use git_core::{GitRunner, Progress};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::{Disableable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Subscription, Task,
    Window, div,
};
use ui::Palette;

pub enum CloneEvent {
    Cloned(PathBuf),
    /// Close requested while idle.
    Dismiss,
}

enum Phase {
    Editing { error: Option<SharedString> },
    Cloning { progress: Option<Progress> },
}

enum Msg {
    Progress(Progress),
    Done(git_core::Result<PathBuf>),
}

pub struct CloneDialog {
    url: Entity<InputState>,
    parent: Entity<InputState>,
    phase: Phase,
    runner: GitRunner,
    /// Holds the running clone; dropping it cancels.
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<CloneEvent> for CloneDialog {}

impl CloneDialog {
    pub fn new(
        runner: GitRunner,
        parent_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = cx
            .new(|cx| InputState::new(window, cx).placeholder("https://github.com/owner/repo.git"));
        let parent = cx
            .new(|cx| InputState::new(window, cx).default_value(parent_dir.display().to_string()));
        let _subs =
            [&url, &parent]
                .map(|input| {
                    cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Change => cx.notify(),
                            InputEvent::PressEnter { .. } => this.start(window, cx),
                            _ => {}
                        }
                    })
                })
                .into();
        url.update(cx, |state, cx| state.focus(window, cx));
        Self {
            url,
            parent,
            phase: Phase::Editing { error: None },
            runner,
            task: None,
            _subs,
        }
    }

    fn url_text(&self, cx: &Context<Self>) -> String {
        self.url.read(cx).value().trim().to_string()
    }

    /// Destination folder the clone would create, if the inputs make sense.
    fn destination(&self, cx: &Context<Self>) -> Option<PathBuf> {
        let name = setup::default_dir_name(&self.url_text(cx))?;
        let parent = self.parent.read(cx).value().trim().to_string();
        (!parent.is_empty()).then(|| PathBuf::from(parent).join(name))
    }

    /// The folder the user cloned into, to suggest next time.
    pub fn parent_dir(&self, cx: &App) -> PathBuf {
        PathBuf::from(self.parent.read(cx).value().trim().to_string())
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone into".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                this.parent.update(cx, |state, cx| {
                    state.set_value(dir.display().to_string(), window, cx)
                });
            });
        })
        .detach();
    }

    fn start(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Cloning { .. }) {
            return;
        }
        let url = self.url_text(cx);
        if let Err(err) = setup::validate_clone_url(&url) {
            return self.fail(err.to_string(), cx);
        }
        let Some(dest) = self.destination(cx) else {
            return self.fail("Choose a folder to clone into.".into(), cx);
        };

        let (tx, rx) = async_channel::unbounded::<Msg>();
        let runner = self.runner.clone();
        let job = cx.background_executor().spawn(async move {
            let progress_tx = tx.clone();
            let result = setup::clone(&runner, &url, &dest, move |p| {
                _ = progress_tx.try_send(Msg::Progress(p));
            })
            .await;
            _ = tx.try_send(Msg::Done(result));
        });
        self.phase = Phase::Cloning { progress: None };
        self.task = Some(cx.spawn(async move |this, cx| {
            // Owning `job` here ties the clone's lifetime to this task.
            let _job = job;
            while let Ok(msg) = rx.recv().await {
                let done = matches!(msg, Msg::Done(_));
                let alive = this.update(cx, |this, cx| {
                    match msg {
                        Msg::Progress(p) => this.phase = Phase::Cloning { progress: Some(p) },
                        Msg::Done(Ok(path)) => {
                            this.phase = Phase::Editing { error: None };
                            cx.emit(CloneEvent::Cloned(path));
                        }
                        Msg::Done(Err(err)) => {
                            this.phase = Phase::Editing {
                                error: Some(err.to_string().into()),
                            }
                        }
                    }
                    cx.notify();
                });
                if done || alive.is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.task.take().is_some() && matches!(self.phase, Phase::Cloning { .. }) {
            self.phase = Phase::Editing {
                error: Some("Clone cancelled.".into()),
            };
            cx.notify();
        } else {
            cx.emit(CloneEvent::Dismiss);
        }
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.phase = Phase::Editing {
            error: Some(message.into()),
        };
        cx.notify();
    }
}

impl Render for CloneDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let cloning = matches!(self.phase, Phase::Cloning { .. });
        let dest = self.destination(cx);
        let label = |text: &'static str| div().text_xs().text_color(palette.muted).child(text);

        let status = match &self.phase {
            Phase::Editing { error: Some(err) } => Some(
                div()
                    .text_sm()
                    .text_color(palette.error)
                    .child(err.clone())
                    .into_any_element(),
            ),
            Phase::Editing { error: None } => None,
            Phase::Cloning { progress } => {
                let (text, value) = match progress {
                    Some(p) => (p.line.clone(), p.percent.map_or(0., f32::from)),
                    None => ("Starting…".to_string(), 0.),
                };
                Some(
                    v_flex()
                        .gap_1()
                        .child(ProgressBar::new("clone-progress").value(value))
                        .child(
                            div()
                                .text_xs()
                                .text_color(palette.muted)
                                .truncate()
                                .child(text),
                        )
                        .into_any_element(),
                )
            }
        };

        v_flex()
            .gap_3()
            .child(label("Repository URL"))
            .child(Input::new(&self.url).disabled(cloning))
            .child(label("Clone into"))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.parent).disabled(cloning)),
                    )
                    .child(
                        Button::new("clone-browse")
                            .outline()
                            .label("Browse…")
                            .disabled(cloning)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.browse(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .truncate()
                    .child(match &dest {
                        Some(dest) => format!("Creates {}", dest.display()),
                        None => "Enter a URL to see the folder name.".into(),
                    }),
            )
            .children(status)
            .child(
                h_flex()
                    .pt_2()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("clone-cancel")
                            .ghost()
                            .label(if cloning { "Stop" } else { "Cancel" })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Button::new("clone-go")
                            .primary()
                            .label("Clone")
                            .loading(cloning)
                            .disabled(cloning || dest.is_none())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.start(window, cx)
                            })),
                    ),
            )
    }
}
