//! One ACP session with the adapter, on its own thread (`async_io::block_on` drives the
//! protocol and the adapter's pipes); commands in and [`Event`]s out over `async_channel`s that
//! the UI awaits on the GPUI executor.
//!
//! The client side of ACP needed here is small (initialize, session/new, session/prompt,
//! session/cancel; session/update and session/request_permission from the agent), so it is a
//! ~250-line newline-delimited JSON-RPC loop over the lenient types in [`crate::wire`] rather
//! than the official SDK (`agent-client-protocol`), which adds ~2.9 MB to the release exe. The
//! tests run this client against an agent built with that SDK.
//!
//! Permission requests are parked until the user answers; anything left unanswered (turn over,
//! cancel, stop, UI gone) is answered `cancelled`. Nothing is ever approved without an explicit
//! [`Decision::AllowOnce`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};
use futures_lite::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use serde_json::{Value, json};

use crate::adapter::{self, AdapterCommand};
use crate::event::{self, clean};
use crate::model::{Decision, Event, PermissionAsk};
use crate::wire::{
    InitializeResponse, NewSessionResponse, PromptResponse, RequestPermission, SessionNotification,
};

/// Bytes of the adapter's stderr kept for error messages.
const STDERR_TAIL: usize = 2048;
/// JSON-RPC "method not found": our answer to client methods we do not offer (fs, terminal).
const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug)]
enum Command {
    Prompt(String),
    Cancel,
    Answer { id: u64, decision: Decision },
    Stop,
}

/// A running session. Dropping it stops the adapter.
pub struct Session {
    tx: Sender<Command>,
}

/// What [`Session::start_with`]'s connector returns: the agent's stdout and stdin, plus the
/// process (killed at the end) and its stderr (kept for error messages) when there is one.
pub type Connection<R, W> = (
    R,
    W,
    Option<async_process::Child>,
    Option<async_process::ChildStderr>,
);

impl Session {
    /// Spawns the adapter in `cwd` (the repository root) on a new thread and opens a session.
    /// Progress and output arrive on the returned receiver, starting with [`Event::Ready`] or
    /// an [`Event::Error`].
    pub fn start(cmd: AdapterCommand, cwd: PathBuf) -> (Self, Receiver<Event>) {
        Self::start_with(cwd, move |cwd| {
            let mut child = adapter::spawn(&cmd, cwd)?;
            let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
            let stderr = child.stderr.take();
            Ok((stdout, stdin, Some(child), stderr))
        })
    }

