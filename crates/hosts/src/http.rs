//! The HTTP transport behind every provider.
//!
//! Hand-rolled REST over the shared reqwest stack rather than octocrab: one rustls client,
//! redirects followed here, and conditional requests per endpoint in [`crate::api`].

use std::fmt;
use std::time::Duration;

use accounts::Secret;
use async_trait::async_trait;
use url::Url;

use crate::error::HostError;

/// Largest response body accepted.
pub const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// How the token is sent. Debug never shows it.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Bearer(Secret),
    /// GitLab's `PRIVATE-TOKEN` header.
    PrivateToken(Secret),
    /// Basic with an empty user (Azure DevOps PATs).
    Basic(Secret),
}

impl Auth {
    pub fn header_name(&self) -> &'static str {
        match self {
            Auth::Bearer(_) | Auth::Basic(_) => "authorization",
            Auth::PrivateToken(_) => "private-token",
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Auth::Bearer(_) => "Bearer",
            Auth::PrivateToken(_) => "PrivateToken",
            Auth::Basic(_) => "Basic",
        };
        write!(f, "Auth::{kind}(\u{2022}\u{2022}\u{2022}\u{2022})")
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    /// Kept apart from `headers` so a redirect can drop it.
    pub auth: Option<Auth>,
    /// JSON body.
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Names lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request, no redirects followed.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: Request) -> Result<Response, HostError>;
}

/// Passes requests through and calls `on_rejected` when the host answers 401: the token
/// it carried is no good (expired, revoked).
pub struct WatchSignIn {
    inner: std::sync::Arc<dyn Transport>,
    on_rejected: Box<dyn Fn() + Send + Sync>,
}

impl WatchSignIn {
    pub fn new(
        inner: std::sync::Arc<dyn Transport>,
        on_rejected: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            on_rejected: Box::new(on_rejected),
        }
    }
}

#[async_trait]
impl Transport for WatchSignIn {
    async fn send(&self, request: Request) -> Result<Response, HostError> {
        let response = self.inner.send(request).await?;
        if response.status == 401 {
            (self.on_rejected)();
        }
        Ok(response)
    }
}

