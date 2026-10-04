//! E2E for M2a: turn queue, session ops (pin/archive/rename/delete),
//! rewind (fork + inplace), workspace trust, and history paging —
//! all through the real protocol client against the fixture binary.

use std::path::PathBuf;
use std::time::Duration;

use vibe_protocol::client::Connection;
use vibe_protocol::models::*;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vibe-fixture"))
}

fn store() -> (PathBuf, Vec<(String, String)>) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "vibe-fx-ops-{}-{}",
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

/// Start a session and run one scripted turn to the approval block,
/// then answer it so the turn completes. Returns (session_id, rx).
async fn session_with_completed_turn(
    conn: &mut Connection,
) -> (String, futures::channel::mpsc::Receiver<ServerMessage>) {
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
    let mut rx = conn.take_events().expect("events");

    conn.turn_start(&session_id, "run the tests")
        .await
        .expect("turn/start");
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
        .expect("callback id")
        .to_string();
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
    wait_for(
        &mut rx,
        |m| matches!(m, ServerMessage::Notification { method, .. } if method == "turn/completed"),
    )
    .await;
    (session_id, rx)
}

async fn cleanup(conn: Connection, dir: &std::path::Path) {
    drop(conn);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Enqueue while a turn runs → item lands; remove → gone; interrupt pauses
/// the queue; resume promotes the head into a running turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_enqueue_remove_resume() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();

    let state = conn
        .session_start(SessionStartParams {
            agent_config: AgentConfig {
                cwd: Some("/tmp/q".into()),
                ..Default::default()
            },
            history_limit: 50,
            idempotency_key: None,
            kind: None,
        })
        .await
        .unwrap();
    let sid = state.session.id.clone();
    let mut rx = conn.take_events().unwrap();

    // A running turn holds the queue.
    let turn = conn.turn_start(&sid, "long work").await.unwrap();
    let item = conn.turn_enqueue(&sid, "queued follow-up").await.unwrap();
    let queue = conn.turn_queue_read(&sid).await.expect("queue/read");
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].id, item);
    assert!(!queue.paused);

    conn.turn_queue_remove(&sid, &item).await.unwrap();
    // Queue changes arrive as turn/queueUpdated notifications; the
    // enqueue already emitted one, so wait for the empty-items update.
    let msgs = wait_for(&mut rx, |m| {
        matches!(m, ServerMessage::Notification { method, params, .. }
            if method == "turn/queueUpdated"
                && params["queue"]["items"].as_array().is_some_and(|i| i.is_empty()))
    })
    .await;
    let last = msgs
        .iter()
        .rev()
        .find_map(|m| match m {
            ServerMessage::Notification { params, .. } => Some(params),
            _ => None,
        })
        .expect("queueUpdated params");
    assert_eq!(last["queue"]["items"].as_array().unwrap().len(), 0);
    let queue = conn.turn_queue_read(&sid).await.unwrap();
    assert!(queue.items.is_empty());

    // A stale expectedTurnId is rejected without side effects — the
    // running turn and the queue must survive (upstream raises
    // "Active turn does not match expectedTurnId").
    assert!(conn.turn_interrupt(&sid, "bogus-turn").await.is_err());
    let queue = conn.turn_queue_read(&sid).await.unwrap();
    assert!(!queue.paused);

    // Interrupt pauses the queue (upstream _after_turn_terminal).
    conn.turn_interrupt(&sid, &turn.id).await.unwrap();
    wait_for(&mut rx, |m| {
        matches!(m, ServerMessage::Notification { method, params, .. }
            if method == "turn/queueUpdated" && params["queue"]["paused"] == true)
    })
    .await;
    let queue = conn.turn_queue_read(&sid).await.unwrap();
    assert!(queue.paused);

    // Enqueue while paused → stays queued; resume promotes it.
    let item2 = conn.turn_enqueue(&sid, "after resume").await.unwrap();
    let queue = conn.turn_queue_read(&sid).await.unwrap();
    assert_eq!(queue.items.len(), 1);
    conn.turn_queue_resume(&sid).await.unwrap();
    let msgs = wait_for(&mut rx, |m| {
        matches!(m, ServerMessage::Notification { method, params, .. }
            if method == "turn/started"
                && params["turn"]["queueItemId"] == item2)
    })
    .await;
    let _ = msgs;
    let queue = conn.turn_queue_read(&sid).await.unwrap();
    assert!(queue.items.is_empty());
    assert!(!queue.paused);

    cleanup(conn, &dir).await;
}

