//! Scripted fake `vibe-app-server` — speaks the same NDJSON JSON-RPC protocol
//! so `vibe-desktop` and protocol tests can run without credentials.
//!
//! Scenario: `turn/start` emits a streamed assistant message, a shell effect
//! that blocks on an approval callback, then completes once `callback/respond`
//! arrives — exercising the full reducer path.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct SessionData {
    state: Value,
    event_id: AtomicU64,
}

fn session_value(id: &str, cwd: &str, status: Value) -> Value {
    json!({
        "id": id,
        "rootSessionId": null,
        "parentSessionId": null,
        "title": format!("Fixture {}", &id[..id.len().min(6)]),
        "preview": "fake session for development",
        "status": status,
        "createdAt": 1_700_000_000,
        "updatedAt": 1_700_000_000,
        "bumpedAt": null,
        "pinnedAt": null,
        "archivedAt": null,
        "isUnseen": false,
        "cwd": cwd,
        "workspaceRoots": [cwd],
        "model": "fixture-model",
        "reasoningEffort": null,
        "agent": {"name": "fixture", "displayName": "Fixture Agent", "description": "", "safety": "safe", "agentType": "default"},
        "tokenUsage": {"inputTokens": 0, "outputTokens": 0, "totalTokens": 0},
        "contextUsage": null,
        "harness": "unified"
    })
}

fn state_value(session: Value, event_id: u64, history: Value) -> Value {
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": event_id,
        "session": session,
        "isQuiescent": true,
        "history": history,
        "historyBeforeCursor": null,
        "turns": [],
        "activeCallbacks": [],
        "childSessions": [],
        "turnQueue": {"items": [], "paused": false, "maxItems": 32},
        "retrying": null
    })
}

fn msg_base(id: &str, session_id: &str, turn_id: Option<&str>) -> Value {
    json!({
        "id": id, "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": 1_700_000_100, "updatedAt": 1_700_000_100,
        "generationStatus": "completed",
        "relatedEntryId": null
    })
}

fn message_entry(id: &str, session_id: &str, turn_id: &str, role: &str, text: &str) -> Value {
    let mut v = msg_base(id, session_id, Some(turn_id));
    v["type"] = json!("message");
    v["role"] = json!(role);
    v["content"] = json!([{"type": "text", "text": text}]);
    v["source"] = json!("turn_start");
    v
}

fn shell_effect(id: &str, session_id: &str, turn_id: &str, state: Value) -> Value {
    let mut v = msg_base(id, session_id, Some(turn_id));
    v["type"] = json!("effect");
    v["title"] = json!("fixture-tool");
    v["detail"] = json!({
        "kind": "shell",
        "toolName": "shell",
        "display": {
            "summary": "Run fixture command",
            "content": "echo fixture-output",
            "suffix": "",
            "verb": "Ran",
            "message": null,
            "settledVerb": "ran",
            "settledMessage": "fixture-output",
            "statusText": "shell"
        },
        "input": {"command": "echo fixture-output"}
    });
    v["state"] = state;
    v
}

fn approval_callback(
    id: &str,
    session_id: &str,
    turn_id: &str,
    callback_id: &str,
    effect_id: &str,
) -> Value {
    let mut v = msg_base(id, session_id, Some(turn_id));
    v["type"] = json!("callback");
    v["callbackId"] = json!(callback_id);
    v["title"] = json!("Approve fixture-tool");
    v["detail"] = json!({
        "kind": "approval",
        "effect": {
            "kind": "shell", "toolName": "shell",
            "display": {"summary": "Run fixture command", "statusText": "shell"},
            "input": {"command": "echo fixture-output"}
        },
        "requiredPermissions": [{
            "scope": "command_pattern",
            "invocationPattern": "echo fixture-output",
            "sessionPattern": "echo *",
            "label": "run `echo fixture-output`"
        }],
        "choices": ["approve", "approve_for_session", "approve_permanently", "deny", "cancel_turn"],
        "pathScopeChoices": [],
        "relatedEntryId": effect_id,
        "reason": "fixture: demonstrate the approval card"
    });
    v["state"] = json!({"status": "open"});
    v
}

