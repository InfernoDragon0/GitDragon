//! Browser sign-in: GitHub device flow and GitLab authorization code + PKCE on a loopback port.
//!
//! The client IDs are public (no client secret ships; checklist P1). Tokens go to the keychain
//! as one JSON secret per account ([`OAuthTokens`]); access tokens that expire are refreshed
//! with the refresh token.
//!
//! PKCE (checklist P1): S256 challenge, random `state` checked on the callback, listener on
//! 127.0.0.1 only, single use, closed after the callback or [`CALLBACK_TIMEOUT`].

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::net;
use crate::secret::Secret;

/// GitHub OAuth App (device flow enabled) registered for GitDragon.
pub const GITHUB_CLIENT_ID: &str = "Ov23lisyEn80CxLY0RK3";
/// Scopes for GitHub; `project` is added once Boards is used.
pub const GITHUB_SCOPES: &str = "repo read:org";
/// gitlab.com application (public, PKCE) registered for GitDragon.
pub const GITLAB_CLIENT_ID: &str =
    "f8c1d84438f11b73bba5e34a8f7fc082f3190d4e4ba78dbec2c191624113fa6b";
/// Must match the application's redirect URI exactly.
pub const GITLAB_REDIRECT_PORT: u16 = 7613;
pub const GITLAB_SCOPES: &str = "api";
/// How long the loopback listener waits for the browser.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("sign-in was cancelled or denied")]
    Denied,
    #[error("the sign-in code expired; start again")]
    Expired,
    #[error("sign-in timed out")]
    TimedOut,
    #[error("sign-in answer did not match this request (state mismatch)")]
    StateMismatch,
    #[error("port {0} is busy; close whatever uses it and try again")]
    PortBusy(u16),
    #[error("could not reach {host}: {message}")]
    Network { host: String, message: String },
    #[error("unexpected answer from {0}")]
    Unexpected(String),
}

