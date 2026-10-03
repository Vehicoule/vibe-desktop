//! One attached session: its `vibe-app-server` process, protocol projection,
//! composer state, and callback handling.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use gpui::{AsyncApp, Context, FocusHandle, Task, WeakEntity};
use vibe_protocol::client::{ClientResult, Connection};
use vibe_protocol::models::*;
use vibe_protocol::projection::{Projection, Reduce};

use crate::app::VibeApp;

/// User action destined for a callback.
pub enum CallbackAnswer {
    Approval(ApprovalDecision),
    UserInput(UserQuestionResult),
}

pub struct SessionView {
    pub conn: Arc<Connection>,
    pub projection: Projection,
    pub composer: String,
    pub composer_focus: FocusHandle,
    pub error: Option<String>,
    /// Focus the composer once when the session is first shown.
    pub autofocused: bool,
    /// Root view, for opening forked sessions in new tabs.
    pub app: Option<WeakEntity<VibeApp>>,
    /// `user_input` callback selections, per callback id → per question.
    /// Accumulated until the user submits the whole request.
    pub question_selections: HashMap<String, Vec<Vec<String>>>,
    events_task: Option<Task<()>>,
}

impl SessionView {
    /// Attach to `conn` with an initial state; start pumping its event stream.
    pub fn attached(
        conn: Connection,
        state: PublicSessionState,
        composer_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut conn = conn;
        let events = conn.take_events();
        let conn = Arc::new(conn);
        let projection = Projection::new(state);
        let mut view = Self {
            conn,
            projection,
            composer: String::new(),
            composer_focus,
            error: None,
            autofocused: false,
            app: None,
            question_selections: HashMap::new(),
            events_task: None,
        };
        if let Some(rx) = events {
            view.events_task = Some(Self::pump(rx, cx));
        }
        view
    }

    fn pump(
        mut rx: futures::channel::mpsc::Receiver<ServerMessage>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some(msg) = rx.next().await {
                let keep = this.update(cx, |view, cx| view.on_server_message(&msg, cx));
                match keep {
                    Ok(true) | Err(_) => {}
                    Ok(false) => break,
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        })
    }

    /// Returns false when the session should stop consuming events.
    fn on_server_message(&mut self, msg: &ServerMessage, cx: &mut Context<Self>) -> bool {
        match msg {
            ServerMessage::Notification { method, params } => {
                let resync = matches!(
                    self.projection.on_notification(method, params),
                    Reduce::Resync { .. }
                );
                if resync {
                    self.resync(cx);
                }
            }
            ServerMessage::Request { id, method, params } => {
                self.on_server_request(id.clone(), method, params, cx);
            }
        }
        cx.notify();
        true
    }

    fn on_server_request(
        &mut self,
        id: serde_json::Value,
        method: &str,
        params: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        match method {
            "callback/call" => {
                if let Ok(p) = serde_json::from_value::<CallbackCallParams>(params.clone()) {
                    // Delivery ack, then record the callback entry.
                    let conn = self.conn.clone();
                    let ack = CallbackCallResponse {
                        callback_id: p
                            .callback
                            .callback_id()
                            .or_else(|| p.callback.id())
                            .unwrap_or_default()
                            .to_string(),
                        accepted: true,
                    };
                    cx.spawn(async move |_, _| {
                        let _ = conn.respond(&id, ack).await;
                    })
                    .detach();
                    self.ensure_callback_entry(p.callback);
                } else {
                    let conn = self.conn.clone();
                    cx.spawn(async move |_, _| {
                        let _ = conn
                            .respond_error(&id, "invalid_params", "bad callback")
                            .await;
                    })
                    .detach();
                }
            }
            // clientTool/* — not yet implemented: answer honestly.
            _ => {
                let conn = self.conn.clone();
                let method = method.to_string();
                cx.spawn(async move |_, _| {
                    let _ = conn.respond_error(&id, "not_implemented", &method).await;
                })
                .detach();
            }
        }
    }

    fn ensure_callback_entry(&mut self, entry: PublicHistoryEntry) {
        let history = self.projection.state.history.get_or_insert_with(Vec::new);
        if history.iter().all(|e| e.id() != entry.id()) {
            history.push(entry.clone());
        }
        if self
            .projection
            .state
            .active_callbacks
            .iter()
            .all(|e| e.id() != entry.id())
        {
            self.projection.state.active_callbacks.push(entry);
        }
    }

