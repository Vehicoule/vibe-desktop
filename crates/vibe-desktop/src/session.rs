//! One attached session: its `vibe-app-server` process, protocol projection,
//! composer state, and callback handling.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use gpui::{AsyncApp, Context, FocusHandle, Task, WeakEntity};
use vibe_protocol::client::{ClientResult, Connection};
use vibe_protocol::models::*;
use vibe_protocol::projection::{Projection, Reduce};

use crate::app::VibeApp;
use crate::voice::{self, DictationEvent, NarrationEvent};

/// User action destined for a callback.
pub enum CallbackAnswer {
    Approval(ApprovalDecision),
    UserInput(UserQuestionResult),
}

/// State of the rewind confirmation sheet. `has_changes == None` means the
/// `session/rewind/read` preview is still in flight — rewind stays disabled
/// until it resolves (a failed read surfaces in `read_error`).
pub struct RewindDialog {
    pub entry_id: String,
    pub preview: String,
    pub has_changes: Option<bool>,
    pub paths: Vec<String>,
    pub restore_files: bool,
    pub read_error: Option<String>,
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
    /// `config/read` client subset — `None` until the read lands (or
    /// fails: voice + settings UI stay hidden then).
    pub config: Option<ConfigView>,
    /// `agents/list` snapshot for the settings sheet.
    pub agents: Vec<AgentSummary>,
    /// Name of the active agent (from `agents/list` or a session update).
    pub active_agent: String,
    /// `config/fields/read` snapshot for the settings sheet.
    pub config_fields: Vec<ConfigFieldView>,
    /// Settings sheet open/closed.
    pub settings_open: bool,
    /// Agent a `session/agent/update` mutation reported as `pending` —
    /// not yet the active agent; reconciled from `agents/list` on the
    /// next `session/updated`.
    pub pending_agent: Option<String>,
    /// Config paths with an in-flight `config/write` — repeat clicks
    /// during the write would send the same value again.
    cfg_inflight: HashSet<String>,
    /// Installed skills (`skills/installed`).
    pub ext_skills: Vec<SkillSummary>,
    /// Skill names with a pending `skills/setEnabled`, mapped to the
    /// requested enabled state — cleared once a refreshed list confirms it.
    pending_skills: HashMap<String, bool>,
    /// MCP catalog state (`mcp/read`).
    pub mcp_state: Option<MCPState>,
    /// Connector counts (`connectors/read`).
    pub connector_counts: Option<ConnectorCounts>,
    /// Plugin catalog (`plugins/read`) + dropped descriptors.
    pub plugins: Vec<PluginCatalogEntry>,
    pub plugin_dropped: Vec<PluginCatalogDropped>,
    /// Extension entities with an in-flight mutation
    /// (`"skill:{name}"` | `"mcp:{name}"` | `"loop:{id}"` |
    /// `"loop:create"`) — second clicks are ignored while unsettled.
    ext_inflight: HashSet<String>,
    /// Generation for settings reads — a stale `load_settings` batch
    /// can't undo a newer mutation reply (e.g. `runtime.mcp` after
    /// `mcp/toggle`).
    settings_gen: u64,
    /// Linked worktrees (`workspace/git/worktrees/list`) + main branch.
    pub worktrees: Vec<WorkspaceLinkedWorktree>,
    pub repo_branch: Option<String>,
    /// Scheduled loops (`loops/list`) + the create-input state.
    pub loops: Vec<ScheduledLoop>,
    pub loop_input: String,
    pub loop_focus: FocusHandle,
    /// `review/state` snapshot for the review sheet.
    pub review: Option<ReviewStateResponse>,
    /// Review sheet open/closed.
    pub review_open: bool,
    /// Open per-file diff — (path, owner, response). `None` until
    /// `review/turnDiff` lands.
    pub review_diff: Option<(String, ReviewOwner, ReviewTurnDiffResponse)>,
    /// Generation counter — a stale `review/state` or `review/turnDiff`
    /// response must not overwrite a newer read or selection.
    review_gen: u64,
    /// Files with an in-flight approve/revert — both controls stay
    /// disabled until the mutation settles so opposing decisions
    /// can't race on one file.
    review_inflight: HashSet<String>,
    /// Dictation run in flight while the mic toggle is down.
    pub dictation: Option<DictationRun>,
    dictation_pump: Option<Task<()>>,
    /// Periodic-redraw task so the mic level tracks speech between deltas.
    dictation_meter: Option<Task<()>>,
    /// Narration phase — `Idle` shows nothing.
    pub narrating: Narration,
    /// Stop flag shared with the in-flight narration task.
    narration_stop: Arc<AtomicBool>,
    /// Generation bumped on every narrate/cancel — stale pumps' events
    /// for superseded runs are ignored.
    narration_gen: u64,
    /// Per-view narrator switch, initialized from `narrator_enabled`.
    pub narrator_on: bool,
    narration_pump: Option<Task<()>>,
    events_task: Option<Task<()>>,
}

