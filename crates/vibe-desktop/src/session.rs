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

/// State of the rewind confirmation sheet. `has_changes == None` means the
/// `session/rewind/read` preview is still in flight.
pub struct RewindDialog {
    pub entry_id: String,
    pub preview: String,
    pub has_changes: Option<bool>,
    pub paths: Vec<String>,
    pub restore_files: bool,
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
    /// Session id last reported to `VibeApp::open_ids` — handoffs can
    /// replace the session id under the same view.
    reported_id: String,
    /// `user_input` callback selections, per callback id → per question.
    /// Accumulated until the user submits the whole request.
    pub question_selections: HashMap<String, Vec<Vec<String>>>,
    /// Free-text "other" answers, keyed by (callback id, question index).
    pub question_other: HashMap<(String, usize), String>,
    /// Focus handle per other-input, keyed "{callback id}:{question index}".
    pub other_focus: HashMap<String, FocusHandle>,
    /// Rewind sheet — `Some` while the dialog is open.
    pub rewind: Option<RewindDialog>,
    /// Workspace trust details when the cwd is untrusted (banner).
    pub trust: Option<WorkspaceTrustDetails>,
    /// Banner hidden by the user for this view.
    pub trust_dismissed: bool,
    /// A `session/history/list` backward page is in flight.
    pub loading_earlier: bool,
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
        let reported_id = projection.state.session.id.clone();
        let mut view = Self {
            conn,
            projection,
            reported_id,
            composer: String::new(),
            composer_focus,
            error: None,
            autofocused: false,
            app: None,
            question_selections: HashMap::new(),
            question_other: HashMap::new(),
            other_focus: HashMap::new(),
            rewind: None,
            trust: None,
            trust_dismissed: false,
            loading_earlier: false,
            events_task: None,
        };
        if let Some(rx) = events {
            view.events_task = Some(Self::pump(rx, cx));
        }
        view
    }

    /// Called once right after attach: an untrusted workspace surfaces the
    /// trust banner until the user decides or dismisses it.
    pub fn check_trust(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let cwd = self.projection.state.session.cwd.clone();
        cx.spawn(async move |this, cx| {
            let result = conn.workspace_trust_status(cwd.as_deref()).await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(status) = result {
                    if status.status == "untrusted" {
                        view.trust = status.details;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn trust_decision(&mut self, decision: &str, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let cwd = self.projection.state.session.cwd.clone();
        let sid = self.session_id().to_string();
        let decision = decision.to_string();
        cx.spawn(async move |this, cx| {
            let result = conn
                .workspace_trust_decision(&decision, cwd.as_deref(), Some(&sid))
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(()) => view.trust = None,
                    Err(e) => view.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
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
        self.sync_reported_id(cx);
        cx.notify();
        true
    }

    /// Keep `VibeApp::open_ids` pointing at this view when a handoff swaps
    /// the underlying session id.
    fn sync_reported_id(&mut self, cx: &mut Context<Self>) {
        let current = self.projection.state.session.id.clone();
        if current != self.reported_id {
            let old = std::mem::replace(&mut self.reported_id, current.clone());
            if let Some(app) = self.app.as_ref().and_then(|w| w.upgrade()) {
                app.update(cx, |app, _| app.sync_session_id(&old, &current));
            }
        }
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

    /// Re-read the full state after a watermark gap. The read can itself
    /// race behind the live stream — either rejected by `adopt`, or adopted
    /// while buffered events still detect a gap — so keep reading until the
    /// projection converges (bounded; a persistent failure surfaces in the
    /// status line).
    fn resync(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            for _ in 0..8 {
                let Ok(state) = conn.session_read(&session_id).await else {
                    return;
                };
                let done = this
                    .update(cx, |view, _cx| {
                        view.projection.adopt(state) && !view.projection.needs_resync()
                    })
                    .unwrap_or(true);
                if done {
                    let _ = this.update(cx, |view, cx| {
                        view.sync_reported_id(cx);
                        cx.notify();
                    });
                    return;
                }
            }
            let _ = this.update(cx, |view, cx| {
                view.error = Some("session could not catch up with the stream".into());
                cx.notify();
            });
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

    // -- turn queue -------------------------------------------------------------

    pub fn queue_remove(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        let item = item_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.turn_queue_remove(&session_id, &item).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Resume a paused queue — promotion continues server-side.
    pub fn queue_resume(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.turn_queue_resume(&session_id).await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = result {
                    view.error = Some(e.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    // -- rewind ------------------------------------------------------------------

    /// Open the rewind sheet for a user-message entry and fetch the
    /// file-change preview (`session/rewind/read`).
    pub fn open_rewind(&mut self, entry: &PublicHistoryEntry, cx: &mut Context<Self>) {
        let Some(entry_id) = entry.id().map(str::to_string) else {
            return;
        };
        let preview = match entry {
            PublicHistoryEntry::Message { content, .. } => content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        self.rewind = Some(RewindDialog {
            entry_id: entry_id.clone(),
            preview,
            has_changes: None,
            paths: Vec::new(),
            restore_files: false,
        });
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn.session_rewind_read(&session_id, &entry_id).await;
            let _ = this.update(cx, |view, cx| {
                if let Some(dlg) = view.rewind.as_mut() {
                    if dlg.entry_id == entry_id {
                        match result {
                            Ok(r) => {
                                dlg.has_changes = Some(r.has_file_changes);
                                dlg.paths = r.paths;
                            }
                            Err(e) => view.error = Some(e.to_string()),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn close_rewind(&mut self, cx: &mut Context<Self>) {
        self.rewind = None;
        cx.notify();
    }

    /// Rewind to the sheet's entry. `inplace` truncates this session;
    /// otherwise the server forks a child that opens in a new tab.
    pub fn apply_rewind(&mut self, inplace: bool, cx: &mut Context<Self>) {
        let Some(dlg) = self.rewind.take() else {
            return;
        };
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn
                .session_rewind(&session_id, &dlg.entry_id, dlg.restore_files, inplace)
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(resp) => {
                        if inplace {
                            // Same-session truncation: the returned state
                            // supersedes the projection wholesale.
                            if !view.projection.adopt(resp.state) {
                                view.resync(cx);
                            }
                        } else if let Some(app) = view.app.as_ref().and_then(|w| w.upgrade()) {
                            let id = resp.state.session.id.clone();
                            app.update(cx, |app, cx| app.open_session(&id, cx));
                        }
                        if !resp.restore_errors.is_empty() {
                            view.error = Some(resp.restore_errors.join("; "));
                        }
                    }
                    Err(e) => view.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // -- history paging ------------------------------------------------------------

    /// Fetch one older page (`history_before_cursor`) and prepend it.
    pub fn load_earlier(&mut self, cx: &mut Context<Self>) {
        let Some(cursor) = self.projection.state.history_before_cursor.clone() else {
            return;
        };
        if self.loading_earlier {
            return;
        }
        self.loading_earlier = true;
        let conn = self.conn.clone();
        let session_id = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let result = conn
                .session_history_list(
                    &session_id,
                    None,
                    PageRequest {
                        cursor: Some(cursor),
                        limit: 50,
                        direction: "backward".into(),
                    },
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                view.loading_earlier = false;
                match result {
                    Ok(page) => {
                        let history = view.projection.state.history.get_or_insert_with(Vec::new);
                        let mut merged = page.items;
                        merged.append(history);
                        *history = merged;
                        view.projection.state.history_before_cursor = page.previous_cursor;
                    }
                    Err(e) => view.error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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

    /// Free-text input for a question's "other" answer.
    pub fn other_text(&self, callback_id: &str, question_index: usize) -> &str {
        self.question_other
            .get(&(callback_id.to_string(), question_index))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Focus handle for a question's other-input (created lazily).
    pub fn other_focus_handle(
        &mut self,
        callback_id: &str,
        question_index: usize,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        self.other_focus
            .entry(format!("{callback_id}:{question_index}"))
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Edit the other-answer text: `None` deletes the last char.
    pub fn edit_other(
        &mut self,
        callback_id: &str,
        question_index: usize,
        input: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let text = self
            .question_other
            .entry((callback_id.to_string(), question_index))
            .or_default();
        match input {
            Some(s) => text.push_str(s),
            None => {
                text.pop();
            }
        }
        cx.notify();
    }

    /// Every question has at least one selected option or a free-text answer
    /// (when the request permits "other"). A question offering no way to
    /// answer — no options and `hideOther` — is treated as satisfied.
    pub fn question_complete(&self, callback_id: &str, request: &UserQuestionRequest) -> bool {
        request.questions.iter().enumerate().all(|(i, q)| {
            let chosen = self
                .question_selections
                .get(callback_id)
                .and_then(|s| s.get(i))
                .is_some_and(|c| !c.is_empty());
            let other = !q.hide_other && !self.other_text(callback_id, i).trim().is_empty();
            chosen || other || (q.options.is_empty() && q.hide_other)
        })
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
                let mut out: Vec<UserAnswer> = selections
                    .get(i)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|answer| UserAnswer {
                        question: q.question.clone(),
                        answer,
                        is_other: false,
                    })
                    .collect();
                let other = self
                    .question_other
                    .remove(&(callback_id.to_string(), i))
                    .unwrap_or_default();
                if !q.hide_other && !other.trim().is_empty() {
                    out.push(UserAnswer {
                        question: q.question.clone(),
                        answer: other.trim().to_string(),
                        is_other: true,
                    });
                }
                if !q.multi_select && out.len() > 1 {
                    // Single-select: a typed other answer supersedes the
                    // picked option — one answer per question.
                    out.retain(|a| a.is_other);
                }
                out
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
}
