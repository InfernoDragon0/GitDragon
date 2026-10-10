//! The Chat view: transcript (streaming markdown, folded tool rows, plans, permission cards),
//! the input box with Send / Stop, and the "adapter not found" setup state.

use ai::{Decision, Item, Phase, ToolCall, ToolStatus};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use ui::Palette;

use super::ChatSession;
use crate::repo_tab::RepoTab;

pub struct ChatView {
    #[allow(dead_code)]
    tab: Entity<RepoTab>,
    session: Entity<ChatSession>,
    input: Entity<TextareaState>,
    scroll: ScrollHandle,
    /// Focused when a permission card appears: Enter / Escape there deny.
    permission_focus: FocusHandle,
    pending_seen: usize,
    items_seen: usize,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl ChatView {
    pub fn new(tab: Entity<RepoTab>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session = super::session_for(&tab, cx);
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(
                    "Ask Claude about this repository (Enter sends, Shift+Enter for a new line)",
                )
        });
        let _subs = vec![
            cx.observe_in(&session, window, |this, session, window, cx| {
                this.on_session_changed(session, window, cx)
            }),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(window, cx);
                }
            }),
            // The root path is known once the tab opened its repository.
            cx.observe(&tab, |_, _, cx| cx.notify()),
        ];
        Self {
            tab,
            session,
            input,
            scroll: ScrollHandle::new(),
            permission_focus: cx.focus_handle(),
            pending_seen: 0,
            items_seen: 0,
            focus: cx.focus_handle(),
            _subs,
        }
    }

    fn on_session_changed(
        &mut self,
        session: Entity<ChatSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let s = session.read(cx);
        let pending = s.chat.pending_permissions().len();
        let items = s.chat.items.len();
        // New permission request: keyboard focus goes to it, where Enter denies.
        if pending > self.pending_seen {
            window.focus(&self.permission_focus, cx);
        }
        // Follow the tail while the user is at (or near) the bottom.
        let near_bottom = {
            let (offset, max) = (self.scroll.offset().y, self.scroll.max_offset().y);
            (offset + max).abs() < px(80.)
        };
        if near_bottom && items >= self.items_seen {
            self.scroll.scroll_to_bottom();
        }
        self.pending_seen = pending;
        self.items_seen = items;
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() || self.session.read(cx).is_busy() {
            return;
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.session.update(cx, |s, cx| s.send(text, cx));
        self.scroll.scroll_to_bottom();
    }

    fn deny_first_pending(&mut self, cx: &mut Context<Self>) {
        let first = self
            .session
            .read(cx)
            .chat
            .pending_permissions()
            .first()
            .copied();
        if let Some(id) = first {
            self.session
                .update(cx, |s, cx| s.answer(id, Decision::Deny, cx));
        }
    }
}

impl Focusable for ChatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn status_icon(status: ToolStatus, palette: &Palette) -> AnyElement {
    let (icon, color) = match status {
        ToolStatus::Pending => (IconName::Ellipsis, palette.muted),
        ToolStatus::Running => (IconName::LoaderCircle, palette.accent),
        ToolStatus::Done => (IconName::Check, palette.ok),
        ToolStatus::Failed => (IconName::CircleX, palette.error),
    };
    Icon::new(icon)
        .xsmall()
        .text_color(color)
        .into_any_element()
}

fn mono_block(text: &str, mono: SharedString, palette: &Palette) -> impl IntoElement {
    div()
        .p_2()
        .rounded(px(4.))
        .bg(palette.panel2)
        .font_family(mono)
        .text_xs()
        .text_color(palette.fg)
        .child(SharedString::from(text.to_string()))
}

impl ChatView {
    fn render_tool(
        &self,
        ix: usize,
        tool: &ToolCall,
        expanded: bool,
        mono: SharedString,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tool.id.clone();
        v_flex()
            .w_full()
            .rounded(px(6.))
            .border_1()
            .border_color(palette.border)
            .child(
                h_flex()
                    .id(ElementId::NamedInteger("tool".into(), ix as u64))
                    .px_2()
                    .py_1()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(palette.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.session.update(cx, |s, cx| s.toggle_tool(&id, cx))
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(palette.muted),
                    )
                    .child(status_icon(tool.status, palette))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(palette.fg)
                            .child(SharedString::from(tool.title.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(tool.kind.clone())),
                    ),
            )
            .when(expanded, |d| {
                d.child(
                    v_flex()
                        .px_2()
                        .pb_2()
                        .gap_1()
                        .when(!tool.locations.is_empty(), |d| {
                            d.child(
                                div().text_xs().text_color(palette.muted).child(
                                    SharedString::from(
                                        tool.locations
                                            .iter()
                                            .map(|p| p.display().to_string())
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                    ),
                                ),
                            )
                        })
                        .when(!tool.input.is_empty(), |d| {
                            d.child(div().text_xs().text_color(palette.muted).child("Input"))
                                .child(mono_block(&tool.input, mono.clone(), palette))
                        })
                        .when(!tool.output.is_empty(), |d| {
                            d.child(div().text_xs().text_color(palette.muted).child("Output"))
                                .child(mono_block(&tool.output, mono.clone(), palette))
                        }),
                )
            })
            .into_any_element()
    }

