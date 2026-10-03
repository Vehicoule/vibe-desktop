//! E2E: spawn the fixture binary and drive a full turn incl. the approval
//! callback flow. This is the same path the desktop app takes.

use std::path::PathBuf;
use std::time::Duration;

use vibe_protocol::client::Connection;
use vibe_protocol::models::*;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vibe-fixture"))
}

fn info() -> ClientInfo {
    ClientInfo {
        name: "test".into(),
        version: "0".into(),
        title: None,
        entrypoint: "test".into(),
        terminal_emulator: "test".into(),
    }
}

fn caps() -> ClientCapabilities {
    ClientCapabilities {
        callback_kinds: vec!["approval".into(), "user_input".into()],
        client_tools: vec![],
        disabled_notifications: vec![],
    }
}

/// Collect events until `pred` matches or we time out.
async fn wait_for(
    rx: &mut futures::channel::mpsc::Receiver<ServerMessage>,
    pred: impl Fn(&ServerMessage) -> bool,
) -> Vec<ServerMessage> {
    use futures::StreamExt;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), rx.next()).await {
            Ok(Some(msg)) => {
                let hit = pred(&msg);
                seen.push(msg);
                if hit {
                    return seen;
                }
            }
            Ok(None) => panic!("server closed the stream early"),
            Err(_) => panic!("timed out waiting for event, seen: {seen:?}"),
        }
    }
    panic!("deadline reached without matching event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_turn_with_approval() {
    let mut conn = Connection::spawn(&fixture()).await.expect("spawn fixture");
    let init = conn.initialize(info(), caps()).await.expect("initialize");
    assert!(init.server_info.name.contains("vibe"));

    let sessions = conn
        .session_list(SessionListParams::default())
        .await
        .expect("session/list");
    assert_eq!(sessions.items.len(), 1);
    let all = conn
        .session_list(SessionListParams {
            include_archived: true,
            ..Default::default()
        })
        .await
        .expect("session/list archived");
    assert_eq!(all.items.len(), 2);

    let state = conn
        .session_start(SessionStartParams {
            agent_config: AgentConfig {
                cwd: Some("/tmp/demo".into()),
                ..Default::default()
            },
            history_limit: 50,
            idempotency_key: None,
            kind: None,
        })
        .await
        .expect("session/start");
    let session_id = state.session.id.clone();
    assert_eq!(state.session.status.label(), "idle");

    let turn = conn
        .turn_start(&session_id, "run the tests")
        .await
        .expect("turn/start");
    assert!(!turn.id.is_empty());

    let mut rx = conn.take_events().expect("events");
    // Wait for the callback/call server request (shell effect asks approval).
    let msgs = wait_for(
        &mut rx,
        |m| matches!(m, ServerMessage::Request { method, .. } if method == "callback/call"),
    )
    .await;
    let req = msgs
        .iter()
        .find_map(|m| match m {
            ServerMessage::Request { id, method, params } if method == "callback/call" => {
                Some((id.clone(), params.clone()))
            }
            _ => None,
        })
        .expect("callback request");
    let call: CallbackCallParams = serde_json::from_value(req.1).expect("callback params");
    let callback_id = call
        .callback
        .callback_id()
        .expect("callback entry")
        .to_string();

    // Delivery ack then the semantic answer.
    conn.respond(
        &req.0,
        CallbackCallResponse {
            callback_id: callback_id.clone(),
            accepted: true,
        },
    )
    .await
    .expect("ack");
    conn.callback_respond(
        &session_id,
        &callback_id,
        CallbackOutput::Approval {
            decision: ApprovalDecision::of(ApprovalDecision::APPROVE),
            feedback: None,
        },
    )
    .await
    .expect("callback/respond");

    // The turn must now run to completion.
    wait_for(
        &mut rx,
        |m| matches!(m, ServerMessage::Notification { method, .. } if method == "turn/completed"),
    )
    .await;

    // A state re-read is consistent (fixture serves the initial state).
    let state = conn.session_read(&session_id).await.expect("session/read");
    assert_eq!(state.session.id, session_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_returns_accepted() {
    let conn = Connection::spawn(&fixture()).await.expect("spawn");
    conn.initialize(info(), caps()).await.unwrap();
    let state = conn
        .session_start(SessionStartParams {
            agent_config: AgentConfig::default(),
            history_limit: 50,
            idempotency_key: None,
            kind: None,
        })
        .await
        .unwrap();
    let sid = state.session.id.clone();
    conn.turn_start(&sid, "work").await.unwrap();
    // interrupt is fire-and-forget correct: fixture accepts it.
    conn.turn_interrupt(&sid, "turn-1").await.unwrap();
    conn.session_stop(&sid).await.unwrap();
}
