//! The chat transcript and the reducer that folds session [`Event`]s into it. Plain data, so the
//! UI only renders and the reducer is unit-tested.
//!
//! Every string that enters the model went through [`git_core::redact::redact`] when its event
//! was made ([`crate::event`]), so rendering never shows a registered secret.

use std::path::PathBuf;

/// What a session reports to the UI. Strings are already redacted.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The adapter started and a session is open.
    Ready {
        agent: String,
    },
    AgentText(String),
    Thought(String),
    ToolCall(ToolCall),
    ToolUpdate(ToolUpdate),
    Plan(Vec<PlanEntry>),
    Permission(PermissionAsk),
    /// The prompt turn ended (`reason` e.g. `end_turn`, `cancelled`).
    TurnEnded {
        reason: String,
    },
    /// A failure; the session may still be usable (`fatal: false`) or not.
    Error {
        message: String,
        fatal: bool,
    },
    /// The adapter exited / the session closed.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: ToolStatus,
    /// Raw input, pretty JSON.
    pub input: String,
    /// Output text (content blocks, diffs, raw output).
    pub output: String,
    pub locations: Vec<PathBuf>,
}

/// Fields that changed; `None` keeps the old value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolUpdate {
    pub id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<ToolStatus>,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub text: String,
    /// `pending`, `in_progress`, `completed`.
    pub status: String,
}

/// A permission request: the tool and the one allow option we offer (allow *once*; never
/// "always"). Deny is always possible.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    /// Our id for the pending request (answers go back with it).
    pub id: u64,
    pub tool: ToolUpdate,
    /// The adapter's `allow_once` option, if it offered one.
    pub allow_once: Option<String>,
    /// Its `reject_once` option, if any (else deny answers `cancelled`).
    pub reject_once: Option<String>,
}

/// The user's answer to a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    /// The default for every request, and what an unanswered request becomes.
    #[default]
    Deny,
    AllowOnce,
}