/// pin/archive/rename/delete mutate the catalog and stay visible in
/// session/list (stored rows win over the fixture's demo rows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_ops_mutations() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();

    let sid = "saved-aaaa1111"; // demo row materialized into the store on op
    conn.session_pin(sid, true).await.unwrap();
    conn.session_rename(sid, "renamed demo").await.unwrap();
    let list = conn
        .session_list(SessionListParams::default())
        .await
        .unwrap();
    let row = list.items.iter().find(|s| s.id == sid).unwrap();
    assert!(row.pinned_at.is_some());
    assert_eq!(row.title.as_deref(), Some("renamed demo"));

    conn.session_pin(sid, false).await.unwrap();
    conn.session_archive(sid, true).await.unwrap();
    let list = conn
        .session_list(SessionListParams::default())
        .await
        .unwrap();
    assert!(!list.items.iter().any(|s| s.id == sid));
    let all = conn
        .session_list(SessionListParams {
            include_archived: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(all.items.iter().any(|s| s.id == sid));

    conn.request("session/delete", serde_json::json!({"sessionId": sid}))
        .await
        .unwrap();
    let all = conn
        .session_list(SessionListParams {
            include_archived: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!all.items.iter().any(|s| s.id == sid));

    cleanup(conn, &dir).await;
}

/// rewind/read reports the turn's effect; fork-rewind opens a child with
/// truncated history; inplace truncates this session in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewind_fork_and_inplace() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let state = conn.session_read(&sid).await.unwrap();
    let history = state.history.as_deref().unwrap_or(&[]);
    let user_entry = history
        .iter()
        .find(|e| matches!(e, PublicHistoryEntry::Message { role, .. } if role == "user"))
        .expect("user message in history");
    let entry_id = user_entry.id().unwrap().to_string();

    let preview = conn
        .session_rewind_read(&sid, &entry_id)
        .await
        .expect("rewind/read");
    assert!(preview.has_file_changes);
    assert!(!preview.paths.is_empty());

    // Fork rewind: parent untouched, child gets the truncated copy.
    let resp = conn
        .session_rewind(&sid, &entry_id, false, false)
        .await
        .expect("rewind fork");
    assert_ne!(resp.state.session.id, sid);
    assert_eq!(
        resp.state.session.parent_session_id.as_deref(),
        Some(sid.as_str())
    );
    let child_len = resp.state.history.as_deref().unwrap_or(&[]).len();
    assert!(child_len < history.len());
    let parent = conn.session_read(&sid).await.unwrap();
    assert_eq!(
        parent.history.as_deref().map(<[_]>::len),
        Some(history.len())
    );

    // Inplace: same session id, history truncated at the target.
    let resp = conn
        .session_rewind(&sid, &entry_id, false, true)
        .await
        .expect("rewind inplace");
    assert_eq!(resp.state.session.id, sid);
    assert_eq!(
        resp.state.history.as_deref().map(<[_]>::len),
        Some(child_len)
    );
    // Upstream carries the truncated state in the rewind RESPONSE, not a
    // /history patch (projection only applies session/updated to session
    // metadata). Nothing else should be emitted when the queue was empty.
    let state = conn.session_read(&sid).await.unwrap();
    assert_eq!(state.history.as_deref().map(<[_]>::len), Some(child_len));

    cleanup(conn, &dir).await;
}

/// Deleting a session with a running turn must stick: the orphaned
/// script keeps persisting, and each later write must bounce off the
/// tombstone instead of resurrecting the catalog row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_during_active_turn_stays_deleted() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();

    let state = conn
        .session_start(SessionStartParams {
            agent_config: AgentConfig {
                cwd: Some("/tmp/demo-delete".into()),
                ..Default::default()
            },
            history_limit: 50,
            idempotency_key: None,
            kind: None,
        })
        .await
        .unwrap();
    let sid = state.session.id.clone();
    conn.turn_start(&sid, "keep me busy").await.unwrap();

    conn.request("session/delete", serde_json::json!({"sessionId": sid}))
        .await
        .unwrap();

    // Let the orphaned turn run long enough to persist again (the
    // scripted turn bumps/persists on every emitted entry).
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let list = conn
        .session_list(SessionListParams {
            include_archived: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !list.items.iter().any(|s| s.id == sid),
        "tombstone must survive the orphaned turn's persists"
    );
    assert!(conn.session_read(&sid).await.is_err());

    cleanup(conn, &dir).await;
}

/// workspace/trust round-trip: untrusted → decision → trusted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_roundtrip() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let cwd = "/tmp/demo-trust";

    let status = conn.workspace_trust_status(Some(cwd)).await.unwrap();
    assert_eq!(status.status, "untrusted");
    let details = status.details.expect("details");
    assert!(details.available_decisions.iter().any(|d| d == "trust_cwd"));
    let cfg = conn
        .workspace_trust_untrusted_config(Some(cwd))
        .await
        .unwrap();
    assert_eq!(cfg.dirs, vec![cwd.to_string()]);

    conn.workspace_trust_decision("trust_cwd", Some(cwd), None)
        .await
        .unwrap();
    let status = conn.workspace_trust_status(Some(cwd)).await.unwrap();
    assert_eq!(status.status, "trusted");
    let cfg = conn
        .workspace_trust_untrusted_config(Some(cwd))
        .await
        .unwrap();
    assert!(cfg.dirs.is_empty());

    // Trust is file-backed: a second fixture process agrees without a
    // new prompt (matches the real settings file).
    let conn2 = spawn_fixture(&envs).await;
    conn2.initialize(info(), caps()).await.unwrap();
    let status = conn2.workspace_trust_status(Some(cwd)).await.unwrap();
    assert_eq!(status.status, "trusted");

    cleanup(conn, &dir).await;
    cleanup(conn2, &dir).await;
}

