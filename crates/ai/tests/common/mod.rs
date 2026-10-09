//! An in-memory ACP agent for tests: echoes the prompt, asks for one permission (offering
//! "always", "once" and "reject"), reports which option it got, then ends the turn.

#![allow(dead_code)]

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Implementation, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};

fn say(text: String) -> SessionNotification {
    SessionNotification::new(
        "s1",
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    )
}

/// Starts the fake agent on its own thread; returns the client's ends of the pipes (agent
/// stdout, agent stdin).
pub fn fake_agent() -> (piper::Reader, piper::Writer) {
    let (agent_out_r, agent_out_w) = piper::pipe(64 * 1024);
    let (agent_in_r, agent_in_w) = piper::pipe(64 * 1024);
    let agent_end = ByteStreams::new(agent_out_w, agent_in_r);
    std::thread::spawn(move || {
        async_io::block_on(async move {
            _ = Agent
                .builder()
                .on_receive_request(
                    async move |req: InitializeRequest, responder, _cx| {
                        responder.respond(
                            InitializeResponse::new(req.protocol_version)
                                .agent_info(Implementation::new("fake-agent", "1")),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |_req: NewSessionRequest, responder, _cx| {
                        responder.respond(NewSessionResponse::new("s1"))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                        let text = match req.prompt.first() {
                            Some(ContentBlock::Text(t)) => t.text.clone(),
                            _ => String::new(),
                        };
                        let conn = cx.clone();
                        cx.spawn(async move {
                            conn.send_notification(say(format!("echo {text}")))?;
                            let asked = conn
                                .send_request(RequestPermissionRequest::new(
                                    "s1",
                                    ToolCallUpdate::new(
                                        "t1",
                                        ToolCallUpdateFields::new().title(format!("Write {text}")),
                                    ),
                                    vec![
                                        PermissionOption::new(
                                            "always",
                                            "Always allow",
                                            PermissionOptionKind::AllowAlways,
                                        ),
                                        PermissionOption::new(
                                            "once",
                                            "Allow",
                                            PermissionOptionKind::AllowOnce,
                                        ),
                                        PermissionOption::new(
                                            "no",
                                            "Reject",
                                            PermissionOptionKind::RejectOnce,
                                        ),
                                    ],
                                ))
                                .block_task()
                                .await?;
                            let picked = match asked.outcome {
                                RequestPermissionOutcome::Selected(s) => s.option_id.0.to_string(),
                                _ => "cancelled".to_string(),
                            };
                            conn.send_notification(say(format!(" perm={picked}")))?;
                            responder.respond(PromptResponse::new(StopReason::EndTurn))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(agent_end)
                .await;
        })
    });
    (agent_out_r, agent_in_w)
}

/// A session against the fake agent.
pub fn session() -> (ai::Session, async_channel::Receiver<ai::Event>) {
    ai::Session::start_with(std::env::temp_dir(), |_| {
        let (reader, writer) = fake_agent();
        Ok((reader, writer, None, None))
    })
}

/// Next event, failing the test after 10 s.
pub fn next(events: &async_channel::Receiver<ai::Event>) -> ai::Event {
    async_io::block_on(futures_lite::future::or(
        async { events.recv().await.expect("event stream closed") },
        async {
            async_io::Timer::after(std::time::Duration::from_secs(10)).await;
            panic!("timed out waiting for an event")
        },
    ))
}
