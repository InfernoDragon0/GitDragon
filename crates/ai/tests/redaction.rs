//! Canary (security P0): a registered token never shows in rendered chat text or error strings,
//! whether it arrives in agent output, tool calls, permission prompts, protocol errors or
//! Claude API errors. Its own test binary: the redaction registry is process-wide.

mod common;

use ai::wire::{RequestPermission, SessionUpdate};
use ai::{Chat, Event};
use git_core::redact;

const CANARY: &str = "CANARY-ai-chat-0123456789abcdef";

fn update(json: serde_json::Value) -> SessionUpdate {
    serde_json::from_value(json).unwrap()
}

#[test]
fn canary_never_reaches_chat_or_errors() {
    redact::register(CANARY);
    let mut chat = Chat::default();
    chat.push_user("hello");
    let updates = [
        serde_json::json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": format!("token is {CANARY}")}}),
        serde_json::json!({"sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": format!("https://me:{CANARY}@host/x")}}),
        serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": format!("id-{CANARY}"),
            "title": format!("echo {CANARY}"), "rawInput": {"cmd": CANARY},
            "rawOutput": CANARY, "locations": [{"path": format!("/tmp/{CANARY}")}]}),
        serde_json::json!({"sessionUpdate": "tool_call_update",
            "toolCallId": format!("id-{CANARY}"),
            "content": [{"type": "diff", "path": "a.txt", "newText": CANARY}]}),
        serde_json::json!({"sessionUpdate": "plan",
            "entries": [{"content": CANARY, "priority": "low", "status": "pending"}]}),
    ];
    for json in updates {
        chat.apply(ai::event::from_update(&update(json)).expect("handled update"));
    }
    let request: RequestPermission = serde_json::from_value(serde_json::json!({
        "sessionId": "s",
        "toolCall": {"toolCallId": "c", "title": format!("Run {CANARY}"), "rawInput": CANARY},
        "options": [{"optionId": "o", "name": "Allow", "kind": "allow_once"}]
    }))
    .unwrap();
    chat.apply(Event::Permission(ai::event::permission(1, &request)));
    chat.apply(Event::Error {
        message: ai::event::error_text(&format!("adapter failed: {CANARY}")),
        fatal: false,
    });

    let rendered = chat.rendered_text().join("\n");
    assert!(!rendered.contains(CANARY), "{rendered}");
    assert!(rendered.contains(redact::MASK));
    assert!(rendered.contains("token is"));

    let api = ai::claude_api::parse_error(
        401,
        &format!(r#"{{"error":{{"type":"authentication_error","message":"bad key {CANARY}"}}}}"#),
    );
    assert!(!api.contains(CANARY), "{api}");

    // End to end: the in-memory agent echoes the prompt (with the canary) back.
    let (session, events) = common::session();
    let text = async_io::block_on(ai::session::collect(
        &session,
        &events,
        format!("say {CANARY}"),
    ))
    .unwrap();
    assert!(!text.contains(CANARY), "{text}");
    assert!(text.contains(redact::MASK), "{text}");
}
