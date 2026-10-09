//! [`ApiClient`]: one host API base with auth, a per-endpoint cache and rate-limit tracking.
//!
//! - GETs are cached by URL. Within the TTL the cache answers without a request; after it, a
//!   conditional request (`If-None-Match` / `If-Modified-Since`) revalidates, and a 304 costs
//!   no GitHub quota.
//! - Rate limits come from `x-ratelimit-*` / `ratelimit-*` / `retry-after`. While blocked, GETs
//!   answer from the cache (marked stale) or fail with [`HostError::RateLimited`] without
//!   touching the network.
//! - Per-repo invalidation drops every cached URL under `repos/<owner>/<repo>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::error::HostError;
use crate::http::{Auth, Method, Request, Response, Transport, send_following};

/// Fallback wait when a host says "rate limited" without saying until when.
const DEFAULT_BLOCK: Duration = Duration::from_secs(60);
const MAX_MESSAGE: usize = 300;

/// The host's API quota as last reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    pub reset_at: Option<SystemTime>,
    /// No requests until then (quota spent, 429, secondary limit).
    pub blocked_until: Option<SystemTime>,
}

impl RateLimit {
    pub fn blocked(&self, now: SystemTime) -> Option<SystemTime> {
        self.blocked_until.filter(|t| *t > now)
    }
}

/// A GET answer.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub body: Arc<[u8]>,
    /// The `Link` header (or `x-next-page`) points at another page.
    pub next_page: bool,
    /// Served from the cache because the host could not be asked (rate limit, offline).
    pub stale: bool,
}

#[derive(Clone)]
struct Entry {
    fetched: Instant,
    etag: Option<String>,
    last_modified: Option<String>,
    body: Arc<[u8]>,
    next_page: bool,
}

impl Entry {
    fn answer(&self, stale: bool) -> Fetched {
        Fetched {
            body: self.body.clone(),
            next_page: self.next_page,
            stale,
        }
    }
}

