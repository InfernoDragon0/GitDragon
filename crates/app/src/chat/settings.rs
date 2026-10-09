//! Settings → AI: where the ACP adapter comes from (PATH, an absolute path, or opt-in npx) and the
//! Claude API key + model for the commit-message fallback. The key goes straight to the OS
//! keychain; it is never shown again, written to disk, or passed to the adapter.
//! Also the "Install MCP server" helper ([`super::mcp_install`]): registers `gitdragon-mcp` with
//! Claude Code / Claude Desktop after showing exactly what will be written.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use ui::Palette;

use super::mcp_install::{self, Client};
use super::{AiHub, AiSettings};

pub struct AiCard {
    hub: Entity<AiHub>,
    adapter_path: Entity<InputState>,
    api_key: Entity<InputState>,
    adapter_status: SharedString,
    /// `None` while checking the keychain.
    has_key: Option<bool>,
    key_error: Option<SharedString>,
    key_task: Option<Task<()>>,
    /// Repository the MCP server is installed for (prefilled with the active tab's).
    mcp_repo: Entity<InputState>,
    /// Add `--allow-writes` to the installed entry.
    mcp_writes: bool,
    mcp_status: Option<SharedString>,
    _subs: Vec<Subscription>,
}

impl AiCard {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hub = super::hub(cx);
        let current = hub.read(cx).settings.adapter_path.clone();
        let adapter_path = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .placeholder("Absolute path to claude-code-acp (optional)");
            if let Some(path) = current {
                state.set_value(path.display().to_string(), window, cx);
            }
            state
        });
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("sk-ant-\u{2026}")
        });
        let mcp_repo = cx.new(|cx| {
            let mut state =
                InputState::new(window, cx).placeholder("Absolute path to a repository");
            if let Some(path) = active_repo() {
                state.set_value(path.display().to_string(), window, cx);
            }
            state
        });
        let _subs = vec![cx.observe(&hub, |this, _, cx| {
            this.check_adapter(cx);
            cx.notify();
        })];
        let mut card = Self {
            hub,
            adapter_path,
            api_key,
            adapter_status: SharedString::default(),
            has_key: None,
            key_error: None,
            key_task: None,
            mcp_repo,
            mcp_writes: false,
            mcp_status: None,
            _subs,
        };
        card.check_adapter(cx);
        card.load_key(cx);
        card
    }

    fn check_adapter(&mut self, cx: &mut Context<Self>) {
        let lookup = self.hub.read(cx).settings.lookup();
        self.adapter_status = match ai::adapter::find(&lookup) {
            Ok(cmd) => {
                let how = match cmd.source {
                    ai::AdapterSource::Override => "from Settings",
                    ai::AdapterSource::Installed => "installed",
                    ai::AdapterSource::Npx => "via npx (downloads on first use)",
                };
                let target = cmd
                    .args
                    .first()
                    .filter(|_| cmd.source != ai::AdapterSource::Npx)
                    .map(|a| std::path::PathBuf::from(a).display().to_string())
                    .unwrap_or_else(|| cmd.program.display().to_string());
                format!("Found ({how}): {target}")
            }
            Err(err) => format!("{err}. Install: npm install -g @zed-industries/claude-code-acp"),
        }
        .into();
    }

    fn update_settings(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut AiSettings)) {
        let mut settings = self.hub.read(cx).settings.clone();
        f(&mut settings);
        self.hub
            .update(cx, |hub, cx| hub.set_settings(settings, cx));
    }

    fn save_adapter_path(&mut self, cx: &mut Context<Self>) {
        let text = self.adapter_path.read(cx).value().trim().to_string();
        let path = (!text.is_empty()).then(|| std::path::PathBuf::from(text));
        self.update_settings(cx, |s| s.adapter_path = path);
    }

    fn load_key(&mut self, cx: &mut Context<Self>) {
        let job = cx
            .background_executor()
            .spawn(async move { ai::claude_api::load_key().map(|k| k.is_some()) });
        self.key_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                match result {
                    Ok(has) => this.has_key = Some(has),
                    Err(err) => {
                        this.has_key = Some(false);
                        this.key_error = Some(err.into());
                    }
                }
                cx.notify();
            });
        }));
    }

    fn store_key(&mut self, clear: bool, window: &mut Window, cx: &mut Context<Self>) {
        let value = if clear {
            String::new()
        } else {
            self.api_key.read(cx).value().to_string()
        };
        if !clear && value.trim().is_empty() {
            return;
        }
        // The field never keeps the key.
        self.api_key.update(cx, |s, cx| s.set_value("", window, cx));
        self.has_key = None;
        self.key_error = None;
        let job = cx
            .background_executor()
            .spawn(async move { ai::claude_api::store_key(&value) });
        self.key_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.key_error = Some(err.into());
                }
                this.load_key(cx);
            });
        }));
        cx.notify();
    }
}

