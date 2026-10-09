//! The Accounts card on the Settings page: git's credential helper with its saved sign-ins
//! (link one for API calls, or sign in to Credential Manager), then the token / browser
//! accounts GitDragon keeps itself with an inline "add account" form.

use accounts::credential::SavedSignIn;
use accounts::oauth::{self, Canceller};
use accounts::{Account, AuthMode, HostKind, NewAccount, Origin, Secret};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, ElementId, Entity, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use ui::Palette;

use crate::dialogs;

pub struct AccountsCard {
    kind: HostKind,
    auth: AuthMode,
    url: Entity<InputState>,
    username: Entity<InputState>,
    token: Entity<InputState>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    adding: Option<Task<()>>,
    /// A running browser sign-in.
    sign_in: Option<SignIn>,
    /// Git's own credential helpers (system / global config); `None` while loading.
    system_helpers: Option<Vec<String>>,
    /// Sign-ins the system helper has saved (hosts + usernames, no secrets).
    saved: Vec<SavedSignIn>,
    /// The "Sign in…" picker under Credential Manager is open.
    gcm_open: bool,
    gcm_kind: HostKind,
    gcm_url: Entity<InputState>,
    /// A Credential Manager sign-in window is up.
    gcm_signing: Option<Task<()>>,
    _load_helpers: Task<()>,
}

/// Browser sign-in in progress: GitHub shows a code to type; GitLab waits for the redirect.
struct SignIn {
    /// (code, page) for the GitHub device flow.
    device: Option<(SharedString, SharedString)>,
    cancel: Option<Canceller>,
    _task: Task<()>,
}