    /// Re-read the full state after a watermark gap.
    fn resync(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            if let Ok(state) = conn.session_read(&session_id).await {
                let _ = this.update(cx, |view, cx| {
                    if view.projection.adopt(state) {
                        cx.notify();
                    } else {
                        // The read raced behind the live stream — its
                        // snapshot is already stale; read again.
                        view.resync(cx);
                    }
                });
            }
        })
        .detach();
    }

    pub fn session_id(&self) -> &str {
        &self.projection.state.session.id
    }

    pub fn session(&self) -> &PublicSession {
        &self.projection.state.session
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        match &self.projection.state.session.status {
            PublicSessionStatus::Running { active_turn_id }
            | PublicSessionStatus::Blocked { active_turn_id, .. } => Some(active_turn_id),
            _ => None,
        }
    }

    /// Open callbacks for the approval/question cards.
    pub fn open_callbacks(&self) -> Vec<&PublicHistoryEntry> {
        self.projection
            .state
            .active_callbacks
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    PublicHistoryEntry::Callback {
                        state: CallbackState::Open,
                        ..
                    }
                )
            })
            .collect()
    }

    // -- actions ------------------------------------------------------------

    pub fn send_message(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer.clear();
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        let steer_to = self.active_turn_id().map(str::to_string);
        cx.spawn(async move |this, cx| {
            let result: ClientResult<()> = async {
                if let Some(turn_id) = steer_to {
                    conn.turn_steer(&session_id, &turn_id, &text).await
                } else {
                    conn.turn_start(&session_id, &text).await.map(|_| ())
                }
            }
            .await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(turn_id) = self.active_turn_id().map(str::to_string) else {
            return;
        };
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.turn_interrupt(&session_id, &turn_id).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn enqueue(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer.clear();
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.turn_enqueue(&session_id, &text).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn answer_callback(
        &mut self,
        callback_id: &str,
        answer: CallbackAnswer,
        cx: &mut Context<Self>,
    ) {
        let output = match answer {
            CallbackAnswer::Approval(decision) => CallbackOutput::Approval {
                decision,
                feedback: None,
            },
            CallbackAnswer::UserInput(result) => CallbackOutput::UserInput { result },
        };
        // Optimistically mark answered so the card flips immediately.
        for e in self.projection.state.active_callbacks.iter_mut() {
            if e.callback_id() == Some(callback_id) {
                if let PublicHistoryEntry::Callback { state, .. } = e {
                    *state = CallbackState::Answered {
                        output: output.clone(),
                    };
                }
            }
        }
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        let cb = callback_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.callback_respond(&session_id, &cb, output).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn compact(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.session_compact(&session_id).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fork this session server-side, then open the child in a new tab
    /// through the root app (a fork needs its own server process).
    pub fn request_fork(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.session_fork(&session_id, None, 200).await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(state) => {
                        if let Some(app) = view.app.as_ref().and_then(|w| w.upgrade()) {
                            let id = state.session.id.clone();
                            app.update(cx, |app, cx| app.open_session(&id, cx));
                        }
                    }
                    Err(e) => view.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Toggle a question option for a `user_input` callback.
    pub fn select_question_option(
        &mut self,
        callback_id: &str,
        question_index: usize,
        option: &str,
        multi_select: bool,
        cx: &mut Context<Self>,
    ) {
        let selections = self
            .question_selections
            .entry(callback_id.to_string())
            .or_default();
        if selections.len() <= question_index {
            selections.resize(question_index + 1, Vec::new());
        }
        let chosen = &mut selections[question_index];
        if multi_select {
            if let Some(pos) = chosen.iter().position(|o| o == option) {
                chosen.remove(pos);
            } else {
                chosen.push(option.to_string());
            }
        } else {
            *chosen = vec![option.to_string()];
        }
        cx.notify();
    }

    /// Current selections for a `user_input` callback.
    pub fn selected_options(&self, callback_id: &str, question_index: usize) -> Vec<String> {
        self.question_selections
            .get(callback_id)
            .and_then(|s| s.get(question_index))
            .cloned()
            .unwrap_or_default()
    }

    /// Submit the accumulated answers for a `user_input` callback.
    pub fn submit_question(
        &mut self,
        callback_id: &str,
        request: &UserQuestionRequest,
        cx: &mut Context<Self>,
    ) {
        let selections = self
            .question_selections
            .remove(callback_id)
            .unwrap_or_default();
        let answers = request
            .questions
            .iter()
            .enumerate()
            .flat_map(|(i, q)| {
                selections
                    .get(i)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|answer| UserAnswer {
                        question: q.question.clone(),
                        answer,
                        is_other: false,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        self.answer_callback(
            callback_id,
            CallbackAnswer::UserInput(UserQuestionResult {
                answers,
                cancelled: false,
            }),
            cx,
        );
    }

    /// Every question has at least one selected option.
    pub fn question_complete(&self, callback_id: &str, request: &UserQuestionRequest) -> bool {
        request.questions.iter().enumerate().all(|(i, _)| {
            self.question_selections
                .get(callback_id)
                .and_then(|s| s.get(i))
                .is_some_and(|c| !c.is_empty())
        })
    }
}
