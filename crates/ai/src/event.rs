//! ACP messages -> chat [`Event`]s. Every string is redacted here, at the one place agent output
//! enters GitDragon.

use git_core::redact::redact;

use crate::model::{Event, PermissionAsk, PlanEntry, ToolCall, ToolStatus, ToolUpdate};
use crate::wire as acp;

/// Longest tool input / output kept for display.
const MAX_TOOL_TEXT: usize = 16 * 1024;

pub(crate) fn clean(text: &str) -> String {
    redact(text).into_owned()
}

/// `text` cut at a char boundary to at most `max` bytes, with a marker when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
}

fn block_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text { text } => text.clone(),
        acp::ContentBlock::ResourceLink { name, uri } => format!("[{name}]({uri})"),
        acp::ContentBlock::Resource { resource } => resource
            .text
            .clone()
            .unwrap_or_else(|| "[binary resource]".into()),
        acp::ContentBlock::Image {} => "[image]".into(),
        acp::ContentBlock::Audio {} => "[audio]".into(),
        acp::ContentBlock::Other => String::new(),
    }
}

fn tool_content_text(content: &[acp::ToolCallContent]) -> String {
    let mut out = String::new();
    for part in content {
        if !out.is_empty() {
            out.push('\n');
        }
        match part {
            acp::ToolCallContent::Content { content } => out.push_str(&block_text(content)),
            acp::ToolCallContent::Diff { path, new_text } => {
                out.push_str(&format!("--- {}\n", path.display()));
                out.push_str(new_text);
            }
            acp::ToolCallContent::Terminal {} => out.push_str("[terminal]"),
            acp::ToolCallContent::Other => {}
        }
    }
    out
}