/// A small `historyLimit` windows the resume tail and exposes
/// `historyBeforeCursor`; `session/history/list` pages it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_paging() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let full = conn.session_read(&sid).await.unwrap();
    let total = full.history.as_deref().unwrap_or(&[]).len();
    assert!(total > 2, "scripted turn should produce several entries");

    // Resume with a 2-entry window: tail only + before-cursor.
    let conn2 = spawn_fixture(&envs).await;
    conn2.initialize(info(), caps()).await.unwrap();
    let windowed = conn2
        .session_resume(&sid, AgentConfig::default(), 2)
        .await
        .unwrap();
    let tail = windowed.history.as_deref().unwrap_or(&[]);
    assert_eq!(tail.len(), 2);
    assert!(windowed.history_before_cursor.is_some());

    let page = conn2
        .session_history_list(
            &sid,
            None,
            PageRequest {
                cursor: windowed.history_before_cursor.clone(),
                limit: 50,
                direction: "backward".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), total - 2);
    assert!(page.previous_cursor.is_none());
    // Earlier + tail reconstructs the full history.
    let mut combined: Vec<_> = page
        .items
        .iter()
        .map(|e| e.id().unwrap().to_string())
        .collect();
    combined.extend(tail.iter().map(|e| e.id().unwrap().to_string()));
    let full_ids: Vec<_> = full
        .history
        .as_deref()
        .unwrap()
        .iter()
        .map(|e| e.id().unwrap().to_string())
        .collect();
    assert_eq!(combined, full_ids);

    cleanup(conn, &dir).await;
    cleanup(conn2, &dir).await;
}

// ── M2b: voice config + narration summarize ─────────────────────────────────

#[tokio::test]
async fn config_read_returns_voice_views() {
    let (dir, envs) = store();
    let conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();

    let cfg = conn.config_read().await.unwrap();
    assert!(cfg.voice_mode_enabled);
    assert!(cfg.narrator_enabled);
    let speech = cfg.speech.expect("fixture sends a speech view");
    assert_eq!(speech.model.name, "voxtral-mini-tts-latest");
    assert_eq!(speech.provider.api_base, "https://tts.fixture.invalid");
    let transcription = cfg.transcription.expect("fixture sends a transcription view");
    assert_eq!(
        transcription.model.encoding, "pcm_s16le",
        "transcription model should mirror upstream realtime encoding"
    );
    assert_eq!(transcription.model.target_streaming_delay_ms, 240);

    cleanup(conn, &dir).await;
}

