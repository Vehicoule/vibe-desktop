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

/// Live session contents. Session record, history, watermark, turn queue,
/// and the in-flight turn live behind ONE lock so every read-modify-write
/// stays consistent — a snapshot never pairs a patched entry with the
/// previous event id, and a queue patch never races a turn transition.
struct SessionData {
    inner: tokio::sync::Mutex<SessionInner>,
    turn_seq: AtomicU64,
    queue_seq: AtomicU64,
    store: std::path::PathBuf,
}

struct SessionInner {
    session: Value,
    history: Vec<Value>,
    event_id: u64,
    queue_items: Vec<Value>,
    queue_paused: bool,
    active_turn: Option<String>,
    active_abort: Option<tokio::task::AbortHandle>,
}

fn queue_value(inner: &SessionInner) -> Value {
    json!({"items": inner.queue_items, "paused": inner.queue_paused, "maxItems": 32})
}

/// Trusted cwd set — file-backed in the shared store like the real
/// settings file, so trust survives across fixture processes.
async fn trusted_cwds(store: &std::path::Path) -> std::collections::HashSet<String> {
    tokio::fs::read_to_string(store.join("trusted.json"))
        .await
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn inner_default() -> SessionInner {
    SessionInner {
        session: json!({}),
        history: Vec::new(),
        event_id: 0,
        queue_items: Vec::new(),
        queue_paused: false,
        active_turn: None,
        active_abort: None,
    }
}

/// File-backed session store shared by every fixture process — the real
/// app-server persists sessions on disk, so a fork made on one connection
/// must be resumable from a different process.
/// Read a `{key: value}` overlay file from the store — returns an empty
/// map when absent or unparsable (same tolerance as config_values).
fn read_overlay(store: &std::path::Path, file: &str) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(store.join(file))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Write an overlay atomically (tmp + rename), like config_values.
async fn persist_overlay(
    store: &std::path::Path,
    file: &str,
    overlay: &serde_json::Map<String, Value>,
) -> bool {
    let tmp = store.join(format!("{file}.tmp"));
    tokio::fs::write(&tmp, serde_json::to_string(overlay).unwrap_or_default())
        .await
        .is_ok()
        && tokio::fs::rename(&tmp, store.join(file)).await.is_ok()
}

/// `skills/installed` view — defaults overlaid with skill_states.json
/// (`{name: bool}` enabled overrides), so setEnabled round-trips.
fn skills_value(store: &std::path::Path) -> Value {
    let states = read_overlay(store, "skill_states.json");
    let mut skills = json!([
        {"name": "rust-conventions", "description": "Rust style rules", "userInvocable": true, "source": "local", "scope": "project", "enabled": true, "locked": false},
        {"name": "vibe-release", "description": "Release checklist", "userInvocable": true, "source": "builtin", "scope": "builtin", "enabled": true, "locked": true},
        {"name": "deploy-notes", "description": "Deploy runbook", "userInvocable": true, "source": "local", "scope": "global", "enabled": false, "locked": false},
    ]);
    for s in skills.as_array_mut().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or_default();
        if let Some(v) = states.get(name) {
            s["enabled"] = v.clone();
        }
    }
    skills
}

/// `mcp/read` view — sources overlaid with mcp_states.json
/// (`{name: bool}` disabled flags); shared by read + toggle replies.
fn mcp_state_value(store: &std::path::Path) -> Value {
    let states = read_overlay(store, "mcp_states.json");
    let mut sources = json!([
        {"name": "fs", "displayName": "fs", "kind": "server", "transport": "stdio", "status": "enabled", "tools": [{"name": "read_file", "description": "", "enabled": true}]},
        {"name": "github", "displayName": "GitHub", "kind": "server", "transport": "streamable-http", "status": "disabled", "tools": []},
        {"name": "slack", "displayName": "Slack", "kind": "connector", "transport": "streamable-http", "status": "connected", "tools": [{"name": "send", "description": "", "enabled": true}]},
    ]);
    for s in sources.as_array_mut().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or_default();
        if let Some(disabled) = states.get(name).and_then(Value::as_bool) {
            // enabled↔disabled are the only toggleable statuses.
            if matches!(s["status"].as_str(), Some("enabled" | "disabled")) {
                s["status"] = json!(if disabled { "disabled" } else { "enabled" });
            }
        }
    }
    json!({"sources": sources, "discoveryErrors": {}, "connectorError": null, "manageConnectorsUrl": null})
}

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

/// Loops are per-session state — each session gets its own
/// `loops_{sid}.json` overlay so two sessions can't see or delete each
/// other's entries.
fn loops_file(params: &Value) -> Option<String> {
    let sid = params["sessionId"].as_str()?;
    store_safe_id(sid).then(|| format!("loops_{sid}.json"))
}

impl SessionData {
    fn new(session: Value, store: std::path::PathBuf) -> Self {
        let inner = SessionInner {
            session,
            ..inner_default()
        };
        Self {
            inner: tokio::sync::Mutex::new(inner),
            turn_seq: AtomicU64::new(0),
            queue_seq: AtomicU64::new(0),
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
        if v["deleted"] == true {
            return None;
        }
        let inner = SessionInner {
            session: v["session"].clone(),
            history: serde_json::from_value(v["history"].clone()).unwrap_or_default(),
            event_id: v["event_id"].as_u64().unwrap_or(0),
            queue_items: serde_json::from_value(v["queue_items"].clone()).unwrap_or_default(),
            queue_paused: v["queue_paused"].as_bool().unwrap_or(false),
            ..inner_default()
        };
        Some(Arc::new(SessionData {
            inner: tokio::sync::Mutex::new(inner),
            turn_seq: AtomicU64::new(v["turn_seq"].as_u64().unwrap_or(0)),
            queue_seq: AtomicU64::new(v["queue_seq"].as_u64().unwrap_or(0)),
            store: store.to_path_buf(),
        }))
    }

    /// Snapshot current contents to the shared store (atomic write).
    ///
    /// Catalog fields (`title`, `pinnedAt`, `archivedAt`) are
    /// disk-authoritative: rail ops can land on a different fixture
    /// process than the one holding a running turn, and the turn's later
    /// persists must not revert them — or resurrect a tombstoned session.
    async fn persist(&self, inner: &SessionInner) {
        let id = inner.session["id"].as_str().unwrap_or("x");
        if !store_safe_id(id) {
            return;
        }
        let path = self.store.join(format!("{id}.json"));
        let mut session = inner.session.clone();
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            if let Ok(disk) = serde_json::from_str::<Value>(&raw) {
                if disk["deleted"] == true {
                    return;
                }
                for k in ["title", "pinnedAt", "archivedAt"] {
                    if disk["session"].get(k).is_some() {
                        session[k] = disk["session"][k].clone();
                    }
                }
            }
        }
        let body = json!({
            "session": session,
            "history": inner.history,
            "event_id": inner.event_id,
            "turn_seq": self.turn_seq.load(Ordering::SeqCst),
            "queue_items": inner.queue_items,
            "queue_paused": inner.queue_paused,
            "queue_seq": self.queue_seq.load(Ordering::SeqCst),
        });
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

    /// Mutate one field of the session record (title, pinnedAt, …) and
    /// persist under the same lock — returned patch carries the new value.
    /// The disk file gets the field first so concurrent persists on other
    /// fixture processes pick it up as disk-authoritative.
    async fn set_session_field(&self, key: &str, value: Value) {
        let mut inner = self.inner.lock().await;
        inner.session[key] = value.clone();
        let id = inner.session["id"].as_str().unwrap_or("x");
        if store_safe_id(id) {
            let path = self.store.join(format!("{id}.json"));
            if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                if let Ok(mut disk) = serde_json::from_str::<Value>(&raw) {
                    if disk["deleted"] != true {
                        disk["session"][key] = value;
                        let tmp = self.store.join(format!("{id}.json.tmp"));
                        if tokio::fs::write(&tmp, disk.to_string()).await.is_ok() {
                            let _ = tokio::fs::rename(&tmp, &path).await;
                        }
                    }
                }
            }
        }
        self.persist(&inner).await;
    }

