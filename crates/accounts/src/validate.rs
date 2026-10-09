//! Personal access token validation: one "who am I" call per host kind. The token travels only
//! in a header to the account's own API origin; redirects are not followed.

use serde_json::Value;

use crate::model::HostKind;
use crate::net;
use crate::secret::Secret;

#[derive(Debug, thiserror::Error)]
pub enum ValidateError {
    #[error("the token was rejected by {host} (HTTP {status})")]
    Rejected { host: String, status: u16 },
    #[error("{0} has no API to check tokens against; enter your username instead")]
    NoApi(&'static str),
    #[error("bad base URL: {0}")]
    BadUrl(String),
    #[error("could not reach {host}: {message}")]
    Network { host: String, message: String },
    #[error("unexpected answer from {0}")]
    Unexpected(String),
}

/// What the host says about the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    /// Login to use as the git username.
    pub username: String,
    /// Granted scopes when the host reports them (GitHub classic tokens).
    pub scopes: Vec<String>,
}

/// How a token is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    Bearer,
    /// GitLab's `PRIVATE-TOKEN` header.
    PrivateToken,
    /// Azure DevOps: Basic with an empty user.
    Basic,
}

/// The "who am I" endpoint for an account, from its web base URL.
fn user_endpoint(kind: HostKind, base_url: &str) -> Result<(url::Url, Auth), ValidateError> {
    let base = url::Url::parse(base_url.trim().trim_end_matches('/'))
        .map_err(|_| ValidateError::BadUrl(base_url.to_string()))?;
    let host = base.host_str().unwrap_or_default().to_ascii_lowercase();
    let join = |path: &str| {
        let mut url = base.clone();
        let prefix = base.path().trim_end_matches('/');
        url.set_path(&format!("{prefix}{path}"));
        url
    };
    let api = match kind {
        HostKind::GitHub if host == "github.com" => (
            url::Url::parse("https://api.github.com/user").expect("static URL"),
            Auth::Bearer,
        ),
        // GitHub Enterprise Server.
        HostKind::GitHub => (join("/api/v3/user"), Auth::Bearer),
        HostKind::GitLab => (join("/api/v4/user"), Auth::PrivateToken),
        // `https://dev.azure.com/<org>`: org-scoped PATs answer connectionData.
        HostKind::AzureDevOps => {
            let mut url = join("/_apis/connectionData");
            url.set_query(Some("api-version=7.1-preview"));
            (url, Auth::Basic)
        }
        HostKind::Generic => return Err(ValidateError::NoApi("This server")),
    };
    Ok(api)
}

/// Login (and scopes) from the host's answer.
fn parse_user(kind: HostKind, body: &Value, scopes_header: Option<&str>) -> Option<TokenInfo> {
    let username = match kind {
        HostKind::GitHub => body.get("login")?.as_str()?,
        HostKind::GitLab => body.get("username")?.as_str()?,
        HostKind::AzureDevOps => {
            let user = body.get("authenticatedUser")?;
            user.pointer("/properties/Account/$value")
                .and_then(Value::as_str)
                .or_else(|| user.get("providerDisplayName").and_then(Value::as_str))?
        }
        HostKind::Generic => return None,
    };
    let scopes = scopes_header
        .map(|h| {
            h.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(TokenInfo {
        username: username.to_string(),
        scopes,
    })
}

/// Checks a personal access token against the host's API (on the network runtime).
pub async fn validate_pat(
    kind: HostKind,
    base_url: &str,
    token: &Secret,
) -> Result<TokenInfo, ValidateError> {
    who_am_i(kind, base_url, token, false).await
}

/// The login for an OAuth access token (always sent as `Bearer`).
pub async fn validate_oauth(
    kind: HostKind,
    base_url: &str,
    token: &Secret,
) -> Result<TokenInfo, ValidateError> {
    who_am_i(kind, base_url, token, true).await
}

async fn who_am_i(
    kind: HostKind,
    base_url: &str,
    token: &Secret,
    oauth: bool,
) -> Result<TokenInfo, ValidateError> {
    let (url, auth) = user_endpoint(kind, base_url)?;
    let auth = if oauth { Auth::Bearer } else { auth };
    let host = url.host_str().unwrap_or_default().to_string();
    let token = token.clone();
    let task = net::spawn(async move {
        let request = net::client().get(url.clone());
        let request = match auth {
            Auth::Bearer => request
                .bearer_auth(token.expose())
                .header("Accept", "application/vnd.github+json"),
            Auth::PrivateToken => request.header("PRIVATE-TOKEN", token.expose()),
            Auth::Basic => request.basic_auth("", Some(token.expose())),
        };
        let response = request.send().await.map_err(|e| ValidateError::Network {
            host: host.clone(),
            message: git_core::redact::redact(&e.to_string()).into_owned(),
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(ValidateError::Rejected {
                host,
                status: status.as_u16(),
            });
        }
        let scopes = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body: Value = response
            .json()
            .await
            .map_err(|_| ValidateError::Unexpected(host.clone()))?;
        parse_user(kind, &body, scopes.as_deref()).ok_or(ValidateError::Unexpected(host))
    });
    task.await.map_err(|e| ValidateError::Network {
        host: String::new(),
        message: e.to_string(),
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let (url, auth) = user_endpoint(HostKind::GitHub, "https://github.com").unwrap();
        assert_eq!(url.as_str(), "https://api.github.com/user");
        assert_eq!(auth, Auth::Bearer);
        let (url, _) = user_endpoint(HostKind::GitHub, "https://ghe.example.com/").unwrap();
        assert_eq!(url.as_str(), "https://ghe.example.com/api/v3/user");
        let (url, auth) =
            user_endpoint(HostKind::GitLab, "https://gitlab.example.com:8443").unwrap();
        assert_eq!(url.as_str(), "https://gitlab.example.com:8443/api/v4/user");
        assert_eq!(auth, Auth::PrivateToken);
        let (url, auth) = user_endpoint(
            HostKind::AzureDevOps,
            "https://dev.azure.com/infernodragon0",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://dev.azure.com/infernodragon0/_apis/connectionData?api-version=7.1-preview"
        );
        assert_eq!(auth, Auth::Basic);
        assert!(user_endpoint(HostKind::Generic, "https://git.example.com").is_err());
        assert!(user_endpoint(HostKind::GitHub, "not a url").is_err());
    }

    #[test]
    fn parses_users() {
        let gh = serde_json::json!({"login": "mushr", "id": 1});
        assert_eq!(
            parse_user(HostKind::GitHub, &gh, Some("repo, read:org")),
            Some(TokenInfo {
                username: "mushr".into(),
                scopes: vec!["repo".into(), "read:org".into()],
            })
        );
        let gl = serde_json::json!({"username": "me"});
        assert_eq!(
            parse_user(HostKind::GitLab, &gl, None).unwrap().username,
            "me"
        );
        let az = serde_json::json!({"authenticatedUser": {
            "providerDisplayName": "Mushr",
            "properties": {"Account": {"$type": "System.String", "$value": "me@example.com"}}
        }});
        assert_eq!(
            parse_user(HostKind::AzureDevOps, &az, None)
                .unwrap()
                .username,
            "me@example.com"
        );
        assert!(parse_user(HostKind::GitHub, &serde_json::json!({}), None).is_none());
    }
}