#[tokio::test]
async fn narration_summarize_returns_summary() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let summary = conn
        .narration_summarize(&sid, "fix the bug", "fixed it")
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("Fixture narration: fix the bug"));

    cleanup(conn, &dir).await;
}

#[tokio::test]
async fn settings_roundtrip_reads_and_writes() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let cfg = conn.config_read().await.unwrap();
    assert_eq!(cfg.active_model.alias, "large");
    assert_eq!(cfg.models.len(), 2);
    assert!(cfg.enable_notifications);

    let fields = conn.config_fields_read(&sid).await.unwrap();
    assert_eq!(fields.fields.len(), 3);
    let notif = fields
        .fields
        .iter()
        .find(|f| f.path == "enable_notifications")
        .unwrap();
    assert_eq!(notif.kind, "bool");
    assert_eq!(fields.targets, vec!["user", "project"]);

    let write = conn
        .config_write(
            &sid,
            vec![vibe_protocol::models::ConfigWriteOp {
                op: "set".into(),
                path: "enable_notifications".into(),
                value: Some(serde_json::Value::Bool(false)),
                target_layer: None,
            }],
        )
        .await
        .unwrap();
    assert!(!write.rejected);

    // The write is observable: a later read reflects the stored value.
    let after = conn.config_fields_read(&sid).await.unwrap();
    let notif = after
        .fields
        .iter()
        .find(|f| f.path == "enable_notifications")
        .unwrap();
    assert_eq!(notif.value, serde_json::Value::Bool(false));

    let model = conn.config_model_write(&sid, "small", None).await.unwrap();
    assert_eq!(model.status.as_deref(), Some("applied"));

    let agents = conn.agents_list(&sid).await.unwrap();
    assert_eq!(agents.active.name, "build");
    assert_eq!(agents.agents.len(), 3);

    let resp = conn.session_agent_update(&sid, "plan").await.unwrap();
    assert!(!resp.rejected);
    assert_eq!(resp.status.as_deref(), Some("applied"));

    // The applied switch persists — agents/list reports it instead of
    // reverting to build; unknown agents reject.
    let agents = conn.agents_list(&sid).await.unwrap();
    assert_eq!(agents.active.name, "plan");
    assert!(conn.session_agent_update(&sid, "bogus").await.is_err());

    cleanup(conn, &dir).await;
}

/// review/state + turnDiff + approve/revert round-trip.
#[tokio::test]
async fn review_state_diff_and_mutation() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let state = conn.review_state(&sid).await.unwrap();
    assert_eq!(state.files.len(), 2);
    assert_eq!(state.scopes.len(), 1);
    let owner = state.scopes[0].owner.clone();
    assert!(matches!(
        owner,
        vibe_protocol::models::ReviewOwner::Agent { turn_id: 1 }
    ));

    let diff = conn
        .review_turn_diff(&sid, "src/main.rs", &owner)
        .await
        .unwrap();
    assert_eq!(diff.status, "modified");
    assert!(diff.baseline.contains("old_call"));
    assert!(diff.current.contains("new_call"));

    conn.review_approve(&sid, &vibe_protocol::models::ReviewTarget::File {
        path: "src/main.rs".into(),
    })
    .await
    .unwrap();
    // A decided file resolves off the pending list — the mutation is
    // observable on the next read.
    let after_keep = conn.review_state(&sid).await.unwrap();
    assert_eq!(after_keep.files.len(), 1);
    assert_eq!(after_keep.files[0].path, "src/lib.rs");
    assert_eq!(after_keep.scopes[0].files.len(), 1);

    conn.review_revert(&sid, &vibe_protocol::models::ReviewTarget::File {
        path: "src/lib.rs".into(),
    })
    .await
    .unwrap();
    let after_all = conn.review_state(&sid).await.unwrap();
    assert!(after_all.files.is_empty());
    assert!(after_all.scopes[0].files.is_empty());

    cleanup(conn, &dir).await;
}