    async fn state(&self) -> Value {
        self.state_windowed(None).await
    }

    /// `limit` windows the history to its tail; when older entries exist
    /// `historyBeforeCursor` points at the boundary (fixture cursor `h-N`).
    async fn state_windowed(&self, limit: Option<usize>) -> Value {
        let inner = self.inner.lock().await;
        let len = inner.history.len();
        let start = match limit {
            Some(l) if l < len => len - l,
            _ => 0,
        };
        state_value(
            inner.session.clone(),
            inner.event_id,
            json!(inner.history[start..].to_vec()),
            if start > 0 {
                json!(format!("h-{start}"))
            } else {
                Value::Null
            },
            queue_value(&inner),
        )
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

fn state_value(
    session: Value,
    event_id: u64,
    history: Value,
    before_cursor: Value,
    queue: Value,
) -> Value {
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": event_id,
        "session": session,
        "isQuiescent": true,
        "history": history,
        "historyBeforeCursor": before_cursor,
        "turns": [],
        "activeCallbacks": [],
        "childSessions": [],
        "turnQueue": queue,
        "retrying": null
    })
}

fn empty_queue() -> Value {
    json!({"items": [], "paused": false, "maxItems": 32})
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
    turn_with_queue(id, session_id, status, Value::Null)
}

fn turn_with_queue(id: &str, session_id: &str, status: &str, queue_item_id: Value) -> Value {
    json!({
        "id": id,
        "sessionId": session_id,
        "status": status,
        "startedAt": 1_700_000_000,
        "completedAt": null,
        "error": null,
        "stopReason": null,
        "queueItemId": queue_item_id
    })
}

type Sessions = Arc<tokio::sync::Mutex<BTreeMap<String, Arc<SessionData>>>>;
type PendingCallbacks = Arc<tokio::sync::Mutex<BTreeMap<String, (Arc<SessionData>, u64, String)>>>;
type Tx = tokio::sync::mpsc::Sender<String>;

/// Session lookup: in-memory first, then the shared store — a session may
/// live in a different fixture process (forks, earlier runs).
async fn find_session(
    sessions: &Sessions,
    store: &std::path::Path,
    sid: &str,
) -> Option<Arc<SessionData>> {
    if let Some(d) = sessions.lock().await.get(sid).cloned() {
        return Some(d);
    }
    SessionData::load(store, sid).await
}

/// Emit a `session/updated` JSON-patch notification (claiming its event id).
async fn emit_updated(tx: &Tx, data: &Arc<SessionData>, sid: &str, patch: Value) {
    let eid = data.bump().await;
    let _ = tx
        .send(
            json!({"jsonrpc": "2.0", "method": "session/updated", "params": {
                "eventId": eid, "sessionId": sid, "emittedAt": 1_700_000_000,
                "patch": patch
            }})
            .to_string(),
        )
        .await;
}

/// Upstream emits a dedicated `turn/queueUpdated` notification carrying the
/// whole queue (ADR `_emit_queue_updated`), not a session/updated patch.
async fn emit_queue_updated(tx: &Tx, data: &Arc<SessionData>, sid: &str) {
    let queue = queue_value(&*data.inner.lock().await);
    let eid = data.bump().await;
    let _ = tx
        .send(
            json!({"jsonrpc": "2.0", "method": "turn/queueUpdated", "params": {
                "eventId": eid, "sessionId": sid, "emittedAt": 1_700_000_000,
                "queue": queue,
            }})
            .to_string(),
        )
        .await;
}

/// Project turn state into `session.status` — the real server keeps it in
/// the session record (running/blocked/idle) so clients can derive the
/// active turn id; the fixture serialized "idle" forever before this.
async fn set_status(data: &Arc<SessionData>, tx: &Tx, sid: &str, status: Value) {
    data.set_session_field("status", status.clone()).await;
    emit_updated(
        tx,
        data,
        sid,
        json!([{"op": "replace", "path": "/session/status", "value": status}]),
    )
    .await;
}

/// Status a session lands on when its turn ends without another queued:
/// archived if it was archived mid-turn, else idle. Also the right status
/// for a forked child — it inherits the archive marker but not the turn.
fn status_without_turn(session: &Value) -> Value {
    if session["archivedAt"].is_null() {
        json!({"type": "idle"})
    } else {
        json!({"type": "archived"})
    }
}

/// Terminal status for `data`, preferring the on-disk record — a rail
/// process may have archived the session since this one loaded it into
/// memory (catalog fields are disk-authoritative in `persist`).
async fn terminal_status(data: &Arc<SessionData>) -> Value {
    let (id, fallback) = {
        let inner = data.inner.lock().await;
        (
            inner.session["id"].as_str().unwrap_or("").to_string(),
            status_without_turn(&inner.session),
        )
    };
    if store_safe_id(&id) {
        let path = data.store.join(format!("{id}.json"));
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            if let Ok(disk) = serde_json::from_str::<Value>(&raw) {
                if disk["deleted"] != true {
                    return status_without_turn(&disk["session"]);
                }
            }
        }
    }
    fallback
}

/// Merge the disk-authoritative catalog fields into a session record —
/// for children built from an in-memory parent snapshot that may have
/// missed a rail op on another process (same merge `persist` applies).
async fn refresh_catalog_fields(data: &Arc<SessionData>, session: &mut Value) {
    let id = session["id"].as_str().unwrap_or("").to_string();
    if !store_safe_id(&id) {
        return;
    }
    let path = data.store.join(format!("{id}.json"));
    if let Ok(raw) = tokio::fs::read_to_string(&path).await {
        if let Ok(disk) = serde_json::from_str::<Value>(&raw) {
            if disk["deleted"] != true {
                for key in ["title", "pinnedAt", "archivedAt"] {
                    if let Some(v) = disk["session"].get(key) {
                        session[key] = v.clone();
                    }
                }
            }
        }
    }
}