impl AiCard {
    /// Shows what would be written to `client`'s config; writes it only on confirmation.
    fn install_mcp(&mut self, client: Client, window: &mut Window, cx: &mut Context<Self>) {
        let repo = PathBuf::from(self.mcp_repo.read(cx).value().trim());
        let plan = (|| {
            let exe = mcp_install::server_exe()
                .ok_or("gitdragon-mcp was not found next to the GitDragon executable")?;
            if !repo.is_absolute() || !repo.join(".git").exists() {
                return Err("Enter the absolute path of a repository (a folder with .git)");
            }
            let path = client
                .config_path()
                .ok_or("Could not find the config folder")?;
            Ok((exe, path))
        })();
        let (exe, path) = match plan {
            Ok(plan) => plan,
            Err(err) => {
                self.mcp_status = Some(err.into());
                cx.notify();
                return;
            }
        };
        let entry = mcp_install::entry(client, &exe, &repo, self.mcp_writes);
        let body = mcp_install::preview(client, &path, &entry);
        let this = cx.entity().downgrade();
        crate::dialogs::confirm(
            window,
            cx,
            format!("Add GitDragon to {}?", client.label()),
            body,
            "Write config",
            self.mcp_writes,
            move |_, cx| {
                let status = match mcp_install::install(&path, &entry) {
                    Ok(backup) => format!(
                        "Added to {}{}. Restart it to load the server.",
                        client.label(),
                        backup
                            .map(|b| format!(" (backup: {})", b.display()))
                            .unwrap_or_default()
                    ),
                    Err(err) => format!("Not written: {err}"),
                };
                _ = this.update(cx, |this, cx| {
                    this.mcp_status = Some(status.into());
                    cx.notify();
                });
            },
        );
    }
}

/// The active tab's repository as last saved.
fn active_repo() -> Option<PathBuf> {
    let state = crate::state::SavedState::load();
    state.open_tabs.get(state.active_tab?).cloned()
}

fn row(
    name: &'static str,
    state: impl Into<SharedString>,
    control: impl IntoElement,
    palette: &Palette,
) -> AnyElement {
    h_flex()
        .w_full()
        .gap_4()
        .py_2()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().text_color(palette.emph).child(name))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(state.into()),
                ),
        )
        .child(control)
        .into_any_element()
}