/// M3c: skills/mcp/connectors/plugins reads + toggles persist across reads.
#[tokio::test]
async fn extensions_roundtrip() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    // skills: installed list → toggle → read-back reflects it.
    let inst = conn.skills_installed(&sid).await.unwrap();
    assert_eq!(inst.skills.len(), 3);
    let deploy = inst.skills.iter().find(|s| s.name == "deploy-notes").unwrap();
    assert!(!deploy.enabled);

    let resp = conn.skills_set_enabled(&sid, "deploy-notes", true).await.unwrap();
    assert!(!resp.rejected);
    let after = conn.skills_installed(&sid).await.unwrap();
    let deploy = after.skills.iter().find(|s| s.name == "deploy-notes").unwrap();
    assert!(deploy.enabled);

    // Locked skills reject and stay unchanged.
    let locked = conn.skills_set_enabled(&sid, "vibe-release", false).await.unwrap();
    assert!(locked.rejected);
    assert!(!locked.failures.is_empty());
    let still = conn.skills_installed(&sid).await.unwrap();
    assert!(still.skills.iter().find(|s| s.name == "vibe-release").unwrap().enabled);

    // mcp: read → toggle → runtime.mcp and the next read both agree.
    let mcp = conn.mcp_read(&sid).await.unwrap();
    let fs = mcp.mcp.sources.iter().find(|s| s.name == "fs").unwrap();
    assert_eq!(fs.status, "enabled");

    let toggled = conn.mcp_toggle(&sid, "fs", "server", true).await.unwrap();
    let fresh = toggled.runtime.expect("runtime").get("mcp").cloned().unwrap();
    let state: vibe_protocol::models::MCPState = serde_json::from_value(fresh).unwrap();
    assert_eq!(
        state.sources.iter().find(|s| s.name == "fs").unwrap().status,
        "disabled"
    );
    let reread = conn.mcp_read(&sid).await.unwrap();
    assert_eq!(
        reread.mcp.sources.iter().find(|s| s.name == "fs").unwrap().status,
        "disabled"
    );

    // connectors + plugins are read-only surfaces here.
    let conn_counts = conn.connectors_read(&sid).await.unwrap();
    assert_eq!(conn_counts.counts.connected, 2);
    assert_eq!(conn_counts.counts.total, Some(5));

    let plugs = conn.plugins_read(&sid).await.unwrap();
    assert_eq!(plugs.plugins.plugins.len(), 1);
    assert_eq!(plugs.plugins.plugins[0].name, "acme-pack");
    assert!(plugs.plugins.dropped.is_empty());

    cleanup(conn, &dir).await;
}

/// M3d: workspace worktrees + scheduled loops.
#[tokio::test]
async fn worktrees_and_loops() {
    let (dir, envs) = store();
    let mut conn = spawn_fixture(&envs).await;
    conn.initialize(info(), caps()).await.unwrap();
    let (sid, _rx) = session_with_completed_turn(&mut conn).await;

    let wt = conn.workspace_worktrees("/repo").await.unwrap();
    assert_eq!(wt.worktrees.len(), 1);
    assert_eq!(wt.worktrees[0].branch, "feature-x");
    assert_eq!(
        wt.worktrees[0]
            .branch_changes
            .as_ref()
            .map(|c| (c.additions, c.deletions)),
        Some((12, 3))
    );
    assert_eq!(wt.repository_branch.as_deref(), Some("main"));

    // loops: empty → create (interval parses 5m → 300s) → list → delete.
    assert!(conn.loops_list(&sid).await.unwrap().loops.is_empty());
    let created = conn
        .loops_create(&sid, "5m", "summarize the diff")
        .await
        .unwrap();
    assert_eq!(created.scheduled_loop.interval_seconds, 300);
    let listed = conn.loops_list(&sid).await.unwrap();
    assert_eq!(listed.loops.len(), 1);
    assert_eq!(listed.loops[0].prompt, "summarize the diff");

    let removed = conn
        .loops_delete(&sid, &created.scheduled_loop.id)
        .await
        .unwrap();
    assert_eq!(removed.scheduled_loop.id, created.scheduled_loop.id);
    assert!(conn.loops_list(&sid).await.unwrap().loops.is_empty());
    assert!(conn.loops_delete(&sid, "loop-9").await.is_err());

    cleanup(conn, &dir).await;
}