impl PermissionAsk {
    /// The option id to answer with, or `None` for a `cancelled` outcome. Allow falls back to
    /// deny when no allow-once option exists.
    pub fn answer(&self, decision: Decision) -> Option<String> {
        match decision {
            Decision::AllowOnce if self.allow_once.is_some() => self.allow_once.clone(),
            _ => self.reject_once.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User(String),
    Agent(String),
    Thought(String),
    Tool(ToolCall),
    Plan(Vec<PlanEntry>),
    Permission {
        ask: PermissionAsk,
        /// `None` while waiting.
        answered: Option<Decision>,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Not started (or stopped).
    #[default]
    Idle,
    Starting,
    /// Session open, waiting for the user.
    Ready,
    /// A prompt turn is running.
    Busy,
    /// The adapter exited or failed to start.
    Closed,
}

#[derive(Debug, Clone, Default)]
pub struct Chat {
    pub items: Vec<Item>,
    pub phase: Phase,
    pub agent: Option<String>,
}

impl Chat {
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.items.push(Item::User(text.into()));
        self.phase = Phase::Busy;
    }

    /// Folds one event in.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Ready { agent } => {
                self.agent = Some(agent);
                if self.phase != Phase::Busy {
                    self.phase = Phase::Ready;
                }
            }
            Event::AgentText(text) => match self.items.last_mut() {
                Some(Item::Agent(buf)) => buf.push_str(&text),
                _ => self.items.push(Item::Agent(text)),
            },
            Event::Thought(text) => match self.items.last_mut() {
                Some(Item::Thought(buf)) => buf.push_str(&text),
                _ => self.items.push(Item::Thought(text)),
            },
            Event::ToolCall(call) => match self.tool_mut(&call.id) {
                Some(existing) => *existing = call,
                None => self.items.push(Item::Tool(call)),
            },
            Event::ToolUpdate(update) => {
                if self.tool_mut(&update.id).is_none() {
                    self.items.push(Item::Tool(ToolCall {
                        id: update.id.clone(),
                        title: update.id.clone(),
                        ..ToolCall::default()
                    }));
                }
                if let Some(tool) = self.tool_mut(&update.id) {
                    merge_tool(tool, update);
                }
            }
            Event::Plan(entries) => {
                // One live plan: replace the latest one in place.
                match self.items.iter_mut().rev().find_map(|i| match i {
                    Item::Plan(p) => Some(p),
                    _ => None,
                }) {
                    Some(plan) => *plan = entries,
                    None => self.items.push(Item::Plan(entries)),
                }
            }
            Event::Permission(ask) => self.items.push(Item::Permission {
                ask,
                answered: None,
            }),
            Event::TurnEnded { reason } => {
                self.deny_pending();
                if reason != "end_turn" {
                    self.items.push(Item::Notice(format!("Stopped: {reason}")));
                }
                if self.phase == Phase::Busy {
                    self.phase = Phase::Ready;
                }
            }
            Event::Error { message, fatal } => {
                self.items.push(Item::Error(message));
                if fatal {
                    self.deny_pending();
                    self.phase = Phase::Closed;
                } else if self.phase == Phase::Busy {
                    self.phase = Phase::Ready;
                }
            }
            Event::Closed => {
                self.deny_pending();
                self.phase = Phase::Closed;
            }
        }
    }

    /// Records the answer for permission `id`; false when it was already answered / unknown.
    pub fn answer(&mut self, id: u64, decision: Decision) -> bool {
        for item in &mut self.items {
            if let Item::Permission { ask, answered } = item
                && ask.id == id
                && answered.is_none()
            {
                *answered = Some(decision);
                return true;
            }
        }
        false
    }

    /// Ids of permission requests still waiting.
    pub fn pending_permissions(&self) -> Vec<u64> {
        self.items
            .iter()
            .filter_map(|i| match i {
                Item::Permission {
                    ask,
                    answered: None,
                } => Some(ask.id),
                _ => None,
            })
            .collect()
    }

    /// Unanswered requests become denied (turn over, cancelled or closed).
    fn deny_pending(&mut self) {
        for item in &mut self.items {
            if let Item::Permission { answered, .. } = item
                && answered.is_none()
            {
                *answered = Some(Decision::Deny);
            }
        }
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.items.iter_mut().rev().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    /// The last agent message (one-shot answers, tests).
    pub fn last_agent_text(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Agent(t) => Some(t.as_str()),
            _ => None,
        })
    }

    /// Every user-visible string of the transcript, in order: what the panel renders.
    pub fn rendered_text(&self) -> Vec<String> {
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                Item::User(t) | Item::Agent(t) | Item::Thought(t) | Item::Notice(t) => {
                    out.push(t.clone())
                }
                Item::Error(t) => out.push(t.clone()),
                Item::Tool(t) => {
                    out.extend([
                        t.title.clone(),
                        t.kind.clone(),
                        t.input.clone(),
                        t.output.clone(),
                    ]);
                    out.extend(t.locations.iter().map(|p| p.display().to_string()));
                }
                Item::Plan(entries) => out.extend(entries.iter().map(|e| e.text.clone())),
                Item::Permission { ask, .. } => {
                    out.extend(ask.tool.title.clone());
                    out.extend(ask.tool.input.clone());
                }
            }
        }
        out
    }
}

