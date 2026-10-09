//! A [`Transport`] that answers from recorded responses, for tests. Routes match on method,
//! full URL and optional request headers (`when`), in the order they were added; `times`
//! limits how often one answers. Every request is logged (auth by kind only, never the token).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::error::HostError;
use crate::http::{Auth, Method, Request, Response, Transport};

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub method: String,
    pub url: String,
    #[serde(default = "ok")]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Value,
    /// Request headers that must be present with these values.
    #[serde(default)]
    pub when: BTreeMap<String, String>,
    #[serde(default)]
    pub times: Option<u32>,
}

fn ok() -> u16 {
    200
}

impl Route {
    pub fn new(method: &str, url: &str, status: u16, body: Value) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            status,
            headers: BTreeMap::new(),
            body,
            when: BTreeMap::new(),
            times: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_ascii_lowercase(), value.into());
        self
    }

    pub fn when(mut self, name: &str, value: &str) -> Self {
        self.when.insert(name.to_ascii_lowercase(), value.into());
        self
    }

    pub fn once(mut self) -> Self {
        self.times = Some(1);
        self
    }
}

/// One request the transport saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// `Bearer` / `PrivateToken` / `Basic` when a token was sent.
    pub auth: Option<&'static str>,
    pub body: Option<Value>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
pub struct Replay {
    routes: Mutex<Vec<Route>>,
    seen: Mutex<Vec<Seen>>,
}

impl Replay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes from a JSON array of [`Route`]s. Panics on bad JSON (test fixtures).
    pub fn from_json(json: &str) -> Self {
        let routes: Vec<Route> = serde_json::from_str(json).expect("recorded routes");
        Self {
            routes: Mutex::new(routes),
            seen: Mutex::default(),
        }
    }

    /// Adds a route in front of the others.
    pub fn first(&self, route: Route) {
        self.routes.lock().unwrap().insert(0, route);
    }

    pub fn push(&self, route: Route) {
        self.routes.lock().unwrap().push(route);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Requests for `url` so far.
    pub fn count(&self, url: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.url == url)
            .count()
    }
}

#[async_trait]
impl Transport for Replay {
    async fn send(&self, request: Request) -> Result<Response, HostError> {
        let has = |name: &str, value: &str| {
            request
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case(name) && v == value)
        };
        self.seen.lock().unwrap().push(Seen {
            method: request.method,
            url: request.url.to_string(),
            headers: request.headers.clone(),
            auth: request.auth.as_ref().map(|a| match a {
                Auth::Bearer(_) => "Bearer",
                Auth::PrivateToken(_) => "PrivateToken",
                Auth::Basic(_) => "Basic",
            }),
            body: request
                .body
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok()),
        });
        let mut routes = self.routes.lock().unwrap();
        let route = routes.iter_mut().find(|r| {
            r.method.eq_ignore_ascii_case(request.method.as_str())
                && r.url == request.url.as_str()
                && r.when.iter().all(|(k, v)| has(k, v))
                && r.times != Some(0)
        });
        let Some(route) = route else {
            return Err(HostError::Network {
                host: request.url.host_str().unwrap_or_default().to_string(),
                message: format!(
                    "no recording for {} {}",
                    request.method.as_str(),
                    request.url
                ),
            });
        };
        if let Some(times) = &mut route.times {
            *times -= 1;
        }
        Ok(Response {
            status: route.status,
            headers: route
                .headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect(),
            body: if route.body.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&route.body).expect("JSON")
            },
        })
    }
}