impl AccountsCard {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kind = HostKind::GitHub;
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://github.com")
                .default_value(kind.default_url().unwrap_or_default())
        });
        let username =
            cx.new(|cx| InputState::new(window, cx).placeholder("Username on the server"));
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste a personal access token")
                .masked(true)
        });
        let load = cx.background_executor().spawn(async {
            let runner = git_core::GitRunner::detect().ok()?;
            // Outside any repository: only system and global settings count.
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let helpers = accounts::credential::configured_helpers(&runner, &home)
                .await
                .ok()?;
            Some((helpers, accounts::credential::saved_sign_ins()))
        });
        let _load_helpers = cx.spawn(async move |this, cx| {
            let (helpers, saved) = load.await.unwrap_or_default();
            _ = this.update(cx, |this, cx| {
                this.system_helpers = Some(helpers);
                this.saved = saved;
                cx.notify();
            });
        });
        let gcm_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://dev.azure.com/<organisation>")
                .default_value(kind.default_url().unwrap_or_default())
        });
        Self {
            system_helpers: None,
            saved: Vec::new(),
            gcm_open: false,
            gcm_kind: kind,
            gcm_url,
            gcm_signing: None,
            _load_helpers,
            kind,
            auth: AuthMode::OAuth,
            url,
            username,
            token,
            error: None,
            notice: None,
            adding: None,
            sign_in: None,
        }
    }

    fn set_kind(&mut self, kind: HostKind, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = kind;
        let url = kind.default_url().unwrap_or("https://").to_string();
        self.url
            .update(cx, |input, cx| input.set_value(url, window, cx));
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auth == AuthMode::OAuth {
            self.browser_sign_in(window, cx);
            return;
        }
        let Some(accounts) = super::accounts(cx) else {
            self.error = Some("Accounts are unavailable (no config folder).".into());
            cx.notify();
            return;
        };
        let new = NewAccount {
            kind: self.kind,
            base_url: self.url.read(cx).value().to_string(),
            auth: self.auth,
            username: Some(self.username.read(cx).value().to_string()),
            token: (self.auth == AuthMode::Pat)
                .then(|| Secret::new(self.token.read(cx).value().to_string())),
        };
        self.error = None;
        self.notice = None;
        let job = cx
            .background_executor()
            .spawn(async move { accounts.add(new).await });
        self.adding = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.adding = None;
                match result {
                    Ok(account) => {
                        this.notice = Some(format!("Added {}.", account.name).into());
                        this.token
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// GitHub (github.com): device flow. GitLab (gitlab.com): PKCE on the loopback port.
    fn browser_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(accounts) = super::accounts(cx) else {
            return;
        };
        let base = self
            .url
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let kind = self.kind;
        self.error = None;
        self.notice = None;
        let supported = matches!(
            (kind, base.as_str()),
            (HostKind::GitHub, "https://github.com") | (HostKind::GitLab, "https://gitlab.com")
        );
        if !supported {
            self.error = Some(
                "Browser sign-in works with github.com and gitlab.com; use a token for other servers."
                    .into(),
            );
            cx.notify();
            return;
        }
        let finish = |result: Result<Account, accounts::AddError>,
                      this: &mut Self,
                      cx: &mut Context<Self>| {
            this.sign_in = None;
            match result {
                Ok(account) => this.notice = Some(format!("Signed in as {}.", account.name).into()),
                Err(err) => this.error = Some(err.to_string().into()),
            }
            cx.notify();
        };
        if kind == HostKind::GitHub {
            let task = cx.spawn_in(window, async move |this, cx| {
                let code = cx
                    .background_executor()
                    .spawn(oauth::github_device_start())
                    .await;
                let code = match code {
                    Ok(code) => code,
                    Err(err) => {
                        _ = this.update(cx, |this, cx| {
                            finish(Err(accounts::AddError::OAuth(err)), this, cx)
                        });
                        return;
                    }
                };
                let page = code.verification_uri.clone();
                _ = this.update(cx, |this, cx| {
                    if let Some(sign_in) = &mut this.sign_in {
                        sign_in.device = Some((code.user_code.clone().into(), page.clone().into()));
                    }
                    cx.open_url(&page);
                    cx.notify();
                });
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let tokens = oauth::github_device_finish(code).await?;
                        accounts.add_oauth(HostKind::GitHub, &base, tokens).await
                    })
                    .await;
                _ = this.update(cx, |this, cx| finish(result, this, cx));
            });
            self.sign_in = Some(SignIn {
                device: None,
                cancel: None,
                _task: task,
            });
        } else {
            let login = match oauth::gitlab_start(&base) {
                Ok(login) => login,
                Err(err) => {
                    self.error = Some(err.to_string().into());
                    cx.notify();
                    return;
                }
            };
            let cancel = login.canceller();
            cx.open_url(&login.authorize_url);
            let task = cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let tokens = oauth::gitlab_finish(login).await?;
                        accounts.add_oauth(HostKind::GitLab, &base, tokens).await
                    })
                    .await;
                _ = this.update(cx, |this, cx| finish(result, this, cx));
            });
            self.sign_in = Some(SignIn {
                device: None,
                cancel: Some(cancel),
                _task: task,
            });
        }
        cx.notify();
    }

    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        if let Some(sign_in) = self.sign_in.take() {
            if let Some(cancel) = sign_in.cancel {
                cancel.cancel();
            }
            self.notice = Some("Sign-in cancelled.".into());
            cx.notify();
        }
    }

    fn sign_out(&mut self, account: Account, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        // Token and browser accounts only: Credential Manager links use `unlink`.
        let body = format!(
            "{} is removed and its token is deleted from the system keychain.",
            account.name
        );
        dialogs::confirm(
            window,
            cx,
            "Sign out?",
            body,
            "Sign out",
            true,
            move |_, cx| {
                let id = account.id.clone();
                _ = this.update(cx, |this, cx| {
                    if let Some(accounts) = super::accounts(cx) {
                        match accounts.remove(&id) {
                            Ok(()) => this.notice = Some("Signed out.".into()),
                            Err(err) => this.error = Some(err.to_string().into()),
                        }
                    }
                    cx.notify();
                });
            },
        );
    }

    /// Git's own credential helper: always in effect, for every host, before any account.
    fn render_system_helper(&self, palette: &Palette) -> AnyElement {
        let (title, detail, color): (String, String, gpui_kit::Hsla) = match &self.system_helpers {
            None => ("Checking git's credential helper\u{2026}".into(), String::new(), palette.muted),
            Some(helpers) if helpers.is_empty() => (
                "No git credential helper".into(),
                "Git asks nobody for passwords: add an account below, or install Git Credential Manager.".into(),
                palette.warn,
            ),
            Some(helpers) => {
                let labels: Vec<(String, bool)> = helpers
                    .iter()
                    .map(|h| accounts::credential::helper_label(h))
                    .collect();
                let names: Vec<&str> = labels.iter().map(|(name, _)| name.as_str()).collect();
                let plain = labels.iter().any(|(_, plain)| *plain);
                (
                    names.join(" + "),
                    if plain {
                        "Set in git's config; used for every host. Warning: it keeps passwords in a plain-text file.".into()
                    } else {
                        "Set in git's config; used for every host when git needs a sign-in.".into()
                    },
                    if plain { palette.warn } else { palette.lane(0) },
                )
            }
        };
        let chip = |text: &'static str| {
            div()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.14))
                .child(text)
        };
        h_flex()
            .w_full()
            .gap_3()
            .py_2()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(title)),
                    )
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(palette.muted)
                                .child(SharedString::from(detail)),
                        )
                    }),
            )
            .child(chip("System"))
            .into_any_element()
    }

    /// Is git's helper Git Credential Manager (which can open its own sign-in window)?
    fn has_gcm(&self) -> bool {
        self.system_helpers.as_ref().is_some_and(|helpers| {
            helpers
                .iter()
                .any(|h| accounts::credential::helper_label(h).0 == "Git Credential Manager")
        })
    }

    /// The GitDragon link for a saved sign-in: a Credential Manager account on the same host
    /// (and Azure organisation) with the same login, or one that has no login yet.
    fn linked<'a>(list: &'a [Account], saved: &SavedSignIn) -> Option<&'a Account> {
        list.iter().find(|a| {
            a.auth == AuthMode::Gcm
                && place_of(&a.base_url).is_some_and(|p| p.eq_ignore_ascii_case(&saved.place))
                && a.username
                    .as_deref()
                    .is_none_or(|u| u.eq_ignore_ascii_case(&saved.username))
        })
    }

    /// Lets GitDragon use a saved sign-in for API calls (PRs, issues, boards). Nothing secret
    /// is copied: it asks the helper each time.
    fn link(&mut self, saved: SavedSignIn, cx: &mut Context<Self>) {
        let Some(accounts) = super::accounts(cx) else {
            return;
        };
        let host = saved.place.split('/').next().unwrap_or_default();
        let new = NewAccount {
            kind: HostKind::of_host(host),
            base_url: format!("https://{}", saved.place),
            auth: AuthMode::Gcm,
            username: Some(saved.username.clone()),
            token: None,
        };
        self.error = None;
        // No network for Credential Manager links: only accounts.json is written.
        match accounts::net::block_on(accounts.add(new)) {
            Ok(account) => self.notice = Some(format!("Linked {}.", account.name).into()),
            Err(err) => self.error = Some(err.to_string().into()),
        }
        cx.notify();
    }

    fn unlink(&mut self, account: Account, cx: &mut Context<Self>) {
        if let Some(accounts) = super::accounts(cx) {
            match accounts.remove(&account.id) {
                Ok(()) => {
                    self.notice = Some(
                        format!(
                            "Unlinked {}. Credential Manager keeps its sign-in.",
                            account.name
                        )
                        .into(),
                    )
                }
                Err(err) => self.error = Some(err.to_string().into()),
            }
        }
        cx.notify();
    }

    fn set_gcm_kind(&mut self, kind: HostKind, window: &mut Window, cx: &mut Context<Self>) {
        self.gcm_kind = kind;
        let url = kind.default_url().unwrap_or("https://").to_string();
        self.gcm_url
            .update(cx, |input, cx| input.set_value(url, window, cx));
        cx.notify();
    }

    /// Asks Credential Manager to sign in now (its own window), then links the sign-in.
    fn gcm_sign_in(&mut self, cx: &mut Context<Self>) {
        let base = self
            .gcm_url
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let parsed = url::Url::parse(&base).ok();
        let (Some(origin), Some(parsed)) = (Origin::of(&base), parsed) else {
            self.error = Some(format!("Not a URL: {base}").into());
            cx.notify();
            return;
        };
        let path = parsed
            .path_segments()
            .and_then(|mut s| s.find(|p| !p.is_empty()).map(str::to_owned));
        if self.gcm_kind == HostKind::AzureDevOps
            && origin.host == "dev.azure.com"
            && path.is_none()
        {
            self.error = Some("Add your organisation: https://dev.azure.com/<organisation>".into());
            cx.notify();
            return;
        }
        let kind = self.gcm_kind;
        self.error = None;
        self.notice = None;
        let job = cx.background_executor().spawn(async move {
            let runner = git_core::GitRunner::detect().map_err(|e| e.to_string())?;
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let credential = accounts::credential::fill(&runner, &home, &origin, path.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            // Tell GCM it is good so it keeps it.
            _ = accounts::credential::report(
                &runner,
                &home,
                &origin,
                path.as_deref(),
                &credential,
                true,
            )
            .await;
            let saved = accounts::credential::saved_sign_ins();
            Ok::<_, String>((credential.username, saved))
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                this.gcm_signing = None;
                match result {
                    Ok((username, saved)) => {
                        this.saved = saved;
                        this.gcm_open = false;
                        let linked = super::accounts(cx).map(|accounts| {
                            accounts::net::block_on(accounts.add(NewAccount {
                                kind,
                                base_url: base,
                                auth: AuthMode::Gcm,
                                username: Some(username.clone()),
                                token: None,
                            }))
                        });
                        match linked {
                            Some(Err(err)) => this.error = Some(err.to_string().into()),
                            _ => {
                                this.notice = Some(
                                    format!("Credential Manager signed in as {username}.").into(),
                                )
                            }
                        }
                    }
                    Err(err) => this.error = Some(format!("Credential Manager: {err}").into()),
                }
                cx.notify();
            });
        });
        self.gcm_signing = Some(task);
        cx.notify();
    }

    /// The helper's saved sign-ins, each linkable for API calls, plus GitDragon links the
    /// helper no longer lists (or cannot list, off Windows), then "Sign in…".
    fn render_saved(
        &self,
        list: &[Account],
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let helpers = self.system_helpers.as_ref()?;
        let gcm_links: Vec<&Account> = list.iter().filter(|a| a.auth == AuthMode::Gcm).collect();
        if helpers.is_empty() && gcm_links.is_empty() {
            return None;
        }
        let kind_cell = |text: &'static str| {
            div()
                .w(px(96.))
                .flex_none()
                .text_color(palette.muted)
                .child(text)
        };
        let row = |kind: &'static str, place: String, user: String| {
            h_flex()
                .w_full()
                .gap_2()
                .text_xs()
                .child(kind_cell(kind))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .text_color(palette.fg)
                                .child(SharedString::from(place)),
                        )
                        .child(
                            div()
                                .text_color(palette.emph)
                                .child(SharedString::from(format!("\u{b7} {user}"))),
                        ),
                )
        };
        let linked_chip = || {
            div()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .text_color(palette.ok)
                .bg(palette.ok.opacity(0.14))
                .child("Linked")
        };

        let mut body = v_flex().gap_1();
        if self.saved.is_empty() {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child("No saved sign-ins yet: it asks the first time you push or fetch."),
            );
        }
        for (ix, saved) in self.saved.iter().enumerate() {
            let host = saved.place.split('/').next().unwrap_or_default();
            let kind = HostKind::of_host(host).label();
            let row = row(kind, saved.place.clone(), saved.username.clone());
            body = body.child(match Self::linked(list, saved) {
                Some(account) => {
                    let account = account.clone();
                    row.child(linked_chip()).child(
                        Button::new(ElementId::NamedInteger("gcm-unlink".into(), ix as u64))
                            .ghost()
                            .xsmall()
                            .label("Unlink")
                            .tooltip("Stop using it in GitDragon (Credential Manager keeps it)")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.unlink(account.clone(), cx)
                            })),
                    )
                }
                None => {
                    let saved = saved.clone();
                    row.child(
                        Button::new(ElementId::NamedInteger("gcm-link".into(), ix as u64))
                            .outline()
                            .xsmall()
                            .label("Link")
                            .tooltip("Use this sign-in for pull requests, issues and boards")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.link(saved.clone(), cx)
                            })),
                    )
                }
            });
        }
        // Links without a saved entry to show (removed from the store, or not Windows).
        for (ix, account) in gcm_links
            .into_iter()
            .filter(|a| {
                !self
                    .saved
                    .iter()
                    .any(|s| Self::linked(&[(*a).clone()], s).is_some())
            })
            .enumerate()
        {
            let account = account.clone();
            let place = place_of(&account.base_url).unwrap_or_else(|| account.base_url.clone());
            let user = account
                .username
                .clone()
                .unwrap_or_else(|| "asks at the first push".into());
            body = body.child(
                row(account.kind.label(), place, user)
                    .child(linked_chip())
                    .child(
                        Button::new(ElementId::NamedInteger(
                            "gcm-unlink-orphan".into(),
                            ix as u64,
                        ))
                        .ghost()
                        .xsmall()
                        .label("Unlink")
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.unlink(account.clone(), cx),
                        )),
                    ),
            );
        }

        if self.has_gcm() {
            let signing = self.gcm_signing.is_some();
            if self.gcm_open {
                let kinds: Vec<(HostKind, &'static str, bool)> = HostKind::ALL
                    .iter()
                    .map(|k| (*k, k.label(), true))
                    .collect();
                body = body.child(
                    v_flex()
                        .gap_2()
                        .pt_2()
                        .child(segmented(
                            "gcm-kind",
                            &kinds,
                            self.gcm_kind,
                            cx,
                            |this, kind, window, cx| this.set_gcm_kind(kind, window, cx),
                        ))
                        .child(Input::new(&self.gcm_url).small())
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("gcm-sign-in-go")
                                        .primary()
                                        .small()
                                        .label(if signing {
                                            "Waiting for the Credential Manager window\u{2026}"
                                        } else {
                                            "Open Credential Manager"
                                        })
                                        .disabled(signing)
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.gcm_sign_in(cx)
                                        })),
                                )
                                .when(!signing, |d| {
                                    d.child(
                                        Button::new("gcm-sign-in-close")
                                            .ghost()
                                            .small()
                                            .label("Cancel")
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, _, cx| {
                                                    this.gcm_open = false;
                                                    cx.notify();
                                                },
                                            )),
                                    )
                                }),
                        ),
                );
            } else {
                body = body.child(
                    div().pt_1().child(
                        Button::new("gcm-sign-in")
                            .outline()
                            .small()
                            .label("Sign in\u{2026}")
                            .tooltip("Sign in to a host with Credential Manager's own window")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.gcm_open = true;
                                cx.notify();
                            })),
                    ),
                );
            }
        }
        Some(
            body.pl_3()
                .pb_2()
                .ml_1()
                .border_l_2()
                .border_color(palette.border)
                .into_any_element(),
        )
    }

    fn render_account(
        &self,
        ix: usize,
        account: Account,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = |text: &'static str, color: gpui_kit::Hsla| {
            div()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.14))
                .child(text)
        };
        let auth = match account.auth {
            AuthMode::Gcm => chip("Credential Manager", palette.lane(0)),
            AuthMode::Pat => chip("Token in keychain", palette.ok),
            AuthMode::OAuth => chip("Signed in", palette.ok),
        };
        let status = format!("{} \u{b7} {}", account.kind.label(), account.base_url);
        h_flex()
            .w_full()
            .gap_3()
            .py_2()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(account.name.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(status)),
                    ),
            )
            .child(auth)
            .child(
                Button::new(ElementId::NamedInteger(
                    "account-sign-out".into(),
                    ix as u64,
                ))
                .ghost()
                .small()
                .label("Sign out")
                .tooltip("Delete the token from the keychain and remove the account")
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.sign_out(account.clone(), window, cx)
                })),
            )
            .into_any_element()
    }
}

