//! NDJSON JSON-RPC client over a spawned `vibe-app-server` child's stdio.
//!
//! Mirrors `vibe/app_server/client.py`: client→server request ids are
//! `client-N`; incoming lines are notifications, server→client requests
//! (answered via [`Connection::respond`]), or responses resolving pending
//! requests.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::models::*;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error {code}: {message}")]
    Protocol {
        code: String,
        message: String,
        data: Value,
    },
    #[error("connection closed")]
    Closed,
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<ProtocolErrorBody> for ClientError {
    fn from(e: ProtocolErrorBody) -> Self {
        ClientError::Protocol {
            code: e.code,
            message: e.message,
            data: e.data,
        }
    }
}

pub type ClientResult<T> = Result<T, ClientError>;

type PendingMap = HashMap<String, oneshot::Sender<Result<Value, ProtocolErrorBody>>>;

/// A `vibe-app-server` child process speaking NDJSON over stdio.
pub struct Connection {
    child: Child,
    writer: mpsc::Sender<Value>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    events: mpsc::Receiver<ServerMessage>,
}

impl Connection {
    /// Spawn `vibe-app-server` (or any compatible binary) at `program`.
    pub async fn spawn(program: &Path) -> ClientResult<Self> {
        Self::spawn_inner(program, &[], None, &[]).await
    }

