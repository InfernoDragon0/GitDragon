//! The cards of the Settings page, one method per section of `settings_find::Section`. Each
//! asks the [`Filter`] which of its rows to draw and returns the whole card as one element, so
//! the page can scroll them as direct children.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClickEvent, ClipboardItem, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, SharedString, Styled as _, Window, div, px,
};
use ui::theme;
use ui::{Mode, Palette, Skin};

use crate::prefs::{self, CONTEXT_CHOICES, FETCH_CHOICES, PullPref, REFRESH_CHOICES};
use crate::settings::{SettingsView, choice, row};
use crate::settings_find::{Filter, Section};

/// A card under construction: only the rows the filter keeps.
pub(crate) struct Card<'a> {
    section: Section,
    filter: &'a Filter,
    palette: Palette,
    rows: Vec<AnyElement>,
}

impl<'a> Card<'a> {
    pub(crate) fn new(section: Section, filter: &'a Filter, palette: &Palette) -> Self {
        Self {
            section,
            filter,
            palette: *palette,
            rows: Vec::new(),
        }
    }

    /// A setting row, kept when `label` matches the filter.
    pub(crate) fn row(
        mut self,
        label: &'static str,
        state: impl Into<SharedString>,
        control: impl IntoElement,
    ) -> Self {
        if self.filter.row_visible(self.section, label) {
            self.rows.push(row(label, state, control, &self.palette));
        }
        self
    }

    /// Anything else (a list, a sub-card), kept when `label` matches the filter.
    pub(crate) fn element(mut self, label: &str, element: AnyElement) -> Self {
        if self.filter.row_visible(self.section, label) {
            self.rows.push(element);
        }
        self
    }

    pub(crate) fn finish(self) -> AnyElement {
        v_flex()
            .w_full()
            .p_4()
            .gap_1()
            .rounded(px(8.))
            .border_1()
            .border_color(self.palette.border)
            .bg(self.palette.panel)
            .child(
                div()
                    .pb_2()
                    .text_color(self.palette.emph)
                    .child(self.section.title()),
            )
            .children(self.rows)
            .into_any_element()
    }
}

fn on_off(
    id: &'static str,
    current: bool,
    set: impl Fn(bool, &mut gpui_kit::App) + Clone + 'static,
) -> AnyElement {
    choice(id, &[(false, "Off"), (true, "On")], current, set).into_any_element()
}

fn nothing() -> gpui_kit::Div {
    div()
}