/// The real network, on `accounts::net`'s runtime.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    /// The shared client: built-in roots, redirects off.
    pub fn new() -> Self {
        Self {
            client: accounts::net::client().clone(),
        }
    }

    /// For a self-hosted server behind a private CA: trusts the PEM roots in addition to the
    /// built-in ones. Verification stays on; HTTPS only.
    pub fn with_extra_ca(pem: &[u8]) -> Result<Self, HostError> {
        let bad = |e: String| HostError::Config(format!("custom CA: {e}"));
        let certs = reqwest::Certificate::from_pem_bundle(pem).map_err(|e| bad(e.to_string()))?;
        if certs.is_empty() {
            return Err(bad("no certificates in the file".into()));
        }
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("GitDragon/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none());
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(|e| bad(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: Request) -> Result<Response, HostError> {
        let client = self.client.clone();
        let host = request.url.host_str().unwrap_or_default().to_string();
        let net = {
            let host = host.clone();
            move |e: reqwest::Error| HostError::Network {
                host: host.clone(),
                message: git_core::redact::redact(&e.to_string()).into_owned(),
            }
        };
        let task_host = host.clone();
        let task = accounts::net::spawn(async move {
            let host = task_host;
            let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
                .expect("static method");
            let mut builder = client.request(method, request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder = match &request.auth {
                Some(Auth::Bearer(t)) => builder.bearer_auth(t.expose()),
                Some(Auth::PrivateToken(t)) => builder.header("PRIVATE-TOKEN", t.expose()),
                Some(Auth::Basic(t)) => builder.basic_auth("", Some(t.expose())),
                None => builder,
            };
            if let Some(body) = request.body {
                // JSON unless the caller chose a type (`application/json-patch+json`).
                let typed = (request.headers.iter())
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
                if !typed {
                    builder = builder.header("content-type", "application/json");
                }
                builder = builder.body(body);
            }
            let mut response = builder.send().await.map_err(&net)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect();
            let too_large = || HostError::Decode {
                host: host.clone(),
                message: "response too large".into(),
            };
            if response
                .content_length()
                .is_some_and(|n| n as usize > MAX_BODY)
            {
                return Err(too_large());
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(&net)? {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_BODY {
                    return Err(too_large());
                }
            }
            Ok(Response {
                status,
                headers,
                body,
            })
        });
        task.await.map_err(|e| HostError::Network {
            host: host.clone(),
            message: e.to_string(),
        })?
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Sends `request`, following up to five redirects for GETs (renamed repos answer 301). The
/// token goes only to the origin it was meant for: a hop to another origin drops it for good.
/// Downgrades to plain HTTP and redirected writes are refused.
pub async fn send_following(
    transport: &dyn Transport,
    mut request: Request,
) -> Result<Response, HostError> {
    for _ in 0..=MAX_REDIRECTS {
        let response = transport.send(request.clone()).await?;
        if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let location = response
            .header("location")
            .ok_or_else(|| HostError::Redirect("redirect without a Location".into()))?;
        let next = request
            .url
            .join(location)
            .map_err(|_| HostError::Redirect("bad Location".into()))?;
        let secure = request.url.scheme() == "https";
        if !matches!(next.scheme(), "https" | "http") || (secure && next.scheme() != "https") {
            return Err(HostError::Redirect(format!(
                "{} would leave HTTPS",
                next.host_str().unwrap_or("the target")
            )));
        }
        if request.method != Method::Get {
            return Err(HostError::Redirect(
                "the server moved this endpoint; refresh and try again".into(),
            ));
        }
        if !same_origin(&request.url, &next) {
            request.auth = None;
        }
        request.url = next;
    }
    Err(HostError::Redirect("too many redirects".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Answers(u16);

    #[async_trait]
    impl Transport for Answers {
        async fn send(&self, _: Request) -> Result<Response, HostError> {
            Ok(Response {
                status: self.0,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn watch_sign_in_reports_401_only() {
        let seen = |status| {
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            let watched = WatchSignIn::new(Arc::new(Answers(status)), move || {
                counter.fetch_add(1, Ordering::Relaxed);
            });
            let request = Request {
                method: Method::Get,
                url: Url::parse("https://api.github.com/user").unwrap(),
                headers: Vec::new(),
                auth: None,
                body: None,
            };
            let response = accounts::net::block_on(watched.send(request)).unwrap();
            assert_eq!(response.status, status, "passed through");
            hits.load(Ordering::Relaxed)
        };
        assert_eq!(seen(401), 1);
        assert_eq!(seen(200), 0);
        assert_eq!(seen(403), 0);
        assert_eq!(seen(404), 0);
    }

    #[test]
    fn auth_debug_is_masked() {
        let auth = Auth::Bearer(Secret::new("ghp_supersecret"));
        assert!(!format!("{auth:?}").contains("supersecret"));
        let request = Request {
            method: Method::Get,
            url: Url::parse("https://api.github.com/user").unwrap(),
            headers: vec![],
            auth: Some(auth),
            body: None,
        };
        assert!(!format!("{request:?}").contains("supersecret"));
    }

    #[test]
    fn custom_ca_must_hold_certificates() {
        assert!(HttpTransport::with_extra_ca(b"not a pem").is_err());
        assert!(HttpTransport::with_extra_ca(b"").is_err());
    }

    /// P0 guard: nothing in this crate may switch certificate checks off.
    #[test]
    fn tls_verification_never_disabled() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        // Split so this file does not match itself.
        let forbidden = [
            ["danger", "_accept_invalid"].concat(),
            ["tls_", "danger"].concat(),
        ];
        let mut dirs = vec![std::path::PathBuf::from(dir)];
        let mut files = Vec::new();
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            for word in &forbidden {
                assert!(
                    !text.contains(word.as_str()),
                    "{} uses {word}",
                    path.display()
                );
            }
        }
    }
}