    /// [`Self::start`] over any byte streams (tests use an in-memory agent).
    pub fn start_with<R, W, F>(cwd: PathBuf, connect: F) -> (Self, Receiver<Event>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
        F: FnOnce(&Path) -> std::io::Result<Connection<R, W>> + Send + 'static,
    {
        let (tx, cmd_rx) = async_channel::unbounded();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        let spawned = std::thread::Builder::new()
            .name("gitdragon-acp".into())
            .spawn(move || {
                async_io::block_on(async move {
                    match connect(&cwd) {
                        Ok((reader, writer, child, stderr)) => {
                            run(reader, writer, child, stderr, cwd, cmd_rx, ev_tx).await
                        }
                        Err(err) => {
                            _ = ev_tx.try_send(Event::Error {
                                message: format!(
                                    "Could not start the adapter: {}",
                                    event::error_text(&err)
                                ),
                                fatal: true,
                            });
                            _ = ev_tx.try_send(Event::Closed);
                        }
                    }
                })
            });
        if let Err(err) = spawned {
            tracing::warn!(%err, "ACP thread failed to start");
        }
        (Self { tx }, ev_rx)
    }

    /// Sends a user prompt (one turn).
    pub fn prompt(&self, text: impl Into<String>) {
        _ = self.tx.try_send(Command::Prompt(text.into()));
    }

    /// Cancels the running turn; pending permission requests are answered `cancelled`.
    pub fn cancel(&self) {
        _ = self.tx.try_send(Command::Cancel);
    }

    /// Answers permission request `id`.
    pub fn answer(&self, id: u64, decision: Decision) {
        _ = self.tx.try_send(Command::Answer { id, decision });
    }

    /// Ends the session and the adapter.
    pub fn stop(&self) {
        _ = self.tx.try_send(Command::Stop);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn missing(what: &str) -> std::io::Error {
    std::io::Error::other(format!("adapter {what} unavailable"))
}

/// Who waits for the response to one of our requests.
enum Waiter {
    /// `call` awaits it.
    Reply(Sender<Result<Value, String>>),
    /// A prompt turn: the reader ends the turn when it arrives.
    Prompt,
}

/// State shared by the reader, the writer and the command loop (one thread, so contention is
/// nil; `Mutex` keeps it simple and `Send`).
struct Shared {
    out: Sender<String>,
    events: Sender<Event>,
    next_id: AtomicU64,
    waiters: Mutex<HashMap<u64, Waiter>>,
    /// Our permission id -> (agent's JSON-RPC id, the request).
    parked: Mutex<HashMap<u64, (Value, PermissionAsk)>>,
}

impl Shared {
    fn send(&self, message: Value) {
        _ = self.out.try_send(message.to_string());
    }

    fn respond(&self, id: Value, result: Result<Value, (i64, &str)>) {
        self.send(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        });
    }

    fn request(&self, method: &str, params: impl serde::Serialize, waiter: Waiter) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut w) = self.waiters.lock() {
            w.insert(id, waiter);
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Sends a request and waits for its result.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: impl serde::Serialize,
    ) -> Result<T, String> {
        let (tx, rx) = async_channel::bounded(1);
        self.request(method, params, Waiter::Reply(tx));
        let value = rx
            .recv()
            .await
            .map_err(|_| "the adapter closed the connection".to_string())??;
        serde_json::from_value(value).map_err(|e| format!("bad {method} response: {e}"))
    }

    fn notify(&self, method: &str, params: impl serde::Serialize) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Answers a parked permission request (unknown / already answered ids are ignored).
    fn answer(&self, id: u64, decision: Decision) {
        let parked = self.parked.lock().ok().and_then(|mut p| p.remove(&id));
        if let Some((rpc_id, ask)) = parked {
            self.respond(rpc_id, Ok(permission_result(ask.answer(decision))));
        }
    }

    /// Answers every parked permission request with `cancelled` (never approve by default).
    fn cancel_parked(&self) {
        let parked: Vec<_> = self
            .parked
            .lock()
            .map(|mut p| p.drain().collect())
            .unwrap_or_default();
        for (_, (rpc_id, _)) in parked {
            self.respond(rpc_id, Ok(permission_result(None)));
        }
    }

    /// Handles one line from the agent.
    fn incoming(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("non-JSON line from the adapter ignored");
            return;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            // A response to one of our requests.
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let waiter = self.waiters.lock().ok().and_then(|mut w| w.remove(&id));
                let result = match message.get("error") {
                    Some(err) => Err(rpc_error_text(err)),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                match waiter {
                    Some(Waiter::Reply(tx)) => _ = tx.try_send(result),
                    Some(Waiter::Prompt) => self.turn_ended(result),
                    None => {}
                }
            }
            (Some("session/update"), None) => {
                if let Ok(n) = serde_json::from_value::<SessionNotification>(params)
                    && let Some(event) = event::from_update(&n.update)
                {
                    _ = self.events.try_send(event);
                }
            }
            (Some("session/request_permission"), Some(rpc_id)) => {
                let Ok(request) = serde_json::from_value::<RequestPermission>(params) else {
                    self.respond(rpc_id, Err((-32602, "invalid params")));
                    return;
                };
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let ask = event::permission(id, &request);
                if let Ok(mut p) = self.parked.lock() {
                    p.insert(id, (rpc_id, ask.clone()));
                }
                if self.events.try_send(Event::Permission(ask)).is_err() {
                    // Nobody to ask: deny.
                    self.answer(id, Decision::Deny);
                }
            }
            // fs/*, terminal/* and anything else we did not advertise.
            (Some(_), Some(rpc_id)) => {
                self.respond(rpc_id, Err((METHOD_NOT_FOUND, "method not supported")))
            }
            _ => {}
        }
    }

