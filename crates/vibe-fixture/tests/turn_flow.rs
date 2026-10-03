//! E2E: spawn the fixture binary and drive a full turn incl. the approval
//! callback flow. This is the same path the desktop app takes.

use std::path::PathBuf;
use std::time::Duration;

use vibe_protocol::client::{ClientError, Connection};
use vibe_protocol::models::*;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vibe-fixture"))
}

/// Fresh isolated session store per test — fixture processes persist
/// sessions under `VIBE_FIXTURE_STORE` and share it across processes,
/// so tests must not leak state into each other.
fn store() -> (PathBuf, Vec<(String, String)>) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "vibe-fx-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let envs = vec![(
        "VIBE_FIXTURE_STORE".to_string(),
        dir.to_string_lossy().to_string(),
    )];
    (dir, envs)
}

async fn spawn_fixture(envs: &[(String, String)]) -> Connection {
    Connection::spawn_with_env(&fixture(), envs)
        .await
        .expect("spawn fixture")
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
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
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
    // Kill the fixture before deleting its store so no scripted task can
    // persist into a directory that no longer exists.
    drop(conn);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_returns_accepted() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
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
    let turn = conn.turn_start(&sid, "work").await.unwrap();
    // session.status must project the live turn — clients gate
    // steer/interrupt on `active_turn_id` (status was idle-only before,
    // which deaded both controls against the fixture).
    tokio::time::sleep(Duration::from_millis(80)).await;
    let st = conn.session_read(&sid).await.unwrap();
    assert_eq!(
        st.session.status,
        vibe_protocol::models::PublicSessionStatus::Running {
            active_turn_id: turn.id.clone()
        }
    );
    // interrupt is fire-and-forget correct: fixture accepts it when the
    // expected turn id matches (upstream rejects a stale id).
    conn.turn_interrupt(&sid, &turn.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = conn.session_read(&sid).await.unwrap();
    assert_eq!(
        st.session.status,
        vibe_protocol::models::PublicSessionStatus::Idle
    );
    conn.session_stop(&sid).await.unwrap();
    drop(conn);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fork made on one fixture process must be resumable from a different
/// process — the real app-server persists sessions on disk, the fixture
/// mirrors that with a shared file store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_resumes_across_processes() {
    let (dir, envs) = store();
    let conn_a = spawn_fixture(&envs).await;
    conn_a.initialize(info(), caps()).await.unwrap();
    let state = conn_a
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
    let parent_id = state.session.id.clone();

    let forked = conn_a
        .session_fork(&parent_id, None, 50)
        .await
        .expect("session/fork");
    assert_ne!(forked.session.id, parent_id);
    assert_eq!(
        forked.session.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );

    // A second process (the fork tab's own server) sees the child via the
    // shared store — previously it fabricated an empty session instead.
    let conn_b = spawn_fixture(&envs).await;
    conn_b.initialize(info(), caps()).await.unwrap();
    let resumed = conn_b
        .session_resume(&forked.session.id, AgentConfig::default(), 50)
        .await
        .expect("session/resume");
    assert_eq!(resumed.session.id, forked.session.id);
    assert_eq!(
        resumed.session.parent_session_id.as_deref(),
        Some(parent_id.as_str())
    );

    // The child also appears in a different process's catalog.
    let list = conn_b
        .session_list(SessionListParams::default())
        .await
        .expect("session/list");
    assert!(list.items.iter().any(|s| s.id == forked.session.id));
    drop(conn_a);
    drop(conn_b);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// `session/fork` must reject unsafe source ids with a JSON-RPC error,
/// not a malformed null result — the wire client should surface it as
/// `ClientError::Protocol`, not a decode failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_rejects_unsafe_source_id() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    for bad in ["../escape", "a/b", ".."] {
        let err = conn
            .session_fork(bad, None, 50)
            .await
            .expect_err("unsafe source id must be rejected");
        assert!(
            matches!(err, ClientError::Protocol { .. }),
            "expected protocol error for {bad}, got {err}"
        );
    }
    // Unknown (but safe) source ids reject the same way.
    let err = conn
        .session_fork("missing-1234", None, 50)
        .await
        .expect_err("unknown source id must be rejected");
    assert!(matches!(err, ClientError::Protocol { .. }));
    drop(conn);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
