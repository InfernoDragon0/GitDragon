//! AI chat with Claude Code over ACP: one session per repository (cwd = its root), shown in the
//! centre panel's Chat view. The protocol runs in the `ai` crate on its own thread; events come
//! back over a channel into a GPUI task here.
//!
//! - [`ChatSession`]: transcript + running adapter for one repository (no window needed, so
//!   context actions anywhere can queue prompts).
//! - [`view::ChatView`]: the panel (messages, tool rows, permission cards, input).
//! - [`context`]: "Explain this diff", "Write commit message", "Resolve this conflict",
//!   "Summarise PR" entry points.
//! - [`settings::AiCard`]: adapter path / npx opt-in, Claude API key and model, MCP install.
//! - [`mcp_install`]: merges a `gitdragon-mcp` entry into Claude Code / Desktop config.

pub mod context;
pub mod mcp_install;
pub mod settings;
pub mod view;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use ai::{Chat, Decision, Event, Item, Phase};
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};

use crate::repo_tab::RepoTab;

/// Saved AI settings (`<config>/ai.json`). The API key is not here: it lives in the keychain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AiSettings {
    /// Absolute path to `claude-code-acp` (or its launcher) when it is not on PATH.
    #[serde(default)]
    pub adapter_path: Option<PathBuf>,
    /// Allow `npx --yes @zed-industries/claude-code-acp` when no adapter is installed.
    #[serde(default)]
    pub allow_npx: bool,
    /// Claude API model for the commit-message fallback.
    #[serde(default)]
    pub model: Option<String>,
}

impl AiSettings {
    fn path() -> Option<PathBuf> {
        crate::state::config_dir().map(|d| d.join("ai.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self)
            && let Err(err) = std::fs::write(&path, text)
        {
            tracing::warn!(%err, "saving ai.json failed");
        }
    }

    pub fn model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| ai::claude_api::DEFAULT_MODEL.to_string())
    }

    pub fn lookup(&self) -> ai::Lookup {
        ai::Lookup::from_env(self.adapter_path.clone(), self.allow_npx)
    }
}

/// App-wide AI state: settings and one chat per repository tab.
pub struct AiHub {
    pub settings: AiSettings,
    /// Dropped with their tab (which stops the adapter).
    sessions: Vec<(WeakEntity<RepoTab>, Entity<ChatSession>)>,
}

struct AiHubGlobal(Entity<AiHub>);

impl Global for AiHubGlobal {}

/// The hub, created on first use.
pub fn hub(cx: &mut App) -> Entity<AiHub> {
    if let Some(g) = cx.try_global::<AiHubGlobal>() {
        return g.0.clone();
    }
    let hub = cx.new(|_| AiHub {
        settings: AiSettings::load(),
        sessions: Vec::new(),
    });
    cx.set_global(AiHubGlobal(hub.clone()));
    hub
}

impl AiHub {
    pub fn set_settings(&mut self, settings: AiSettings, cx: &mut Context<Self>) {
        if self.settings != settings {
            self.settings = settings;
            self.settings.save();
            cx.notify();
        }
    }
}

/// The repository root a tab's chat runs in.
pub fn root_of(tab: &Entity<RepoTab>, cx: &App) -> PathBuf {
    let tab = tab.read(cx);
    tab.repo
        .as_ref()
        .map(|r| git_core::GitRepo::root(r).to_path_buf())
        .unwrap_or_else(|| tab.path.clone())
}

/// The chat for `tab`'s repository (made on first use; not started until needed). Chats of
/// closed tabs are dropped here, which stops their adapters.
pub fn session_for(tab: &Entity<RepoTab>, cx: &mut App) -> Entity<ChatSession> {
    let hub = hub(cx);
    let weak = tab.downgrade();
    hub.update(cx, |hub, _| {
        hub.sessions.retain(|(t, _)| t.upgrade().is_some())
    });
    if let Some((_, s)) = hub.read(cx).sessions.iter().find(|(t, _)| *t == weak) {
        return s.clone();
    }
    let root = root_of(tab, cx);
    let session = cx.new(|_| ChatSession::new(root));
    hub.update(cx, |hub, _| hub.sessions.push((weak, session.clone())));
    session
}