    fn turn_ended(&self, result: Result<Value, String>) {
        self.cancel_parked();
        let event = match result
            .and_then(|v| serde_json::from_value::<PromptResponse>(v).map_err(|e| e.to_string()))
        {
            Ok(response) => Event::TurnEnded {
                reason: serde_json::to_value(response.stop_reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "end_turn".into()),
            },
            Err(err) => Event::Error {
                message: clean(&err),
                fatal: false,
            },
        };
        _ = self.events.try_send(event);
    }

    /// The connection is gone: fail every waiter.
    fn close(&self) {
        let waiters: Vec<_> = self
            .waiters
            .lock()
            .map(|mut w| w.drain().collect())
            .unwrap_or_default();
        for (_, waiter) in waiters {
            if let Waiter::Reply(tx) = waiter {
                _ = tx.try_send(Err("the adapter closed the connection".into()));
            }
        }
        self.parked.lock().map(|mut p| p.clear()).ok();
    }
}

/// A `session/request_permission` result: the chosen option, or `cancelled`.
fn permission_result(option: Option<String>) -> Value {
    match option {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

fn rpc_error_text(err: &Value) -> String {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let code = err.get("code").and_then(Value::as_i64).unwrap_or_default();
    let data = err
        .get("data")
        .filter(|d| !d.is_null())
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    event::error_text(&format!("{message} [{code}]{data}"))
}

async fn run<R, W>(
    reader: R,
    mut writer: W,
    child: Option<async_process::Child>,
    stderr: Option<async_process::ChildStderr>,
    cwd: PathBuf,
    cmd_rx: Receiver<Command>,
    ev_tx: Sender<Event>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = async_channel::unbounded::<String>();
    let shared = Arc::new(Shared {
        out: out_tx,
        events: ev_tx.clone(),
        next_id: AtomicU64::new(1),
        waiters: Mutex::default(),
        parked: Mutex::default(),
    });
    let tail = Arc::new(Mutex::new(String::new()));

    let write_loop = async {
        while let Ok(line) = out_rx.recv().await {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if let Err(err) = async {
                writer.write_all(&bytes).await?;
                writer.flush().await
            }
            .await
            {
                return Err(format!("writing to the adapter failed: {err}"));
            }
        }
        Ok(())
    };
    let read_loop = async {
        let mut lines = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line).await {
                Ok(0) => return Err("the adapter exited".to_string()),
                Ok(_) => shared.incoming(line.trim_end()),
                Err(err) => return Err(format!("reading from the adapter failed: {err}")),
            }
        }
    };
    let main_loop = async {
        let init: InitializeResponse = shared
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    // No fs / terminal: the agent uses its own tools (behind permission prompts).
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false
                    },
                    "clientInfo": {"name": "gitdragon", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let agent = init
            .agent_info
            .map(|i| i.title.unwrap_or(i.name))
            .unwrap_or_else(|| "agent".into());
        let session: NewSessionResponse = shared
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let sid = session.session_id;
        _ = ev_tx.try_send(Event::Ready {
            agent: clean(&agent),
        });
        while let Ok(command) = cmd_rx.recv().await {
            match command {
                Command::Prompt(text) => {
                    let request = json!({
                        "sessionId": sid,
                        "prompt": [{"type": "text", "text": text}]
                    });
                    shared.request("session/prompt", request, Waiter::Prompt);
                }
                Command::Cancel => {
                    shared.cancel_parked();
                    shared.notify("session/cancel", json!({"sessionId": sid}));
                }
                Command::Answer { id, decision } => shared.answer(id, decision),
                Command::Stop => break,
            }
        }
        Ok::<(), String>(())
    };
    let drain = drain_stderr(stderr, tail.clone());

    let result = futures_lite::future::or(
        main_loop,
        futures_lite::future::or(read_loop, futures_lite::future::or(write_loop, drain)),
    )
    .await;
    shared.close();
    if let Some(mut child) = child {
        _ = child.kill();
    }
    if let Err(err) = result {
        let mut message = clean(&err);
        let stderr = tail
            .lock()
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        if message.to_ascii_lowercase().contains("auth") {
            message.push_str(
                "\nSign in to Claude Code first: run `claude` in a terminal and use /login.",
            );
        }
        if !stderr.is_empty() {
            message.push_str("\n\nAdapter output:\n");
            message.push_str(&clean(&stderr));
        }
        _ = ev_tx.try_send(Event::Error {
            message,
            fatal: true,
        });
    }
    _ = ev_tx.try_send(Event::Closed);
}

