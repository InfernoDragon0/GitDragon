//! The ACP client against an in-memory agent: streaming, permissions (default deny, allow once
//! only, cancel), the one-shot path, and shutdown.

mod common;

use ai::{Chat, Decision, Event, Item, Phase};
use common::{next, session};

/// Runs one prompt, answering the permission request with `answer` (None: cancel the turn).
fn turn(answer: Option<Decision>) -> Chat {
    let (session, events) = session();
    let mut chat = Chat {
        phase: Phase::Starting,
        ..Chat::default()
    };
    loop {
        let event = next(&events);
        let ready = matches!(event, Event::Ready { .. });
        let ask = match &event {
            Event::Permission(ask) => Some(ask.id),
            _ => None,
        };
        let ended = matches!(event, Event::TurnEnded { .. });
        chat.apply(event);
        if ready {
            chat.push_user("hi");
            session.prompt("hi");
        }
        if let Some(id) = ask {
            match answer {
                Some(decision) => {
                    assert!(chat.answer(id, decision));
                    session.answer(id, decision);
                }
                None => session.cancel(),
            }
        }
        if ended {
            return chat;
        }
    }
}

/// All agent text of the transcript (the permission card splits it in two items).
fn agent_text(chat: &Chat) -> String {
    chat.items
        .iter()
        .filter_map(|i| match i {
            Item::Agent(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn deny_answers_reject_once() {
    let chat = turn(Some(Decision::Deny));
    assert_eq!(chat.agent.as_deref(), Some("fake-agent"));
    assert_eq!(agent_text(&chat), "echo hi perm=no");
    assert_eq!(chat.phase, Phase::Ready);
}

#[test]
fn allow_answers_allow_once_never_always() {
    let chat = turn(Some(Decision::AllowOnce));
    assert_eq!(agent_text(&chat), "echo hi perm=once");
}

#[test]
fn cancel_answers_cancelled() {
    let chat = turn(None);
    assert_eq!(agent_text(&chat), "echo hi perm=cancelled");
    assert!(chat.pending_permissions().is_empty());
}

#[test]
fn one_shot_denies_permissions() {
    let (session, events) = session();
    let text = async_io::block_on(ai::session::collect(&session, &events, "msg".into()));
    assert_eq!(text.unwrap(), "echo msg perm=no");
}

#[test]
fn dropping_the_session_closes_it() {
    let (session, events) = session();
    assert!(matches!(next(&events), Event::Ready { .. }));
    drop(session);
    assert_eq!(next(&events), Event::Closed);
}