impl Render for AiCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let settings = self.hub.read(cx).settings.clone();
        let model = settings.model();
        let key_state = match (&self.key_error, self.has_key) {
            (Some(err), _) => err.to_string(),
            (None, None) => "Checking the keychain\u{2026}".into(),
            (None, Some(true)) => "Stored in the system keychain. Used only for commit messages \
                                   when the adapter is not available."
                .into(),
            (None, Some(false)) => "Not set. Optional: used for commit messages when the \
                                    adapter is not available."
                .into(),
        };
        v_flex()
            .w_full()
            .p_4()
            .gap_1()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(div().pb_2().text_color(palette.emph).child("AI"))
            .child(row(
                "Claude Code adapter",
                self.adapter_status.clone(),
                Button::new("ai-recheck")
                    .outline()
                    .small()
                    .label("Check again")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.check_adapter(cx);
                        cx.notify();
                    })),
                &palette,
            ))
            .child(row(
                "Adapter path",
                "Only needed when claude-code-acp is not on PATH. Must be absolute.",
                h_flex()
                    .gap_1()
                    .w(px(360.))
                    .child(div().flex_1().child(Input::new(&self.adapter_path).small()))
                    .child(
                        Button::new("ai-path-save")
                            .outline()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.save_adapter_path(cx)
                            })),
                    ),
                &palette,
            ))
            .child(row(
                "Run with npx",
                "When no adapter is installed, run `npx --yes @zed-industries/claude-code-acp` \
                 (downloads and runs code from npm).",
                h_flex()
                    .gap_1()
                    .children(
                        [(false, "Off"), (true, "On")]
                            .into_iter()
                            .map(|(on, label)| {
                                let button =
                                    Button::new(("ai-npx", on as usize)).small().label(label);
                                let button = if settings.allow_npx == on {
                                    button.primary()
                                } else {
                                    button.outline()
                                };
                                button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.update_settings(cx, |s| s.allow_npx = on)
                                }))
                            }),
                    ),
                &palette,
            ))
            .child(row(
                "Claude API key",
                key_state,
                h_flex()
                    .gap_1()
                    .w(px(360.))
                    .child(div().flex_1().child(Input::new(&self.api_key).small()))
                    .child(
                        Button::new("ai-key-save")
                            .outline()
                            .small()
                            .label("Save")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.store_key(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-key-clear")
                            .ghost()
                            .small()
                            .label("Clear")
                            .disabled(self.has_key != Some(true))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.store_key(true, window, cx)
                            })),
                    ),
                &palette,
            ))
            .child(row(
                "API model",
                "Model for API commit messages.",
                h_flex()
                    .gap_1()
                    .children(ai::claude_api::MODELS.iter().enumerate().map(|(ix, id)| {
                        let label = id.trim_start_matches("claude-");
                        let button = Button::new(("ai-model", ix)).small().label(label);
                        let button = if model == *id {
                            button.primary()
                        } else {
                            button.outline()
                        };
                        let id = id.to_string();
                        button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let id = id.clone();
                            this.update_settings(cx, |s| s.model = Some(id))
                        }))
                    })),
                &palette,
            ))
            .child(row(
                "MCP server",
                self.mcp_status
                    .clone()
                    .unwrap_or_else(|| match mcp_install::server_exe() {
                        Some(_) => "Lets Claude Code / Claude Desktop read this repository's git \
                                    history through gitdragon-mcp (stdio; read-only unless write \
                                    tools are on)."
                            .into(),
                        None => "gitdragon-mcp was not found next to GitDragon.".into(),
                    }),
                h_flex()
                    .gap_1()
                    .w(px(360.))
                    .child(div().flex_1().child(Input::new(&self.mcp_repo).small())),
                &palette,
            ))
            .child(row(
                "MCP write tools",
                "Allow stage, unstage, commit and create branch (adds --allow-writes).",
                h_flex()
                    .gap_1()
                    .children(
                        [(false, "Off"), (true, "On")]
                            .into_iter()
                            .map(|(on, label)| {
                                let button = Button::new(("mcp-writes", on as usize))
                                    .small()
                                    .label(label);
                                let button = if self.mcp_writes == on {
                                    button.primary()
                                } else {
                                    button.outline()
                                };
                                button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.mcp_writes = on;
                                    cx.notify();
                                }))
                            }),
                    ),
                &palette,
            ))
            .child(row(
                "Install MCP server",
                "Shows the entry before writing; other servers are kept and the file is backed up.",
                h_flex().gap_1().children(
                    [Client::ClaudeCode, Client::ClaudeDesktop]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, client)| {
                            Button::new(("mcp-install", ix))
                                .outline()
                                .small()
                                .label(client.label())
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.install_mcp(client, window, cx)
                                }))
                        }),
                ),
                &palette,
            ))
    }
}
