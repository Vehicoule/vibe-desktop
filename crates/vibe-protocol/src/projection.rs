//! Session-state projection: reduce app-server notifications into a single
//! `PublicSessionState`, mirroring `ClientProjection` in `events.py`.
//!
//! Watermark rule: every event carries `eventId`; the projection ignores
//! `event_id <= last`, accepts `last + 1`, and reports [`Reduce::Resync`] on a
//! gap so the caller issues `session/read`.

use serde_json::Value;

use crate::json_patch::apply_patch;
use crate::models::*;

/// Outcome of reducing one notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reduce {
    /// Notification applied.
    Applied,
    /// Older or duplicate event (watermark already past it).
    Stale,
    /// Gap in the event stream — issue `session/read` and adopt the state.
    Resync { expected: u64, got: u64 },
    /// Method not part of the session stream (warning/error/runtime/…).
    Ignored,
}

pub struct Projection {
    pub state: PublicSessionState,
    last_event_id: u64,
    /// Latest `runtime/updated` snapshot (status bar data).
    pub runtime: Option<RuntimeSnapshot>,
    /// Latest stats from `session/statsUpdated`.
    pub stats: Option<AgentStatsSnapshot>,
    pub context_window: u64,
    /// Latest server warning/error for display.
    pub server_notices: Vec<PublicError>,
    resync_pending: bool,
    /// Watermarked notifications that arrived while a `session/read` resync
    /// was in flight. Replayed through the gate after [`adopt`](Self::adopt)
    /// so events the snapshot predates are not lost.
    pending_events: Vec<(String, Value)>,
}

impl Projection {
    pub fn new(state: PublicSessionState) -> Self {
        let last_event_id = state.event_id;
        Self {
            state,
            last_event_id,
            runtime: None,
            stats: None,
            context_window: 0,
            server_notices: Vec::new(),
            resync_pending: false,
            pending_events: Vec::new(),
        }
    }

    /// Adopt a state wholesale (attach response, snapshot, handoff, resync).
    /// The state's own `event_id` re-baselines the watermark.
    ///
    /// Returns false when the snapshot is older than the live watermark — a
    /// `session/read` that raced behind the stream. The caller should issue
    /// another read instead of adopting, or newer events would be discarded.
    /// On success, buffered resync-window events replay through the gate.
    pub fn adopt(&mut self, state: PublicSessionState) -> bool {
        if state.event_id < self.last_event_id {
            return false;
        }
        self.replace_state(state);
        true
    }

    /// Adopt a state wholesale without the staleness check — used by
    /// handoffs (`session/compacted`/`session/contextCleared`), which carry a
    /// NEW session id and an event sequence that does not continue the old
    /// session's watermark. Buffered resync-window events still replay through
    /// the gate; anything emitted for the replaced session is dropped there.
    fn replace_state(&mut self, state: PublicSessionState) {
        self.last_event_id = state.event_id;
        self.state = state;
        self.resync_pending = false;
        let buffered = std::mem::take(&mut self.pending_events);
        for (method, params) in buffered {
            self.on_notification(&method, &params);
        }
    }

    pub fn history(&self) -> &[PublicHistoryEntry] {
        self.state.history.as_deref().unwrap_or(&[])
    }

    pub fn needs_resync(&self) -> bool {
        self.resync_pending
    }