/// First text block of a queued turn's user entry (what the UI lists).
fn queued_item_text(item: &Value) -> String {
    item.pointer("/entries/0/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// First text block of a history entry (rewind response `message`).
fn entry_text(entry: &Value) -> String {
    entry
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// History index of `entry_id` when it names a user message (the only
/// entries `session/rewind` may target upstream).
fn rewind_index(inner: &SessionInner, entry_id: &str) -> Option<usize> {
    inner
        .history
        .iter()
        .position(|e| e["id"].as_str() == Some(entry_id))
        .filter(|&i| {
            inner.history[i]["type"].as_str() == Some("message")
                && inner.history[i]["role"].as_str() == Some("user")
        })
}

/// Whether any entry after `idx` ran a tool (fixture says file changes
/// exist only when an effect followed the target message).
fn rewind_has_changes(inner: &SessionInner, idx: usize) -> bool {
    inner
        .history
        .iter()
        .skip(idx + 1)
        .any(|e| e["type"].as_str() == Some("effect"))
}

/// Open the turn queue when it has items and no turn is in flight — the
/// real server promotes the head item into a running turn.
async fn promote_next(data: &Arc<SessionData>, sid: &str, tx: &Tx, pending: &PendingCallbacks) {
    let item = {
        let mut inner = data.inner.lock().await;
        if inner.queue_paused || inner.active_turn.is_some() || inner.queue_items.is_empty() {
            None
        } else {
            Some(inner.queue_items.remove(0))
        }
    };
    let Some(item) = item else { return };
    emit_queue_updated(tx, data, sid).await;
    let text = queued_item_text(&item);
    let qid = item["id"].clone();
    spawn_turn(data, sid, text, qid, tx, pending).await;
}

/// Register a scripted turn as active and spawn part 1 (up to the
/// approval callback, which `callback/respond` resumes).
async fn spawn_turn(
    data: &Arc<SessionData>,
    sid: &str,
    text: String,
    queue_item_id: Value,
    tx: &Tx,
    pending: &PendingCallbacks,
) -> String {
    let n = data.turn_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let turn_id = format!("t-{sid}-{n}");
    let task = tokio::spawn(run_turn_script(
        data.clone(),
        sid.to_string(),
        text,
        n,
        turn_id.clone(),
        queue_item_id,
        tx.clone(),
        pending.clone(),
    ));
    {
        let mut inner = data.inner.lock().await;
        inner.active_turn = Some(turn_id.clone());
        inner.active_abort = Some(task.abort_handle());
        data.persist(&inner).await;
    }
    set_status(
        data,
        tx,
        sid,
        json!({"type": "running", "activeTurnId": turn_id}),
    )
    .await;
    turn_id
}

/// Part 1 of the scripted turn: user entry → reasoning → assistant stream
/// → shell effect → approval `callback/call`. Part 2 resumes in the
/// `callback/respond` handler once the client answers.
#[allow(clippy::too_many_arguments)]
async fn run_turn_script(
    data: Arc<SessionData>,
    sid: String,
    text: String,
    n: u64,
    turn_id: String,
    queue_item_id: Value,
    tx2: Tx,
    pending: PendingCallbacks,
) {
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
                .send(json!({"jsonrpc": "2.0", "method": m, "params": p}).to_string())
                .await;
        }
    };
    let server_request = |id: Value, m: String, p: Value| {
        let tx = tx2.clone();
        async move {
            let _ = tx
                .send(json!({"jsonrpc": "2.0", "id": id, "method": m, "params": p}).to_string())
                .await;
        }
    };
    let e_user = format!("e-user-{n}");
    let e_think = format!("e-think-{n}");
    let e_asst = format!("e-asst-{n}");
    let e_shell = format!("e-shell-{n}");
    let e_cb = format!("e-cb-{n}");
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
        json!({"turn": turn_with_queue(&turn_id, &sid, "in_progress", queue_item_id)}),
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
    let patch = json!([{"op": "append", "path": "/text", "value": "Thinking about the request… "}]);
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
        "Fixture response: ".to_string(),
        format!("echo “{text}” — "),
        "running a shell effect next.".to_string(),
    ] {
        tokio::time::sleep(Duration::from_millis(90)).await;
        let patch = json!([{"op": "append", "path": "/content/0/text", "value": chunk}]);
        let eid = data.patch_entry(&e_asst, &patch).await;
        let (m, p) = evt(
            "history/entryUpdated",
            eid,
            json!({"turnId": turn_id, "entryId": e_asst, "patch": patch}),
        );
        send(m, p).await;
    }
    // Shell effect: pending → running → blocked on approval.
    let shell_entry = shell_effect(&e_shell, &sid, &turn_id, json!({"status": "pending"}));
    let eid = data.add_entry(shell_entry.clone()).await;
    let (m, p) = evt(
        "history/entryAdded",
        eid,
        json!({"turnId": turn_id, "entry": shell_entry}),
    );
    send(m, p).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let patch = json!([{"op": "replace", "path": "/state", "value": {"status": "running", "outputText": ""}}]);
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
    let patch = json!([{"op": "replace", "path": "/state", "value": {"status": "blocked", "callbackId": callback_id, "outputText": ""}}]);
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
        .insert(callback_id.clone(), (data.clone(), n, turn_id.clone()));
    set_status(
        &data,
        &tx2,
        &sid,
        json!({
            "type": "blocked",
            "activeTurnId": turn_id,
            "callbackId": callback_id,
            "reason": "approval",
        }),
    )
    .await;
    server_request(
        json!(format!("srv-cb-{n}")),
        "callback/call".to_string(),
        json!({"callback": cb_entry}),
    )
    .await;
}