pub struct ApiClient {
    transport: Arc<dyn Transport>,
    base: Url,
    host: String,
    headers: Vec<(String, String)>,
    auth: RwLock<Option<Auth>>,
    ttl: Duration,
    cache: Mutex<HashMap<String, Entry>>,
    rate: Mutex<RateLimit>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ApiClient {
    /// `base` is the API root (`https://api.github.com`, `https://ghe/api/v3`); `headers` go
    /// on every request (Accept, API version).
    pub fn new(transport: Arc<dyn Transport>, base: Url, headers: Vec<(String, String)>) -> Self {
        Self {
            host: base.host_str().unwrap_or_default().to_string(),
            transport,
            base,
            headers,
            auth: RwLock::new(None),
            ttl: Duration::from_secs(30),
            cache: Mutex::default(),
            rate: Mutex::default(),
        }
    }

    /// How long a cached GET is used without asking the host (0 = always revalidate).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn set_auth(&self, auth: Option<Auth>) {
        *self.auth.write().unwrap_or_else(PoisonError::into_inner) = auth;
        lock(&self.cache).clear();
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn rate_limit(&self) -> RateLimit {
        *lock(&self.rate)
    }

    /// `base/segment/segment?query`, each segment percent-encoded.
    pub fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("http(s) base")
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    /// Drops cached answers at or below `segments` (e.g. `["repos", owner, repo]`).
    pub fn invalidate(&self, segments: &[&str]) {
        self.invalidate_url(&self.url(segments, &[]));
    }

    /// Drops cached answers at or below `url` (also cached POSTs to it, see
    /// [`Self::post_cached`]); for endpoints outside the API base (GHES GraphQL).
    pub fn invalidate_url(&self, url: &Url) {
        let prefix = url.to_string();
        lock(&self.cache).retain(|key, _| {
            let rest = key.strip_prefix(&prefix);
            !rest.is_some_and(|r| {
                r.is_empty() || r.starts_with('/') || r.starts_with('?') || r.starts_with('#')
            })
        });
    }

    pub fn clear_cache(&self) {
        lock(&self.cache).clear();
    }

    fn request(&self, method: Method, url: Url) -> Request {
        Request {
            method,
            url,
            headers: self.headers.clone(),
            auth: self
                .auth
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            body: None,
        }
    }

    fn rate_limited(&self, until: SystemTime) -> HostError {
        HostError::RateLimited {
            host: self.host.clone(),
            until: Some(until),
        }
    }

    /// Cached, conditional GET.
    pub async fn get(&self, url: Url) -> Result<Fetched, HostError> {
        let key = url.to_string();
        let cached = lock(&self.cache).get(&key).cloned();
        if let Some(entry) = &cached
            && entry.fetched.elapsed() < self.ttl
        {
            return Ok(entry.answer(false));
        }
        if let Some(until) = self.rate_limit().blocked(SystemTime::now()) {
            return match &cached {
                Some(entry) => Ok(entry.answer(true)),
                None => Err(self.rate_limited(until)),
            };
        }
        let mut request = self.request(Method::Get, url);
        if let Some(entry) = &cached {
            if let Some(etag) = &entry.etag {
                request.headers.push(("if-none-match".into(), etag.clone()));
            }
            if let Some(modified) = &entry.last_modified {
                request
                    .headers
                    .push(("if-modified-since".into(), modified.clone()));
            }
        }
        let response = match send_following(self.transport.as_ref(), request).await {
            Ok(response) => response,
            Err(err @ HostError::Network { .. }) => {
                return match &cached {
                    Some(entry) => Ok(entry.answer(true)),
                    None => Err(err),
                };
            }
            Err(err) => return Err(err),
        };
        self.note_rate(&response);
        match response.status {
            304 => {
                let mut cache = lock(&self.cache);
                match (cache.get_mut(&key), cached) {
                    (Some(entry), _) => {
                        entry.fetched = Instant::now();
                        Ok(entry.answer(false))
                    }
                    // Invalidated while the request was in flight: still a valid answer.
                    (None, Some(entry)) => Ok(entry.answer(false)),
                    (None, None) => Err(self.decode_error("304 without a cached answer")),
                }
            }
            200..=299 => {
                let entry = Entry {
                    fetched: Instant::now(),
                    etag: response.header("etag").map(str::to_owned),
                    last_modified: response.header("last-modified").map(str::to_owned),
                    next_page: has_next_page(&response),
                    body: response.body.into(),
                };
                let answer = entry.answer(false);
                lock(&self.cache).insert(key, entry);
                Ok(answer)
            }
            _ => Err(self.error(&response)),
        }
    }

    /// A read-only POST (GraphQL query, Azure WIQL) cached by URL and body for the TTL. No
    /// revalidation: these endpoints send no ETag. While rate limited the cached answer is
    /// served stale, like [`Self::get`].
    pub async fn post_cached(&self, url: Url, body: &Value) -> Result<Fetched, HostError> {
        let key = format!("{url}#post:{body}");
        let cached = lock(&self.cache).get(&key).cloned();
        if let Some(entry) = &cached
            && entry.fetched.elapsed() < self.ttl
        {
            return Ok(entry.answer(false));
        }
        if let Some(until) = self.rate_limit().blocked(SystemTime::now()) {
            return match &cached {
                Some(entry) => Ok(entry.answer(true)),
                None => Err(self.rate_limited(until)),
            };
        }
        match self.send_json(Method::Post, url, body).await {
            Ok(answer) => {
                let entry = Entry {
                    fetched: Instant::now(),
                    etag: None,
                    last_modified: None,
                    next_page: false,
                    body: answer.into(),
                };
                let fetched = entry.answer(false);
                lock(&self.cache).insert(key, entry);
                Ok(fetched)
            }
            Err(err @ (HostError::Network { .. } | HostError::RateLimited { .. })) => match &cached
            {
                Some(entry) => Ok(entry.answer(true)),
                None => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    /// Uncached write (POST / PATCH / PUT) with a JSON body; returns the response body.
    pub async fn send_json(
        &self,
        method: Method,
        url: Url,
        body: &Value,
    ) -> Result<Vec<u8>, HostError> {
        self.send_json_as(method, url, body, None).await
    }

    /// [`send_json`](Self::send_json) with another `content-type` (Azure's
    /// `application/json-patch+json`).
    pub async fn send_json_as(
        &self,
        method: Method,
        url: Url,
        body: &Value,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>, HostError> {
        if let Some(until) = self.rate_limit().blocked(SystemTime::now()) {
            return Err(self.rate_limited(until));
        }
        let mut request = self.request(method, url);
        if let Some(content_type) = content_type {
            request
                .headers
                .push(("content-type".into(), content_type.into()));
        }
        request.body = Some(serde_json::to_vec(body).expect("JSON value"));
        let response = send_following(self.transport.as_ref(), request).await?;
        self.note_rate(&response);
        if (200..300).contains(&response.status) {
            Ok(response.body)
        } else {
            Err(self.error(&response))
        }
    }

    pub fn decode<T: DeserializeOwned>(&self, body: &[u8]) -> Result<T, HostError> {
        serde_json::from_slice(body).map_err(|e| self.decode_error(&e.to_string()))
    }

    fn decode_error(&self, message: &str) -> HostError {
        HostError::Decode {
            host: self.host.clone(),
            message: message.to_string(),
        }
    }

    fn note_rate(&self, response: &Response) {
        let number = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|n| response.header(n))
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let mut rate = lock(&self.rate);
        let remaining = number(["x-ratelimit-remaining", "ratelimit-remaining"]);
        if remaining.is_some() {
            rate.limit = number(["x-ratelimit-limit", "ratelimit-limit"]).map(clamp_u32);
            rate.remaining = remaining.map(clamp_u32);
            rate.reset_at = number(["x-ratelimit-reset", "ratelimit-reset"])
                .map(|s| UNIX_EPOCH + Duration::from_secs(s));
        }
        let now = SystemTime::now();
        rate.blocked_until = match (rate.remaining, rate.reset_at) {
            (Some(0), Some(reset)) if remaining.is_some() && reset > now => Some(reset),
            _ => rate.blocked_until.filter(|t| *t > now),
        };
    }

    /// Maps a failed response; a rate-limit answer also blocks further calls until it ends.
    fn error(&self, response: &Response) -> HostError {
        let message = error_message(&response.body);
        let status = response.status;
        let rate = self.rate_limit();
        let limited = status == 429
            || (status == 403
                && (response.header("retry-after").is_some()
                    || rate.remaining == Some(0)
                    || message.to_ascii_lowercase().contains("rate limit")));
        if limited {
            let now = SystemTime::now();
            let until = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| now + Duration::from_secs(s))
                .or(rate.reset_at.filter(|t| *t > now))
                .unwrap_or(now + DEFAULT_BLOCK);
            lock(&self.rate).blocked_until = Some(until);
            return self.rate_limited(until);
        }
        let host = self.host.clone();
        match status {
            401 => HostError::Unauthorized { host },
            403 => HostError::Forbidden { host, message },
            404 | 410 => HostError::NotFound { host },
            400 | 409 | 422 => HostError::Invalid { host, message },
            _ => HostError::Server {
                host,
                status,
                message,
            },
        }
    }
}

fn clamp_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn has_next_page(response: &Response) -> bool {
    let link = response
        .header("link")
        .is_some_and(|l| l.split(',').any(|part| part.contains("rel=\"next\"")));
    link || response
        .header("x-next-page")
        .is_some_and(|n| !n.trim().is_empty())
}

/// `message` (+ GitHub's `errors[].message` / GitLab's `error`) from an error body, redacted
/// and capped.
fn error_message(body: &[u8]) -> String {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        let text = String::from_utf8_lossy(body);
        return cap(git_core::redact::redact(text.trim()).as_ref());
    };
    let mut parts: Vec<String> = Vec::new();
    for key in ["message", "error", "error_description"] {
        match json.get(key) {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(other @ (Value::Object(_) | Value::Array(_))) => parts.push(other.to_string()),
            _ => {}
        }
    }
    if let Some(Value::Array(errors)) = json.get("errors") {
        for e in errors {
            let detail = e.get("message").and_then(Value::as_str).map_or_else(
                || {
                    let field = e.get("field").and_then(Value::as_str).unwrap_or_default();
                    let code = e.get("code").and_then(Value::as_str).unwrap_or_default();
                    format!("{field} {code}").trim().to_string()
                },
                str::to_owned,
            );
            if !detail.is_empty() {
                parts.push(detail);
            }
        }
    }
    cap(git_core::redact::redact(&parts.join(": ")).as_ref())
}

fn cap(text: &str) -> String {
    match text.char_indices().nth(MAX_MESSAGE) {
        Some((i, _)) => format!("{}\u{2026}", &text[..i]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages() {
        let body = br#"{"message":"Validation Failed","errors":[{"resource":"PullRequest","code":"custom","message":"A pull request already exists for o:b."},{"field":"base","code":"invalid"}]}"#;
        assert_eq!(
            error_message(body),
            "Validation Failed: A pull request already exists for o:b.: base invalid"
        );
        assert_eq!(error_message(b"  Bad gateway \n"), "Bad gateway");
        let long = "x".repeat(400);
        assert_eq!(
            error_message(long.as_bytes()).chars().count(),
            MAX_MESSAGE + 1
        );
    }

    #[test]
    fn next_page_from_link() {
        let response = |headers: Vec<(&str, &str)>| Response {
            status: 200,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: vec![],
        };
        assert!(has_next_page(&response(vec![(
            "link",
            "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=5>; rel=\"last\""
        )])));
        assert!(!has_next_page(&response(vec![(
            "link",
            "<https://api.github.com/x?page=1>; rel=\"prev\""
        )])));
        assert!(has_next_page(&response(vec![("x-next-page", "3")])));
        assert!(!has_next_page(&response(vec![("x-next-page", "")])));
    }
}
