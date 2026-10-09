//! The slice of the ACP v1 wire format the client reads, as lenient serde types (unknown
//! fields ignored, unknown variants become `Other`). Requests are written with `json!` in
//! [`crate::session`]. Hand-written instead of the official schema crate, whose full serde code
//! costs ~1.5 MB of release exe for messages we never read; the tests run this client against
//! an agent built with the official SDK, so the shapes are checked against it.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

/// `session/update` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub update: SessionUpdate,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    Plan {
        #[serde(default)]
        entries: Vec<PlanEntry>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        #[serde(default)]
        name: String,
        #[serde(default)]
        uri: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
    Image {},
    Audio {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddedResource {
    /// Set for text resources; blobs have none.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub content: Vec<ToolCallContent>,
    #[serde(default)]
    pub locations: Vec<Location>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
}

/// Changed fields only.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<ToolCallContent>>,
    #[serde(default)]
    pub raw_input: Option<Value>,
    #[serde(default)]
    pub raw_output: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content {
        content: ContentBlock,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        path: PathBuf,
        #[serde(default)]
        new_text: String,
    },
    Terminal {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Location {
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct PlanEntry {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub status: String,
}

/// `session/request_permission` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermission {
    pub tool_call: ToolCallUpdate,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
}

#[derive(Debug, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    #[serde(default = "end_turn")]
    pub stop_reason: String,
}

fn end_turn() -> String {
    "end_turn".into()
}