/// Part 2: approval answered → resolve the callback, complete the effect,
/// finish the turn, then promote the next queued turn (fixture mirrors
/// `_after_turn_terminal`).
async fn finish_turn(
    data: Arc<SessionData>,
    n: u64,
    turn_id: String,
    output: Value,
    tx2: Tx,
    pending: PendingCallbacks,
) {
    let sid = {
        let inner = data.inner.lock().await;
        inner.session["id"].as_str().unwrap_or("").to_string()
    };
    let send = |m: &str, p: Value| {
        let tx = tx2.clone();
        let m = m.to_string();
        async move {
            let _ = tx
                .send(json!({"jsonrpc": "2.0", "method": m, "params": p}).to_string())
                .await;
        }
    };
    let evt = |eid: u64, extra: Value| {
        let mut p = extra;
        p["eventId"] = json!(eid);
        p["sessionId"] = json!(sid);
        p["emittedAt"] = json!(1_700_000_000);
        p
    };
    let e_think = format!("e-think-{n}");
    let e_asst = format!("e-asst-{n}");
    let e_shell = format!("e-shell-{n}");
    let e_cb = format!("e-cb-{n}");
    tokio::time::sleep(Duration::from_millis(80)).await;
    // Resolve the callback entry.
    let patch = json!([{"op": "replace", "path": "/state", "value": {"status": "answered", "output": output}}]);
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
    let patch = json!([{"op": "replace", "path": "/state", "value": {
        "status": "completed", "output": {"stdout": "fixture-output"},
        "outputText": "fixture-output", "durationMs": 42,
        "display": {"success": true, "verb": "Ran", "message": "fixture-output", "warnings": [], "suffix": ""}
    }}]);
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
    let patch = json!([{"op": "append", "path": "/content/0/text", "value": " Done — effect approved and completed."}]);
    let eid = data.patch_entry(&e_asst, &patch).await;
    send(
        "history/entryUpdated",
        evt(
            eid,
            json!({"turnId": turn_id, "entryId": e_asst, "patch": patch}),
        ),
    )
    .await;
    let patch = json!([{"op": "replace", "path": "/generationStatus", "value": "completed"}]);
    let eid = data.patch_entry(&e_think, &patch).await;
    send(
        "history/entryUpdated",
        evt(
            eid,
            json!({"turnId": turn_id, "entryId": e_think, "patch": patch}),
        ),
    )
    .await;
    {
        let mut inner = data.inner.lock().await;
        inner.active_turn = None;
        inner.active_abort = None;
    }
    let term = terminal_status(&data).await;
    set_status(&data, &tx2, &sid, term).await;
    send(
        "turn/completed",
        evt(
            data.bump().await,
            json!({"turn": {"id": turn_id, "sessionId": sid, "status": "completed", "startedAt": 1, "completedAt": 2, "error": null, "stopReason": null, "queueItemId": null}}),
        ),
    )
    .await;
    promote_next(&data, &sid, &tx2, &pending).await;
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

    let sessions: Sessions = Arc::new(tokio::sync::Mutex::new(
        BTreeMap::<String, Arc<SessionData>>::new(),
    ));
    let pending_callbacks: PendingCallbacks = Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));
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
        // Client responses to fixture→client requests (callback/call) share
        // the pipe: they carry `result`/`error` and no method — not ours.
        if env.get("method").is_none() {
            continue;
        }
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
        let respond_err = |code: &'static str, message: &'static str| {
            let tx = tx.clone();
            let id = id.clone();
            async move {
                if let Some(id) = id {
                    let _ = tx
                        .send(
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": null}})
                                .to_string(),
                        )
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
                // Stored sessions win over the canned demo rows so pin /
                // archive / rename mutations stay visible across lists.
                // `stored_ids` tracks every stored session — even an archived
                // one filtered out of `items` — so its demo twin can't
                // slip back in.
                let mut items: Vec<Value> = Vec::new();
                let mut stored_ids: std::collections::HashSet<String> = Default::default();
                if let Ok(mut dir) = tokio::fs::read_dir(&store).await {
                    while let Ok(Some(ent)) = dir.next_entry().await {
                        let path = ent.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                                if let Some(sid) = v["session"]["id"].as_str() {
                                    stored_ids.insert(sid.to_string());
                                    // Tombstone: a deleted id still suppresses
                                    // its canned demo row.
                                    if v["deleted"] == true {
                                        continue;
                                    }
                                    if (include_archived || v["session"]["archivedAt"].is_null())
                                        && !items.iter().any(|i| i["id"] == json!(sid))
                                    {
                                        items.push(v["session"].clone());
                                    }
                                }
                            }
                        }
                    }
                }
                for mut demo in [
                    session_value("saved-aaaa1111", "/tmp/project-a", json!({"type": "idle"})),
                    session_value(
                        "saved-bbbb2222",
                        "/tmp/project-b",
                        json!({"type": "archived"}),
                    ),
                ] {
                    // An archived row carries `archivedAt` — the rail reads
                    // the timestamp to decide archive vs unarchive.
                    if demo["status"]["type"] == json!("archived") && demo["archivedAt"].is_null() {
                        demo["archivedAt"] = json!(1_700_000_000);
                    }
                    let archived = !demo["archivedAt"].is_null()
                        || demo["status"]["type"] == json!("archived");
                    if archived && !include_archived {
                        continue;
                    }
                    if !stored_ids.contains(demo["id"].as_str().unwrap_or("")) {
                        items.push(demo);
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
                let data = find_session(&sessions, &store, sid).await;
                // A tombstoned id reads as not_found, never the demo stub —
                // read must not resurrect a deleted session.
                let tombstoned = data.is_none() && {
                    store_safe_id(sid)
                        && tokio::fs::read_to_string(store.join(format!("{sid}.json")))
                            .await
                            .ok()
                            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                            .is_some_and(|v| v["deleted"] == true)
                };
                if tombstoned {
                    respond_err("not_found", "session").await;
                    continue;
                }
                let state = match data {
                    Some(d) => {
                        sessions.lock().await.insert(sid.to_string(), d.clone());
                        // Honor historyLimit (resume) / history.limit (read)
                        // like the real server: tail window + before-cursor.
                        let limit = params["historyLimit"]
                            .as_u64()
                            .or_else(|| params.pointer("/history/limit").and_then(Value::as_u64))
                            .map(|l| l as usize);
                        d.state_windowed(limit).await
                    }
                    None => state_value(
                        session_value(sid, "/tmp", json!({"type": "idle"})),
                        0,
                        json!([]),
                        Value::Null,
                        empty_queue(),
                    ),
                };
                respond(json!({"state": state, "lastEventId": 0})).await;
            }
            "session/fork" => {
                let src = params["sourceSessionId"].as_str().unwrap_or("");
                if !store_safe_id(src) {
                    respond_err("invalid_params", "sourceSessionId").await;
                    continue;
                }
                let data = sessions.lock().await.get(src).cloned();
                let data = match data {
                    Some(d) => Some(d),
                    None => SessionData::load(&store, src).await,
                };
                let Some(d) = data else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let fork_id = format!("{src}-fork");
                // Snapshot session + history + watermark + turn counter
                // under one parent lock — the child must never claim a
                // watermark ahead of the history it inherited.
                let (mut session, history, event_id, turn_seq) = {
                    let p = d.inner.lock().await;
                    (
                        p.session.clone(),
                        p.history.clone(),
                        p.event_id,
                        d.turn_seq.load(Ordering::SeqCst),
                    )
                };
                // Refresh catalog fields from the parent's disk record while
                // the clone still carries the parent's id — a rail process's
                // archive/rename lands in memory only via this merge.
                refresh_catalog_fields(&d, &mut session).await;
                session["id"] = json!(fork_id);
                session["parentSessionId"] = json!(src);
                // The child has no turn — it inherits the archive marker,
                // not the parent's live running/blocked status.
                session["status"] = status_without_turn(&session);
                let fork = Arc::new(SessionData::new(session, store.clone()));
                {
                    let mut inner = fork.inner.lock().await;
                    inner.history = history;
                    inner.event_id = event_id;
                    fork.turn_seq.store(turn_seq, Ordering::SeqCst);
                    fork.persist(&inner).await;
                }
                // Return the CHILD's state — the caller opens it.
                let state = fork.state().await;
                sessions.lock().await.insert(fork_id, fork);
                respond(json!({"state": state, "sourceSessionId": src, "lastEventId": 0})).await;
            }
            "turn/start" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let text = params
                    .pointer("/message/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let turn_id =
                    spawn_turn(&data, &sid, text, Value::Null, &tx, &pending_callbacks).await;
                respond(json!({"turn": turn(&turn_id, &sid, "in_progress"), "lastEventId": 0}))
                    .await;
            }
            "callback/respond" => {
                let cb_id = params["callbackId"].as_str().unwrap_or("").to_string();
                respond(json!({"status": "accepted"})).await;
                // Finish the scripted turn once the answer lands.
                if let Some((data, n, turn_id)) = pending_callbacks.lock().await.remove(&cb_id) {
                    let tx2 = tx.clone();
                    let pending = pending_callbacks.clone();
                    let output = params["output"].clone();
                    let sid = {
                        let inner = data.inner.lock().await;
                        inner.session["id"].as_str().unwrap_or("").to_string()
                    };
                    set_status(
                        &data,
                        &tx2,
                        &sid,
                        json!({"type": "running", "activeTurnId": turn_id}),
                    )
                    .await;
                    tokio::spawn(async move {
                        finish_turn(data, n, turn_id, output, tx2, pending).await;
                    });
                }
            }
            "session/turn/enqueue" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let qn = data.queue_seq.fetch_add(1, Ordering::SeqCst) + 1;
                let qid = format!("q-{sid}-{qn}");
                {
                    let mut inner = data.inner.lock().await;
                    inner.queue_items.push(json!({
                        "id": qid,
                        "createdAt": 1_700_000_000,
                        "entries": params["entries"],
                    }));
                    data.persist(&inner).await;
                }
                respond(json!({"queueItemId": qid})).await;
                emit_queue_updated(&tx, &data, &sid).await;
                // Idle queue promotes immediately — same as the real server.
                promote_next(&data, &sid, &tx, &pending_callbacks).await;
            }
            "session/turn/queue/read" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let queue = queue_value(&*data.inner.lock().await);
                respond(json!({"queue": queue})).await;
            }
            "session/turn/queue/remove" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let item_id = params["queueItemId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                {
                    let mut inner = data.inner.lock().await;
                    inner
                        .queue_items
                        .retain(|i| i["id"].as_str() != Some(item_id));
                    data.persist(&inner).await;
                }
                respond(json!({})).await;
                emit_queue_updated(&tx, &data, &sid).await;
            }
            "session/turn/queue/replace" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let item_id = params["queueItemId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                {
                    let mut inner = data.inner.lock().await;
                    if let Some(item) = inner
                        .queue_items
                        .iter_mut()
                        .find(|i| i["id"].as_str() == Some(item_id))
                    {
                        item["entries"] = params["entries"].clone();
                    }
                    data.persist(&inner).await;
                }
                respond(json!({"queueItemId": item_id})).await;
                emit_queue_updated(&tx, &data, &sid).await;
            }
            "session/turn/queue/steer" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let item_id = params["queueItemId"].as_str().unwrap_or("");
                let expected = params["expectedTurnId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                {
                    let mut inner = data.inner.lock().await;
                    inner
                        .queue_items
                        .retain(|i| i["id"].as_str() != Some(item_id));
                    data.persist(&inner).await;
                }
                respond(json!({"queueItemId": item_id, "turnId": expected, "lastEventId": 0}))
                    .await;
                emit_queue_updated(&tx, &data, &sid).await;
            }
            "session/turn/queue/resume" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                {
                    let mut inner = data.inner.lock().await;
                    inner.queue_paused = false;
                    data.persist(&inner).await;
                }
                respond(json!({})).await;
                emit_queue_updated(&tx, &data, &sid).await;
                promote_next(&data, &sid, &tx, &pending_callbacks).await;
            }
            "turn/interrupt" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let expected = params["expectedTurnId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                // Upstream validates expectedTurnId BEFORE aborting
                // (`_require_active_turn` raises "Active turn does not match
                // expectedTurnId"). Abort first and a stale id strands the
                // queue: the script dies but active_turn stays set, so
                // nothing can promote.
                let (aborted, killed_status) = {
                    let mut inner = data.inner.lock().await;
                    match inner.active_turn.clone() {
                        Some(t) if expected.is_empty() || t == expected => {
                            if let Some(abort) = inner.active_abort.take() {
                                abort.abort();
                            }
                            inner.active_turn = None;
                            pending_callbacks.lock().await.remove(&format!("cb-{t}"));
                            // Interrupting pauses the queue (upstream
                            // _after_turn_terminal).
                            inner.queue_paused = true;
                            data.persist(&inner).await;
                            (true, true)
                        }
                        Some(_) => (false, false),
                        None => (true, false),
                    }
                };
                if !aborted {
                    respond_err("invalid_params", "expectedTurnId").await;
                    continue;
                }
                if killed_status {
                    let term = terminal_status(&data).await;
                    set_status(&data, &tx, &sid, term).await;
                }
                respond(json!({"accepted": true, "lastEventId": 0})).await;
                emit_queue_updated(&tx, &data, &sid).await;
            }
            "turn/steer" => {
                respond(json!({"accepted": true, "lastEventId": 0})).await;
            }
            "session/stop" | "session/close" => {
                respond(json!({"closed": true})).await;
            }
            "session/pin" | "session/archive" | "session/markAsSeen" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                // Unknown-but-safe ids materialize — the demo catalog rows
                // aren't in the store until an op persists them.
                let data = find_session(&sessions, &store, &sid).await.or_else(|| {
                    store_safe_id(&sid).then(|| {
                        Arc::new(SessionData::new(
                            session_value(&sid, "/tmp", json!({"type": "idle"})),
                            store.clone(),
                        ))
                    })
                });
                let Some(data) = data else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                sessions.lock().await.insert(sid.clone(), data.clone());
                let (field, value) = match method.as_str() {
                    "session/pin" => (
                        "pinnedAt",
                        if params["pinned"].as_bool().unwrap_or(false) {
                            json!(1_700_000_000)
                        } else {
                            Value::Null
                        },
                    ),
                    "session/archive" => (
                        "archivedAt",
                        if params["archived"].as_bool().unwrap_or(false) {
                            json!(1_700_000_000)
                        } else {
                            Value::Null
                        },
                    ),
                    _ => ("isUnseen", json!(false)),
                };
                data.set_session_field(field, value.clone()).await;
                // `status.type` tracks archive state too (upstream keeps them
                // consistent) — without this an unarchived stored row would
                // still report status "archived". A live turn keeps its
                // running/blocked/failed status; only idle↔archived move.
                let mut patch_ops = vec![
                    json!({"op": "replace", "path": format!("/session/{field}"), "value": value}),
                ];
                if field == "archivedAt" {
                    let archived = params["archived"].as_bool().unwrap_or(false);
                    let cur = {
                        let inner = data.inner.lock().await;
                        inner.session["status"]["type"]
                            .as_str()
                            .unwrap_or("")
                            .to_string()
                    };
                    let status = match (cur.as_str(), archived) {
                        ("idle", true) => Some(json!({"type": "archived"})),
                        ("archived", false) => Some(json!({"type": "idle"})),
                        _ => None,
                    };
                    if let Some(status) = status {
                        data.set_session_field("status", status.clone()).await;
                        patch_ops.push(
                            json!({"op": "replace", "path": "/session/status", "value": status}),
                        );
                    }
                }
                let body = match field {
                    "pinnedAt" => json!({"pinnedAt": value}),
                    "archivedAt" => json!({"archivedAt": value}),
                    _ => json!({}),
                };
                respond(body).await;
                emit_updated(&tx, &data, &sid, Value::Array(patch_ops)).await;
            }
            "session/rename" | "session/title/update" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let title = params["title"].as_str().unwrap_or("").to_string();
                let data = find_session(&sessions, &store, &sid).await.or_else(|| {
                    store_safe_id(&sid).then(|| {
                        Arc::new(SessionData::new(
                            session_value(&sid, "/tmp", json!({"type": "idle"})),
                            store.clone(),
                        ))
                    })
                });
                let Some(data) = data else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                sessions.lock().await.insert(sid.clone(), data.clone());
                data.set_session_field("title", json!(title)).await;
                respond(json!({"title": title, "updatedAt": "1", "lastEventId": 0})).await;
                emit_updated(
                    &tx,
                    &data,
                    &sid,
                    json!([{"op": "replace", "path": "/session/title", "value": title}]),
                )
                .await;
            }
            "session/delete" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                sessions.lock().await.remove(&sid);
                if store_safe_id(&sid) {
                    // Tombstone instead of a bare remove: the canned demo
                    // row for this id must stay suppressed on later lists.
                    let _ = tokio::fs::write(
                        store.join(format!("{sid}.json")),
                        json!({"session": {"id": sid}, "deleted": true}).to_string(),
                    )
                    .await;
                }
                respond(json!({})).await;
            }
            "session/rewind/read" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let entry_id = params["entryId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let inner = data.inner.lock().await;
                match rewind_index(&inner, entry_id) {
                    Some(idx) => {
                        let changed = rewind_has_changes(&inner, idx);
                        respond(json!({
                            "hasFileChanges": changed,
                            "paths": if changed { json!(["src/fixture.rs"]) } else { json!([]) }
                        }))
                        .await;
                    }
                    None => respond_err("invalid_params", "entryId").await,
                }
            }
            "session/rewind" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let entry_id = params["entryId"].as_str().unwrap_or("");
                let inplace = params["inplace"].as_bool().unwrap_or(false);
                let restore_files = params["restoreFiles"].as_bool().unwrap_or(false);
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let idx = {
                    let inner = data.inner.lock().await;
                    match rewind_index(&inner, entry_id) {
                        Some(i) => i,
                        None => {
                            respond_err("invalid_params", "entryId").await;
                            continue;
                        }
                    }
                };
                let restored = if restore_files {
                    json!(["src/fixture.rs"])
                } else {
                    json!([])
                };
                // Upstream _rewind runs _turns.reset() on the source session
                // either way: kills the active turn and clears the queue, then
                // emits turn/queueUpdated if anything changed. The new state
                // travels in the RESPONSE (state), not a history patch.
                let (queue_changed, killed_status) = {
                    let mut inner = data.inner.lock().await;
                    let changed = !inner.queue_items.is_empty()
                        || inner.queue_paused
                        || inner.active_turn.is_some();
                    inner.queue_items.clear();
                    inner.queue_paused = false;
                    if let Some(abort) = inner.active_abort.take() {
                        abort.abort();
                    }
                    let killed = inner.active_turn.is_some();
                    if let Some(t) = inner.active_turn.take() {
                        pending_callbacks.lock().await.remove(&format!("cb-{t}"));
                    }
                    data.persist(&inner).await;
                    (changed, killed)
                };
                if killed_status {
                    let term = terminal_status(&data).await;
                    set_status(&data, &tx, &sid, term).await;
                }
                if inplace {
                    // Drop the target message and everything after it.
                    let (message, state) = {
                        let mut inner = data.inner.lock().await;
                        let message = entry_text(&inner.history[idx]);
                        inner.history.truncate(idx);
                        inner.event_id += 1;
                        let state = state_value(
                            inner.session.clone(),
                            inner.event_id,
                            json!(inner.history.clone()),
                            Value::Null,
                            queue_value(&inner),
                        );
                        data.persist(&inner).await;
                        (message, state)
                    };
                    respond(json!({
                        "message": message, "restoreErrors": [],
                        "restoredPaths": restored, "state": state,
                        "sessionLog": {"enabled": false}
                    }))
                    .await;
                } else {
                    // Fork semantics: parent keeps its history; the child
                    // carries the truncated copy.
                    let rewind_id = format!("{sid}-rewind");
                    let (mut session, history, event_id, turn_seq, message) = {
                        let p = data.inner.lock().await;
                        (
                            p.session.clone(),
                            p.history[..idx].to_vec(),
                            p.event_id,
                            data.turn_seq.load(Ordering::SeqCst),
                            entry_text(&p.history[idx]),
                        )
                    };
                    refresh_catalog_fields(&data, &mut session).await;
                    session["id"] = json!(rewind_id);
                    session["parentSessionId"] = json!(sid);
                    // Same normalization: the child's own turn set is empty,
                    // so it must not advertise the parent's activeTurnId.
                    session["status"] = status_without_turn(&session);
                    let child = Arc::new(SessionData::new(session, store.clone()));
                    {
                        let mut inner = child.inner.lock().await;
                        inner.history = history;
                        inner.event_id = event_id;
                        child.turn_seq.store(turn_seq, Ordering::SeqCst);
                        child.persist(&inner).await;
                    }
                    let state = child.state().await;
                    sessions.lock().await.insert(rewind_id, child);
                    respond(json!({
                        "message": message, "restoreErrors": [],
                        "restoredPaths": restored, "state": state,
                        "sessionLog": {"enabled": false}
                    }))
                    .await;
                }
                if queue_changed {
                    emit_queue_updated(&tx, &data, &sid).await;
                }
            }
            "session/history/list" => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let Some(data) = find_session(&sessions, &store, &sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                let inner = data.inner.lock().await;
                let len = inner.history.len();
                let limit = params
                    .pointer("/page/limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as usize;
                let cursor = params
                    .pointer("/page/cursor")
                    .and_then(Value::as_str)
                    .and_then(|c| c.strip_prefix("h-"))
                    .and_then(|c| c.parse::<usize>().ok());
                let direction = params
                    .pointer("/page/direction")
                    .and_then(Value::as_str)
                    .unwrap_or("backward");
                let (items, next, prev) = if direction == "forward" {
                    let start = cursor.unwrap_or(0).min(len);
                    let end = (start + limit).min(len);
                    (
                        inner.history[start..end].to_vec(),
                        if end < len {
                            json!(format!("h-{end}"))
                        } else {
                            Value::Null
                        },
                        Value::Null,
                    )
                } else {
                    let end = cursor.unwrap_or(len).min(len);
                    let start = end.saturating_sub(limit);
                    (
                        inner.history[start..end].to_vec(),
                        Value::Null,
                        if start > 0 {
                            json!(format!("h-{start}"))
                        } else {
                            Value::Null
                        },
                    )
                };
                respond(json!({"items": items, "nextCursor": next, "previousCursor": prev})).await;
            }
            "workspace/trust/status" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("/tmp")
                    .to_string();
                let trusted = trusted_cwds(&store).await.contains(&cwd);
                if trusted {
                    respond(json!({"status": "trusted", "details": null})).await;
                } else {
                    respond(json!({
                        "status": "untrusted",
                        "details": {
                            "cwd": cwd,
                            "repoRoot": null,
                            "detectedFiles": [".vibe/settings.toml"],
                            "repoDetectedFiles": [],
                            "repoExplicitlyUntrusted": false,
                            "settingsPath": store.join("trusted.json").to_string_lossy(),
                            "availableDecisions": ["trust_repo", "trust_cwd", "decline"]
                        }
                    }))
                    .await;
                }
            }
            "workspace/trust/decision" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("/tmp")
                    .to_string();
                if params["decision"].as_str().unwrap_or("decline") != "decline" {
                    let mut set = trusted_cwds(&store).await;
                    set.insert(cwd);
                    let mut list: Vec<String> = set.into_iter().collect();
                    list.sort();
                    let _ = tokio::fs::write(
                        store.join("trusted.json"),
                        serde_json::to_string(&list).unwrap_or_default(),
                    )
                    .await;
                }
                respond(json!({})).await;
            }
            "workspace/trust/untrustedConfig" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("/tmp")
                    .to_string();
                let dirs = if trusted_cwds(&store).await.contains(&cwd) {
                    json!([])
                } else {
                    json!([cwd])
                };
                respond(json!({
                    "dirs": dirs,
                    "settingsPath": store.join("trusted.json").to_string_lossy()
                }))
                .await;
            }
            "session/compact" => {
                let sid = params["sessionId"].as_str().unwrap_or("");
                let Some(data) = find_session(&sessions, &store, sid).await else {
                    respond_err("not_found", "session").await;
                    continue;
                };
                // Compaction hands off to a fresh session carrying a summary
                // entry — same event shape as the real server.
                let compact_id = format!("{sid}-compact");
                let (mut session, event_id, turn_seq) = {
                    let p = data.inner.lock().await;
                    (
                        p.session.clone(),
                        p.event_id,
                        data.turn_seq.load(Ordering::SeqCst),
                    )
                };
                session["id"] = json!(compact_id);
                session["parentSessionId"] = json!(sid);
                let child = Arc::new(SessionData::new(session, store.clone()));
                {
                    let mut inner = child.inner.lock().await;
                    let summary = message_entry(
                        "e-compact-1",
                        &compact_id,
                        "",
                        "assistant",
                        "Fixture summary of the compacted conversation.",
                    );
                    inner.history = vec![summary];
                    inner.event_id = event_id + 1;
                    child.turn_seq.store(turn_seq, Ordering::SeqCst);
                    child.persist(&inner).await;
                }
                let state = child.state().await;
                sessions
                    .lock()
                    .await
                    .insert(compact_id.clone(), child.clone());
                respond(json!({
                    "summary": "fixture summary",
                    "state": state,
                    "sessionLog": {"enabled": false}
                }))
                .await;
                let eid = child.bump().await;
                let _ = tx
                    .send(
                        json!({"jsonrpc": "2.0", "method": "session/compacted", "params": {
                            "eventId": eid, "sessionId": compact_id, "emittedAt": 1_700_000_000,
                            "oldSessionId": sid, "state": state,
                            "sessionLog": {"enabled": false}, "summaryLength": 15
                        }})
                        .to_string(),
                    )
                    .await;
            }
            "config/read" => {
                // Voice-bearing ConfigView subset; endpoints point at
                // unreachable fixture hosts — tests never dial them, they
                // only verify the client parses the views.
                respond(json!({
                    "config": {
                        "voiceModeEnabled": true,
                        "narratorEnabled": true,
                        "speech": {
                            "model": {"name": "voxtral-mini-tts-latest", "voice": "fixture", "responseFormat": "wav"},
                            "provider": {"apiBase": "https://tts.fixture.invalid", "apiKeyEnvVar": "VIBE_FIXTURE_TTS_KEY", "client": "mistral"},
                        },
                        "transcription": {
                            "model": {"name": "voxtral-mini-transcribe-realtime-2602", "sampleRate": 16000, "encoding": "pcm_s16le", "language": "en", "targetStreamingDelayMs": 240},
                            "provider": {"apiBase": "https://transcribe.fixture.invalid", "apiKeyEnvVar": "VIBE_FIXTURE_TRANSCRIBE_KEY", "client": "mistral"},
                        },
                        "activeModel": {"name": "mistral-large-latest", "alias": "large", "thinking": "medium", "supportsImages": true, "displayName": "Large"},
                        "activeModelPinned": false,
                        "defaultModelAlias": "large",
                        "defaultAgent": "build",
                        "models": [
                            {"name": "mistral-large-latest", "alias": "large", "thinking": "medium", "supportsImages": true, "displayName": "Large"},
                            {"name": "mistral-small-latest", "alias": "small", "thinking": "low", "supportsImages": false, "displayName": "Small"},
                        ],
                        "theme": "vibe",
                        "worktreeLimit": 3,
                        "enableNotifications": true,
                        "transcribeModels": ["voxtral-mini-transcribe-realtime-2602"],
                        "ttsModels": ["voxtral-mini-tts-latest"],
                        "validationWarnings": [],
                    }
                }))
                .await;
            }
            "config/fields/read" => {
                // Writes persist to config_values.json so a later read
                // reflects them — the round-trip must be observable.
                let overlays = read_overlay(&store, "config_values.json");
                let mut fields = json!([
                    {"name": "enable_notifications", "kind": "bool", "description": "Desktop notifications", "value": true, "path": "enable_notifications", "popular": true, "enumChoices": [], "valueLabels": {}, "layerValues": [{"layer": "user", "value": true}]},
                    {"name": "theme", "kind": "enum", "description": "UI theme", "value": "vibe", "path": "theme", "popular": true, "enumChoices": ["vibe", "light", "dark"], "valueLabels": {}, "layerValues": [{"layer": "default", "value": "vibe"}]},
                    {"name": "worktree_limit", "kind": "int", "description": "Max git worktrees", "value": 3, "path": "worktree_limit", "popular": false, "enumChoices": [], "valueLabels": {}, "layerValues": [{"layer": "default", "value": 3}]},
                ]);
                for f in fields.as_array_mut().into_iter().flatten() {
                    let path = f["path"].as_str().unwrap_or_default();
                    if let Some(v) = overlays.get(path) {
                        f["value"] = v.clone();
                    }
                }
                respond(json!({"fields": fields, "targets": ["user", "project"]}))
                    .await;
            }
            "config/write" => {
                let mut overlays = read_overlay(&store, "config_values.json");
                for op in params["ops"].as_array().into_iter().flatten() {
                    let path = op["path"].as_str().unwrap_or_default();
                    match op["op"].as_str() {
                        Some("set") => {
                            overlays.insert(path.to_string(), op["value"].clone());
                        }
                        Some("remove") => {
                            overlays.remove(path);
                        }
                        _ => {}
                    }
                }
                let persisted = persist_overlay(&store, "config_values.json", &overlays).await;
                if persisted {
                    respond(json!({"rejected": false, "failures": [], "status": "applied"})).await;
                } else {
                    respond(json!({"rejected": true, "failures": ["fixture: config persist failed"], "status": null})).await;
                }
            }
            "config/model/write" => {
                respond(json!({"status": "applied"})).await;
            }
            "agents/list" => {
                let agents = json!([
                    {"name": "build", "displayName": "Build", "description": "Edits code freely", "safety": "neutral", "agentType": "agent"},
                    {"name": "plan", "displayName": "Plan", "description": "Read-only planning", "safety": "safe", "agentType": "agent"},
                    {"name": "accept-edits", "displayName": "Accept Edits", "description": "Auto-approves edits", "safety": "yolo", "agentType": "agent"},
                ]);
                // Applied switches persist per session — an unconditional
                // "build" here would visually revert the user's pick.
                let sid = params["sessionId"].as_str().unwrap_or("saved-aaaa1111");
                let active_name = if store_safe_id(sid) {
                    read_overlay(&store, &format!("agent_{sid}.json"))
                        .get("active")
                        .and_then(|v| v.as_str())
                        .unwrap_or("build")
                        .to_string()
                } else {
                    "build".to_string()
                };
                let active = agents.as_array().into_iter().flatten()
                    .find(|a| a["name"].as_str() == Some(active_name.as_str()))
                    .cloned().unwrap_or_else(|| agents[0].clone());
                respond(json!({"active": active, "agents": agents})).await;
            }
            "session/agent/update" => {
                let name = params["agentName"]
                    .as_str()
                    .or_else(|| params["name"].as_str())
                    .or_else(|| params["agent"].as_str());
                let known = ["build", "plan", "accept-edits"];
                match name {
                    Some(n) if known.contains(&n) => {
                        let sid = params["sessionId"].as_str().unwrap_or("saved-aaaa1111");
                        if store_safe_id(sid) {
                            let mut ov = read_overlay(&store, &format!("agent_{sid}.json"));
                            ov.insert("active".to_string(), json!(n));
                            if !persist_overlay(&store, &format!("agent_{sid}.json"), &ov).await {
                                respond_err("internal_error", "fixture: agent persist failed").await;
                                continue;
                            }
                        }
                        respond(json!({"status": "applied"})).await;
                    }
                    _ => respond_err("invalid_params", "fixture: unknown agent").await,
                }
            }
            "narration/summarize" => {
                respond(json!({"summary": format!("Fixture narration: {}",
                    params["userMessage"].as_str().unwrap_or(""))}))
                .await;
            }
            "review/state" => {
                // Decisions persist to review_decisions.json — an
                // approve/revert on a file target resolves it off the
                // pending list (all regions decided ⇒ nothing left to
                // review), so the round-trip is observable.
                let decisions = read_overlay(&store, "review_decisions.json");
                let pending = |path: &str| !decisions.contains_key(path);
                let files: Vec<Value> = [
                    (json!({"path": "src/main.rs", "status": "modified", "regions": [
                        {"kind": "text", "versionIndex": 0, "ordinal": 0, "owner": {"kind": "agent", "turnId": 1}, "baselineStart": 1, "baselineLineCount": 2, "currentStart": 1, "currentLineCount": 3, "decision": "pending", "dependsOn": []}
                    ]}), "src/main.rs"),
                    (json!({"path": "src/lib.rs", "status": "created", "regions": [
                        {"kind": "text", "versionIndex": 0, "ordinal": 1, "owner": {"kind": "agent", "turnId": 1}, "baselineStart": 0, "baselineLineCount": 0, "currentStart": 0, "currentLineCount": 2, "decision": "pending", "dependsOn": []}
                    ]}), "src/lib.rs"),
                ]
                .into_iter()
                .filter(|(_, p)| pending(p))
                .map(|(f, _)| f)
                .collect();
                let scope_files: Vec<Value> = [
                    json!({"path": "src/main.rs", "status": "modified", "regionCount": 1}),
                    json!({"path": "src/lib.rs", "status": "created", "regionCount": 1}),
                ]
                .into_iter()
                .filter(|f| pending(f["path"].as_str().unwrap_or_default()))
                .collect();
                respond(json!({
                    "files": files,
                    "scopes": [
                        {"owner": {"kind": "agent", "turnId": 1}, "files": scope_files}
                    ]
                }))
                .await;
            }
            "review/turnDiff" => {
                respond(json!({
                    "status": "modified",
                    "baseline": "fn main() {\n    old_call();\n}\n",
                    "current": "fn main() {\n    old_call();\n    new_call();\n}\n"
                }))
                .await;
            }
            "review/approve" | "review/revert" => {
                let decision = if method == "review/approve" { "keep" } else { "revert" };
                let mut decisions = read_overlay(&store, "review_decisions.json");
                // Resolve the target's paths — file/scopeFile name one,
                // all covers the whole pending list; other kinds accept
                // but decide nothing (unmodeled granularity).
                let target = &params["target"];
                let paths: Vec<String> = match target["kind"].as_str() {
                    Some("file") | Some("scopeFile") => {
                        target["path"].as_str().map(|p| vec![p.to_string()]).unwrap_or_default()
                    }
                    Some("all") => vec!["src/main.rs".to_string(), "src/lib.rs".to_string()]
                        .into_iter()
                        .filter(|p| !decisions.contains_key(p))
                        .collect(),
                    _ => Vec::new(),
                };
                for p in paths {
                    decisions.insert(p, Value::String(decision.to_string()));
                }
                if persist_overlay(&store, "review_decisions.json", &decisions).await {
                    respond(json!({})).await;
                } else {
                    respond_err("internal_error", "fixture: review persist failed").await;
                }
            }
            "skills/installed" => {
                respond(json!({"skills": skills_value(&store)})).await;
            }
            "skills/setEnabled" => {
                // RuntimeMutationResponse-shaped; persists so installed
                // reads reflect the toggle. Locked skills reject, same
                // as upstream `_require_toggleable`.
                let mut states = read_overlay(&store, "skill_states.json");
                let name = params["name"].as_str().unwrap_or_default();
                let skill = skills_value(&store).as_array().into_iter().flatten().find(|s| s["name"].as_str() == Some(name)).cloned();
                match skill {
                    None => {
                        respond_err("invalid_params", "fixture: unknown skill").await;
                        continue;
                    }
                    Some(s) if s["locked"].as_bool().unwrap_or(false) => {
                        respond(json!({"rejected": true, "failures": [format!("fixture: skill `{name}` is locked")], "status": null})).await;
                        continue;
                    }
                    _ => {}
                }
                states.insert(name.to_string(), params["enabled"].clone());
                if persist_overlay(&store, "skill_states.json", &states).await {
                    respond(json!({"status": "applied"})).await;
                } else {
                    respond_err("internal_error", "fixture: skill persist failed").await;
                }
            }
            "mcp/read" => {
                respond(json!({"mcp": mcp_state_value(&store)})).await;
            }
            "mcp/toggle" => {
                let mut states = read_overlay(&store, "mcp_states.json");
                let name = params["name"].as_str().unwrap_or_default();
                states.insert(name.to_string(), params["disabled"].clone());
                if persist_overlay(&store, "mcp_states.json", &states).await {
                    respond(json!({"runtime": {"mcp": mcp_state_value(&store)}})).await;
                } else {
                    respond_err("internal_error", "fixture: mcp persist failed").await;
                }
            }
            "connectors/read" => {
                respond(json!({"counts": {"connected": 2, "total": 5}})).await;
            }
            "plugins/read" => {
                respond(json!({"plugins": {
                    "plugins": [{
                        "name": "acme-pack",
                        "version": "1.2.0",
                        "sourceFormat": "claude_code",
                        "manifestDigest": "d1",
                        "description": "ACME tools pack",
                        "author": null,
                        "scope": "global",
                        "components": [{"kind": "skill", "name": "acme-skill"}],
                        "drifted": 0
                    }],
                    "dropped": []
                }}))
                .await;
            }
            "workspace/git/worktrees/list" => {
                respond(json!({
                    "worktrees": [
                        {"name": "feature-x", "branch": "feature-x",
                         "cwd": "/repo/.worktrees/feature-x", "root": "/repo",
                         "repoRoot": "/repo",
                         "branchChanges": {"additions": 12, "deletions": 3}}
                    ],
                    "repositoryBranch": "main",
                    "repositoryCwd": "",
                    "repositoryMappedCwd": "",
                    "repositoryRoot": "/repo"
                }))
                .await;
            }
            "loops/list" => {
                let Some(file) = loops_file(&params) else {
                    respond_err("invalid_params", "fixture: unsafe session id").await;
                    continue;
                };
                let ov = read_overlay(&store, &file);
                let items = ov.get("items").and_then(|v| v.as_object()).cloned().unwrap_or_default();
                respond(json!({"loops": items.values().cloned().collect::<Vec<_>>()}))
                    .await;
            }
            "loops/create" => {
                let Some(file) = loops_file(&params) else {
                    respond_err("invalid_params", "fixture: unsafe session id").await;
                    continue;
                };
                let mut ov = read_overlay(&store, &file);
                // Monotonic per-session sequence — len()+1 would reuse a
                // deleted loop's id and overwrite its survivor.
                let seq = ov.get("_seq").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                let id = format!("loop-{seq}");
                // `<n><unit>` like upstream parse_interval; the unit is a
                // char (not a byte slice) so multibyte tails can't panic,
                // and unknown intervals reject instead of silently
                // becoming 60s.
                let Some(interval) = params["interval"].as_str().and_then(|s| {
                    let u = s.chars().next_back()?;
                    let mult = match u {
                        's' => 1u64,
                        'm' => 60,
                        'h' => 3600,
                        'd' => 86400,
                        _ => return None,
                    };
                    s[..s.len() - u.len_utf8()].parse::<u64>().ok().map(|v| v * mult)
                }) else {
                    respond_err("invalid_params", "fixture: bad interval").await;
                    continue;
                };
                let entry = json!({
                    "id": id,
                    "prompt": params["prompt"],
                    "intervalSeconds": interval,
                    "nextFireAt": 0.0
                });
                ov.insert("_seq".to_string(), json!(seq));
                if !ov.get("items").is_some_and(|v| v.is_object()) {
                    ov.insert("items".to_string(), json!({}));
                }
                ov.get_mut("items").and_then(|v| v.as_object_mut()).unwrap().insert(id, entry.clone());
                if persist_overlay(&store, &file, &ov).await {
                    respond(json!({"loop": entry})).await;
                } else {
                    respond_err("internal_error", "fixture: loop persist failed").await;
                }
            }
            "loops/delete" => {
                let Some(file) = loops_file(&params) else {
                    respond_err("invalid_params", "fixture: unsafe session id").await;
                    continue;
                };
                let mut ov = read_overlay(&store, &file);
                let id = params["loopId"].as_str().unwrap_or_default();
                let entry = ov
                    .get_mut("items")
                    .and_then(|v| v.as_object_mut())
                    .and_then(|m| m.remove(id));
                let Some(entry) = entry else {
                    respond_err("not_found", "fixture: unknown loop").await;
                    continue;
                };
                if persist_overlay(&store, &file, &ov).await {
                    respond(json!({"loop": entry})).await;
                } else {
                    respond_err("internal_error", "fixture: loop persist failed").await;
                }
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
