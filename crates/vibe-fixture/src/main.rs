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
use vibe_protocol::json_patch::apply_patch;
use vibe_protocol::models::JsonPatchOperation;

/// Live session contents. History and its watermark live behind ONE lock so
/// `session/read` always returns a consistent (history, eventId) pair — a
/// snapshot never pairs a patched entry with the previous event id.
struct SessionData {
    session: Value,
    inner: tokio::sync::Mutex<SessionInner>,
    turn_seq: AtomicU64,
    store: std::path::PathBuf,
}

struct SessionInner {
    history: Vec<Value>,
    event_id: u64,
}

/// File-backed session store shared by every fixture process — the real
/// app-server persists sessions on disk, so a fork made on one connection
/// must be resumable from a different process.
fn store_dir() -> std::path::PathBuf {
    std::env::var("VIBE_FIXTURE_STORE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/vibe-fixture-store"))
}

/// Session ids are caller-supplied and become filenames — only ids made of
/// filename-safe characters may touch the store (no `../` escapes).
fn store_safe_id(sid: &str) -> bool {
    !sid.is_empty()
        && !sid.contains("..")
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl SessionData {
    fn new(session: Value, store: std::path::PathBuf) -> Self {
        Self {
            session,
            inner: tokio::sync::Mutex::new(SessionInner {
                history: Vec::new(),
                event_id: 0,
            }),
            turn_seq: AtomicU64::new(0),
            store,
        }
    }

    /// Rebuild a session from the shared store (a different fixture
    /// process may have created or forked it).
    async fn load(store: &std::path::Path, sid: &str) -> Option<Arc<SessionData>> {
        if !store_safe_id(sid) {
            return None;
        }
        let path = store.join(format!("{sid}.json"));
        let raw = tokio::fs::read_to_string(path).await.ok()?;
        let v: Value = serde_json::from_str(&raw).ok()?;
        Some(Arc::new(SessionData {
            session: v["session"].clone(),
            inner: tokio::sync::Mutex::new(SessionInner {
                history: serde_json::from_value(v["history"].clone()).unwrap_or_default(),
                event_id: v["event_id"].as_u64().unwrap_or(0),
            }),
            turn_seq: AtomicU64::new(v["turn_seq"].as_u64().unwrap_or(0)),
            store: store.to_path_buf(),
        }))
    }

    /// Snapshot current contents to the shared store (atomic write).
    async fn persist(&self, inner: &SessionInner) {
        let body = json!({
            "session": self.session,
            "history": inner.history,
            "event_id": inner.event_id,
            "turn_seq": self.turn_seq.load(Ordering::SeqCst),
        });
        let id = self.session["id"].as_str().unwrap_or("x");
        if !store_safe_id(id) {
            return;
        }
        let path = self.store.join(format!("{id}.json"));
        let tmp = self.store.join(format!("{id}.json.tmp"));
        if tokio::fs::write(&tmp, body.to_string()).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    /// Next event id — for notifications that don't mutate history.
    async fn bump(&self) -> u64 {
        let mut inner = self.inner.lock().await;
        inner.event_id += 1;
        let eid = inner.event_id;
        self.persist(&inner).await;
        eid
    }

    /// Append a history entry and claim its event id atomically.
    async fn add_entry(&self, entry: Value) -> u64 {
        let mut inner = self.inner.lock().await;
        inner.event_id += 1;
        inner.history.push(entry);
        let eid = inner.event_id;
        self.persist(&inner).await;
        eid
    }

    /// Apply a patch to a stored entry and claim its event id atomically.
    async fn patch_entry(&self, entry_id: &str, patch: &Value) -> u64 {
        let mut inner = self.inner.lock().await;
        inner.event_id += 1;
        if let Some(entry) = inner
            .history
            .iter_mut()
            .find(|e| e["id"].as_str() == Some(entry_id))
        {
            if let Ok(ops) = serde_json::from_value::<Vec<JsonPatchOperation>>(patch.clone()) {
                if let Ok(next) = apply_patch(entry, &ops) {
                    *entry = next;
                }
            }
        }
        let eid = inner.event_id;
        self.persist(&inner).await;
        eid
    }

    async fn state(&self) -> Value {
        let inner = self.inner.lock().await;
        state_value(
            self.session.clone(),
            inner.event_id,
            json!(inner.history.clone()),
        )
    }

    async fn clone_history(&self) -> Vec<Value> {
        self.inner.lock().await.history.clone()
    }
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
    let pending_callbacks = Arc::new(tokio::sync::Mutex::new(BTreeMap::<
        String,
        (Arc<SessionData>, u64),
    >::new()));
    let store = store_dir();
    let _ = tokio::fs::create_dir_all(&store).await;

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
                let include_archived = params["includeArchived"].as_bool().unwrap_or(false);
                let mut items = vec![session_value(
                    "saved-aaaa1111",
                    "/tmp/project-a",
                    json!({"type": "idle"}),
                )];
                if include_archived {
                    items.push(session_value(
                        "saved-bbbb2222",
                        "/tmp/project-b",
                        json!({"type": "archived"}),
                    ));
                }
                // Merge stored sessions (created/forked by any fixture
                // process) into the demo catalog.
                if let Ok(mut dir) = tokio::fs::read_dir(&store).await {
                    while let Ok(Some(ent)) = dir.next_entry().await {
                        let path = ent.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                                if let Some(sid) = v["session"]["id"].as_str() {
                                    if !items.iter().any(|i| i["id"] == json!(sid)) {
                                        items.push(v["session"].clone());
                                    }
                                }
                            }
                        }
                    }
                }
                respond(json!({
                    "items": items,
                    "nextCursor": null,
                    "previousCursor": null,
                    "continueSessionId": "saved-aaaa1111"
                }))
                .await;
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
                let data = Arc::new(SessionData::new(
                    session_value(&sid, &cwd, json!({"type": "idle"})),
                    store.clone(),
                ));
                {
                    let inner = data.inner.lock().await;
                    data.persist(&inner).await;
                }
                sessions.lock().await.insert(sid.clone(), data.clone());
                respond(json!({"state": data.state().await, "lastEventId": 0})).await;
            }
            "session/resume" | "session/read" => {
                let sid = params["sessionId"].as_str().unwrap_or("saved-aaaa1111");
                let data = sessions.lock().await.get(sid).cloned();
                // Fall back to the shared store — the session may live in
                // another fixture process (e.g. a forked child).
                let data = match data {
                    Some(d) => Some(d),
                    None => SessionData::load(&store, sid).await,
                };
                let state = match data {
                    Some(d) => {
                        sessions.lock().await.insert(sid.to_string(), d.clone());
                        d.state().await
                    }
                    None => state_value(
                        session_value(sid, "/tmp", json!({"type": "idle"})),
                        0,
                        json!([]),
                    ),
                };
                respond(json!({"state": state, "lastEventId": 0})).await;
            }
            "session/fork" => {
                let src = params["sourceSessionId"].as_str().unwrap_or("");
                if !store_safe_id(src) {
                    respond(Value::Null).await;
                    continue;
                }
                let data = sessions.lock().await.get(src).cloned();
                let data = match data {
                    Some(d) => Some(d),
                    None => SessionData::load(&store, src).await,
                };
                let state = match data {
                    Some(d) => {
                        let fork_id = format!("{src}-fork");
                        let mut session = d.session.clone();
                        session["id"] = json!(fork_id);
                        session["parentSessionId"] = json!(src);
                        let fork = Arc::new(SessionData::new(session, store.clone()));
                        {
                            let mut inner = fork.inner.lock().await;
                            inner.history = d.clone_history().await;
                            inner.event_id = d.inner.lock().await.event_id;
                            // Continue the parent's turn counter so the
                            // child's entry ids can never collide with the
                            // history it just inherited.
                            fork.turn_seq
                                .store(d.turn_seq.load(Ordering::SeqCst), Ordering::SeqCst);
                            fork.persist(&inner).await;
                        }
                        // Return the CHILD's state — the caller opens it.
                        let state = fork.state().await;
                        sessions.lock().await.insert(fork_id, fork);
                        state
                    }
                    None => Value::Null,
                };
                respond(json!({"state": state, "sourceSessionId": src, "lastEventId": 0})).await;
            }
            "turn/start" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let text = params
                    .pointer("/message/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let data = sessions.lock().await.get(&sid).cloned();
                let data = match data {
                    Some(d) => Some(d),
                    None => SessionData::load(&store, &sid).await,
                };
                let Some(data) = data else {
                    if let Some(id) = id {
                        let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": "not_found", "message": "session", "data": null}}).to_string()).await;
                    }
                    continue;
                };
                let n = data.turn_seq.fetch_add(1, Ordering::SeqCst) + 1;
                let turn_id = format!("t-{sid}-{n}");
                let e_user = format!("e-user-{n}");
                let e_think = format!("e-think-{n}");
                let e_asst = format!("e-asst-{n}");
                let e_shell = format!("e-shell-{n}");
                let e_cb = format!("e-cb-{n}");
                respond(json!({"turn": turn(&turn_id, &sid, "in_progress"), "lastEventId": 0}))
                    .await;
                // Scripted turn.
                let tx2 = tx.clone();
                let pending = pending_callbacks.clone();
                tokio::spawn(async move {
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
                    let user_entry = message_entry(&e_user, &sid, &turn_id, "user", &text);
                    let eid = data.add_entry(user_entry.clone()).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        eid,
                        json!({"turnId": turn_id, "entry": user_entry}),
                    );
                    send(m, p).await;
                    let (m, p) = evt(
                        "turn/started",
                        data.bump().await,
                        json!({"turn": turn(&turn_id, &sid, "in_progress")}),
                    );
                    send(m, p).await;
                    let think_entry = json!({
                        "type": "reasoning",
                        "id": e_think, "sessionId": sid, "turnId": turn_id,
                        "createdAt": 1, "updatedAt": 1, "generationStatus": "in_progress",
                        "relatedEntryId": null,
                        "text": "", "summary": []
                    });
                    let eid = data.add_entry(think_entry.clone()).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        eid,
                        json!({"turnId": turn_id, "entry": think_entry}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    let patch = json!([
                        {"op": "append", "path": "/text", "value": "Thinking about the request… "}
                    ]);
                    let eid = data.patch_entry(&e_think, &patch).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        eid,
                        json!({"turnId": turn_id, "entryId": e_think, "patch": patch}),
                    );
                    send(m, p).await;
                    let asst_entry = message_entry(&e_asst, &sid, &turn_id, "assistant", "");
                    let eid = data.add_entry(asst_entry.clone()).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        eid,
                        json!({"turnId": turn_id, "entry": asst_entry}),
                    );
                    send(m, p).await;
                    for chunk in [
                        "Fixture response: ",
                        &format!("echo “{text}” — "),
                        "running a shell effect next.",
                    ] {
                        tokio::time::sleep(Duration::from_millis(90)).await;
                        let patch = json!([
                            {"op": "append", "path": "/content/0/text", "value": chunk}
                        ]);
                        let eid = data.patch_entry(&e_asst, &patch).await;
                        let (m, p) = evt(
                            "history/entryUpdated",
                            eid,
                            json!({"turnId": turn_id, "entryId": e_asst, "patch": patch}),
                        );
                        send(m, p).await;
                    }
                    // Shell effect: pending → running → blocked on approval.
                    let shell_entry =
                        shell_effect(&e_shell, &sid, &turn_id, json!({"status": "pending"}));
                    let eid = data.add_entry(shell_entry.clone()).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        eid,
                        json!({"turnId": turn_id, "entry": shell_entry}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let patch = json!([
                        {"op": "replace", "path": "/state", "value": {"status": "running", "outputText": ""}}
                    ]);
                    let eid = data.patch_entry(&e_shell, &patch).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        eid,
                        json!({"turnId": turn_id, "entryId": e_shell, "patch": patch}),
                    );
                    send(m, p).await;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let callback_id = format!("cb-{turn_id}");
                    let cb_entry = approval_callback(&e_cb, &sid, &turn_id, &callback_id, &e_shell);
                    let eid = data.add_entry(cb_entry.clone()).await;
                    let (m, p) = evt(
                        "history/entryAdded",
                        eid,
                        json!({"turnId": turn_id, "entry": cb_entry}),
                    );
                    send(m, p).await;
                    let patch = json!([
                        {"op": "replace", "path": "/state", "value": {"status": "blocked", "callbackId": callback_id, "outputText": ""}}
                    ]);
                    let eid = data.patch_entry(&e_shell, &patch).await;
                    let (m, p) = evt(
                        "history/entryUpdated",
                        eid,
                        json!({"turnId": turn_id, "entryId": e_shell, "patch": patch}),
                    );
                    send(m, p).await;
                    // Server→client request: the client must ack, then answer via callback/respond.
                    pending
                        .lock()
                        .await
                        .insert(callback_id.clone(), (data.clone(), n));
                    server_request(
                        json!(format!("srv-cb-{n}")),
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
                if let Some((data, n)) = pending_callbacks.lock().await.remove(&cb_id) {
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
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
                            p["sessionId"] = data.session["id"].clone();
                            p["emittedAt"] = json!(1_700_000_000);
                            p
                        };
                        let sid = data.session["id"].as_str().unwrap_or("").to_string();
                        let turn_id = format!("t-{sid}-{n}");
                        let e_think = format!("e-think-{n}");
                        let e_asst = format!("e-asst-{n}");
                        let e_shell = format!("e-shell-{n}");
                        let e_cb = format!("e-cb-{n}");
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        // Resolve the callback entry.
                        let patch = json!([
                            {"op": "replace", "path": "/state", "value": {"status": "answered", "output": params["output"]}}
                        ]);
                        let eid = data.patch_entry(&e_cb, &patch).await;
                        send(
                            "history/entryUpdated",
                            evt(
                                eid,
                                json!({"turnId": turn_id, "entryId": e_cb, "patch": patch}),
                            ),
                        )
                        .await;
                        // Effect completes.
                        let patch = json!([
                            {"op": "replace", "path": "/state", "value": {
                                "status": "completed", "output": {"stdout": "fixture-output"},
                                "outputText": "fixture-output", "durationMs": 42,
                                "display": {"success": true, "verb": "Ran", "message": "fixture-output", "warnings": [], "suffix": ""}
                            }}
                        ]);
                        let eid = data.patch_entry(&e_shell, &patch).await;
                        send(
                            "history/entryUpdated",
                            evt(
                                eid,
                                json!({"turnId": turn_id, "entryId": e_shell, "patch": patch}),
                            ),
                        )
                        .await;
                        // Final assistant text + turn completed.
                        let patch = json!([
                            {"op": "append", "path": "/content/0/text", "value": " Done — effect approved and completed."}
                        ]);
                        let eid = data.patch_entry(&e_asst, &patch).await;
                        send(
                            "history/entryUpdated",
                            evt(
                                eid,
                                json!({"turnId": turn_id, "entryId": e_asst, "patch": patch}),
                            ),
                        )
                        .await;
                        let patch = json!([
                            {"op": "replace", "path": "/generationStatus", "value": "completed"}
                        ]);
                        let eid = data.patch_entry(&e_think, &patch).await;
                        send(
                            "history/entryUpdated",
                            evt(
                                eid,
                                json!({"turnId": turn_id, "entryId": e_think, "patch": patch}),
                            ),
                        )
                        .await;
                        send("turn/completed", evt(data.bump().await, json!({"turn": {"id": turn_id, "sessionId": sid, "status": "completed", "startedAt": 1, "completedAt": 2, "error": null, "stopReason": null, "queueItemId": null}}))).await;
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
                let state = match data {
                    Some(d) => d.state().await,
                    None => Value::Null,
                };
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

#[cfg(test)]
mod tests {
    use super::store_safe_id;

    #[test]
    fn store_safe_id_rejects_traversal() {
        for bad in ["../etc/passwd", "a/b", "..", "", "x\\y", "a:b"] {
            assert!(!store_safe_id(bad), "{bad} must be rejected");
        }
        for good in ["fx-02fd7bbf", "fx-x-fork", "saved-aaaa1111", "s_1.2"] {
            assert!(store_safe_id(good), "{good} must be accepted");
        }
    }
}