/// A live dictation run: stop flag for the capture thread, peak meter for
/// the mic badge. `stopping` = capture told to end but final transcript
/// deltas may still arrive — the run stays until its `Done` event.
pub struct DictationRun {
    pub stop: Arc<AtomicBool>,
    pub peak: Arc<AtomicU32>,
    pub stopping: bool,
}

/// Narration lifecycle — mirrors upstream `NarratorState`. The stop flag
/// lives on `narration_stop` (shared with the host-runtime playback task).
pub enum Narration {
    Idle,
    Preparing,
    Speaking,
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
            config: None,
            agents: Vec::new(),
            active_agent: String::new(),
            config_fields: Vec::new(),
            settings_open: false,
            pending_agent: None,
            cfg_inflight: HashSet::new(),
            ext_skills: Vec::new(),
            pending_skills: HashMap::new(),
            mcp_state: None,
            connector_counts: None,
            plugins: Vec::new(),
            plugin_dropped: Vec::new(),
            ext_inflight: HashSet::new(),
            settings_gen: 0,
            worktrees: Vec::new(),
            repo_branch: None,
            loops: Vec::new(),
            loop_input: String::new(),
            loop_focus: cx.focus_handle(),
            review: None,
            review_open: false,
            review_diff: None,
            review_gen: 0,
            review_inflight: HashSet::new(),
            dictation: None,
            dictation_pump: None,
            dictation_meter: None,
            narrating: Narration::Idle,
            narration_stop: Arc::new(AtomicBool::new(false)),
            narration_gen: 0,
            narrator_on: false,
            narration_pump: None,
            events_task: None,
        };
        if let Some(rx) = events {
            view.events_task = Some(Self::pump(rx, cx));
        }
        view.load_voice_config(cx);
        view
    }

    /// Pull `config/read` once — voice UI only appears when the server
    /// reports `voice_mode_enabled`.
    fn load_voice_config(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let cfg = conn.config_read().await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(cfg) = cfg {
                    view.narrator_on = cfg.narrator_enabled && cfg.voice_mode_enabled;
                    view.config = Some(cfg);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetch `agents/list` + `config/fields/read` + extension reads for
    /// the settings sheet. `settings_gen` drops a batch that loses the
    /// race against a newer mutation reply.
    fn load_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_gen += 1;
        let gen = self.settings_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let agents = conn.agents_list(&sid).await;
            let fields = conn.config_fields_read(&sid).await;
            let skills = conn.skills_installed(&sid).await;
            let mcp = conn.mcp_read(&sid).await;
            let connectors = conn.connectors_read(&sid).await;
            let plugins = conn.plugins_read(&sid).await;
            let loops = conn.loops_list(&sid).await;
            let _ = this.update(cx, |view, cx| {
                // A handoff may have swapped the session while this was
                // in flight — the response belongs to the old session; a
                // newer mutation reply already beat this batch.
                if view.session_id() != sid || view.settings_gen != gen {
                    return;
                }
                if let Ok(list) = agents {
                    view.active_agent = list.active.name.clone();
                    view.agents = list.agents;
                }
                if let Ok(resp) = fields {
                    view.config_fields = resp.fields;
                }
                if let Ok(inst) = skills {
                    view.ext_skills = inst.skills;
                    // A pending toggle resolves once the list confirms the
                    // requested state — or the skill disappears; resolved
                    // entries also release their inflight key so the row
                    // becomes clickable again.
                    view.pending_skills.retain(|name, target| {
                        let unconfirmed = view
                            .ext_skills
                            .iter()
                            .find(|s| &s.name == name)
                            .map(|s| s.enabled != *target)
                            .unwrap_or(false);
                        if !unconfirmed {
                            view.ext_inflight.remove(&format!("skill:{name}"));
                        }
                        unconfirmed
                    });
                }
                if let Ok(resp) = mcp {
                    view.mcp_state = Some(resp.mcp);
                }
                if let Ok(resp) = connectors {
                    view.connector_counts = Some(resp.counts);
                }
                if let Ok(resp) = plugins {
                    view.plugins = resp.plugins.plugins;
                    view.plugin_dropped = resp.plugins.dropped;
                }
                if let Ok(resp) = loops {
                    view.loops = resp.loops;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Settings sheet toggle — refreshes every section on every open so
    /// a read that failed last time retries (gen-guarded).
    pub fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            self.load_settings(cx);
            self.load_worktrees(cx);
        }
        cx.notify();
    }

    /// `skills/setEnabled` — flip a skill's enabled flag. Locked skills
    /// can't be toggled; a pending switch clears when a refreshed list
    /// confirms the requested state.
    pub fn toggle_skill(&mut self, name: String, cx: &mut Context<Self>) {
        let key = format!("skill:{name}");
        if !self.ext_inflight.insert(key.clone()) {
            return;
        }
        let Some(skill) = self.ext_skills.iter().find(|s| s.name == name) else {
            self.ext_inflight.remove(&key);
            return;
        };
        if skill.locked {
            self.ext_inflight.remove(&key);
            return;
        }
        let enabled = !skill.enabled;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.skills_set_enabled(&sid, &name, enabled).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(r) if r.rejected => {
                        view.ext_inflight.remove(&key);
                        view.error = Some(format!(
                            "skill toggle rejected: {}",
                            r.failures.join(", ")
                        ));
                    }
                    Ok(r) if r.status.as_deref() == Some("pending") => {
                        // Keep the inflight key — the row stays
                        // non-clickable until a refreshed list confirms
                        // the requested state (load_settings frees it).
                        view.pending_skills.insert(name.clone(), enabled);
                        view.load_settings(cx);
                    }
                    Ok(_) => {
                        view.ext_inflight.remove(&key);
                        if let Some(s) = view.ext_skills.iter_mut().find(|s| s.name == name) {
                            s.enabled = enabled;
                        }
                    }
                    Err(e) => {
                        view.ext_inflight.remove(&key);
                        view.error = Some(format!("skill toggle failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `mcp/toggle` — flip a source between enabled/disabled. Other
    /// statuses (needs_auth, connected, …) aren't toggleable here.
    pub fn toggle_mcp(&mut self, name: String, cx: &mut Context<Self>) {
        let key = format!("mcp:{name}");
        if !self.ext_inflight.insert(key.clone()) {
            return;
        }
        let Some((kind, disabled)) = self
            .mcp_state
            .as_ref()
            .and_then(|m| m.sources.iter().find(|s| s.name == name))
            .and_then(|s| match s.status.as_str() {
                "enabled" => Some((s.kind.clone(), true)),
                "disabled" => Some((s.kind.clone(), false)),
                _ => None,
            })
        else {
            self.ext_inflight.remove(&key);
            return;
        };
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.mcp_toggle(&sid, &name, &kind, disabled).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                view.ext_inflight.remove(&key);
                match resp {
                    Ok(r) => {
                        // `runtime.mcp` carries the post-toggle state; a
                        // sessionless/absent runtime falls back to a read.
                        let fresh = r
                            .runtime
                            .and_then(|rt| serde_json::from_value::<MCPState>(rt["mcp"].clone()).ok());
                        if let Some(mcp) = fresh {
                            // Invalidate any settings batch still in
                            // flight — its older mcp read must not undo
                            // this toggle's fresh state.
                            view.settings_gen += 1;
                            view.mcp_state = Some(mcp);
                        }
                        // A discarded batch may have been carrying other
                        // sections (e.g. a pending-skill confirmation) —
                        // start a replacement read so nothing stays
                        // dropped; its mcp read is post-toggle anyway.
                        view.load_settings(cx);
                    }
                    Err(e) => view.error = Some(format!("mcp toggle failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `skill:{name}` / `mcp:{name}` has an unsettled mutation.
    pub fn ext_busy(&self, key: &str) -> bool {
        self.ext_inflight.contains(key)
    }

    /// A pending `skills/setEnabled` hasn't been confirmed yet.
    pub fn skill_pending(&self, name: &str) -> bool {
        self.pending_skills.contains_key(name)
    }

    /// `workspace/git/worktrees/list` — workspace-scoped, keyed by the
    /// session's project root rather than a session id.
    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.projection.state.session.cwd.clone() else {
            return;
        };
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.workspace_worktrees(&cwd).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                if let Ok(resp) = resp {
                    view.worktrees = resp.worktrees;
                    view.repo_branch = resp.repository_branch;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `loops/list` refresh after a create/delete mutation.
    fn refresh_loops(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.loops_list(&sid).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                if let Ok(resp) = resp {
                    view.loops = resp.loops;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Loop-create input editing (same pattern as the composer).
    pub fn edit_loop_input(&mut self, ch: Option<&str>, cx: &mut Context<Self>) {
        match ch {
            Some(c) => self.loop_input.push_str(c),
            None => {
                self.loop_input.pop();
            }
        }
        cx.notify();
    }

    /// `loops/create` — input is `{interval} {prompt}` (e.g. `5m check the
    /// build`); the first token is the interval string upstream parses.
    pub fn create_loop(&mut self, cx: &mut Context<Self>) {
        let text = self.loop_input.trim().to_string();
        let Some((interval, prompt)) = text.split_once(char::is_whitespace) else {
            return;
        };
        let prompt = prompt.trim();
        if interval.is_empty() || prompt.is_empty() || !self.ext_inflight.insert("loop:create".into())
        {
            return;
        }
        let interval = interval.to_string();
        let prompt = prompt.to_string();
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.loops_create(&sid, &interval, &prompt).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                view.ext_inflight.remove("loop:create");
                match resp {
                    Ok(_) => {
                        view.loop_input.clear();
                        view.refresh_loops(cx);
                    }
                    Err(e) => view.error = Some(format!("loop create failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `loops/delete` — one delete per loop at a time.
    pub fn delete_loop(&mut self, id: String, cx: &mut Context<Self>) {
        let key = format!("loop:{id}");
        if !self.ext_inflight.insert(key.clone()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.loops_delete(&sid, &id).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                view.ext_inflight.remove(&key);
                match resp {
                    Ok(_) => view.refresh_loops(cx),
                    Err(e) => view.error = Some(format!("loop delete failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Fetch `review/state` for the review sheet. Refreshes every call —
    /// the state changes as turns complete, so a cached snapshot goes
    /// stale; `review_gen` drops a response that loses the race.
    fn load_review(&mut self, cx: &mut Context<Self>) {
        self.review_gen += 1;
        let gen = self.review_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let state = conn.review_state(&sid).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.review_gen != gen {
                    return;
                }
                match state {
                    Ok(s) => view.review = Some(s),
                    Err(e) => view.error = Some(format!("review read failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Review sheet toggle — refreshes the state on every open.
    pub fn toggle_review(&mut self, cx: &mut Context<Self>) {
        self.review_open = !self.review_open;
        if self.review_open {
            self.load_review(cx);
        } else {
            // Pending reads must not land after the sheet closed.
            self.review_gen += 1;
        }
        cx.notify();
    }

    /// Owner a file belongs to — the scope listing it, else the file's
    /// own region owner, else the first scope (`turnDiff` needs an owner
    /// even for a plain file view; `None` when the state offers none).
    fn review_owner_for(&self, path: &str) -> Option<ReviewOwner> {
        let state = self.review.as_ref()?;
        if let Some(scope) = state
            .scopes
            .iter()
            .find(|s| s.files.iter().any(|f| f.path == path))
        {
            return Some(scope.owner.clone());
        }
        if let Some(owner) = state
            .files
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.regions.iter().find_map(|r| r.owner()))
        {
            return Some(owner);
        }
        state.scopes.first().map(|s| s.owner.clone())
    }

    /// `review/turnDiff` — open the baseline→current diff for a file.
    /// Scoped rows pass their scope's owner explicitly so two owners on
    /// one file show their own diffs; unscoped rows derive one.
    pub fn open_review_diff(
        &mut self,
        path: String,
        owner: Option<ReviewOwner>,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = owner.or_else(|| self.review_owner_for(&path)) else {
            return;
        };
        self.review_gen += 1;
        let gen = self.review_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.review_turn_diff(&sid, &path, &owner).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.review_gen != gen {
                    return;
                }
                match resp {
                    Ok(d) => view.review_diff = Some((path.clone(), owner, d)),
                    Err(e) => view.error = Some(format!("diff read failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// True when `path` has a resolvable owner — rows without one render
    /// non-clickable instead of silently doing nothing.
    pub fn review_file_openable(&self, path: &str) -> bool {
        self.review_owner_for(path).is_some()
    }

    pub fn close_review_diff(&mut self, cx: &mut Context<Self>) {
        self.review_diff = None;
        self.review_gen += 1;
        cx.notify();
    }

    /// `review/approve` (`keep`) or `review/revert` for a whole file, then
    /// refresh the state and close the diff. One decision per file at a
    /// time — a second click while the first is in flight is ignored.
    pub fn review_apply_file(&mut self, path: String, keep: bool, cx: &mut Context<Self>) {
        if !self.review_inflight.insert(path.clone()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let target = ReviewTarget::File { path: path.clone() };
            let result = if keep {
                conn.review_approve(&sid, &target).await
            } else {
                conn.review_revert(&sid, &target).await
            };
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                view.review_inflight.remove(&path);
                match result {
                    Ok(()) => {
                        view.review_diff = None;
                        view.review_gen += 1;
                        view.load_review(cx);
                    }
                    Err(e) => view.error = Some(format!("review write failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `path` currently has an approve/revert in flight.
    pub fn review_busy(&self, path: &str) -> bool {
        self.review_inflight.contains(path)
    }

    /// `config/model/write` — pin a model (and keep its thinking effort).
    pub fn pick_model(&mut self, alias: String, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.config_model_write(&sid, &alias, None).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(r) if !r.rejected => {
                        if let Some(cfg) = &mut view.config {
                            cfg.active_model.alias = alias.clone();
                            cfg.active_model.display_name = cfg
                                .models
                                .iter()
                                .find(|m| m.alias == alias)
                                .map(|m| m.display_name.clone())
                                .unwrap_or_else(|| alias.clone());
                            cfg.active_model_pinned = true;
                        }
                    }
                    Ok(r) => {
                        view.error = Some(format!("model pick rejected: {}", r.failures.join(", ")))
                    }
                    Err(e) => view.error = Some(format!("model pick failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `session/agent/update` — switch the session's agent. A `pending`
    /// mutation keeps `pending_agent` until `session/updated` reconciles
    /// the authoritative active agent; `applied` reads the runtime
    /// snapshot when present.
    pub fn pick_agent(&mut self, name: String, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.session_agent_update(&sid, &name).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(r) if r.rejected => {
                        view.error = Some(format!(
                            "agent switch rejected: {}",
                            r.failures.join(", ")
                        ));
                    }
                    Ok(r) if r.status.as_deref() == Some("pending") => {
                        // The switch may already have applied (its
                        // session/updated raced ahead of this reply) —
                        // reconcile immediately instead of waiting for
                        // a notification that may never come.
                        view.pending_agent = Some(name.clone());
                        view.refresh_agents(cx);
                    }
                    Ok(r) => {
                        let applied = r
                            .runtime
                            .as_ref()
                            .and_then(|rt| rt.get("activeAgent"))
                            .and_then(|a| a.get("name"))
                            .and_then(|n| n.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| name.clone());
                        view.active_agent = applied.clone();
                        view.pending_agent = None;
                        if let Some(cfg) = &mut view.config {
                            cfg.default_agent = applied;
                        }
                    }
                    Err(e) => view.error = Some(format!("agent switch failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-fetch `agents/list` to learn the authoritative active agent.
    /// A pending switch only clears once the list actually reports it
    /// active — an unrelated `session/updated` answering with the old
    /// agent must not drop the pending marker.
    fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let agents = conn.agents_list(&sid).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                if let Ok(list) = agents {
                    let active = list.active.name.clone();
                    view.agents = list.agents;
                    view.active_agent = active.clone();
                    if view.pending_agent.as_deref() == Some(active.as_str()) {
                        view.pending_agent = None;
                    }
                    if let Some(cfg) = &mut view.config {
                        cfg.default_agent = view.active_agent.clone();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `config/write` — flip a bool field; enum fields cycle their
    /// `enum_choices`. One write per path at a time: a click while the
    /// previous write is in flight would resend the same value.
    pub fn toggle_config_field(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.cfg_inflight.insert(path.clone()) {
            return;
        }
        let Some(field) = self.config_fields.iter().find(|f| f.path == path).cloned() else {
            self.cfg_inflight.remove(&path);
            return;
        };
        let next = if field.kind == "bool" {
            serde_json::Value::Bool(!field.value.as_bool().unwrap_or(false))
        } else if field.kind == "enum" && !field.enum_choices.is_empty() {
            let cur = field.value.as_str().unwrap_or_default();
            // A value outside `enum_choices` starts cycling at the first
            // choice rather than skipping it.
            let idx = field
                .enum_choices
                .iter()
                .position(|c| c == cur)
                .map(|i| (i + 1) % field.enum_choices.len())
                .unwrap_or(0);
            serde_json::Value::String(field.enum_choices[idx].clone())
        } else {
            self.cfg_inflight.remove(&path);
            return;
        };
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn
                .config_write(
                    &sid,
                    vec![ConfigWriteOp {
                        op: "set".to_string(),
                        path: field.path.clone(),
                        value: Some(next.clone()),
                        target_layer: None,
                    }],
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                // Skip entirely if a handoff swapped the session: the
                // response is for the old session and the new one may
                // have re-marked this path in flight.
                if view.session_id() != sid {
                    return;
                }
                view.cfg_inflight.remove(&field.path);
                match resp {
                    Ok(r) if !r.rejected => {
                        if let Some(f) =
                            view.config_fields.iter_mut().find(|f| f.path == field.path)
                        {
                            f.value = next;
                        }
                    }
                    Ok(r) => {
                        view.error = Some(format!("config rejected: {}", r.failures.join(", ")))
                    }
                    Err(e) => view.error = Some(format!("config write failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
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
                let session_updated = method == "session/updated";
                let narrate_turn = (method == "turn/completed")
                    .then(|| {
                        params
                            .get("turn")
                            .and_then(|t| t.get("id"))
                            .and_then(|i| i.as_str())
                            .map(str::to_string)
                    })
                    .flatten();
                let resync = matches!(
                    self.projection.on_notification(method, params),
                    Reduce::Resync { .. }
                );
                if resync {
                    self.resync(cx);
                }
                if session_updated && self.pending_agent.is_some() {
                    self.refresh_agents(cx);
                }
                // New turn output / config changes re-shape the review —
                // re-read while the sheet is open instead of serving a
                // stale snapshot.
                if (session_updated || narrate_turn.is_some()) && self.review_open {
                    self.load_review(cx);
                }
                // A queued skill toggle settles via session/updated —
                // refresh until the list confirms it.
                if session_updated && !self.pending_skills.is_empty() {
                    self.load_settings(cx);
                }
                if narrate_turn.is_some() && self.narrator_on {
                    self.narrate_turn(narrate_turn, cx);
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
            // A compact/fork handoff swaps the session id under this view —
            // drop session-scoped settings so nothing stale survives it.
            self.agents.clear();
            self.active_agent.clear();
            self.pending_agent = None;
            self.config_fields.clear();
            self.cfg_inflight.clear();
            self.review = None;
            self.review_diff = None;
            self.review_gen += 1;
            self.review_inflight.clear();
            self.ext_skills.clear();
            self.pending_skills.clear();
            self.mcp_state = None;
            self.connector_counts = None;
            self.plugins.clear();
            self.plugin_dropped.clear();
            self.ext_inflight.clear();
            self.worktrees.clear();
            self.repo_branch = None;
            self.loops.clear();
            if self.settings_open {
                self.load_worktrees(cx);
            }
            if self.settings_open {
                self.load_settings(cx);
            }
            if self.review_open {
                self.load_review(cx);
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
            read_error: None,
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
                            Err(e) => dlg.read_error = Some(e.to_string()),
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
    /// A compact handoff or rewind that lands while the request is in
    /// flight replaces the state — the stale page must not be prepended.
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
                        cursor: Some(cursor.clone()),
                        limit: 50,
                        direction: "backward".into(),
                    },
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                view.loading_earlier = false;
                match result {
                    Ok(page) => {
                        // Same session and still the same pagination
                        // boundary — otherwise the state was replaced
                        // mid-flight and this page no longer belongs.
                        let still_valid = view.session_id() == session_id
                            && view.projection.state.history_before_cursor.as_deref()
                                == Some(cursor.as_str());
                        if still_valid {
                            let history =
                                view.projection.state.history.get_or_insert_with(Vec::new);
                            let mut merged = page.items;
                            merged.append(history);
                            *history = merged;
                            view.projection.state.history_before_cursor = page.previous_cursor;
                        }
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

    // ── Voice ─────────────────────────────────────────────────────────

    /// Mic toggle: start dictation, or mark the run in flight as stopping.
    /// A stopping run keeps its pump until `Done` so the final transcript
    /// still lands in the composer — the mic can't restart until then.
    pub fn toggle_dictation(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.dictation.as_mut() {
            if !run.stopping {
                run.stopping = true;
                run.stop.store(true, Ordering::Relaxed);
            }
            cx.notify();
            return;
        }
        let Some(cfg) = &self.config else { return };
        if !cfg.voice_mode_enabled {
            return;
        }
        let Some(t) = cfg.transcription.as_ref() else {
            self.error = Some("dictation needs a transcription config".to_string());
            cx.notify();
            return;
        };
        let tcfg = voice::TranscriptionConfig {
            api_base: t.provider.api_base.clone(),
            api_key_env_var: t.provider.api_key_env_var.clone(),
            model_name: t.model.name.clone(),
            encoding: t.model.encoding.clone(),
            target_streaming_delay_ms: t.model.target_streaming_delay_ms,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU32::new(0));
        let (chunks, rate) = match voice::start_recording(stop.clone(), peak.clone()) {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let (tx, rx) = futures::channel::mpsc::unbounded::<DictationEvent>();
        // The task must always end with `Done` — an early `transcribe`
        // failure (missing key, handshake) would otherwise leak the
        // recorder and leave the run marked active forever.
        crate::host::runtime().spawn(async move {
            if let Err(e) = voice::transcribe(tcfg, rate, chunks, tx.clone()).await {
                let _ = tx.unbounded_send(DictationEvent::Error(e));
            }
            let _ = tx.unbounded_send(DictationEvent::Done);
        });
        self.dictation_pump = Some(cx.spawn(async move |this, cx| {
            let mut rx = rx;
            while let Some(ev) = rx.next().await {
                let keep = this.update(cx, |view, cx| view.on_dictation_event(ev, cx));
                match keep {
                    Ok(true) | Err(_) => {}
                    Ok(false) => break,
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        }));
        self.dictation = Some(DictationRun {
            stop,
            peak,
            stopping: false,
        });
        // The peak atom changes in the audio callback without notifying
        // gpui — tick redraws so the level meter tracks speech between
        // transcript deltas. Exits once the run drops.
        self.dictation_meter = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            match this.update(cx, |view, cx| {
                if view.dictation.is_some() {
                    cx.notify();
                    true
                } else {
                    false
                }
            }) {
                Ok(true) => {}
                _ => break,
            }
        }));
        cx.notify();
    }

    fn on_dictation_event(&mut self, ev: DictationEvent, cx: &mut Context<Self>) -> bool {
        match ev {
            DictationEvent::TextDelta(t) => {
                self.composer.push_str(&t);
            }
            DictationEvent::Notice(n) | DictationEvent::Error(n) => {
                self.error = Some(n);
            }
            DictationEvent::Done => {
                if let Some(run) = self.dictation.take() {
                    run.stop.store(true, Ordering::Relaxed);
                }
                self.dictation_pump = None;
                self.dictation_meter = None;
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
    }

    /// Narrator switch — also cancels anything playing.
    pub fn toggle_narrator(&mut self, cx: &mut Context<Self>) {
        self.narrator_on = !self.narrator_on;
        if !self.narrator_on {
            self.cancel_narration(cx);
        }
        cx.notify();
    }

    /// Stop a preparing/speaking narration run — the shared flag also
    /// short-circuits `play` if the run reaches it after cancel. Bumping
    /// the generation makes the old pump's late events inert.
    pub fn cancel_narration(&mut self, cx: &mut Context<Self>) {
        self.narration_stop.store(true, Ordering::Relaxed);
        self.narrating = Narration::Idle;
        self.narration_gen = self.narration_gen.wrapping_add(1);
        cx.notify();
    }

    /// `turn/completed` while the narrator is on: summarize → TTS → play,
    /// all on the host runtime; phases arrive back over the channel.
    fn narrate_turn(&mut self, turn_id: Option<String>, cx: &mut Context<Self>) {
        let Some(cfg) = &self.config else { return };
        if !cfg.voice_mode_enabled {
            return;
        }
        let Some(turn_id) = turn_id else { return };
        let Some(s) = cfg.speech.as_ref() else { return };
        let history = self.projection.state.history.as_deref().unwrap_or(&[]);
        let Some((user, asst)) = voice::turn_text(history, &turn_id) else {
            return;
        };
        let scfg = voice::SpeechConfig {
            api_base: s.provider.api_base.clone(),
            api_key_env_var: s.provider.api_key_env_var.clone(),
            model_name: s.model.name.clone(),
            voice: s.model.voice.clone(),
            response_format: s.model.response_format.clone(),
        };
        self.cancel_narration(cx);
        self.narrating = Narration::Preparing;
        self.narration_gen = self.narration_gen.wrapping_add(1);
        let gen = self.narration_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        self.narration_stop = stop.clone();
        let (tx, rx) = futures::channel::mpsc::unbounded::<NarrationEvent>();
        crate::host::runtime().spawn(async move {
            // Cancel between async steps: a cancelled run must not emit
            // Speaking or surface its error after the fact.
            let run = async {
                let summary = conn
                    .narration_summarize(&sid, &user, &asst)
                    .await
                    .map_err(|e| e.to_string())?
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "no summary".to_string())?;
                if stop.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let bytes = voice::speak(&scfg, &summary).await?;
                if stop.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let _ = tx.unbounded_send(NarrationEvent::Speaking);
                voice::play(bytes, stop)
                    .await
                    .map(|_| Some(NarrationEvent::Done))
            };
            let ev = match run.await {
                Ok(ev) => ev,
                Err(e) => Some(NarrationEvent::Error(e)),
            };
            if let Some(ev) = ev {
                let _ = tx.unbounded_send(ev);
            }
        });
        self.narration_pump = Some(cx.spawn(async move |this, cx| {
            let mut rx = rx;
            while let Some(ev) = rx.next().await {
                let keep = this.update(cx, |view, cx| view.on_narration_event(ev, gen, cx));
                match keep {
                    Ok(true) | Err(_) => {}
                    Ok(false) => break,
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Events from a superseded/cancelled run (stale generation) drain
    /// silently so they can't restore a killed Speaking badge or error.
    fn on_narration_event(&mut self, ev: NarrationEvent, gen: u64, cx: &mut Context<Self>) -> bool {
        if gen != self.narration_gen {
            return true;
        }
        match ev {
            NarrationEvent::Speaking => {
                self.narrating = Narration::Speaking;
            }
            NarrationEvent::Done => {
                self.narrating = Narration::Idle;
                self.narration_pump = None;
                cx.notify();
                return false;
            }
            NarrationEvent::Error(e) => {
                self.narrating = Narration::Idle;
                self.narration_pump = None;
                if e != "no summary" {
                    self.error = Some(format!("narration: {e}"));
                }
                cx.notify();
                return false;
            }
        }
        cx.notify();
        true
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

/// A dropped view must not leak the capture thread or a speaking sink —
/// both honor their shared stop flags within tens of ms.
impl Drop for SessionView {
    fn drop(&mut self) {
        if let Some(run) = self.dictation.take() {
            run.stop.store(true, Ordering::Relaxed);
        }
        self.narration_stop.store(true, Ordering::Relaxed);
    }
}
