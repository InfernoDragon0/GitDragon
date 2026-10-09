//! AI features: the ACP client for Claude Code's adapter, the chat model, context-action
//! prompts, and the direct Claude API fallback for commit messages.
//!
//! - [`adapter`]: find (absolute paths only) and spawn the adapter with a scrubbed environment.
//! - [`session`]: one ACP session on its own thread; commands in, [`Event`]s out.
//! - [`model`]: the transcript and its reducer; permissions default to deny.
//! - [`prompts`]: context-action prompts and diff text.
//! - [`claude_api`]: Messages API call with a keychain-stored key.

pub mod adapter;
pub mod claude_api;
pub mod event;
pub mod model;
pub mod prompts;
pub mod session;
pub mod wire;

pub use adapter::{AdapterCommand, AdapterSource, FindError, Lookup};
pub use model::{
    Chat, Decision, Event, Item, PermissionAsk, Phase, PlanEntry, ToolCall, ToolStatus,
};
pub use session::{Session, one_shot};
