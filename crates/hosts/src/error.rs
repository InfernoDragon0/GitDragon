//! What can go wrong talking to a host, in words the UI can show. Messages from hosts pass
//! through `git_core::redact` before they land here.

use std::time::{Duration, SystemTime};

use crate::model::Feature;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("{} is not available on this host", .0.label())]
    Unsupported(Feature),
    #[error("{host} rejected the sign-in; sign in again")]
    Unauthorized { host: String },
    #[error("{host} refused access: {message}")]
    Forbidden { host: String, message: String },
    /// The token lacks a permission (OAuth scope) the call needs; signing in again grants it.
    #[error("{host} needs the `{scope}` permission for this; sign in again to grant it")]
    MissingScope { host: String, scope: String },
    #[error("not found on {host} (or this account has no access)")]
    NotFound { host: String },
    #[error("{host} rate limit reached; {}", retry_hint(*.until))]
    RateLimited {
        host: String,
        until: Option<SystemTime>,
    },
    #[error("{host} rejected the request: {message}")]
    Invalid { host: String, message: String },
    #[error("{host} failed (HTTP {status}): {message}")]
    Server {
        host: String,
        status: u16,
        message: String,
    },
    #[error("could not reach {host}: {message}")]
    Network { host: String, message: String },
    #[error("unexpected answer from {host}: {message}")]
    Decode { host: String, message: String },
    #[error("redirect refused: {0}")]
    Redirect(String),
    #[error("{0}")]
    Config(String),
}

impl HostError {
    /// Worth retrying later without user action.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            HostError::Network { .. } | HostError::RateLimited { .. } | HostError::Server { .. }
        )
    }
}

fn retry_hint(until: Option<SystemTime>) -> String {
    let Some(wait) = until.and_then(|t| t.duration_since(SystemTime::now()).ok()) else {
        return "try again shortly".into();
    };
    let minutes = wait.as_secs().div_ceil(60);
    if wait < Duration::from_secs(60) {
        "try again in under a minute".into()
    } else {
        format!("try again in {minutes} min")
    }
}