/// Tokens stored in the keychain (serialised as one secret).
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` = does not expire (GitHub OAuth App tokens).
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuthTokens(\u{2022}\u{2022}\u{2022}\u{2022})")
    }
}

impl OAuthTokens {
    pub fn to_secret(&self) -> Secret {
        Secret::new(serde_json::to_string(self).expect("serialisable"))
    }

    pub fn from_secret(secret: &Secret) -> Option<Self> {
        serde_json::from_str(secret.expose()).ok()
    }

    /// Expired or expiring within a minute.
    pub fn needs_refresh(&self) -> bool {
        self.expires_at.is_some_and(|at| at - 60 <= now()) && self.refresh_token.is_some()
    }

    /// Masks all tokens in logs and errors from now on.
    pub fn register_redaction(&self) {
        git_core::redact::register(&self.access_token);
        if let Some(refresh) = &self.refresh_token {
            git_core::redact::register(refresh);
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn network(host: &str) -> impl Fn(reqwest::Error) -> OAuthError + '_ {
    move |e| OAuthError::Network {
        host: host.to_string(),
        message: git_core::redact::redact(&e.to_string()).into_owned(),
    }
}

/// Token endpoint answer shared by GitHub and GitLab.
#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    interval: Option<u64>,
}

impl TokenAnswer {
    fn tokens(self) -> Option<OAuthTokens> {
        let tokens = OAuthTokens {
            access_token: self.access_token?,
            refresh_token: self.refresh_token,
            expires_at: self.expires_in.map(|s| now() + s),
        };
        tokens.register_redaction();
        Some(tokens)
    }
}

// ---- GitHub device flow ---------------------------------------------------------------------

/// What the user sees: go to `verification_uri` and type `user_code`.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    device_code: Secret,
    interval: u64,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct DeviceAnswer {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// Step 1: get a user code.
pub async fn github_device_start() -> Result<DeviceCode, OAuthError> {
    net::spawn(async {
        let host = "github.com";
        let answer: DeviceAnswer = net::client()
            .post("https://github.com/login/device/code")
            .header("Accept", "application/json")
            .form(&[("client_id", GITHUB_CLIENT_ID), ("scope", GITHUB_SCOPES)])
            .send()
            .await
            .map_err(network(host))?
            .json()
            .await
            .map_err(|_| OAuthError::Unexpected(host.into()))?;
        Ok(DeviceCode {
            user_code: answer.user_code,
            verification_uri: answer.verification_uri,
            device_code: Secret::new(answer.device_code),
            interval: answer.interval.max(1),
            expires_at: Instant::now() + Duration::from_secs(answer.expires_in),
        })
    })
    .await
    .map_err(|e| OAuthError::Unexpected(e.to_string()))?
}

/// Step 2: poll until the user approves (or denies, or the code expires).
pub async fn github_device_finish(code: DeviceCode) -> Result<OAuthTokens, OAuthError> {
    net::spawn(async move {
        let host = "github.com";
        let mut interval = code.interval;
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if Instant::now() >= code.expires_at {
                return Err(OAuthError::Expired);
            }
            let answer: TokenAnswer = net::client()
                .post("https://github.com/login/oauth/access_token")
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", GITHUB_CLIENT_ID),
                    ("device_code", code.device_code.expose()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .await
                .map_err(network(host))?
                .json()
                .await
                .map_err(|_| OAuthError::Unexpected(host.into()))?;
            match answer.error.as_deref() {
                None => return answer.tokens().ok_or(OAuthError::Unexpected(host.into())),
                Some("authorization_pending") => {}
                Some("slow_down") => interval = answer.interval.unwrap_or(interval + 5),
                Some("expired_token") => return Err(OAuthError::Expired),
                Some("access_denied") => return Err(OAuthError::Denied),
                Some(_) => return Err(OAuthError::Unexpected(host.into())),
            }
        }
    })
    .await
    .map_err(|e| OAuthError::Unexpected(e.to_string()))?
}

// ---- GitLab authorization code + PKCE ----------------------------------------------------------

/// A started GitLab sign-in: open [`Self::authorize_url`] in the browser, then
/// [`gitlab_finish`].
pub struct PkceLogin {
    pub authorize_url: String,
    base_url: String,
    verifier: Secret,
    state: Secret,
    listener: TcpListener,
    cancelled: Arc<AtomicBool>,
}

impl PkceLogin {
    /// Stops the wait (and frees the port) from another thread, e.g. a Cancel button.
    pub fn canceller(&self) -> Canceller {
        Canceller(self.cancelled.clone())
    }
}

#[derive(Clone)]
pub struct Canceller(Arc<AtomicBool>);

impl Canceller {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn random_url_safe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random source");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn redirect_uri() -> String {
    format!("http://127.0.0.1:{GITLAB_REDIRECT_PORT}/callback")
}

/// S256 code challenge for `verifier`.
fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Binds the loopback port and builds the authorize URL (`base_url` = `https://gitlab.com`).
pub fn gitlab_start(base_url: &str) -> Result<PkceLogin, OAuthError> {
    let listener = TcpListener::bind(("127.0.0.1", GITLAB_REDIRECT_PORT))
        .map_err(|_| OAuthError::PortBusy(GITLAB_REDIRECT_PORT))?;
    let verifier = Secret::new(random_url_safe(48));
    let state = Secret::new(random_url_safe(24));
    let base = base_url.trim_end_matches('/');
    let mut url = url::Url::parse(&format!("{base}/oauth/authorize"))
        .map_err(|_| OAuthError::Unexpected(base.into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", GITLAB_CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri())
        .append_pair("response_type", "code")
        .append_pair("scope", GITLAB_SCOPES)
        .append_pair("state", state.expose())
        .append_pair("code_challenge", &challenge(verifier.expose()))
        .append_pair("code_challenge_method", "S256");
    Ok(PkceLogin {
        authorize_url: url.into(),
        base_url: base.to_string(),
        verifier,
        state,
        listener,
        cancelled: Arc::default(),
    })
}

/// Waits for the browser's redirect (one request), checks `state`, exchanges the code.
pub async fn gitlab_finish(login: PkceLogin) -> Result<OAuthTokens, OAuthError> {
    let PkceLogin {
        base_url,
        verifier,
        state,
        listener,
        cancelled,
        ..
    } = login;
    let code = tokio_blocking(move || wait_for_code(listener, &state, &cancelled)).await??;
    let form = [
        ("client_id", GITLAB_CLIENT_ID.to_string()),
        ("code", code.expose().to_string()),
        ("grant_type", "authorization_code".into()),
        ("redirect_uri", redirect_uri()),
        ("code_verifier", verifier.expose().to_string()),
    ];
    gitlab_token(base_url, form.to_vec()).await
}

/// New tokens from a refresh token (GitLab access tokens last two hours).
pub async fn gitlab_refresh(
    base_url: &str,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let form = vec![
        ("client_id", GITLAB_CLIENT_ID.to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("grant_type", "refresh_token".into()),
        ("redirect_uri", redirect_uri()),
    ];
    gitlab_token(base_url.trim_end_matches('/').to_string(), form).await
}

async fn gitlab_token(
    base_url: String,
    form: Vec<(&'static str, String)>,
) -> Result<OAuthTokens, OAuthError> {
    net::spawn(async move {
        let host = url::Url::parse(&base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let response = net::client()
            .post(format!("{base_url}/oauth/token"))
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(network(&host))?;
        if response.status() == reqwest::StatusCode::BAD_REQUEST
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(OAuthError::Denied);
        }
        let answer: TokenAnswer = response
            .json()
            .await
            .map_err(|_| OAuthError::Unexpected(host.clone()))?;
        answer.tokens().ok_or(OAuthError::Unexpected(host))
    })
    .await
    .map_err(|e| OAuthError::Unexpected(e.to_string()))?
}

async fn tokio_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, OAuthError> {
    net::spawn(async move { tokio::task::spawn_blocking(f).await })
        .await
        .map_err(|e| OAuthError::Unexpected(e.to_string()))?
        .map_err(|e| OAuthError::Unexpected(e.to_string()))
}

/// Accepts connections until one carries `/callback?code=…&state=…` (or the timeout), answers
/// the browser with a short page, and returns the code. Other paths (favicon) get a 404.
fn wait_for_code(
    listener: TcpListener,
    state: &Secret,
    cancelled: &AtomicBool,
) -> Result<Secret, OAuthError> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    listener
        .set_nonblocking(true)
        .map_err(|e| OAuthError::Unexpected(e.to_string()))?;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(OAuthError::Denied);
        }
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut);
        }
        let (stream, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(OAuthError::Unexpected(e.to_string())),
        };
        _ = stream.set_nonblocking(false);
        _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut request_line = String::new();
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != "/callback" {
            reply(&stream, 404, "Not found.");
            continue;
        }
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(error) = param("error") {
            reply(
                &stream,
                200,
                "Sign-in was cancelled. You can close this tab.",
            );
            tracing::info!(%error, "GitLab sign-in denied");
            return Err(OAuthError::Denied);
        }
        let given = param("state").unwrap_or_default();
        if !constant_time_eq(given.as_bytes(), state.expose().as_bytes()) {
            reply(
                &stream,
                400,
                "This sign-in link is not the one GitDragon started.",
            );
            return Err(OAuthError::StateMismatch);
        }
        let Some(code) = param("code") else {
            reply(&stream, 400, "No authorization code.");
            return Err(OAuthError::Unexpected("callback".into()));
        };
        reply(
            &stream,
            200,
            "Signed in to GitDragon. You can close this tab.",
        );
        return Ok(Secret::new(code));
    }
}