/// `https://dev.azure.com/org` -> `dev.azure.com/org`, the shape of [`SavedSignIn::place`].
fn place_of(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(
        match url
            .path_segments()
            .and_then(|mut s| s.find(|p| !p.is_empty()))
        {
            Some(first) => format!("{host}/{}", first.to_ascii_lowercase()),
            None => host,
        },
    )
}

/// Segmented buttons, current one filled.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str, bool)],
    current: T,
    cx: &mut Context<AccountsCard>,
    on_pick: fn(&mut AccountsCard, T, &mut Window, &mut Context<AccountsCard>),
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(
            options
                .iter()
                .enumerate()
                .map(|(ix, &(value, label, enabled))| {
                    let button = Button::new((id, ix))
                        .small()
                        .label(label)
                        .disabled(!enabled);
                    let button = if value == current {
                        button.primary()
                    } else {
                        button.outline()
                    };
                    button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        on_pick(this, value, window, cx)
                    }))
                }),
        )
}

impl Render for AccountsCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let list = super::accounts(cx).map(|a| a.list()).unwrap_or_default();
        let busy = self.adding.is_some() || self.sign_in.is_some();
        // Leaving a host without browser sign-in falls back to a token.
        if self.auth == AuthMode::OAuth && !matches!(self.kind, HostKind::GitHub | HostKind::GitLab)
        {
            self.auth = AuthMode::Pat;
        }
        let field = |label: &'static str, input: AnyElement| {
            h_flex()
                .w_full()
                .gap_3()
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .text_sm()
                        .text_color(palette.fg)
                        .child(label),
                )
                .child(div().flex_1().child(input))
        };
        let kinds: Vec<(HostKind, &'static str, bool)> = HostKind::ALL
            .iter()
            .map(|k| (*k, k.label(), true))
            .collect();
        let hint: &'static str = match (self.auth, self.kind) {
            (AuthMode::Gcm, _) => {
                "Use \u{201c}Sign in\u{2026}\u{201d} under Credential Manager above."
            }
            (AuthMode::Pat, HostKind::GitHub) => {
                "Token scopes: repo, read:org (add project for Boards). Stored only in the system keychain."
            }
            (AuthMode::Pat, HostKind::GitLab) => {
                "Token scopes: api (or read_api + write_repository). Stored only in the system keychain."
            }
            (AuthMode::Pat, HostKind::AzureDevOps) => {
                "Use https://dev.azure.com/<organisation> and an organisation-scoped token (Code: Read & write). Stored only in the system keychain."
            }
            (AuthMode::Pat, HostKind::Generic) => {
                "Username and token for this server. Stored only in the system keychain."
            }
            (AuthMode::OAuth, HostKind::GitHub) => {
                "Opens github.com: type the code shown here. GitDragon asks for repo, read:org and project (Boards); tokens stay in the system keychain."
            }
            (AuthMode::OAuth, HostKind::GitLab) => {
                "Opens gitlab.com in your browser; approve and come back. Tokens stay in the system keychain and refresh themselves."
            }
            (AuthMode::OAuth, _) => "Browser sign-in for this host is not set up yet; use a token.",
        };

        let mut form = v_flex()
            .gap_2()
            .pt_3()
            .mt_1()
            .border_t_1()
            .border_color(palette.border)
            .child(
                div()
                    .text_sm()
                    .text_color(palette.emph)
                    .child("Add account"),
            )
            .child(field(
                "Host",
                segmented(
                    "account-kind",
                    &kinds,
                    self.kind,
                    cx,
                    |this, kind, window, cx| this.set_kind(kind, window, cx),
                )
                .into_any_element(),
            ))
            .child(field(
                "Server URL",
                Input::new(&self.url).small().into_any_element(),
            ))
            .child(field(
                "Sign in with",
                segmented(
                    "account-auth",
                    &[
                        (
                            AuthMode::OAuth,
                            "Browser",
                            matches!(self.kind, HostKind::GitHub | HostKind::GitLab),
                        ),
                        (AuthMode::Pat, "Token", true),
                    ],
                    self.auth,
                    cx,
                    |this, auth, _, cx| {
                        this.auth = auth;
                        cx.notify();
                    },
                )
                .into_any_element(),
            ));
        if self.auth == AuthMode::Pat {
            if self.kind == HostKind::Generic {
                form = form.child(field(
                    "Username",
                    Input::new(&self.username).small().into_any_element(),
                ));
            }
            form = form.child(field(
                "Token",
                Input::new(&self.token).small().into_any_element(),
            ));
        }
        form =
            form.child(div().text_xs().text_color(palette.muted).child(hint))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("account-add")
                                .primary()
                                .small()
                                .label(match (busy, self.auth) {
                                    (true, AuthMode::OAuth) => "Waiting for the browser\u{2026}",
                                    (true, _) => "Checking\u{2026}",
                                    (false, AuthMode::OAuth) => "Sign in with browser",
                                    (false, _) => "Add account",
                                })
                                .disabled(busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.add(window, cx)
                                })),
                        )
                        .when(self.sign_in.is_some(), |d| {
                            d.child(
                                Button::new("account-sign-in-cancel")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.cancel_sign_in(cx)
                                    })),
                            )
                        })
                        .when_some(self.error.clone(), |d, err| {
                            d.child(div().text_xs().text_color(palette.error).child(err))
                        })
                        .when_some(self.notice.clone(), |d, notice| {
                            d.child(div().text_xs().text_color(palette.ok).child(notice))
                        }),
                );

        if let Some((code, page)) = self.sign_in.as_ref().and_then(|s| s.device.clone()) {
            let copy = code.clone();
            form = form.child(
                h_flex()
                    .gap_3()
                    .p_3()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(palette.accent)
                    .bg(palette.accent_soft)
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.fg)
                            .child("Enter this code on GitHub:"),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .text_color(palette.emph)
                            .child(code),
                    )
                    .child(
                        Button::new("device-copy")
                            .outline()
                            .small()
                            .label("Copy")
                            .on_click(move |_: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    copy.to_string(),
                                ))
                            }),
                    )
                    .child(
                        Button::new("device-open")
                            .ghost()
                            .small()
                            .label("Open page again")
                            .on_click(move |_: &ClickEvent, _, cx| cx.open_url(&page)),
                    ),
            );
        }

        let mut card = v_flex()
            .w_full()
            .p_4()
            .gap_1()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .child(div().text_color(palette.emph).child("Accounts"))
            .child(div().pb_2().text_xs().text_color(palette.muted).child(
                "Tokens live only in the system keychain. If Git Credential Manager already has a \
                 sign-in for a host, git uses that one first.",
            ));
        card = card
            .child(self.render_system_helper(&palette))
            .children(self.render_saved(&list, &palette, cx));
        // Credential Manager links live under the helper above.
        let own: Vec<Account> = list
            .into_iter()
            .filter(|a| a.auth != AuthMode::Gcm)
            .collect();
        if own.is_empty() {
            card =
                card.child(
                    div().pt_2().pb_1().text_xs().text_color(palette.muted).child(
                        "Or sign in with a token or the browser: GitDragon keeps it in the system keychain.",
                    ),
                );
        }
        for (ix, account) in own.into_iter().enumerate() {
            card = card.child(self.render_account(ix, account, &palette, cx));
        }
        card.child(form)
    }
}