fn turn(id: &str, session_id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "sessionId": session_id,
        "status": status,
        "startedAt": 1_700_000_000,
        "completedAt": null,
        "error": null,
        "stopReason": null,
        "queueItemId": null
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(512);
    // Fan out: single writer task for all output.
    tokio::spawn(async move {
        let mut w = stdout;
        while let Some(line) = rx.recv().await {
            let _ = w.write_all(line.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
        }
    });

    let sessions = Arc::new(tokio::sync::Mutex::new(
        BTreeMap::<String, Arc<SessionData>>::new(),
    ));
    let pending_callbacks = Arc::new(tokio::sync::Mutex::new(
        BTreeMap::<String, Arc<SessionData>>::new(),
    ));

    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let env: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = env.get("id").cloned();
        let method = env
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = env.get("params").cloned().unwrap_or(json!({}));

        let respond = |result: Value| {
            let tx = tx.clone();
            let id = id.clone();
            async move {
                if let Some(id) = id {
                    let _ = tx
                        .send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
                        .await;
                }
            }
        };
        let _notify = {
            let tx = tx.clone();
            move |method: &str, params: Value| {
                let tx = tx.clone();
                let method = method.to_string();
                async move {
                    let _ = tx
                        .send(
                            json!({"jsonrpc": "2.0", "method": method, "params": params})
                                .to_string(),
                        )
                        .await;
                }
            }
        };
        let server_request = {
            let tx = tx.clone();
            move |id: Value, method: &str, params: Value| {
                let tx = tx.clone();
                let method = method.to_string();
                async move {
                    let _ = tx
                        .send(
                            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                                .to_string(),
                        )
                        .await;
                }
            }
        };

        match method.as_str() {
            "initialize" => {
                respond(
                    json!({"serverInfo": {"name": "vibe-app-server-fixture", "version": "2.25.8"}}),
                )
                .await;
            }
            "initialized" => {}
            "session/list" => {
                respond(json!({
                    "items": [
                        session_value("saved-aaaa1111", "/tmp/project-a", json!({"type": "idle"})),
                        session_value("saved-bbbb2222", "/tmp/project-b", json!({"type": "archived"})),
                    ],
                    "nextCursor": null,
                    "previousCursor": null,
                    "continueSessionId": "saved-aaaa1111"
                })).await;
            }
            "session/start" | "session/continue" => {
                let cwd = params
                    .pointer("/agentConfig/cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("/tmp")
                    .to_string();
                let sid = format!(
                    "fx-{:08x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                        & 0xffffffff
                );
                let data = Arc::new(SessionData {
                    state: state_value(
                        session_value(&sid, &cwd, json!({"type": "idle"})),
                        0,
                        json!([]),
                    ),
                    event_id: AtomicU64::new(0),
                });
                sessions.lock().await.insert(sid.clone(), data.clone());
                respond(json!({"state": data.state, "lastEventId": 0})).await;
            }
            "session/resume" | "session/read" => {
                let sid = params["sessionId"].as_str().unwrap_or("saved-aaaa1111");
                let data = sessions.lock().await.get(sid).cloned();
                let state = data.map(|d| d.state.clone()).unwrap_or_else(|| {
                    state_value(
                        session_value(sid, "/tmp", json!({"type": "idle"})),
                        0,
                        json!([]),
                    )
                });
                respond(json!({"state": state, "lastEventId": 0})).await;
            }
            "session/fork" => {
                let src = params["sourceSessionId"].as_str().unwrap_or("");
                let data = sessions.lock().await.get(src).cloned();
                let state = data.map(|d| {
                    let mut s = d.state.clone();
                    s["session"]["id"] = json!(format!("{src}-fork"));
                    s
                });
                respond(json!({"state": state.unwrap_or(Value::Null), "sourceSessionId": src, "lastEventId": 0})).await;
            }
            "turn/start" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let text = params
                    .pointer("/message/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let turn_id = format!("t-{}", sid);
                let data = sessions.lock().await.get(&sid).cloned();
                let Some(data) = data else {
                    if let Some(id) = id {
                        let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": "not_found", "message": "session", "data": null}}).to_string()).await;
                    }
                    continue;
                };
                respond(json!({"turn": turn(&turn_id, &sid, "in_progress"), "lastEventId": 0}))
                    .await;
                // Scripted turn.
                let tx2 = tx.clone();
                let pending = pending_callbacks.clone();
                tokio::spawn(async move {
                    let seq = || data.event_id.fetch_add(1, Ordering::SeqCst) + 1;
                    let evt = |method: &str, eid: u64, extra: Value| {
                        let mut p = extra;
                        p["eventId"] = json!(eid);
                        p["sessionId"] = json!(sid);
                        p["emittedAt"] = json!(1_700_000_000);
                        (method.to_string(), p)
                    };
                    let send = |m: String, p: Value| {
                        let tx = tx2.clone();
                        async move {
                            let _ = tx
                                .send(
                                    json!({"jsonrpc": "2.0", "method": m, "params": p}).to_string(),
                                )
                                .await;
                        }
                    };
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        seq(),
                        json!({"turnId": turn_id, "entry": message_entry("e-user", &sid, &turn_id, "user", &text)}),
                    );
                    send(m, p).await;
                    let (m, p) = evt(
                        "turn/started",
                        seq(),
                        json!({"turn": turn(&turn_id, &sid, "in_progress")}),
                    );
                    send(m, p).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        seq(),
                        json!({"turnId": turn_id, "entry": {
                            "type": "reasoning",
                            "id": "e-think", "sessionId": sid, "turnId": turn_id,
                            "createdAt": 1, "updatedAt": 1, "generationStatus": "in_progress",
                            "relatedEntryId": null,
                            "text": "", "summary": []
                        }}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        seq(),
                        json!({"turnId": turn_id, "entryId": "e-think", "patch": [
                            {"op": "append", "path": "/text", "value": "Thinking about the request… "}
                        ]}),
                    );
                    send(m, p).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        seq(),
                        json!({"turnId": turn_id, "entry": message_entry("e-asst", &sid, &turn_id, "assistant", "")}),
                    );
                    send(m, p).await;
                    for chunk in [
                        "Fixture response: ",
                        &format!("echo “{text}” — "),
                        "running a shell effect next.",
                    ] {
                        tokio::time::sleep(Duration::from_millis(90)).await;
                        let (m, p) = evt(
                            "history/entryUpdated",
                            seq(),
                            json!({"turnId": turn_id, "entryId": "e-asst", "patch": [
                                {"op": "append", "path": "/content/0/text", "value": chunk}
                            ]}),
                        );
                        send(m, p).await;
                    }
                    // Shell effect: pending → running → blocked on approval.
                    let (m, p) = evt(
                        "history/entryAdded",
                        seq(),
                        json!({"turnId": turn_id, "entry": shell_effect("e-shell", &sid, &turn_id, json!({"status": "pending"}))}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        seq(),
                        json!({"turnId": turn_id, "entryId": "e-shell", "patch": [
                            {"op": "replace", "path": "/state", "value": {"status": "running", "outputText": ""}}
                        ]}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let callback_id = format!("cb-{turn_id}");
                    let cb_entry =
                        approval_callback("e-cb", &sid, &turn_id, &callback_id, "e-shell");
                    let (m, p) = evt(
                        "history/entryAdded",
                        seq(),
                        json!({"turnId": turn_id, "entry": cb_entry}),
                    );
                    send(m, p).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        seq(),
                        json!({"turnId": turn_id, "entryId": "e-shell", "patch": [
                            {"op": "replace", "path": "/state", "value": {"status": "blocked", "callbackId": callback_id, "outputText": ""}}
                        ]}),
                    );
                    send(m, p).await;
                    // Server→client request: the client must ack, then answer via callback/respond.
                    pending
                        .lock()
                        .await
                        .insert(callback_id.clone(), data.clone());
                    server_request(
                        json!("srv-cb-1"),
                        "callback/call",
                        json!({"callback": cb_entry}),
                    )
                    .await;
                });
            }
            "callback/respond" => {
                let cb_id = params["callbackId"].as_str().unwrap_or("").to_string();
                respond(json!({"status": "accepted"})).await;
                // Finish the scripted turn once the answer lands.
                if let Some(data) = pending_callbacks.lock().await.remove(&cb_id) {
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        let seq = || data.event_id.fetch_add(1, Ordering::SeqCst) + 1;
                        let send = |m: &str, p: Value| {
                            let tx = tx2.clone();
                            let m = m.to_string();
                            async move {
                                let _ = tx
                                    .send(
                                        json!({"jsonrpc": "2.0", "method": m, "params": p})
                                            .to_string(),
                                    )
                                    .await;
                            }
                        };
                        let evt = |eid: u64, extra: Value| {
                            let mut p = extra;
                            p["eventId"] = json!(eid);
                            p["sessionId"] = json!(data.state["session"]["id"]);
                            p["emittedAt"] = json!(1_700_000_000);
                            p
                        };
                        let sid = data.state["session"]["id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let turn_id = format!("t-{sid}");
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        // Resolve the callback entry.
                        send("history/entryUpdated", evt(seq(), json!({"turnId": turn_id, "entryId": "e-cb", "patch": [
                            {"op": "replace", "path": "/state", "value": {"status": "answered", "output": params["output"]}}
                        ]}))).await;
                        // Effect completes.
                        send("history/entryUpdated", evt(seq(), json!({"turnId": turn_id, "entryId": "e-shell", "patch": [
                            {"op": "replace", "path": "/state", "value": {
                                "status": "completed", "output": {"stdout": "fixture-output"},
                                "outputText": "fixture-output", "durationMs": 42,
                                "display": {"success": true, "verb": "Ran", "message": "fixture-output", "warnings": [], "suffix": ""}
                            }}
                        ]}))).await;
                        // Final assistant text + turn completed.
                        send("history/entryUpdated", evt(seq(), json!({"turnId": turn_id, "entryId": "e-asst", "patch": [
                            {"op": "append", "path": "/content/0/text", "value": " Done — effect approved and completed."}
                        ]}))).await;
                        send("history/entryUpdated", evt(seq(), json!({"turnId": turn_id, "entryId": "e-think", "patch": [
                            {"op": "replace", "path": "/generationStatus", "value": "completed"}
                        ]}))).await;
                        send("turn/completed", evt(seq(), json!({"turn": {"id": turn_id, "sessionId": sid, "status": "completed", "startedAt": 1, "completedAt": 2, "error": null, "stopReason": null, "queueItemId": null}}))).await;
                    });
                }
            }
            "session/turn/enqueue" => {
                respond(json!({"queueItemId": "q-fixture-1"})).await;
            }
            "turn/interrupt"
            | "turn/steer"
            | "session/turn/queue/remove"
            | "session/turn/queue/replace"
            | "session/turn/queue/steer"
            | "session/turn/queue/resume" => {
                respond(json!({"accepted": true, "lastEventId": 0})).await;
            }
            "session/stop" | "session/close" => {
                respond(json!({"closed": true})).await;
            }
            "session/pin"
            | "session/archive"
            | "session/rename"
            | "session/title/update"
            | "session/markAsSeen"
            | "session/delete" => {
                respond(json!({})).await;
            }
            "session/compact" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                let data = sessions.lock().await.get(sid).cloned();
                let state = data.map(|d| d.state.clone()).unwrap_or(Value::Null);
                respond(json!({"summary": "fixture summary", "state": state, "sessionLog": {"enabled": false}})).await;
            }
            "runtime/read" => {
                respond(json!({
                    "runtime": {
                        "config": {}, "activeAgent": {"name": "fixture", "displayName": "Fixture Agent", "description": "", "safety": "safe", "agentType": "default"},
                        "agents": [], "skills": [], "tools": [],
                        "stats": {"steps": 0},
                        "contextWindow": 200000, "issues": [], "hooksCount": 0,
                        "connectors": {"installed": 0, "enabled": 0},
                        "mcp": {"servers": []},
                        "bypassToolPermissions": false, "experimentalHarness": false
                    },
                    "sessionLog": {"enabled": false},
                    "ready": true
                })).await;
            }
            "session/shellCommand" => {
                respond(json!({"output": "fixture"})).await;
            }
            "account/read" => {
                respond(json!({"account": {"status": "ok", "plan": null, "actions": []}})).await;
            }
            _ => {
                if let Some(id) = id {
                    let _ = tx.send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": "method_not_found", "message": format!("fixture: unhandled {method}"), "data": null}
                    }).to_string()).await;
                }
            }
        }
    }
}