fn reply(mut stream: &std::net::TcpStream, status: u16, text: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>GitDragon</title>\
         <body style=\"font-family:sans-serif;padding:3em\">{text}</body>"
    );
    _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    _ = stream.flush();
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn tokens_round_trip_and_never_print() {
        let t = OAuthTokens {
            access_token: "glpat-ACCESS-123456".into(),
            refresh_token: Some("REFRESH-123456789".into()),
            expires_at: Some(now() + 30),
        };
        assert!(!format!("{t:?}").contains("ACCESS"));
        assert!(t.needs_refresh());
        let back = OAuthTokens::from_secret(&t.to_secret()).unwrap();
        assert_eq!(back.access_token, t.access_token);
        let forever = OAuthTokens {
            expires_at: None,
            ..t
        };
        assert!(!forever.needs_refresh());
    }

    /// The loopback listener: wrong paths 404, a wrong state is refused, the right one wins.
    #[test]
    fn callback_listener_checks_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Secret::new("good-state");
        let waiter =
            std::thread::spawn(move || wait_for_code(listener, &state, &AtomicBool::new(false)));
        let get = |path: &str| {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut answer = String::new();
            _ = s.read_to_string(&mut answer);
            answer
        };
        assert!(get("/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(get("/callback?code=abc&state=good-state").starts_with("HTTP/1.1 200"));
        assert_eq!(waiter.join().unwrap().unwrap().expose(), "abc");

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port2 = listener.local_addr().unwrap().port();
        let state = Secret::new("good-state");
        let waiter =
            std::thread::spawn(move || wait_for_code(listener, &state, &AtomicBool::new(false)));
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port2)).unwrap();
        write!(s, "GET /callback?code=abc&state=evil HTTP/1.1\r\n\r\n").unwrap();
        assert!(matches!(
            waiter.join().unwrap(),
            Err(OAuthError::StateMismatch)
        ));

        // Cancel frees the wait without any request.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let waiter = std::thread::spawn(move || wait_for_code(listener, &Secret::new("s"), &flag));
        Canceller(cancelled).cancel();
        assert!(matches!(waiter.join().unwrap(), Err(OAuthError::Denied)));
    }

    #[test]
    fn authorize_url_has_pkce_and_state() {
        // Port 7613 may be in use on a dev machine; only check when it binds.
        let Ok(login) = gitlab_start("https://gitlab.com/") else {
            return;
        };
        let url = url::Url::parse(&login.authorize_url).unwrap();
        assert_eq!(url.host_str(), Some("gitlab.com"));
        assert_eq!(url.path(), "/oauth/authorize");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], challenge(login.verifier.expose()));
        assert_eq!(q["state"], login.state.expose());
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:7613/callback");
        assert_eq!(q["client_id"], GITLAB_CLIENT_ID);
        assert!(!login.authorize_url.contains(login.verifier.expose()));
    }
}