    fn render_permission(
        &self,
        ask: &ai::PermissionAsk,
        answered: Option<Decision>,
        first_pending: bool,
        mono: SharedString,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ask.id;
        let title = ask
            .tool
            .title
            .clone()
            .unwrap_or_else(|| "a tool".to_string());
        let card = v_flex()
            .w_full()
            .p_3()
            .gap_2()
            .rounded(px(6.))
            .border_1()
            .border_color(if answered.is_none() {
                palette.warn
            } else {
                palette.border
            })
            .bg(palette.panel)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(palette.warn),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(format!("Claude wants to run: {title}"))),
                    ),
            )
            .when_some(
                ask.tool.input.clone().filter(|i| !i.is_empty()),
                |d, input| d.child(mono_block(&input, mono, palette)),
            );
        let card = match answered {
            Some(decision) => card.child(div().text_xs().text_color(palette.muted).child(
                match decision {
                    Decision::AllowOnce => "Allowed once.",
                    Decision::Deny => "Denied.",
                },
            )),
            None => card.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(ElementId::NamedInteger("perm-deny".into(), id))
                            .primary()
                            .small()
                            .label("Deny")
                            .tab_index(0)
                            .tooltip("Enter or Escape")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.session
                                    .update(cx, |s, cx| s.answer(id, Decision::Deny, cx))
                            })),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("perm-allow".into(), id))
                            .outline()
                            .small()
                            .label("Allow once")
                            .disabled(ask.allow_once.is_none())
                            .tab_index(1)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.session
                                    .update(cx, |s, cx| s.answer(id, Decision::AllowOnce, cx))
                            })),
                    ),
            ),
        };
        if first_pending {
            div()
                .w_full()
                .track_focus(&self.permission_focus)
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "escape") {
                        this.deny_first_pending(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(card)
                .into_any_element()
        } else {
            card.into_any_element()
        }
    }

    fn render_setup(&self, reason: &str, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let step = |n: &'static str, text: &'static str| {
            h_flex()
                .gap_2()
                .items_start()
                .child(div().text_color(palette.accent).child(n))
                .child(div().flex_1().text_color(palette.fg).child(text))
        };
        v_flex()
            .w_full()
            .max_w(px(640.))
            .mx_auto()
            .mt_6()
            .p_4()
            .gap_2()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .text_sm()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Bot).text_color(palette.accent))
                    .child(
                        div()
                            .text_color(palette.emph)
                            .child("Set up Claude Code for the chat"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(reason.to_string())),
            )
            .child(step(
                "1.",
                "Install Node.js 18 or newer and Claude Code, then sign in once: run `claude` \
                 in a terminal and use /login.",
            ))
            .child(step(
                "2.",
                "Install the ACP adapter: npm install -g @zed-industries/claude-code-acp",
            ))
            .child(step(
                "3.",
                "Restart GitDragon so it sees the new PATH, or set the adapter's absolute path \
                 (or allow npx) in Settings \u{2192} AI.",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new("chat-retry")
                            .primary()
                            .small()
                            .label("Try again")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.session.update(cx, |s, cx| s.ensure_started(cx))
                            })),
                    )
                    .child(
                        Button::new("chat-settings")
                            .outline()
                            .small()
                            .label("Open Settings")
                            .on_click(|_: &ClickEvent, window, cx| {
                                window.dispatch_action(Box::new(crate::actions::ShowSettings), cx)
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let mono = cx.theme().mono_font_family.clone();
        let session = self.session.read(cx);
        let phase = session.chat.phase;
        let busy = session.is_busy();
        let setup = session.setup.clone();
        let items = session.chat.items.clone();
        let expanded = session.expanded.clone();
        let agent = session.chat.agent.clone();
        let first_pending = session.chat.pending_permissions().first().copied();

        let mut rows: Vec<AnyElement> = Vec::new();
        for (ix, item) in items.iter().enumerate() {
            let row = match item {
                Item::User(text) => h_flex()
                    .w_full()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(560.))
                            .px_3()
                            .py_2()
                            .rounded(px(8.))
                            .bg(palette.accent_soft)
                            .text_sm()
                            .text_color(palette.emph)
                            .child(SharedString::from(text.clone())),
                    )
                    .into_any_element(),
                Item::Agent(text) => div()
                    .w_full()
                    .text_sm()
                    .child(
                        TextView::markdown(
                            ElementId::NamedInteger("agent".into(), ix as u64),
                            SharedString::from(text.clone()),
                        )
                        .selectable(true),
                    )
                    .into_any_element(),
                Item::Thought(text) => div()
                    .w_full()
                    .pl_2()
                    .border_l_2()
                    .border_color(palette.border)
                    .text_xs()
                    .italic()
                    .text_color(palette.muted)
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
                Item::Tool(tool) => self.render_tool(
                    ix,
                    tool,
                    expanded.contains(&tool.id),
                    mono.clone(),
                    &palette,
                    cx,
                ),
                Item::Plan(entries) => v_flex()
                    .w_full()
                    .p_2()
                    .gap_0p5()
                    .rounded(px(6.))
                    .bg(palette.panel2)
                    .text_xs()
                    .child(div().text_color(palette.muted).child("Plan"))
                    .children(entries.iter().map(|e| {
                        let (mark, color) = match e.status.as_str() {
                            "completed" => ("\u{2713}", palette.ok),
                            "in_progress" => ("\u{25B8}", palette.accent),
                            _ => ("\u{25CB}", palette.muted),
                        };
                        h_flex()
                            .gap_2()
                            .child(div().text_color(color).child(mark))
                            .child(
                                div()
                                    .text_color(palette.fg)
                                    .child(SharedString::from(e.text.clone())),
                            )
                    }))
                    .into_any_element(),
                Item::Permission { ask, answered } => self.render_permission(
                    ask,
                    *answered,
                    first_pending == Some(ask.id),
                    mono.clone(),
                    &palette,
                    cx,
                ),
                Item::Notice(text) => div()
                    .w_full()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
                Item::Error(text) => div()
                    .w_full()
                    .p_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(palette.error)
                    .text_xs()
                    .text_color(palette.error)
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
            };
            rows.push(row);
        }

        let status = match (phase, &agent) {
            (Phase::Starting, _) => "Starting Claude Code\u{2026}".to_string(),
            (Phase::Busy, _) => "Working\u{2026}".to_string(),
            (Phase::Ready, Some(agent)) => format!("Connected to {agent}"),
            (Phase::Ready, None) => "Connected".to_string(),
            (Phase::Closed, _) => "Stopped. Send a message to start again.".to_string(),
            (Phase::Idle, _) => "Not started".to_string(),
        };

        let header = crate::page_header::page_header(
            "Chat",
            "Ask Claude about this repository.",
            Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(status)),
                    )
                    .child(
                        Button::new("chat-new")
                            .ghost()
                            .small()
                            .label("New chat")
                            .tooltip("Stop the agent and clear the conversation")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.session.update(cx, |s, cx| s.reset(cx))
                            })),
                    )
                    .into_any_element(),
            ),
            cx,
        );

        let empty = rows.is_empty() && setup.is_none();
        let body = div()
            .id("chat-messages")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(820.))
                    .mx_auto()
                    .p_4()
                    .gap_3()
                    .when_some(setup, |d, reason| {
                        d.child(self.render_setup(&reason, &palette, cx))
                    })
                    .when(empty, |d| {
                        d.child(
                            v_flex()
                                .mt_6()
                                .items_center()
                                .gap_1()
                                .text_center()
                                .child(ui::typography::subtitle("Chat with Claude Code", cx))
                                .child(div().text_sm().text_color(palette.muted).child(
                                    "It works in this repository's folder. Tools that change \
                                         anything ask first; nothing is allowed unless you say so.",
                                )),
                        )
                    })
                    .children(rows),
            );

        let can_send = !busy;
        let footer = h_flex()
            .p_2()
            .gap_2()
            .items_end()
            .flex_none()
            .border_t_1()
            .border_color(palette.border)
            .child(div().flex_1().child(Textarea::new(&self.input).small()))
            .child(if busy {
                Button::new("chat-stop")
                    .outline()
                    .small()
                    .icon(IconName::Square)
                    .label("Stop")
                    .tooltip("Cancel the running turn")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.session.update(cx, |s, cx| s.cancel(cx))
                    }))
            } else {
                Button::new("chat-send")
                    .primary()
                    .small()
                    .icon(IconName::ArrowUp)
                    .label("Send")
                    .disabled(!can_send)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.submit(window, cx)),
                    )
            });

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(palette.bg)
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
            .child(footer)
    }
}