fn merge_tool(tool: &mut ToolCall, update: ToolUpdate) {
    if let Some(title) = update.title {
        tool.title = title;
    }
    if let Some(kind) = update.kind {
        tool.kind = kind;
    }
    if let Some(status) = update.status {
        tool.status = status;
    }
    if let Some(input) = update.input {
        tool.input = input;
    }
    if let Some(output) = update.output {
        tool.output = output;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(id: u64) -> PermissionAsk {
        PermissionAsk {
            id,
            tool: ToolUpdate {
                id: "t1".into(),
                title: Some("Edit file".into()),
                ..ToolUpdate::default()
            },
            allow_once: Some("allow".into()),
            reject_once: Some("reject".into()),
        }
    }

    #[test]
    fn streams_text_and_folds_tool_updates() {
        let mut chat = Chat::default();
        chat.apply(Event::Ready {
            agent: "claude".into(),
        });
        assert_eq!(chat.phase, Phase::Ready);
        chat.push_user("hi");
        assert_eq!(chat.phase, Phase::Busy);
        chat.apply(Event::Thought("think".into()));
        chat.apply(Event::Thought("ing".into()));
        chat.apply(Event::AgentText("Hel".into()));
        chat.apply(Event::AgentText("lo".into()));
        chat.apply(Event::ToolCall(ToolCall {
            id: "t1".into(),
            title: "Read".into(),
            ..ToolCall::default()
        }));
        chat.apply(Event::ToolUpdate(ToolUpdate {
            id: "t1".into(),
            status: Some(ToolStatus::Done),
            output: Some("contents".into()),
            ..ToolUpdate::default()
        }));
        chat.apply(Event::AgentText("Done".into()));
        chat.apply(Event::Plan(vec![PlanEntry {
            text: "a".into(),
            status: "pending".into(),
        }]));
        chat.apply(Event::Plan(vec![PlanEntry {
            text: "a".into(),
            status: "completed".into(),
        }]));
        chat.apply(Event::TurnEnded {
            reason: "end_turn".into(),
        });
        assert_eq!(chat.phase, Phase::Ready);
        assert_eq!(chat.items.len(), 6, "{:#?}", chat.items);
        assert_eq!(chat.items[1], Item::Thought("thinking".into()));
        assert_eq!(chat.items[2], Item::Agent("Hello".into()));
        let Item::Tool(tool) = &chat.items[3] else {
            panic!()
        };
        assert_eq!(
            (tool.title.as_str(), tool.status, tool.output.as_str()),
            ("Read", ToolStatus::Done, "contents")
        );
        assert_eq!(chat.last_agent_text(), Some("Done"));
        let Item::Plan(plan) = &chat.items[5] else {
            panic!()
        };
        assert_eq!(plan[0].status, "completed");
    }

    #[test]
    fn update_for_unknown_tool_creates_it() {
        let mut chat = Chat::default();
        chat.apply(Event::ToolUpdate(ToolUpdate {
            id: "x".into(),
            title: Some("Bash".into()),
            ..ToolUpdate::default()
        }));
        assert!(matches!(&chat.items[0], Item::Tool(t) if t.title == "Bash"));
    }

    #[test]
    fn permissions_default_to_deny() {
        assert_eq!(Decision::default(), Decision::Deny);
        let a = ask(1);
        assert_eq!(a.answer(Decision::Deny).as_deref(), Some("reject"));
        assert_eq!(a.answer(Decision::AllowOnce).as_deref(), Some("allow"));
        // No allow-once option: allowing still denies.
        let no_allow = PermissionAsk {
            allow_once: None,
            reject_once: None,
            ..ask(2)
        };
        assert_eq!(no_allow.answer(Decision::AllowOnce), None);

        // Unanswered requests are denied when the turn ends or the session closes.
        let mut chat = Chat::default();
        chat.push_user("go");
        chat.apply(Event::Permission(ask(1)));
        chat.apply(Event::Permission(ask(2)));
        assert_eq!(chat.pending_permissions(), [1, 2]);
        assert!(chat.answer(1, Decision::AllowOnce));
        assert!(!chat.answer(1, Decision::Deny), "answered once only");
        chat.apply(Event::TurnEnded {
            reason: "cancelled".into(),
        });
        assert!(chat.pending_permissions().is_empty());
        let answers: Vec<_> = chat
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Permission { answered, .. } => *answered,
                _ => None,
            })
            .collect();
        assert_eq!(answers, [Decision::AllowOnce, Decision::Deny]);
        assert!(matches!(chat.items.last(), Some(Item::Notice(n)) if n.contains("cancelled")));

        let mut chat = Chat::default();
        chat.apply(Event::Permission(ask(3)));
        chat.apply(Event::Closed);
        assert!(chat.pending_permissions().is_empty());
        assert_eq!(chat.phase, Phase::Closed);
    }

    #[test]
    fn errors_end_busy_turns() {
        let mut chat = Chat::default();
        chat.push_user("x");
        chat.apply(Event::Error {
            message: "boom".into(),
            fatal: false,
        });
        assert_eq!(chat.phase, Phase::Ready);
        chat.apply(Event::Error {
            message: "dead".into(),
            fatal: true,
        });
        assert_eq!(chat.phase, Phase::Closed);
    }
}