fn duration_label(secs: u32) -> String {
    match secs {
        0 => "off".into(),
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

impl SettingsView {
    pub(crate) fn appearance_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let appearance = theme::appearance(cx);
        let zoom = theme::zoom(cx);
        Card::new(Section::Appearance, filter, palette)
            .row(
                "Theme",
                "Colour family for the whole app.",
                choice(
                    "skin",
                    &[(Skin::Aurora, "Aurora"), (Skin::Onyx, "Onyx")],
                    appearance.skin,
                    |skin, cx| {
                        let mut next = theme::appearance(cx);
                        next.skin = skin;
                        theme::set_appearance(next, cx);
                    },
                ),
            )
            .row(
                "Mode",
                "Ctrl+Shift+L switches anywhere.",
                choice(
                    "mode",
                    &[(Mode::Dark, "Dark"), (Mode::Light, "Light")],
                    appearance.mode,
                    |mode, cx| {
                        let mut next = theme::appearance(cx);
                        next.mode = mode;
                        theme::set_appearance(next, cx);
                    },
                ),
            )
            .row(
                "Zoom",
                format!(
                    "{}% · Ctrl+= bigger, Ctrl+- smaller, Ctrl+0 reset. Applies to text and lists.",
                    zoom.percent()
                ),
                h_flex()
                    .gap_2()
                    .w(px(280.))
                    .child(div().flex_1().child(Slider::new(&self.zoom_slider)))
                    .child(
                        div()
                            .w(px(40.))
                            .text_sm()
                            .text_right()
                            .text_color(palette.emph)
                            .child(format!("{}%", zoom.percent())),
                    )
                    .child(
                        Button::new("zoom-reset")
                            .ghost()
                            .small()
                            .label("Reset")
                            .on_click(|_: &ClickEvent, _, cx| {
                                theme::set_zoom(1.0, cx);
                            }),
                    ),
            )
            .finish()
    }

    pub(crate) fn git_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = prefs::get(cx);
        let (program, path) = match &self.git {
            Some(git) => (
                format!("{}\n{}", git.version_line, git.path.display()),
                Some(git.path.display().to_string()),
            ),
            None => ("Looking for git\u{2026}".to_string(), None),
        };
        let branch_state = match (&self.branch_error, prefs.initial_branch()) {
            (Some(err), _) => err.to_string(),
            (None, Some(name)) => format!("New repositories start on \"{name}\"."),
            (None, None) => "Empty: git's own default (init.defaultBranch) is used.".into(),
        };
        let signing = self
            .signing
            .clone()
            .unwrap_or_else(|| "Reading the git config\u{2026}".into());
        Card::new(Section::Git, filter, palette)
            .row(
                "Git program",
                program,
                Button::new("git-copy-path")
                    .outline()
                    .small()
                    .label("Copy path")
                    .on_click(move |_: &ClickEvent, _, cx| {
                        if let Some(path) = &path {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        }
                    }),
            )
            .row(
                "Default branch",
                branch_state,
                div().w(px(220.)).child(Input::new(&self.branch_input)),
            )
            .row(
                "Show git commands",
                "Show the git command being run in the strip at the bottom. Turn off when sharing \
                 your screen.",
                on_off("show-command", prefs.show_command, |on, cx| {
                    prefs::update(cx, |p| p.show_command = on)
                }),
            )
            .row(
                "Commit signing",
                format!(
                    "{} Read from your global git config; GitDragon never changes it.",
                    signing
                ),
                nothing(),
            )
            .finish()
    }

    pub(crate) fn editor_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = prefs::get(cx);
        let detected = if self.editors.is_empty() {
            "None found on PATH or in the usual install folders.".to_string()
        } else {
            self.editors
                .iter()
                .map(|(editor, path)| {
                    format!(
                        "{} \u{2014} {}",
                        crate::conflicts::external::label(*editor, path),
                        path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let custom_state = match (&self.editor_error, &prefs.external_editor) {
            (Some(err), _) => err.to_string(),
            (None, Some(path)) => format!("In use: {}", path.display()),
            (None, None) => "Optional. Full path to a program that opens a file given on its \
                             command line."
                .into(),
        };
        Card::new(Section::Editor, filter, palette)
            .row(
                "Detected editors",
                format!("{detected}\nUsed by \"Open in\u{2026}\" in the Conflicts view."),
                nothing(),
            )
            .row(
                "Custom editor",
                custom_state,
                h_flex()
                    .gap_1()
                    .child(div().w(px(260.)).child(Input::new(&self.editor_input)))
                    .child(
                        Button::new("editor-browse")
                            .outline()
                            .small()
                            .label("Browse\u{2026}")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.browse_editor(window, cx)
                            })),
                    ),
            )
            .finish()
    }

    /// File picker for the custom editor; the chosen path goes through the same validation as
    /// typing it.
    fn browse_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an editor".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                _ = this.update_in(cx, |this, window, cx| {
                    let text = path.display().to_string();
                    this.editor_input
                        .update(cx, |input, cx| input.set_value(text, window, cx));
                    this.apply_editor(cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn diff_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = prefs::get(cx);
        let choices: Vec<(u32, &'static str)> = CONTEXT_CHOICES
            .iter()
            .map(|n| {
                (
                    *n,
                    match n {
                        1 => "1",
                        3 => "3",
                        5 => "5",
                        10 => "10",
                        _ => "20",
                    },
                )
            })
            .collect();
        Card::new(Section::Diff, filter, palette)
            .row(
                "Context lines",
                "Unchanged lines shown around each change (git's default is 3). Applies to diffs \
                 opened from now on.",
                choice("context", &choices, prefs.context_lines, |n, cx| {
                    prefs::update(cx, |p| p.context_lines = n)
                }),
            )
            .row(
                "Ignore whitespace",
                "Hide changes that only add, remove or move spaces and tabs in commit diffs. \
                 Working-copy diffs always show them, so staging a hunk stays exact.",
                on_off("ignore-ws", prefs.ignore_whitespace, |on, cx| {
                    prefs::update(cx, |p| p.ignore_whitespace = on)
                }),
            )
            .finish()
    }

    pub(crate) fn pull_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = prefs::get(cx);
        let options = [
            (PullPref::Config, PullPref::Config.label()),
            (PullPref::Merge, PullPref::Merge.label()),
            (PullPref::Rebase, PullPref::Rebase.label()),
            (PullPref::FastForwardOnly, PullPref::FastForwardOnly.label()),
        ];
        Card::new(Section::PullPush, filter, palette)
            .row(
                "Pull strategy",
                format!(
                    "What the toolbar's Pull button does: {}. The arrow next to it still offers \
                     every mode.",
                    git_core::sync_ops::pull_preview(prefs.pull.mode())
                ),
                choice("pull-pref", &options, prefs.pull, |pull, cx| {
                    prefs::update(cx, |p| p.pull = pull)
                }),
            )
            .row(
                "Prune on fetch",
                "Fetch with --prune: remote-tracking branches deleted on the server disappear \
                 here too. Applies to Fetch and to background fetches.",
                on_off("prune", prefs.prune_on_fetch, |on, cx| {
                    prefs::update(cx, |p| p.prune_on_fetch = on)
                }),
            )
            .row(
                "Force push",
                "Always --force-with-lease, never a plain --force: git refuses if someone pushed \
                 to the branch since you last fetched. It asks before it runs.",
                nothing(),
            )
            .finish()
    }

    pub(crate) fn refresh_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefs = prefs::get(cx);
        let poll = prefs.poll;
        Card::new(Section::Refresh, filter, palette)
            .row(
                "Host refresh",
                format!(
                    "How often pull / merge requests, issues and checks reload while their view \
                     is open ({}). Also reloads when you come back to the window.",
                    duration_label(poll.refresh_secs)
                ),
                choice(
                    "refresh",
                    &REFRESH_CHOICES,
                    poll.refresh_secs,
                    |secs, cx| prefs::update(cx, |p| p.poll.refresh_secs = secs),
                ),
            )
            .row(
                "Background fetch",
                format!(
                    "Runs git fetch for every open repository that has a remote ({}). Waits \
                     for your own operations and backs off after failures.",
                    duration_label(poll.fetch_secs)
                ),
                choice("fetch-secs", &FETCH_CHOICES, poll.fetch_secs, |secs, cx| {
                    prefs::update(cx, |p| p.poll.fetch_secs = secs)
                }),
            )
            .row(
                "Window in the background",
                format!(
                    "Both timers run {} times slower while GitDragon is not the active window.",
                    poll.unfocused_factor
                ),
                nothing(),
            )
            .finish()
    }

    pub(crate) fn shortcuts_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        window: &mut Window,
    ) -> AnyElement {
        let mut card = Card::new(Section::Shortcuts, filter, palette);
        for (ix, (label, action)) in crate::actions::shortcut_actions().into_iter().enumerate() {
            let keys = crate::actions::keys_for(window, action.as_ref());
            let palette = *palette;
            let element = h_flex()
                .id(("shortcut", ix))
                .w_full()
                .py_1()
                .gap_4()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(palette.emph)
                        .child(label),
                )
                .child(match keys {
                    Some(keys) => div()
                        .px_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(palette.border)
                        .text_xs()
                        .text_color(palette.fg)
                        .child(SharedString::from(keys)),
                    None => div().text_xs().text_color(palette.muted).child("not bound"),
                })
                .into_any_element();
            card = card.element(label, element);
        }
        card.finish()
    }

    pub(crate) fn privacy_card(
        &mut self,
        filter: &Filter,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let gravatar = crate::avatars::handle(cx).is_some_and(|a| a.read(cx).gravatar());
        Card::new(Section::Privacy, filter, palette)
            .row(
                "Nothing is sent",
                "GitDragon has no telemetry, analytics or crash upload, and never will without \
                 an opt-in here. It only talks to what you point it at: your git remotes, the \
                 hosting accounts you sign in to, and (if you use it) Claude.",
                nothing(),
            )
            .row(
                "Gravatar",
                "Look up author pictures on gravatar.com. Sends a hash of each author's email \
                 there. GitHub noreply addresses always show their GitHub picture.",
                on_off("gravatar", gravatar, |on, cx| {
                    if let Some(avatars) = crate::avatars::handle(cx) {
                        avatars.update(cx, |a, cx| a.set_gravatar(on, cx));
                    }
                }),
            )
            .finish()
    }
}