    pub async fn spawn_with_args(
        program: &Path,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> ClientResult<Self> {
        Self::spawn_inner(program, args, cwd, &[]).await
    }

    /// Spawn with extra environment variables (e.g. `VIBE_FIXTURE_STORE`).
    pub async fn spawn_with_env(program: &Path, envs: &[(String, String)]) -> ClientResult<Self> {
        Self::spawn_inner(program, &[], None, envs).await
    }

    async fn spawn_inner(
        program: &Path,
        args: &[&str],
        cwd: Option<&Path>,
        envs: &[(String, String)],
    ) -> ClientResult<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .envs(envs.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(ClientError::Closed)?;
        let stdout = child.stdout.take().ok_or(ClientError::Closed)?;

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<_>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel::<ServerMessage>(256);
        let (write_tx, mut write_rx) = mpsc::channel::<Value>(256);

        // Writer task
        let mut stdin_w = stdin;
        tokio::spawn(async move {
            while let Some(msg) = write_rx.next().await {
                let mut line = match serde_json::to_vec(&msg) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                line.push(b'\n');
                if stdin_w.write_all(&line).await.is_err() {
                    return;
                }
                let _ = stdin_w.flush().await;
            }
        });

        // Reader task
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut event_tx = event_tx;
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match parse_incoming(&line) {
                    Ok(Incoming::Message(msg)) => {
                        if event_tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                    Ok(Incoming::Response { id, result }) => {
                        let key = match &id {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let mut map = pending_reader.lock().await;
                        if let Some(tx) = map.remove(&key) {
                            let _ = tx.send(result);
                        }
                    }
                    Err(_) => continue,
                }
            }
            // EOF: fail all pending requests.
            let mut map = pending_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(ProtocolErrorBody {
                    code: "internal_error".into(),
                    message: "server closed the connection".into(),
                    data: Value::Null,
                }));
            }
        });

        Ok(Self {
            child,
            writer: write_tx,
            pending,
            next_id: AtomicU64::new(1),
            events: event_rx,
        })
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Take the stream of notifications + server→client requests.
    /// May only be taken once; a second call yields `None`.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<ServerMessage>> {
        // Replace with a closed receiver on subsequent calls.
        let (tx, rx) = mpsc::channel(0);
        drop(tx);
        Some(std::mem::replace(&mut self.events, rx))
    }

    async fn send_envelope(&self, env: Value) -> ClientResult<()> {
        self.writer
            .clone()
            .send(env)
            .await
            .map_err(|_| ClientError::Closed)
    }

    /// Fire-and-forget request; resolves with the raw `result` object.
    pub async fn request(&self, method: &str, params: impl Serialize) -> ClientResult<Value> {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("client-{n}");
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id.clone(), tx);
        }
        self.send_envelope(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(params)?,
        }))
        .await?;
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(err)) => Err(err.into()),
            Err(_) => Err(ClientError::Closed),
        }
    }

    /// Typed convenience: deserialize the result into `T`.
    pub async fn request_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> ClientResult<T> {
        let result = self.request(method, params).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Send a notification (no id, no response).
    pub async fn notify(&self, method: &str, params: impl Serialize) -> ClientResult<()> {
        self.send_envelope(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params)?,
        }))
        .await
    }

    /// Answer a server→client request (`callback/call`, `clientTool/*`).
    pub async fn respond(&self, id: &Value, result: impl Serialize) -> ClientResult<()> {
        self.send_envelope(json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": serde_json::to_value(result)?,
        }))
        .await
    }

    /// Answer a server→client request with an error.
    pub async fn respond_error(&self, id: &Value, code: &str, message: &str) -> ClientResult<()> {
        self.send_envelope(json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "error": {"code": code, "message": message, "data": null},
        }))
        .await
    }

    // -- Convenience wrappers -------------------------------------------------

    pub async fn initialize(
        &self,
        info: ClientInfo,
        caps: ClientCapabilities,
    ) -> ClientResult<InitializeResponse> {
        let resp: InitializeResponse = self
            .request_typed(
                "initialize",
                InitializeParams {
                    client_info: info,
                    capabilities: caps,
                },
            )
            .await?;
        self.notify("initialized", serde_json::json!({})).await?;
        Ok(resp)
    }

    pub async fn session_list(
        &self,
        params: SessionListParams,
    ) -> ClientResult<SessionListResponse> {
        self.request_typed("session/list", params).await
    }

    pub async fn session_start(
        &self,
        params: SessionStartParams,
    ) -> ClientResult<PublicSessionState> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            state: PublicSessionState,
        }
        Ok(self
            .request_typed::<Resp>("session/start", params)
            .await?
            .state)
    }

    pub async fn session_resume(
        &self,
        session_id: &str,
        agent_config: AgentConfig,
        history_limit: u32,
    ) -> ClientResult<PublicSessionState> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            state: PublicSessionState,
        }
        Ok(self
            .request_typed::<Resp>(
                "session/resume",
                SessionResumeParams {
                    session_id: session_id.to_string(),
                    agent_config,
                    history_limit,
                },
            )
            .await?
            .state)
    }

    pub async fn session_continue(
        &self,
        agent_config: AgentConfig,
        history_limit: u32,
    ) -> ClientResult<PublicSessionState> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            state: PublicSessionState,
        }
        Ok(self
            .request_typed::<Resp>(
                "session/continue",
                SessionContinueParams {
                    agent_config,
                    history_limit,
                },
            )
            .await?
            .state)
    }

    pub async fn session_read(&self, session_id: &str) -> ClientResult<PublicSessionState> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            state: PublicSessionState,
        }
        Ok(self
            .request_typed::<Resp>(
                "session/read",
                SessionReadParams {
                    session_id: session_id.to_string(),
                    history: Some(PageRequest::default()),
                    turns: Some(PageRequest::default()),
                },
            )
            .await?
            .state)
    }

    pub async fn turn_start(&self, session_id: &str, text: &str) -> ClientResult<PublicTurn> {
        let resp: TurnStartResponse = self
            .request_typed(
                "turn/start",
                TurnStartParams {
                    idempotency_key: None,
                    session_id: session_id.to_string(),
                    message: vec![ContentBlock::text(text)],
                    injected: false,
                    client_user_message_id: None,
                    auto_title: None,
                },
            )
            .await?;
        Ok(resp.turn)
    }

    pub async fn turn_steer(
        &self,
        session_id: &str,
        turn_id: &str,
        text: &str,
    ) -> ClientResult<()> {
        self.request(
            "turn/steer",
            TurnSteerParams {
                session_id: session_id.to_string(),
                expected_turn_id: turn_id.to_string(),
                message: vec![ContentBlock::text(text)],
            },
        )
        .await?;
        Ok(())
    }

    pub async fn turn_interrupt(&self, session_id: &str, turn_id: &str) -> ClientResult<()> {
        self.request(
            "turn/interrupt",
            TurnInterruptParams {
                session_id: session_id.to_string(),
                expected_turn_id: turn_id.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn turn_enqueue(&self, session_id: &str, text: &str) -> ClientResult<String> {
        let resp: QueueItemResponse = self
            .request_typed(
                "session/turn/enqueue",
                TurnEnqueueParams {
                    session_id: session_id.to_string(),
                    entries: vec![TurnInputEntry::User {
                        entry_id: None,
                        content: vec![SessionContentBlock::Text {
                            text: text.to_string(),
                        }],
                    }],
                    idempotency_key: None,
                },
            )
            .await?;
        Ok(resp.queue_item_id)
    }

    pub async fn callback_respond(
        &self,
        session_id: &str,
        callback_id: &str,
        output: CallbackOutput,
    ) -> ClientResult<()> {
        self.request(
            "callback/respond",
            CallbackRespondParams {
                session_id: session_id.to_string(),
                callback_id: callback_id.to_string(),
                output,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn session_stop(&self, session_id: &str) -> ClientResult<()> {
        self.request(
            "session/stop",
            SessionIdParams {
                session_id: session_id.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn session_rename(&self, session_id: &str, title: &str) -> ClientResult<()> {
        self.request(
            "session/rename",
            SessionTitleUpdateParams {
                session_id: session_id.to_string(),
                title: title.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn session_pin(&self, session_id: &str, pinned: bool) -> ClientResult<()> {
        self.request(
            "session/pin",
            SessionPinParams {
                session_id: session_id.to_string(),
                pinned,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn session_archive(&self, session_id: &str, archived: bool) -> ClientResult<()> {
        self.request(
            "session/archive",
            SessionArchiveParams {
                session_id: session_id.to_string(),
                archived,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn session_fork(
        &self,
        source_session_id: &str,
        entry_id: Option<&str>,
        history_limit: u32,
    ) -> ClientResult<PublicSessionState> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ForkParams<'a> {
            source_session_id: &'a str,
            entry_id: Option<&'a str>,
            history_limit: u32,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Resp {
            state: PublicSessionState,
        }
        Ok(self
            .request_typed::<Resp>(
                "session/fork",
                ForkParams {
                    source_session_id,
                    entry_id,
                    history_limit,
                },
            )
            .await?
            .state)
    }

    pub async fn session_compact(&self, session_id: &str) -> ClientResult<()> {
        self.request(
            "session/compact",
            SessionCompactParams {
                session_id: session_id.to_string(),
                extra_instructions: String::new(),
            },
        )
        .await?;
        Ok(())
    }

    // -- Turn queue -------------------------------------------------------------

    pub async fn turn_queue_read(&self, session_id: &str) -> ClientResult<PublicTurnQueue> {
        Ok(self
            .request_typed::<TurnQueueReadResponse>(
                "session/turn/queue/read",
                TurnQueueReadParams {
                    session_id: session_id.to_string(),
                },
            )
            .await?
            .queue)
    }

    pub async fn turn_queue_remove(
        &self,
        session_id: &str,
        queue_item_id: &str,
    ) -> ClientResult<()> {
        self.request(
            "session/turn/queue/remove",
            TurnQueueRemoveParams {
                session_id: session_id.to_string(),
                queue_item_id: queue_item_id.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    /// Resume a paused turn queue.
    pub async fn turn_queue_resume(&self, session_id: &str) -> ClientResult<()> {
        self.request(
            "session/turn/queue/resume",
            TurnQueueResumeParams {
                session_id: session_id.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    /// Steer a queued item into the currently active turn.
    pub async fn turn_queue_steer(
        &self,
        session_id: &str,
        queue_item_id: &str,
        expected_turn_id: &str,
    ) -> ClientResult<TurnQueueSteerResponse> {
        self.request_typed(
            "session/turn/queue/steer",
            TurnQueueSteerParams {
                session_id: session_id.to_string(),
                queue_item_id: queue_item_id.to_string(),
                expected_turn_id: expected_turn_id.to_string(),
            },
        )
        .await
    }

    /// Replace a queued item's content (queue edit mode).
    pub async fn turn_queue_replace(
        &self,
        session_id: &str,
        queue_item_id: &str,
        text: &str,
    ) -> ClientResult<String> {
        let resp: QueueItemResponse = self
            .request_typed(
                "session/turn/queue/replace",
                TurnQueueReplaceParams {
                    session_id: session_id.to_string(),
                    queue_item_id: queue_item_id.to_string(),
                    entries: vec![TurnInputEntry::User {
                        entry_id: None,
                        content: vec![SessionContentBlock::Text {
                            text: text.to_string(),
                        }],
                    }],
                    idempotency_key: None,
                },
            )
            .await?;
        Ok(resp.queue_item_id)
    }

    // -- Rewind -------------------------------------------------------------------

    /// Preview what a rewind to `entry_id` would restore (file changes + paths).
    pub async fn session_rewind_read(
        &self,
        session_id: &str,
        entry_id: &str,
    ) -> ClientResult<SessionRewindReadResponse> {
        self.request_typed(
            "session/rewind/read",
            SessionRewindReadParams {
                session_id: session_id.to_string(),
                entry_id: entry_id.to_string(),
            },
        )
        .await
    }

    /// Rewind the session to `entry_id`. `inplace` rewinds this session;
    /// otherwise the server forks into a new session whose state is returned.
    pub async fn session_rewind(
        &self,
        session_id: &str,
        entry_id: &str,
        restore_files: bool,
        inplace: bool,
    ) -> ClientResult<SessionRewindResponse> {
        self.request_typed(
            "session/rewind",
            SessionRewindParams {
                session_id: session_id.to_string(),
                entry_id: entry_id.to_string(),
                restore_files,
                inplace,
            },
        )
        .await
    }

    // -- History ------------------------------------------------------------------

    /// Page through the session's history (older entries via `page.cursor`
    /// seeded from `state.history_before_cursor`).
    pub async fn session_history_list(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        page: PageRequest,
    ) -> ClientResult<SessionHistoryListResponse> {
        self.request_typed(
            "session/history/list",
            SessionHistoryListParams {
                session_id: session_id.to_string(),
                turn_id: turn_id.map(str::to_string),
                page,
            },
        )
        .await
    }

    // -- Workspace trust ------------------------------------------------------------

    pub async fn workspace_trust_status(
        &self,
        cwd: Option<&str>,
    ) -> ClientResult<WorkspaceTrustStatusResponse> {
        self.request_typed(
            "workspace/trust/status",
            WorkspaceTrustStatusParams {
                cwd: cwd.map(str::to_string),
            },
        )
        .await
    }

    /// `decision` is one of `trust_repo`, `trust_cwd`, `decline`.
    pub async fn workspace_trust_decision(
        &self,
        decision: &str,
        cwd: Option<&str>,
        session_id: Option<&str>,
    ) -> ClientResult<()> {
        self.request(
            "workspace/trust/decision",
            WorkspaceTrustDecisionParams {
                decision: decision.to_string(),
                cwd: cwd.map(str::to_string),
                session_id: session_id.map(str::to_string),
            },
        )
        .await?;
        Ok(())
    }

    /// Directories carrying config the workspace doesn't trust yet.
    pub async fn workspace_trust_untrusted_config(
        &self,
        cwd: Option<&str>,
    ) -> ClientResult<WorkspaceUntrustedConfigResponse> {
        self.request_typed(
            "workspace/trust/untrustedConfig",
            WorkspaceUntrustedConfigParams {
                cwd: cwd.map(str::to_string),
            },
        )
        .await
    }

    pub async fn session_shell_command(
        &self,
        session_id: &str,
        command: &str,
    ) -> ClientResult<Value> {
        self.request(
            "session/shellCommand",
            SessionShellCommandParams {
                session_id: session_id.to_string(),
                command: command.to_string(),
            },
        )
        .await
    }

    /// `config/read` → the client-rendered subset of `ConfigView`.
    pub async fn config_read(&self) -> ClientResult<ConfigView> {
        let resp: ConfigReadResponse = self.request_typed("config/read", json!({})).await?;
        Ok(resp.config)
    }

    /// `narration/summarize` → summary text (None when the provider can't).
    pub async fn narration_summarize(
        &self,
        session_id: &str,
        user_message: &str,
        assistant_text: &str,
    ) -> ClientResult<Option<String>> {
        let resp: NarrationSummarizeResponse = self
            .request_typed(
                "narration/summarize",
                NarrationSummarizeParams {
                    session_id: session_id.to_string(),
                    user_message: user_message.to_string(),
                    assistant_text: assistant_text.to_string(),
                    error: None,
                    message_id: None,
                },
            )
            .await?;
        Ok(resp.summary)
    }

    // ── M3a: settings & pickers ──────────────────────────────────────────

    /// `config/fields/read` → editable config fields + write targets.
    pub async fn config_fields_read(
        &self,
        session_id: &str,
    ) -> ClientResult<ConfigFieldsReadResponse> {
        self.request_typed("config/fields/read", json!({"sessionId": session_id}))
            .await
    }

    /// `config/write` → apply set/remove ops against the config layers.
    pub async fn config_write(
        &self,
        session_id: &str,
        ops: Vec<ConfigWriteOp>,
    ) -> ClientResult<ConfigWriteResponse> {
        self.request_typed("config/write", json!({"sessionId": session_id, "ops": ops}))
            .await
    }

    /// `config/model/write` → pick the active model (and thinking effort).
    pub async fn config_model_write(
        &self,
        session_id: &str,
        model_alias: &str,
        reasoning_effort: Option<&str>,
    ) -> ClientResult<ConfigWriteResponse> {
        self.request_typed(
            "config/model/write",
            ModelConfigWriteParams {
                session_id: session_id.to_string(),
                model_alias: Some(model_alias.to_string()),
                reasoning_effort: reasoning_effort.map(str::to_string),
            },
        )
        .await
    }

    /// `agents/list` → the active agent and every choice.
    pub async fn agents_list(&self, session_id: &str) -> ClientResult<AgentsListResponse> {
        self.request_typed("agents/list", json!({"sessionId": session_id}))
            .await
    }

    /// `session/agent/update` → switch the session's agent. The response is
    /// a `RuntimeMutationResponse`-shaped object: `rejected`/`failures`,
    /// `status` ("applied" | "pending"), and a `runtime` snapshot.
    pub async fn session_agent_update(
        &self,
        session_id: &str,
        agent_name: &str,
    ) -> ClientResult<ConfigWriteResponse> {
        self.request_typed(
            "session/agent/update",
            AgentSwitchParams {
                session_id: session_id.to_string(),
                agent_name: agent_name.to_string(),
            },
        )
        .await
    }

    // ── M3b: review diff ─────────────────────────────────────────────────

    /// `review/state` → changed files + per-owner scopes.
    pub async fn review_state(&self, session_id: &str) -> ClientResult<ReviewStateResponse> {
        self.request_typed("review/state", json!({"sessionId": session_id}))
            .await
    }

    /// `review/baseline` → whole-file baseline text (no owner) — the
    /// read for files no scope claims; current side comes from disk.
    pub async fn review_baseline(&self, session_id: &str, path: &str) -> ClientResult<String> {
        #[derive(Deserialize)]
        struct Resp {
            content: String,
        }
        self.request_typed::<Resp>(
            "review/baseline",
            json!({"sessionId": session_id, "path": path}),
        )
        .await
        .map(|r| r.content)
    }

    /// `review/turnDiff` → baseline/current contents for one file+owner.
    pub async fn review_turn_diff(
        &self,
        session_id: &str,
        path: &str,
        owner: &ReviewOwner,
    ) -> ClientResult<ReviewTurnDiffResponse> {
        self.request_typed(
            "review/turnDiff",
            json!({"sessionId": session_id, "path": path, "owner": owner}),
        )
        .await
    }

    /// `review/approve` — keep the target's changes (returns `{}`).
    pub async fn review_approve(
        &self,
        session_id: &str,
        target: &ReviewTarget,
    ) -> ClientResult<()> {
        self.request_typed::<serde_json::Value>(
            "review/approve",
            json!({"sessionId": session_id, "target": target}),
        )
        .await?;
        Ok(())
    }

    /// `review/revert` — revert the target's changes (returns `{}`).
    pub async fn review_revert(
        &self,
        session_id: &str,
        target: &ReviewTarget,
    ) -> ClientResult<()> {
        self.request_typed::<serde_json::Value>(
            "review/revert",
            json!({"sessionId": session_id, "target": target}),
        )
        .await?;
        Ok(())
    }

    // ── M3c: extensions ──────────────────────────────────────────────────

    /// `skills/installed` → the session's installed skill set.
    pub async fn skills_installed(
        &self,
        session_id: &str,
    ) -> ClientResult<SkillsInstalledResponse> {
        self.request_typed("skills/installed", json!({"sessionId": session_id}))
            .await
    }

    /// `skills/setEnabled` → RuntimeMutationResponse-shaped reply.
    pub async fn skills_set_enabled(
        &self,
        session_id: &str,
        name: &str,
        enabled: bool,
    ) -> ClientResult<ConfigWriteResponse> {
        self.request_typed(
            "skills/setEnabled",
            SkillsSetEnabledParams {
                session_id: session_id.to_string(),
                name: name.to_string(),
                enabled,
            },
        )
        .await
    }

    /// `mcp/read` → `MCPState` (sources, statuses, discovery errors).
    pub async fn mcp_read(&self, session_id: &str) -> ClientResult<MCPReadResponse> {
        self.request_typed("mcp/read", json!({"sessionId": session_id}))
            .await
    }

    /// `mcp/toggle` → `{runtime}` whose `mcp` carries the fresh state.
    pub async fn mcp_toggle(
        &self,
        session_id: &str,
        name: &str,
        source: &str,
        disabled: bool,
    ) -> ClientResult<MCPMutationResponse> {
        self.request_typed(
            "mcp/toggle",
            MCPToggleParams {
                session_id: Some(session_id.to_string()),
                name: name.to_string(),
                source: source.to_string(),
                disabled,
                tool_name: None,
            },
        )
        .await
    }

    /// `connector_catalog/toggle` — connector sources don't go through
    /// `mcp/toggle` (upstream rejects `source:"connector"`); they flip
    /// via their own catalog. Same `runtime.mcp` response shape.
    pub async fn connector_catalog_toggle(
        &self,
        session_id: &str,
        alias: &str,
        disabled: bool,
    ) -> ClientResult<MCPMutationResponse> {
        self.request_typed(
            "connector_catalog/toggle",
            json!({
                "sessionId": session_id,
                "alias": alias,
                "disabled": disabled,
            }),
        )
        .await
    }

    /// `connectors/read` → `{counts:{connected,total}}`.
    pub async fn connectors_read(
        &self,
        session_id: &str,
    ) -> ClientResult<ConnectorsReadResponse> {
        self.request_typed("connectors/read", json!({"sessionId": session_id}))
            .await
    }

    /// `plugins/read` → `{plugins:{plugins,dropped}}` catalog state.
    pub async fn plugins_read(&self, session_id: &str) -> ClientResult<PluginsReadResponse> {
        self.request_typed("plugins/read", json!({"sessionId": session_id}))
            .await
    }

    // ── M3d: workspace — worktrees + loops ──────────────────────────────

    /// `workspace/git/worktrees/list` — workspace-scoped; `cwd` names the
    /// checkout (the session's project root).
    pub async fn workspace_worktrees(
        &self,
        cwd: &str,
    ) -> ClientResult<WorkspaceWorktreeListResponse> {
        self.request_typed(
            "workspace/git/worktrees/list",
            WorkspaceWorktreeListParams {
                cwd: cwd.to_string(),
                include_details: true,
            },
        )
        .await
    }

    /// `loops/list` → the session's scheduled loops.
    pub async fn loops_list(&self, session_id: &str) -> ClientResult<LoopsListResponse> {
        self.request_typed("loops/list", json!({"sessionId": session_id}))
            .await
    }

    /// `loops/create` — `interval` is `<n><unit>` (30s/5m/2h/1d).
    pub async fn loops_create(
        &self,
        session_id: &str,
        interval: &str,
        prompt: &str,
    ) -> ClientResult<LoopsCreateResponse> {
        self.request_typed(
            "loops/create",
            LoopsCreateParams {
                session_id: session_id.to_string(),
                interval: interval.to_string(),
                prompt: prompt.to_string(),
            },
        )
        .await
    }

    /// `loops/delete` — removes and returns the deleted loop.
    pub async fn loops_delete(
        &self,
        session_id: &str,
        loop_id: &str,
    ) -> ClientResult<LoopsDeleteResponse> {
        self.request_typed(
            "loops/delete",
            LoopsDeleteParams {
                session_id: session_id.to_string(),
                loop_id: loop_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/open` — open the project picker (`purpose`
    /// is `"configure"` or `"teleport"`).
    pub async fn projects_open(
        &self,
        session_id: &str,
        purpose: &str,
    ) -> ClientResult<VibeCodeProjectsOpenResponse> {
        self.request_typed(
            "vibeCode/projects/open",
            VibeCodeProjectsOpenParams {
                session_id: session_id.to_string(),
                purpose: purpose.to_string(),
                prompt: None,
            },
        )
        .await
    }

    /// `vibeCode/projects/loadMore` — next picker page.
    pub async fn projects_load_more(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> ClientResult<VibeCodeProjectsLoadMoreResponse> {
        self.request_typed(
            "vibeCode/projects/loadMore",
            VibeCodeProjectsLoadMoreParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/select` — pick a project for the session.
    pub async fn projects_select(
        &self,
        session_id: &str,
        picker_id: &str,
        project_id: &str,
    ) -> ClientResult<VibeCodeProjectSelectResponse> {
        self.request_typed(
            "vibeCode/projects/select",
            VibeCodeProjectSelectParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
                project_id: project_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/unlink` — drop the saved project link.
    pub async fn projects_unlink(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> ClientResult<VibeCodeProjectUnlinkResponse> {
        self.request_typed(
            "vibeCode/projects/unlink",
            VibeCodeProjectUnlinkParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/cancel` — close the picker server-side.
    pub async fn projects_cancel(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> ClientResult<Value> {
        self.request(
            "vibeCode/projects/cancel",
            VibeCodeProjectCancelParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/create` — new remote project in the picker.
    pub async fn projects_create(
        &self,
        session_id: &str,
        picker_id: &str,
        name: &str,
        default_branch: &str,
    ) -> ClientResult<VibeCodeProjectCreateResponse> {
        self.request_typed(
            "vibeCode/projects/create",
            VibeCodeProjectCreateParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
                name: name.to_string(),
                default_branch: default_branch.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/teleport/start` — begin the teleport flow after the
    /// picker resolved a project. Progress arrives as
    /// `vibeCode/teleport/event` notifications.
    pub async fn teleport_start(
        &self,
        session_id: &str,
        picker_id: &str,
        operation_id: &str,
        project_id: &str,
    ) -> ClientResult<TeleportStartResponse> {
        self.request_typed(
            "vibeCode/teleport/start",
            TeleportStartParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
                operation_id: operation_id.to_string(),
                prompt: None,
                project_id: project_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/teleport/cancel` — abort an in-flight teleport.
    pub async fn teleport_cancel(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> ClientResult<TeleportCancelResponse> {
        self.request_typed(
            "vibeCode/teleport/cancel",
            TeleportCancelParams {
                session_id: session_id.to_string(),
                operation_id: operation_id.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/teleport/push/respond` — approve/deny the push when the
    /// teleport hits `push_required`.
    pub async fn teleport_push_respond(
        &self,
        session_id: &str,
        operation_id: &str,
        approved: bool,
    ) -> ClientResult<Value> {
        self.request(
            "vibeCode/teleport/push/respond",
            TeleportPushRespondParams {
                session_id: session_id.to_string(),
                operation_id: operation_id.to_string(),
                approved,
            },
        )
        .await
    }

    /// `session/relocate` — move the session to a new cwd; returns the
    /// full relocated state.
    pub async fn session_relocate(
        &self,
        session_id: &str,
        cwd: &str,
    ) -> ClientResult<SessionRelocateResponse> {
        self.request_typed(
            "session/relocate",
            SessionRelocateParams {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        )
        .await
    }

    /// `vibeCode/projects/recover` — recover a stale saved link on an
    /// open picker.
    pub async fn projects_recover(
        &self,
        session_id: &str,
        picker_id: &str,
    ) -> ClientResult<VibeCodeProjectRecoverResponse> {
        self.request_typed(
            "vibeCode/projects/recover",
            VibeCodeProjectRecoverParams {
                session_id: session_id.to_string(),
                picker_id: picker_id.to_string(),
            },
        )
        .await
    }

    // ── projectLinks/* — session-less local↔remote link management ──

    /// `projectLinks/list` — every cloud project with its local links.
    pub async fn project_links_list(&self) -> ClientResult<ProjectLinksListResponse> {
        self.request_typed("projectLinks/list", serde_json::json!({}))
            .await
    }

    /// `projectLinks/resolveRoot` — is this directory linkable?
    pub async fn project_links_resolve_root(
        &self,
        root_path: &str,
    ) -> ClientResult<ProjectLinksResolveRootResponse> {
        self.request_typed(
            "projectLinks/resolveRoot",
            ProjectLinksRootParams {
                root_path: root_path.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/inspectRoot` — resolve + the stored link (if any).
    pub async fn project_links_inspect_root(
        &self,
        root_path: &str,
    ) -> ClientResult<ProjectLinksInspectRootResponse> {
        self.request_typed(
            "projectLinks/inspectRoot",
            ProjectLinksRootParams {
                root_path: root_path.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/picker/load` — candidates to link `root_path` to.
    pub async fn project_links_picker_load(
        &self,
        root_path: &str,
    ) -> ClientResult<ProjectLinksPickerLoadResponse> {
        self.request_typed(
            "projectLinks/picker/load",
            ProjectLinksRootParams {
                root_path: root_path.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/picker/loadMore` — next page of candidates.
    pub async fn project_links_picker_load_more(
        &self,
        root_path: &str,
        cursor: &str,
    ) -> ClientResult<ProjectLinksPickerLoadMoreResponse> {
        self.request_typed(
            "projectLinks/picker/loadMore",
            ProjectLinksPickerLoadMoreParams {
                root_path: root_path.to_string(),
                cursor: cursor.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/create` — create a remote project and link it.
    pub async fn project_links_create(
        &self,
        root_path: &str,
        name: &str,
        default_branch: &str,
    ) -> ClientResult<ProjectLinkMutationResponse> {
        self.request_typed(
            "projectLinks/create",
            ProjectLinksCreateParams {
                root_path: root_path.to_string(),
                name: name.to_string(),
                default_branch: default_branch.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/link` — bind `root_path` to an existing project.
    pub async fn project_links_link(
        &self,
        root_path: &str,
        project_id: &str,
        project_name: &str,
    ) -> ClientResult<ProjectLinkMutationResponse> {
        self.request_typed(
            "projectLinks/link",
            ProjectLinksLinkParams {
                root_path: root_path.to_string(),
                project_id: project_id.to_string(),
                project_name: project_name.to_string(),
            },
        )
        .await
    }

    /// `projectLinks/save` — persist the link, re-checking the repo
    /// remote hasn't drifted (`expected_github_repo_url`).
    pub async fn project_links_save(
        &self,
        root_path: &str,
        project_id: &str,
        project_name: &str,
        expected_github_repo_url: Option<&str>,
    ) -> ClientResult<ProjectLinkMutationResponse> {
        self.request_typed(
            "projectLinks/save",
            ProjectLinksSaveParams {
                root_path: root_path.to_string(),
                project_id: project_id.to_string(),
                project_name: project_name.to_string(),
                expected_github_repo_url: expected_github_repo_url.map(str::to_string),
            },
        )
        .await
    }

    /// `projectLinks/unlink` — drop the stored link for `root_path`.
    pub async fn project_links_unlink(
        &self,
        root_path: &str,
    ) -> ClientResult<ProjectLinksUnlinkResponse> {
        self.request_typed(
            "projectLinks/unlink",
            ProjectLinksRootParams {
                root_path: root_path.to_string(),
            },
        )
        .await
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