/// Keeps the tail of the adapter's stderr; never finishes (the protocol decides).
async fn drain_stderr(
    stderr: Option<async_process::ChildStderr>,
    tail: Arc<Mutex<String>>,
) -> Result<(), String> {
    use futures_lite::AsyncReadExt as _;
    if let Some(mut stderr) = stderr {
        let mut buf = [0u8; 1024];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if let Ok(mut tail) = tail.lock() {
                tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                if tail.len() > STDERR_TAIL {
                    let mut cut = tail.len() - STDERR_TAIL;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        }
    }
    std::future::pending().await
}

/// Runs one prompt in a fresh session and returns the agent's final text (commit messages).
/// Permission requests are denied: a one-shot answer never runs tools on the user's behalf.
pub async fn one_shot(cmd: AdapterCommand, cwd: PathBuf, prompt: String) -> Result<String, String> {
    let (session, events) = Session::start(cmd, cwd);
    collect(&session, &events, prompt).await
}

/// Drives `session` through one prompt and returns the final agent text (see [`one_shot`]).
pub async fn collect(
    session: &Session,
    events: &Receiver<Event>,
    prompt: String,
) -> Result<String, String> {
    let mut chat = crate::model::Chat::default();
    let mut prompt = Some(prompt);
    while let Ok(event) = events.recv().await {
        match &event {
            Event::Ready { .. } => {
                if let Some(p) = prompt.take() {
                    session.prompt(p);
                }
            }
            Event::Permission(ask) => {
                // Not recorded: the answer is one message, not split around a prompt card.
                session.answer(ask.id, Decision::Deny);
                continue;
            }
            Event::TurnEnded { .. } => {
                chat.apply(event);
                return chat
                    .last_agent_text()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| "The agent gave no answer.".to_string());
            }
            Event::Error { message, .. } => return Err(message.clone()),
            Event::Closed => break,
            _ => {}
        }
        chat.apply(event);
    }
    Err("The adapter closed before answering.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> (Arc<Shared>, Receiver<String>, Receiver<Event>) {
        let (out, out_rx) = async_channel::unbounded();
        let (events, ev_rx) = async_channel::unbounded();
        let shared = Arc::new(Shared {
            out,
            events,
            next_id: AtomicU64::new(1),
            waiters: Mutex::default(),
            parked: Mutex::default(),
        });
        (shared, out_rx, ev_rx)
    }

    #[test]
    fn unsupported_client_methods_get_method_not_found() {
        let (shared, out, _ev) = shared();
        shared.incoming(r#"{"jsonrpc":"2.0","id":"r1","method":"fs/read_text_file","params":{}}"#);
        let reply: Value = serde_json::from_str(&out.try_recv().unwrap()).unwrap();
        assert_eq!(reply["id"], "r1");
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn permission_without_listener_is_denied() {
        let (shared, out, ev) = shared();
        drop(ev);
        shared.incoming(
            r#"{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{
                "sessionId":"s","toolCall":{"toolCallId":"t"},
                "options":[{"optionId":"ok","name":"Allow","kind":"allow_once"}]}}"#,
        );
        let reply: Value = serde_json::from_str(&out.try_recv().unwrap()).unwrap();
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn rpc_errors_are_readable() {
        let text = rpc_error_text(&json!({"code": -32000, "message": "Authentication required"}));
        assert_eq!(text, "Authentication required [-32000]");
    }
}