fn json_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn status(status: &str) -> ToolStatus {
    match status {
        "in_progress" => ToolStatus::Running,
        "completed" => ToolStatus::Done,
        "failed" => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

fn output_of(content: &[acp::ToolCallContent], raw: Option<&serde_json::Value>) -> String {
    let text = tool_content_text(content);
    if text.is_empty()
        && let Some(raw) = raw
    {
        return json_text(raw);
    }
    text
}

fn shown(text: &str) -> String {
    clean(&truncate(text, MAX_TOOL_TEXT))
}

pub fn tool_update(update: &acp::ToolCallUpdate) -> ToolUpdate {
    let output = match (&update.content, &update.raw_output) {
        (None, None) => None,
        (content, raw) => Some(shown(&output_of(
            content.as_deref().unwrap_or_default(),
            raw.as_ref(),
        ))),
    };
    ToolUpdate {
        id: clean(&update.tool_call_id),
        title: update.title.as_deref().map(clean),
        kind: update.kind.as_deref().map(clean),
        status: update.status.as_deref().map(status),
        input: update.raw_input.as_ref().map(|v| shown(&json_text(v))),
        output,
    }
}

/// A `session/update` notification as an event (`None` for updates the panel ignores).
pub fn from_update(update: &acp::SessionUpdate) -> Option<Event> {
    Some(match update {
        acp::SessionUpdate::AgentMessageChunk { content } => {
            Event::AgentText(clean(&block_text(content)))
        }
        acp::SessionUpdate::AgentThoughtChunk { content } => {
            Event::Thought(clean(&block_text(content)))
        }
        acp::SessionUpdate::ToolCall(call) => Event::ToolCall(ToolCall {
            id: clean(&call.tool_call_id),
            title: clean(&call.title),
            kind: clean(call.kind.as_deref().unwrap_or("other")),
            status: status(call.status.as_deref().unwrap_or_default()),
            input: call
                .raw_input
                .as_ref()
                .map(|v| shown(&json_text(v)))
                .unwrap_or_default(),
            output: shown(&output_of(&call.content, call.raw_output.as_ref())),
            locations: call
                .locations
                .iter()
                .map(|l| clean(&l.path.display().to_string()).into())
                .collect(),
        }),
        acp::SessionUpdate::ToolCallUpdate(update) => Event::ToolUpdate(tool_update(update)),
        acp::SessionUpdate::Plan { entries } => Event::Plan(
            entries
                .iter()
                .map(|e| PlanEntry {
                    text: clean(&e.content),
                    status: e.status.clone(),
                })
                .collect(),
        ),
        acp::SessionUpdate::Other => return None,
    })
}

/// A permission request as an event: only allow-*once* and reject-*once* options are kept.
pub fn permission(id: u64, request: &acp::RequestPermission) -> PermissionAsk {
    let pick = |kind: &str| {
        request
            .options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| o.option_id.clone())
    };
    PermissionAsk {
        id,
        tool: tool_update(&request.tool_call),
        allow_once: pick("allow_once"),
        reject_once: pick("reject_once"),
    }
}

/// An error string for the UI.
pub fn error_text(err: &impl std::fmt::Display) -> String {
    clean(&truncate(&err.to_string(), 4096))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: serde_json::Value) -> acp::SessionUpdate {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn converts_session_updates() {
        let text = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "Hello"}
        }));
        assert_eq!(from_update(&text), Some(Event::AgentText("Hello".into())));

        let thought = update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "hmm"}
        }));
        assert_eq!(from_update(&thought), Some(Event::Thought("hmm".into())));

        let call = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-1",
            "title": "Read src/main.rs",
            "kind": "read",
            "status": "pending",
            "rawInput": {"path": "src/main.rs"},
            "locations": [{"path": "src/main.rs"}]
        }));
        let Some(Event::ToolCall(tool)) = from_update(&call) else {
            panic!()
        };
        assert_eq!(tool.id, "call-1");
        assert_eq!(tool.kind, "read");
        assert_eq!(tool.status, ToolStatus::Pending);
        assert!(tool.input.contains("\"path\""));
        assert_eq!(tool.locations, [std::path::PathBuf::from("src/main.rs")]);

        let done = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "fn main() {}"}}]
        }));
        let Some(Event::ToolUpdate(up)) = from_update(&done) else {
            panic!()
        };
        assert_eq!(up.status, Some(ToolStatus::Done));
        assert_eq!(up.output.as_deref(), Some("fn main() {}"));
        assert_eq!(up.title, None);

        let plan = update(serde_json::json!({
            "sessionUpdate": "plan",
            "entries": [{"content": "Step", "priority": "high", "status": "in_progress"}]
        }));
        assert_eq!(
            from_update(&plan),
            Some(Event::Plan(vec![PlanEntry {
                text: "Step".into(),
                status: "in_progress".into()
            }]))
        );
    }

    #[test]
    fn permission_keeps_only_once_options() {
        let req: acp::RequestPermission = serde_json::from_value(serde_json::json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "c", "title": "Write file"},
            "options": [
                {"optionId": "always", "name": "Always Allow", "kind": "allow_always"},
                {"optionId": "once", "name": "Allow", "kind": "allow_once"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"}
            ]
        }))
        .unwrap();
        let ask = permission(7, &req);
        assert_eq!(ask.id, 7);
        assert_eq!(ask.allow_once.as_deref(), Some("once"));
        assert_eq!(ask.reject_once.as_deref(), Some("no"));
        assert_eq!(ask.tool.title.as_deref(), Some("Write file"));
        // Never the "always" option, whatever the user picks.
        for d in [crate::Decision::Deny, crate::Decision::AllowOnce] {
            assert_ne!(ask.answer(d).as_deref(), Some("always"));
        }
        assert_eq!(
            ask.answer(crate::Decision::default()).as_deref(),
            Some("no")
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(10);
        let t = truncate(&s, 5);
        assert!(t.starts_with("éé"));
        assert!(t.contains("more bytes"));
        assert_eq!(truncate("short", 10), "short");
    }
}