/// One repository's chat.
pub struct ChatSession {
    pub root: PathBuf,
    pub chat: Chat,
    session: Option<ai::Session>,
    /// Why the adapter could not be started (setup instructions are shown).
    pub setup: Option<String>,
    /// Prompts waiting for the session to be ready / the running turn to end.
    queued: VecDeque<String>,
    /// Tool rows the user unfolded.
    pub expanded: HashSet<String>,
    events_task: Option<Task<()>>,
}

impl ChatSession {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            chat: Chat::default(),
            session: None,
            setup: None,
            queued: VecDeque::new(),
            expanded: HashSet::new(),
            events_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.session.is_some() && !matches!(self.chat.phase, Phase::Closed | Phase::Idle)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.chat.phase, Phase::Busy | Phase::Starting) || !self.queued.is_empty()
    }

    /// Starts the adapter unless it is running.
    pub fn ensure_started(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let lookup = hub(cx).read(cx).settings.lookup();
        let cmd = match ai::adapter::find(&lookup) {
            Ok(cmd) => cmd,
            Err(err) => {
                self.setup = Some(git_core::redact::redact(&err.to_string()).into_owned());
                self.queued.clear();
                self.chat.phase = Phase::Idle;
                cx.notify();
                return;
            }
        };
        self.setup = None;
        tracing::info!(program = %cmd.program.display(), source = ?cmd.source, "starting ACP adapter");
        let (session, events) = ai::Session::start(cmd, self.root.clone());
        self.session = Some(session);
        self.chat.phase = Phase::Starting;
        self.events_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |s, cx| s.on_event(event, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        let next = matches!(event, Event::Ready { .. } | Event::TurnEnded { .. });
        if matches!(event, Event::Closed) {
            self.session = None;
            self.queued.clear();
        }
        self.chat.apply(event);
        if next {
            self.send_queued();
        }
        cx.notify();
    }

    fn send_queued(&mut self) {
        if self.chat.phase != Phase::Ready {
            return;
        }
        if let (Some(session), Some(prompt)) = (&self.session, self.queued.pop_front()) {
            session.prompt(prompt);
            self.chat.phase = Phase::Busy;
        }
    }

    /// Sends what the user typed.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.chat.items.push(Item::User(text.clone()));
        self.enqueue(text, cx);
    }

    /// Queues `prompt` (already announced in the transcript) and starts the adapter if needed.
    pub fn enqueue(&mut self, prompt: String, cx: &mut Context<Self>) {
        self.queued.push_back(prompt);
        self.ensure_started(cx);
        self.send_queued();
        cx.notify();
    }

    /// Shows `label` as the user's message; the prompt follows via [`Self::enqueue`].
    pub fn announce(&mut self, label: impl Into<String>, cx: &mut Context<Self>) {
        self.chat.items.push(Item::User(label.into()));
        cx.notify();
    }

    pub fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.chat
            .items
            .push(Item::Error(git_core::redact::redact(&message).into_owned()));
        cx.notify();
    }

    /// Stops the running turn (pending permission requests are denied).
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.queued.clear();
        if let Some(session) = &self.session {
            session.cancel();
        }
        cx.notify();
    }

    pub fn answer(&mut self, id: u64, decision: Decision, cx: &mut Context<Self>) {
        if self.chat.answer(id, decision)
            && let Some(session) = &self.session
        {
            session.answer(id, decision);
        }
        cx.notify();
    }

    /// Stops the adapter and starts over with an empty transcript.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.events_task = None;
        self.queued.clear();
        self.expanded.clear();
        self.chat = Chat::default();
        self.setup = None;
        cx.notify();
    }

    pub fn toggle_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_and_round_trip() {
        let empty: AiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, AiSettings::default());
        assert_eq!(empty.model(), ai::claude_api::DEFAULT_MODEL);
        assert!(!empty.allow_npx, "npx is opt-in");
        let set = AiSettings {
            adapter_path: Some(PathBuf::from("/opt/acp")),
            allow_npx: true,
            model: Some("claude-sonnet-5-5".into()),
        };
        let text = serde_json::to_string(&set).unwrap();
        assert!(
            !text.to_lowercase().contains("key"),
            "no secrets in ai.json: {text}"
        );
        assert_eq!(serde_json::from_str::<AiSettings>(&text).unwrap(), set);
    }
}