    /// Reduce one notification. Returns what happened.
    pub fn on_notification(&mut self, method: &str, params: &Value) -> Reduce {
        // Non-watermarked channels apply immediately, even mid-resync.
        match method {
            "runtime/updated" => {
                if let Ok(p) = Self::params::<RuntimeUpdatedParams>(params) {
                    self.runtime = Some(p.runtime);
                }
                return Reduce::Applied;
            }
            "turn/retrying" => {
                if let Ok(p) = Self::params::<TurnRetryingParams>(params) {
                    self.state.retrying = Some(PublicRetryState {
                        turn_id: String::new(),
                        category: p.category,
                        detail: p.detail,
                    });
                }
                return Reduce::Applied;
            }
            "warning" => {
                if let Ok(p) = Self::params::<ServerWarningParams>(params) {
                    self.server_notices.push(p.warning);
                }
                return Reduce::Applied;
            }
            "error" => {
                if let Ok(p) = Self::params::<ServerErrorParams>(params) {
                    self.server_notices.push(p.error);
                }
                return Reduce::Applied;
            }
            _ => {}
        }

        // A resync read is in flight: buffer watermarked events so they can
        // be replayed against the adopted snapshot instead of being applied
        // to state that is about to be replaced (or skipped as a gap).
        if self.resync_pending {
            self.pending_events
                .push((method.to_string(), params.clone()));
            return Reduce::Stale;
        }

        match method {
            "session/snapshot" => match Self::params::<SessionSnapshotParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    if self.adopt(p.state) {
                        Reduce::Applied
                    } else {
                        Reduce::Stale
                    }
                }
                Err(_) => Reduce::Ignored,
            },
            // Handoffs re-baseline: the emitted event carries the NEW
            // session's own watermark, so gating it against the old session's
            // sequence would discard the replacement entirely.
            "session/compacted" | "session/contextCleared" => {
                match Self::params::<SessionHandoffParams>(params) {
                    Ok(p) => {
                        if p.old_session_id != self.state.session.id {
                            // Handoff for a session we do not hold — drop it.
                            return Reduce::Stale;
                        }
                        if p.state.session.id != p.base.session_id {
                            return Reduce::Ignored;
                        }
                        self.replace_state(p.state);
                        Reduce::Applied
                    }
                    Err(_) => Reduce::Ignored,
                }
            }
            "session/updated" => match Self::params::<SessionUpdatedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    match serde_json::to_value(&self.state.session).and_then(|raw| {
                        apply_patch(&raw, &p.patch).map_err(|e| {
                            serde_json::Error::io(std::io::Error::other(e.to_string()))
                        })
                    }) {
                        Ok(patched) => match serde_json::from_value::<PublicSession>(patched) {
                            Ok(session) => {
                                self.state.session = session;
                                Reduce::Applied
                            }
                            Err(_) => self.broken_patch(p.base.event_id),
                        },
                        Err(_) => self.broken_patch(p.base.event_id),
                    }
                }
                Err(_) => Reduce::Ignored,
            },
            "history/entryAdded" => match Self::params::<HistoryEntryAddedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    let history = self.state.history.get_or_insert_with(Vec::new);
                    if history.iter().all(|e| e.id() != p.entry.id()) {
                        history.push(p.entry);
                    }
                    Reduce::Applied
                }
                Err(_) => Reduce::Ignored,
            },
            "history/entryUpdated" => match Self::params::<HistoryEntryUpdatedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    let Some(history) = self.state.history.as_mut() else {
                        return Reduce::Applied;
                    };
                    let Some(entry) = history
                        .iter_mut()
                        .find(|e| e.id() == Some(p.entry_id.as_str()))
                    else {
                        return Reduce::Applied;
                    };
                    // A patch that cannot be applied leaves the entry stale
                    // with the watermark already past it — resync to recover.
                    let patched = serde_json::to_value(&*entry)
                        .ok()
                        .and_then(|raw| apply_patch(&raw, &p.patch).ok())
                        .and_then(|value| serde_json::from_value::<PublicHistoryEntry>(value).ok());
                    let Some(next) = patched else {
                        return self.broken_patch(p.base.event_id);
                    };
                    *entry = next.clone();
                    // Also patch active_callbacks mirrors (callbacks live in both).
                    for cb in self.state.active_callbacks.iter_mut() {
                        if cb.id() == Some(p.entry_id.as_str()) {
                            *cb = next.clone();
                        }
                    }
                    Reduce::Applied
                }
                Err(_) => Reduce::Ignored,
            },
            "turn/started" | "turn/completed" => match Self::params::<TurnStartedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    self.upsert_turn(p.turn);
                    Reduce::Applied
                }
                Err(_) => Reduce::Ignored,
            },
            "turn/queueUpdated" => match Self::params::<TurnQueueUpdatedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    self.state.turn_queue = p.queue;
                    Reduce::Applied
                }
                Err(_) => Reduce::Ignored,
            },
            "session/statsUpdated" => match Self::params::<StatsUpdatedParams>(params) {
                Ok(p) => {
                    if !self.gate(method, params, &p.base) {
                        return self.stale_or_resync(&p.base);
                    }
                    self.stats = Some(p.stats);
                    self.context_window = p.context_window;
                    Reduce::Applied
                }
                Err(_) => Reduce::Ignored,
            },
            "session/childSessionUpdated" => {
                match Self::params::<ChildSessionUpdatedParams>(params) {
                    Ok(p) => {
                        if !self.gate(method, params, &p.base) {
                            return self.stale_or_resync(&p.base);
                        }
                        let children = &mut self.state.child_sessions;
                        match children.iter_mut().find(|c| c.id == p.child_session.id) {
                            Some(slot) => *slot = p.child_session,
                            None => {
                                children.push(p.child_session);
                                children.sort_by_key(|c| (c.created_at, c.id.clone()));
                            }
                        }
                        Reduce::Applied
                    }
                    Err(_) => Reduce::Ignored,
                }
            }
            _ => Reduce::Ignored,
        }
    }

    /// The event was consumed (watermark advanced) but its payload could not
    /// be applied — mark resync pending so the next read restores authority.
    fn broken_patch(&mut self, event_id: u64) -> Reduce {
        self.resync_pending = true;
        Reduce::Resync {
            expected: self.last_event_id,
            got: event_id,
        }
    }

    fn upsert_turn(&mut self, turn: PublicTurn) {
        if turn.status != "in_progress" {
            self.state.retrying = None;
        }
        let turns = self.state.turns.get_or_insert_with(Vec::new);
        match turns.iter_mut().find(|t| t.id == turn.id) {
            Some(slot) => *slot = turn,
            None => turns.push(turn),
        }
    }

    fn params<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, serde_json::Error> {
        serde_json::from_value(params.clone())
    }

    /// Watermark gate: returns true when the event is exactly next.
    /// Events stamped for a different session id never apply (post-handoff
    /// stragglers). On a gap the event itself is buffered (resync now
    /// pending), so a lagging snapshot adopted later can still replay it.
    fn gate(&mut self, method: &str, params: &Value, base: &EventNotificationParams) -> bool {
        if base.session_id != self.state.session.id || base.event_id <= self.last_event_id {
            return false;
        }
        if base.event_id == self.last_event_id + 1 {
            self.last_event_id = base.event_id;
            return true;
        }
        self.resync_pending = true;
        self.pending_events
            .push((method.to_string(), params.clone()));
        false
    }

    /// The gate rejected the event: a gap means resync; anything older — or
    /// stamped for a different session — is stale.
    fn stale_or_resync(&self, base: &EventNotificationParams) -> Reduce {
        if base.session_id != self.state.session.id || base.event_id <= self.last_event_id {
            Reduce::Stale
        } else {
            Reduce::Resync {
                expected: self.last_event_id + 1,
                got: base.event_id,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(id: &str) -> PublicSession {
        PublicSession {
            id: id.into(),
            root_session_id: None,
            parent_session_id: None,
            title: None,
            preview: String::new(),
            status: PublicSessionStatus::Idle,
            created_at: 0,
            updated_at: 0,
            bumped_at: None,
            pinned_at: None,
            archived_at: None,
            is_unseen: false,
            cwd: None,
            workspace_roots: vec![],
            model: None,
            reasoning_effort: None,
            agent: None,
            token_usage: None,
            context_usage: None,
            harness: None,
        }
    }

    fn state(event_id: u64) -> PublicSessionState {
        PublicSessionState {
            format: "vibe.public-session-state/v1".into(),
            event_id,
            session: session("s1"),
            is_quiescent: None,
            history: Some(vec![]),
            history_before_cursor: None,
            turns: Some(vec![]),
            active_callbacks: vec![],
            child_sessions: vec![],
            turn_queue: PublicTurnQueue::default(),
            retrying: None,
        }
    }

    fn msg_entry(id: &str, session_id: &str, text: &str) -> PublicHistoryEntry {
        PublicHistoryEntry::Message {
            base: HistoryEntryBase {
                id: id.into(),
                session_id: session_id.into(),
                turn_id: None,
                created_at: 0,
                updated_at: 0,
                generation_status: "completed".into(),
                related_entry_id: None,
            },
            role: "assistant".into(),
            content: vec![ContentBlock::text(text)],
            source: None,
            user_display_content: None,
        }
    }

    #[test]
    fn appends_and_patches_entries() {
        let mut proj = Projection::new(state(0));
        let added = json!({
            "eventId": 1, "sessionId": "s1", "emittedAt": 0,
            "turnId": null,
            "entry": {
                "type": "message", "id": "e1", "sessionId": "s1",
                "createdAt": 0, "updatedAt": 0,
                "generationStatus": "completed",
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
            }
        });
        assert_eq!(
            proj.on_notification("history/entryAdded", &added),
            Reduce::Applied
        );
        assert_eq!(proj.history().len(), 1);

        let updated = json!({
            "eventId": 2, "sessionId": "s1", "emittedAt": 0,
            "turnId": null, "entryId": "e1",
            "patch": [{"op": "append", "path": "/content/0/text", "value": " world"}]
        });
        assert_eq!(
            proj.on_notification("history/entryUpdated", &updated),
            Reduce::Applied
        );
        match &proj.history()[0] {
            PublicHistoryEntry::Message { content, .. } => {
                assert_eq!(content[0].as_text(), Some("hello world"));
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn watermark_gap_requests_resync() {
        let mut proj = Projection::new(state(0));
        let evt = |id| {
            json!({
                "eventId": id, "sessionId": "s1", "emittedAt": 0,
                "turnId": null,
                "entry": {"type": "message", "id": format!("e{id}"), "sessionId": "s1",
                          "createdAt": 0, "updatedAt": 0, "generationStatus": "completed",
                          "role": "assistant", "content": [{"type": "text", "text": "x"}]}
            })
        };
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(1)),
            Reduce::Applied
        );
        // replayed event → stale
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(1)),
            Reduce::Stale
        );
        // gap 2 → resync, not applied
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(3)),
            Reduce::Resync {
                expected: 2,
                got: 3
            }
        );
        assert!(proj.needs_resync());
        // resync: adopt a fresh state whose watermark rebaselines
        proj.adopt(state(3));
        assert!(!proj.needs_resync());
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(4)),
            Reduce::Applied
        );
    }

    #[test]
    fn session_patch_updates_session() {
        let mut proj = Projection::new(state(0));
        let updated = json!({
            "eventId": 1, "sessionId": "s1", "emittedAt": 0,
            "patch": [{"op": "replace", "path": "/title", "value": "renamed"}]
        });
        assert_eq!(
            proj.on_notification("session/updated", &updated),
            Reduce::Applied
        );
        assert_eq!(proj.state.session.title.as_deref(), Some("renamed"));
    }

    #[test]
    fn error_envelope_is_not_swallowed_as_null_result() {
        // Regression: an optional `result` field matched error envelopes and
        // delivered `Ok(null)` instead of the actual error.
        let line =
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":"bad","message":"nope","data":null}}"#;
        match crate::models::parse_incoming(line).unwrap() {
            crate::models::Incoming::Response { id, result } => {
                assert_eq!(id, serde_json::json!(7));
                assert!(result.is_err());
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn adopt_rejects_snapshot_older_than_watermark() {
        let mut proj = Projection::new(state(0));
        let evt = |id| {
            json!({
                "eventId": id, "sessionId": "s1", "emittedAt": 0,
                "turnId": null,
                "entry": {"type": "message", "id": format!("e{id}"), "sessionId": "s1",
                          "createdAt": 0, "updatedAt": 0, "generationStatus": "completed",
                          "role": "assistant", "content": [{"type": "text", "text": "x"}]}
            })
        };
        proj.on_notification("history/entryAdded", &evt(1));
        proj.on_notification("history/entryAdded", &evt(2));
        // A `session/read` snapshot taken before event 2 must not rewind.
        assert!(!proj.adopt(state(1)));
        assert_eq!(proj.last_event_id, 2);
        assert!(proj.adopt(state(2)));
    }

    #[test]
    fn resync_window_events_replay_after_adopt() {
        let mut proj = Projection::new(state(0));
        let evt = |id, entry_id: &str, text: &str| {
            json!({
                "eventId": id, "sessionId": "s1", "emittedAt": 0,
                "turnId": null,
                "entry": {"type": "message", "id": entry_id, "sessionId": "s1",
                          "createdAt": 0, "updatedAt": 0, "generationStatus": "completed",
                          "role": "assistant", "content": [{"type": "text", "text": text}]}
            })
        };
        proj.on_notification("history/entryAdded", &evt(1, "e1", "a"));
        // Gap at 3 → resync pending, event 3 buffered.
        assert!(matches!(
            proj.on_notification("history/entryAdded", &evt(3, "e3", "c")),
            Reduce::Resync { .. }
        ));
        // While pending, further events buffer instead of applying.
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(4, "e4", "d")),
            Reduce::Stale
        );
        // Adopt a snapshot taken at 2 — it already contains e1; the
        // buffered e3 and e4 replay on top of it, in order.
        let mut snap = state(2);
        snap.history = Some(vec![msg_entry("e1", "s1", "a")]);
        assert!(proj.adopt(snap));
        assert_eq!(proj.history().len(), 3);
        assert!(!proj.needs_resync());
    }

    #[test]
    fn broken_patch_requests_resync_instead_of_stale_silence() {
        let mut proj = Projection::new(state(0));
        let added = json!({
            "eventId": 1, "sessionId": "s1", "emittedAt": 0,
            "turnId": null,
            "entry": {"type": "message", "id": "e1", "sessionId": "s1",
                      "createdAt": 0, "updatedAt": 0, "generationStatus": "completed",
                      "role": "assistant", "content": [{"type": "text", "text": "hi"}]}
        });
        proj.on_notification("history/entryAdded", &added);
        // A patch against a field type it cannot handle leaves the entry stale.
        let broken = json!({
            "eventId": 2, "sessionId": "s1", "emittedAt": 0,
            "turnId": null, "entryId": "e1",
            "patch": [{"op": "insert_before", "path": "/role", "value": "nope"}]
        });
        assert!(matches!(
            proj.on_notification("history/entryUpdated", &broken),
            Reduce::Resync { .. }
        ));
        assert!(proj.needs_resync());
    }

    #[test]
    fn handoff_replaces_session_despite_lower_watermark() {
        // Compaction emits eventId for the NEW session's own sequence
        // (server.rs _sequence_notification), so gating against the old
        // session's watermark would drop the replacement wholesale.
        let mut proj = Projection::new(state(0));
        let evt = |id, sid: &str, entry_id: &str| {
            json!({
                "eventId": id, "sessionId": sid, "emittedAt": 0,
                "turnId": null,
                "entry": {"type": "message", "id": entry_id, "sessionId": sid,
                          "createdAt": 0, "updatedAt": 0, "generationStatus": "completed",
                          "role": "assistant", "content": [{"type": "text", "text": "x"}]}
            })
        };
        proj.on_notification("history/entryAdded", &evt(1, "s1", "e1"));
        proj.on_notification("history/entryAdded", &evt(2, "s1", "e2"));

        // Handoff: new session id, event sequence restarts at 1.
        let mut new_state = state(1);
        new_state.session.id = "s2".into();
        let handoff = json!({
            "eventId": 1, "sessionId": "s2", "oldSessionId": "s1",
            "emittedAt": 0, "state": new_state
        });
        assert_eq!(
            proj.on_notification("session/compacted", &handoff),
            Reduce::Applied
        );
        assert_eq!(proj.state.session.id, "s2");
        assert_eq!(proj.last_event_id, 1);

        // A straggler for the old session is stale — never applied, never
        // treated as a gap.
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(3, "s1", "e3-old")),
            Reduce::Stale
        );
        // New-session events continue its own sequence.
        assert_eq!(
            proj.on_notification("history/entryAdded", &evt(2, "s2", "e4")),
            Reduce::Applied
        );
        assert_eq!(proj.history().len(), 1);

        // A handoff not replacing our session is dropped.
        let wrong = json!({
            "eventId": 3, "sessionId": "s9", "oldSessionId": "s1",
            "emittedAt": 0, "state": state(3)
        });
        assert_eq!(
            proj.on_notification("session/compacted", &wrong),
            Reduce::Stale
        );
        assert_eq!(proj.state.session.id, "s2");
    }

    #[test]
    fn unknown_entry_type_does_not_panic() {
        let mut proj = Projection::new(state(0));
        let added = json!({
            "eventId": 1, "sessionId": "s1", "emittedAt": 0,
            "turnId": null,
            "entry": {"type": "future-kind", "id": "e9", "sessionId": "s1",
                      "createdAt": 0, "updatedAt": 0, "generationStatus": "completed"}
        });
        assert_eq!(
            proj.on_notification("history/entryAdded", &added),
            Reduce::Applied
        );
        assert!(proj.history()[0].id().is_none());
        assert!(proj.history()[0].base().is_none());
    }
}
