//! One attached session: its `vibe-app-server` process, protocol projection,
//! composer state, and callback handling.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use gpui::{AsyncApp, Context, FocusHandle, Task, WeakEntity};
use vibe_protocol::client::{ClientError, ClientResult, Connection};
use vibe_protocol::models::*;
use vibe_protocol::projection::{Projection, Reduce};

use crate::app::VibeApp;
use crate::voice::{self, DictationEvent, NarrationEvent};

/// User action destined for a callback.
pub enum CallbackAnswer {
    Approval(ApprovalDecision),
    UserInput(UserQuestionResult),
}

/// Read `path` under `cwd` on the tokio runtime — gpui's executor has no
/// reactor, so `tokio::fs` called directly inside `cx.spawn` panics.
/// A `deleted` review status treats NotFound as empty current; other
/// statuses preserve the error (a genuinely missing modified file must
/// not fake an empty diff).
async fn read_current_file(cwd: &str, path: &str, deleted: bool) -> std::io::Result<String> {
    let full = std::path::Path::new(cwd).join(path);
    match crate::host::runtime()
        .spawn(tokio::fs::read_to_string(full))
        .await
    {
        Ok(Err(e)) if deleted && e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Ok(r) => r,
        Err(join) => Err(std::io::Error::other(join)),
    }
}

/// Does `state` attribute `path` to any owner — a scope listing it or a
/// region naming an owner? The negative case is what a whole-file
/// `ReviewTarget::File` decision is allowed to cover.
fn review_state_claims(state: &ReviewStateResponse, path: &str) -> bool {
    state
        .scopes
        .iter()
        .any(|s| s.files.iter().any(|f| f.path == path))
        || state
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.regions.iter().any(|r| r.owner().is_some()))
            .unwrap_or(false)
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
    /// Ordering guards — `refresh_loops`/`load_worktrees` bump on issue;
    /// a response that loses the race against a newer read (or a
    /// mutation's replacement read) is dropped.
    loops_gen: u64,
    worktrees_gen: u64,
    /// `review/state` snapshot for the review sheet.
    pub review: Option<ReviewStateResponse>,
    /// Review sheet open/closed.
    pub review_open: bool,
    /// Open per-file diff — (path, owner, response), `None` until the
    /// read lands. `None` owner = whole-file preview (no scope claims
    /// the file; baseline→current rendered file-wide).
    pub review_diff: Option<(String, Option<ReviewOwner>, ReviewTurnDiffResponse)>,
    /// Latest diff selection the reviewer asked for — independent of what
    /// `review_diff` currently displays. State refreshes revalidate THIS,
    /// so a refresh can neither reopen a superseded file nor lose a click
    /// whose response is still in flight.
    review_diff_sel: Option<(String, Option<ReviewOwner>)>,
    /// Generation guarding `review/state` reads — a stale response must
    /// not overwrite a newer refresh.
    review_state_gen: u64,
    /// Generation guarding `review/turnDiff` reads — a state refresh must
    /// not cancel a diff open the user just clicked, so it gets its own
    /// counter.
    review_diff_gen: u64,
    /// Files with an in-flight approve/revert — both controls stay
    /// disabled until the mutation settles so opposing decisions
    /// can't race on one file.
    review_inflight: HashSet<String>,
    /// Cloud sheet — `vibeCode/projects` picker + teleport run +
    /// session relocate. Open/closed toggle.
    pub cloud_open: bool,
    /// Latest picker view (`vibeCode/projects/*` responses).
    pub picker: Option<VibeCodePickerView>,
    /// `pickerId` from `projects/open` — every picker call carries it.
    picker_id: Option<String>,
    /// Project the last `projects/select` response selected — the
    /// teleport CTA binds to it.
    pub picker_selected: Option<VibeCodeProject>,
    /// New-project name input for `projects/create`.
    pub project_input: String,
    pub project_focus: FocusHandle,
    /// `session/relocate` cwd input + in-flight guard.
    pub relocate_input: String,
    pub relocate_focus: FocusHandle,
    relocate_inflight: bool,
    /// Live teleport run — phase arrives via `vibeCode/teleport/event`.
    pub teleport: Option<TeleportState>,
    /// Generation for picker reads — a stale `projects/open`/`loadMore`
    /// response can't undo a newer picker action.
    picker_gen: u64,
    /// In-flight picker mutations (`"create"`, `"select:{id}"`,
    /// `"unlink"`, `"load_more"`) — repeat clicks wait for the
    /// response-applied view.
    picker_inflight: HashSet<String>,
    /// `projectLinks/inspectRoot` for the session cwd — drives the
    /// local-link section (saved link + unlink, or candidates).
    pub link: Option<ProjectLinksInspectRootResponse>,
    /// `projectLinks/picker/load` candidates when the cwd is eligible
    /// and unlinked.
    pub link_candidates: Option<ProjectLinksPickerCandidates>,
    /// New-project name input for `projectLinks/create`.
    pub link_name_input: String,
    pub link_focus: FocusHandle,
    /// Generation for link reads — stale inspect/picker replies drop.
    link_gen: u64,
    /// In-flight link ops (`"inspect"`, `"load"`, `"load_more"`,
    /// `"link"`, `"save"`, `"create"`, `"unlink"`).
    link_inflight: HashSet<String>,
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

/// In-flight `vibeCode/teleport` run. `phase` is the last lifecycle
/// event seen; `push` carries the `push_required` approve gate;
/// `url`/`error` settle it. Events for a different `operation_id`
/// update nothing — a superseded run can't bleed into a newer one.
pub struct TeleportState {
    pub operation_id: String,
    pub phase: &'static str,
    /// `(unpushedCount, branchNotPushed)` while `push_required` waits.
    pub push: Option<(u64, bool)>,
    pub url: Option<String>,
    pub error: Option<String>,
    /// A `teleport/cancel` request is in flight — repeated clicks are
    /// ignored until it settles.
    pub cancel_pending: bool,
}

impl TeleportState {
    /// Terminal phase — the run no longer accepts push answers/cancel.
    pub fn settled(&self) -> bool {
        self.url.is_some() || self.error.is_some()
    }
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
            loops_gen: 0,
            worktrees_gen: 0,
            review: None,
            review_open: false,
            review_diff: None,
            review_diff_sel: None,
            review_state_gen: 0,
            review_diff_gen: 0,
            review_inflight: HashSet::new(),
            cloud_open: false,
            picker: None,
            picker_id: None,
            picker_selected: None,
            project_input: String::new(),
            project_focus: cx.focus_handle(),
            relocate_input: String::new(),
            relocate_focus: cx.focus_handle(),
            relocate_inflight: false,
            teleport: None,
            picker_gen: 0,
            picker_inflight: HashSet::new(),
            link: None,
            link_candidates: None,
            link_name_input: String::new(),
            link_focus: cx.focus_handle(),
            link_gen: 0,
            link_inflight: HashSet::new(),
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
        // Loops mutate through their own channel — capture its epoch so
        // this batch can't restore a list a newer refresh already
        // replaced.
        let loops_gen = self.loops_gen;
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
                if view.loops_gen == loops_gen {
                    if let Ok(resp) = loops {
                        view.loops = resp.loops;
                    }
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
                        view.settle_settings_write(cx);
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

    /// A settings write applied server-side: drop any in-flight read
    /// batch (its snapshot predates the write) and start a replacement
    /// so every section re-converges — same race the mcp arm handles.
    fn settle_settings_write(&mut self, cx: &mut Context<Self>) {
        self.settings_gen += 1;
        self.load_settings(cx);
    }

    /// `mcp/toggle` — flip a source between enabled/disabled. Connector-
    /// kind sources toggle `connected`↔`disabled` via
    /// `connector_catalog/toggle` (upstream rejects them on `mcp/toggle`).
    /// Other statuses (needs_auth, needs_setup, …) aren't toggleable.
    pub fn toggle_mcp(&mut self, name: String, cx: &mut Context<Self>) {
        let key = format!("mcp:{name}");
        if !self.ext_inflight.insert(key.clone()) {
            return;
        }
        let Some((kind, disabled)) = self
            .mcp_state
            .as_ref()
            .and_then(|m| m.sources.iter().find(|s| s.name == name))
            .and_then(|s| match (s.kind.as_str(), s.status.as_str()) {
                ("connector", "connected") => Some((s.kind.clone(), true)),
                ("connector", "disabled") => Some((s.kind.clone(), false)),
                (_, "enabled") => Some((s.kind.clone(), true)),
                (_, "disabled") => Some((s.kind.clone(), false)),
                _ => None,
            })
        else {
            self.ext_inflight.remove(&key);
            return;
        };
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = if kind == "connector" {
                conn.connector_catalog_toggle(&sid, &name, disabled).await
            } else {
                conn.mcp_toggle(&sid, &name, &kind, disabled).await
            };
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
                            view.mcp_state = Some(mcp);
                        }
                        // Drop any in-flight batch (its snapshot predates
                        // the write) and start a replacement read so every
                        // section re-converges — same as other writes.
                        view.settle_settings_write(cx);
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
    /// session's project root rather than a session id. `worktrees_gen`
    /// drops a response that loses to a newer read.
    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.projection.state.session.cwd.clone() else {
            return;
        };
        self.worktrees_gen += 1;
        let gen = self.worktrees_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.workspace_worktrees(&cwd).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.worktrees_gen != gen {
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

    /// `loops/list` refresh after a create/delete mutation. Bumping
    /// `loops_gen` also invalidates an in-flight settings batch's loops
    /// arm — it captured the pre-mutation generation.
    fn refresh_loops(&mut self, cx: &mut Context<Self>) {
        self.loops_gen += 1;
        let gen = self.loops_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this, cx| {
            let resp = conn.loops_list(&sid).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.loops_gen != gen {
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
                        // Only clear if the editor still holds the
                        // submitted text — a draft typed while the
                        // create was in flight must survive.
                        if view.loop_input.trim() == text {
                            view.loop_input.clear();
                        }
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
                    Ok(_) => {
                        // Optimistic remove — the row is gone, so its
                        // delete control can't be double-clicked into a
                        // second delete that would surface a misleading
                        // not_found. The gen-guarded refresh confirms.
                        view.loops.retain(|l| l.id != id);
                        view.refresh_loops(cx);
                    }
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
    /// stale; `review_state_gen` drops a response that loses the race.
    /// An open diff is revalidated against the fresh state: still-present
    /// files are re-read, vanished files close the diff.
    fn load_review(&mut self, cx: &mut Context<Self>) {
        self.review_state_gen += 1;
        let gen = self.review_state_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let state = conn.review_state(&sid).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.review_state_gen != gen {
                    return;
                }
                match state {
                    Ok(s) => {
                        let sel = view.review_diff_sel.clone();
                        let still_present = sel.as_ref().is_some_and(|(path, _)| {
                            s.files.iter().any(|f| f.path == *path)
                                || s.scopes.iter().any(|sc| sc.files.iter().any(|f| f.path == *path))
                        });
                        view.review = Some(s);
                        if let Some((path, owner)) = sel {
                            // Keep the reviewer's owner while it still
                            // applies — a scoped sel that outlived its
                            // owner re-resolves (possibly to file-wide).
                            let owner = match owner {
                                Some(o) if view.review_owner_valid(&path, &o) => Some(o),
                                _ => view.review_owner_scoped(&path),
                            };
                            if still_present {
                                view.open_review_diff(path, owner, cx)
                            } else {
                                view.review_diff = None;
                                view.review_diff_sel = None;
                                view.review_diff_gen += 1;
                            }
                        }
                    }
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
            self.review_state_gen += 1;
            self.review_diff_gen += 1;
        }
        cx.notify();
    }

    /// The owner that genuinely claims `path` — a scope listing it, else
    /// the file's own region owner. `None` when no scope or region does:
    /// never borrow an unrelated scope's owner, its `turnDiff` would
    /// show an empty slice (and deciding it could revert unseen edits).
    fn review_owner_scoped(&self, path: &str) -> Option<ReviewOwner> {
        let state = self.review.as_ref()?;
        if let Some(scope) = state
            .scopes
            .iter()
            .find(|s| s.files.iter().any(|f| f.path == path))
        {
            return Some(scope.owner.clone());
        }
        state
            .files
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.regions.iter().find_map(|r| r.owner()))
    }

    /// Open the baseline→current diff for a file. A scoped owner reads
    /// `review/turnDiff` (that scope's slice — two owners on one file
    /// show their own diffs). No owner → whole-file preview:
    /// `review/baseline` + the file as it stands on disk, so a
    /// `ReviewTarget::File` decision later covers exactly what was shown.
    pub fn open_review_diff(
        &mut self,
        path: String,
        owner: Option<ReviewOwner>,
        cx: &mut Context<Self>,
    ) {
        let owner = owner.or_else(|| self.review_owner_scoped(&path));
        // Drop the displayed diff at once — keep/revert must not stay
        // actionable on the previous file while this read is in flight.
        self.review_diff = None;
        cx.notify();
        self.review_diff_sel = Some((path.clone(), owner.clone()));
        self.review_diff_gen += 1;
        let gen = self.review_diff_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        let status = self
            .review
            .as_ref()
            .and_then(|s| s.files.iter().find(|f| f.path == path))
            .map(|f| f.status.clone())
            .unwrap_or_else(|| "modified".into());
        let cwd = self.projection.state.session.cwd.clone();
        cx.spawn(async move |this, cx| {
            let resp = match &owner {
                Some(owner) => conn.review_turn_diff(&sid, &path, owner).await,
                None => {
                    let local_read = |e: String| ClientError::Protocol {
                        code: "local_read".into(),
                        message: format!("file-wide preview needs the file on disk: {e}"),
                        data: serde_json::Value::Null,
                    };
                    match (conn.review_baseline(&sid, &path).await, &cwd) {
                        (Ok(baseline), Some(cwd)) => {
                            match read_current_file(cwd, &path, status == "deleted").await {
                                Ok(current) => Ok(ReviewTurnDiffResponse {
                                    status,
                                    baseline,
                                    current,
                                }),
                                Err(e) => Err(local_read(e.to_string())),
                            }
                        }
                        (Ok(_), None) => Err(local_read("session has no local cwd".into())),
                        (Err(e), _) => Err(e),
                    }
                }
            };
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.review_diff_gen != gen {
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

    /// Does `owner` still author `path` in the current state? Used when
    /// revalidating an open selection after a refresh — a scope naming
    /// the pair counts, and so does an unscoped file whose regions name
    /// the owner — or a regionless file no scope claims (the
    /// derived-owner case).
    fn review_owner_valid(&self, path: &str, owner: &ReviewOwner) -> bool {
        let Some(state) = self.review.as_ref() else {
            return false;
        };
        if state
            .scopes
            .iter()
            .any(|s| s.owner == *owner && s.files.iter().any(|f| f.path == path))
        {
            return true;
        }
        let Some(file) = state.files.iter().find(|f| f.path == path) else {
            return false;
        };
        if !file.regions.is_empty() {
            // A regioned file's owner must still come from its regions —
            // otherwise an obsolete owner survives a region handoff.
            return file
                .regions
                .iter()
                .any(|r| r.owner().as_ref() == Some(owner));
        }
        // Regionless + unscoped: no owner is authoritative — the sel
        // re-resolves to a whole-file preview instead of keeping one.
        false
    }

    /// Every listed file is openable — ownerless rows fall back to the
    /// whole-file preview rather than rendering inert.
    pub fn review_file_openable(&self, _path: &str) -> bool {
        true
    }

    pub fn close_review_diff(&mut self, cx: &mut Context<Self>) {
        self.review_diff = None;
        self.review_diff_sel = None;
        self.review_diff_gen += 1;
        cx.notify();
    }

    /// Is this selection safe to keep/revert? `None` (whole-file preview)
    /// only while the file is STILL ownerless — a scope that claimed it
    /// since the preview rendered means a `File` target could revert
    /// edits the preview never showed. `Some(o)` only when `o` genuinely
    /// claims the file: a scope naming the pair or a regioned file whose
    /// regions name it.
    pub fn review_decidable(&self, path: &str, owner: Option<&ReviewOwner>) -> bool {
        let Some(owner) = owner else {
            return self
                .review
                .as_ref()
                .map(|s| !review_state_claims(s, path))
                .unwrap_or(false);
        };
        let Some(state) = self.review.as_ref() else {
            return false;
        };
        if state
            .scopes
            .iter()
            .any(|s| s.owner == *owner && s.files.iter().any(|f| f.path == path))
        {
            return true;
        }
        state
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| {
                !f.regions.is_empty()
                    && f.regions.iter().any(|r| r.owner().as_ref() == Some(owner))
            })
            .unwrap_or(false)
    }

    /// `review/approve` (`keep`) or `review/revert`, then refresh state.
    /// The decision target always matches the displayed diff: `ScopeFile`
    /// on a scope-claimed file (that scope's slice was shown), `File` on
    /// the whole-file preview (every pending change was shown). A scoped
    /// owner that lost its claim since the diff opened is refused — its
    /// diff no longer matches what a decision would touch. One decision
    /// per file — a second click while in flight is ignored.
    pub fn review_apply_file(
        &mut self,
        path: String,
        owner: Option<ReviewOwner>,
        keep: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.review_decidable(&path, owner.as_ref()) {
            self.error = Some(format!(
                "review: {path}'s owner changed — re-read the diff before deciding"
            ));
            cx.notify();
            return;
        }
        // A file-wide decision needs the displayed preview's contents to
        // re-verify — sel without a rendered diff can't decide safely.
        let displayed = if owner.is_none() {
            match &self.review_diff {
                Some((p, o, d)) if *p == path && o.is_none() => Some(d.clone()),
                _ => None,
            }
        } else {
            None
        };
        if owner.is_none() && displayed.is_none() {
            self.error = Some(format!(
                "review: open {path}'s file-wide preview before deciding"
            ));
            cx.notify();
            return;
        }
        if !self.review_inflight.insert(path.clone()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        let cwd = self.projection.state.session.cwd.clone();
        cx.spawn(async move |this, cx| {
            let target = match &owner {
                Some(owner) => Ok(ReviewTarget::ScopeFile {
                    owner: owner.clone(),
                    path: path.clone(),
                }),
                None => {
                    // `File` covers every pending change — re-verify the
                    // preview against FRESH reads: the file may have
                    // gained a scope or new edits since it rendered.
                    let Some(displayed) = &displayed else {
                        unreachable!("file-wide decision requires the displayed diff")
                    };
                    let verified: Result<(), String> = async {
                        let state = conn
                            .review_state(&sid)
                            .await
                            .map_err(|e| format!("review state re-read failed: {e}"))?;
                        if review_state_claims(&state, &path) {
                            return Err(format!(
                                "{path} gained an owning scope — decide its slices individually"
                            ));
                        }
                        let fresh_base = conn
                            .review_baseline(&sid, &path)
                            .await
                            .map_err(|e| format!("baseline re-read failed: {e}"))?;
                        // Status drift means a different operation than
                        // was previewed — an empty file deleted after
                        // the render still compares equal on text.
                        let fresh_status = state
                            .files
                            .iter()
                            .find(|f| f.path == path)
                            .map(|f| f.status.as_str());
                        if fresh_status != Some(displayed.status.as_str()) {
                            return Err(format!(
                                "{path}'s status changed — re-open the diff"
                            ));
                        }
                        let deleted = fresh_status == Some("deleted");
                        let Some(cwd) = &cwd else {
                            return Err(
                                "no local cwd — can't verify the file".to_string()
                            );
                        };
                        let fresh_cur = read_current_file(cwd, &path, deleted)
                            .await
                            .map_err(|e| format!("file re-read failed: {e}"))?;
                        if fresh_base != displayed.baseline
                            || fresh_cur != displayed.current
                        {
                            return Err(format!(
                                "{path} changed since the preview — re-open the diff"
                            ));
                        }
                        Ok(())
                    }
                    .await;
                    verified.map(|()| ReviewTarget::File {
                        path: path.clone(),
                    })
                }
            };
            let target = match target {
                Ok(t) => t,
                Err(msg) => {
                    let _ = this.update(cx, |view, cx| {
                        if view.session_id() != sid {
                            return;
                        }
                        view.review_inflight.remove(&path);
                        view.error = Some(msg);
                        cx.notify();
                    });
                    return;
                }
            };
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
                        // Only close the diff that the decision applied
                        // to — the exact (path, owner) selection; another
                        // scope's or view's diff on the file stays open.
                        let hit = view
                            .review_diff_sel
                            .as_ref()
                            .is_some_and(|(p, o)| *p == path && *o == owner);
                        if hit {
                            view.review_diff = None;
                            view.review_diff_sel = None;
                            view.review_diff_gen += 1;
                        }
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

    /// Cloud sheet toggle — opens the projects picker (`purpose:
    /// "teleport"); closing cancels the picker server-side so it can't
    /// leak a suspended workflow.
    pub fn toggle_cloud(&mut self, cx: &mut Context<Self>) {
        self.cloud_open = !self.cloud_open;
        if self.cloud_open {
            self.relocate_input = self
                .projection
                .state
                .session
                .cwd
                .clone()
                .unwrap_or_default();
            self.load_picker(cx);
            self.load_link(cx);
        } else {
            let sid = self.session_id().to_string();
            self.close_picker(&sid, cx);
            self.clear_link();
        }
        cx.notify();
    }

    /// Cancel the live picker workflow server-side and drop its state —
    /// shared by sheet-close, session handoff, and relocate refresh.
    /// `cancel_sid` is the session the picker was opened against (the old
    /// id on handoff, where the projection already reports the new one).
    fn close_picker(&mut self, cancel_sid: &str, cx: &mut Context<Self>) {
        self.picker_gen += 1;
        self.picker = None;
        self.picker_selected = None;
        self.picker_inflight.clear();
        if let Some(picker_id) = self.picker_id.take() {
            let conn = self.conn.clone();
            let sid = cancel_sid.to_string();
            cx.spawn(async move |_, _| {
                let _ = conn.projects_cancel(&sid, &picker_id).await;
            })
            .detach();
        }
    }

    // ── projectLinks — the session cwd's local↔remote binding ──

    /// The cwd the current `link` state was inspected for — a relocate or
    /// handoff invalidates it.
    fn link_root(&self) -> Option<String> {
        self.projection.state.session.cwd.clone()
    }

    /// `projectLinks/inspectRoot` — refresh the local-link state. When
    /// the root is eligible and unlinked, the candidate picker loads
    /// right after so the section shows choices, not a spinner.
    fn load_link(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.link_root() else {
            self.link = None;
            self.link_candidates = None;
            return;
        };
        if !self.link_inflight.insert("inspect".to_string()) {
            return;
        }
        self.link_gen += 1;
        let gen = self.link_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.project_links_inspect_root(&root).await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("inspect");
                if view.session_id() != sid || view.link_gen != gen {
                    return;
                }
                match resp {
                    Ok(r) => {
                        let needs_candidates =
                            r.eligible && r.saved_link.is_none();
                        view.link = Some(r);
                        view.link_candidates = None;
                        if needs_candidates {
                            view.link_picker_load(cx);
                        }
                    }
                    Err(e) => {
                        view.link = None;
                        view.error = Some(format!("link inspect failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/picker/load` — first page of link candidates.
    fn link_picker_load(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.link_root() else {
            return;
        };
        if !self.link_inflight.insert("load".to_string()) {
            return;
        }
        self.link_gen += 1;
        let gen = self.link_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.project_links_picker_load(&root).await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("load");
                if view.session_id() != sid || view.link_gen != gen {
                    return;
                }
                match resp {
                    Ok(r) => view.link_candidates = Some(r.candidates),
                    Err(e) => view.error = Some(format!("link picker failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/picker/loadMore` — next candidates page.
    pub fn link_candidates_more(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(cursor)) = (
            self.link_root(),
            self.link_candidates
                .as_ref()
                .and_then(|c| c.next_cursor.clone()),
        ) else {
            return;
        };
        if !self.link_inflight.insert("load_more".to_string()) {
            return;
        }
        self.link_gen += 1;
        let gen = self.link_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.project_links_picker_load_more(&root, &cursor).await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("load_more");
                if view.session_id() != sid || view.link_gen != gen {
                    return;
                }
                match resp {
                    Ok(r) => {
                        if let Some(cands) = view.link_candidates.as_mut() {
                            cands.items.extend(r.candidates.items);
                            cands.next_cursor = r.candidates.next_cursor;
                        } else {
                            view.link_candidates = Some(r.candidates);
                        }
                    }
                    Err(e) => view.error = Some(format!("link page failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/link` — bind the cwd to a picked candidate, then
    /// re-inspect so the section shows the saved link.
    pub fn link_pick(&mut self, project_id: String, project_name: String, cx: &mut Context<Self>) {
        let Some(root) = self.link_root() else {
            return;
        };
        if !self.link_inflight.insert("mutate".to_string()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn
                .project_links_link(&root, &project_id, &project_name)
                .await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("mutate");
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(_) => {
                        view.mark_linked(&root, true, cx);
                        view.load_link(cx);
                    }
                    Err(e) => view.error = Some(format!("link failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/save` — persist the link, carrying the inspected
    /// repo url as the drift check (a changed remote fails server-side).
    pub fn link_save(&mut self, project_id: String, project_name: String, cx: &mut Context<Self>) {
        let Some(root) = self.link_root() else {
            return;
        };
        if !self.link_inflight.insert("mutate".to_string()) {
            return;
        }
        let expected = self
            .link
            .as_ref()
            .and_then(|l| l.root.as_ref())
            .and_then(|r| r.git.as_ref())
            .and_then(|g| g.github_repo_url.clone());
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn
                .project_links_save(&root, &project_id, &project_name, expected.as_deref())
                .await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("mutate");
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(_) => {
                        view.mark_linked(&root, true, cx);
                        view.load_link(cx);
                    }
                    Err(e) => view.error = Some(format!("link save failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/create` — new remote project from the link-name
    /// input, linked to this cwd on the inspected default branch.
    pub fn link_create(&mut self, cx: &mut Context<Self>) {
        let name = self.link_name_input.trim().to_string();
        let Some(root) = self.link_root() else {
            return;
        };
        if name.is_empty() || !self.link_inflight.insert("mutate".to_string()) {
            return;
        }
        let branch = self
            .link
            .as_ref()
            .and_then(|l| l.root.as_ref())
            .and_then(|r| r.git.as_ref())
            .and_then(|g| g.default_branch.clone().or(g.current_branch.clone()))
            .unwrap_or_else(|| "main".to_string());
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.project_links_create(&root, &name, &branch).await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("mutate");
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(_) => {
                        view.link_name_input.clear();
                        view.mark_linked(&root, true, cx);
                        view.load_link(cx);
                    }
                    Err(e) => view.error = Some(format!("link create failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `projectLinks/unlink` — drop the saved binding for this cwd.
    pub fn link_remove(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.link_root() else {
            return;
        };
        if !self.link_inflight.insert("mutate".to_string()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.project_links_unlink(&root).await;
            let _ = this.update(cx, |view, cx| {
                view.link_inflight.remove("mutate");
                if view.session_id() != sid {
                    return;
                }
                match resp {
                    Ok(_) => {
                        view.mark_linked(&root, false, cx);
                        view.load_link(cx);
                    }
                    Err(e) => view.error = Some(format!("link unlink failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Link-name input for `projectLinks/create`.
    pub fn edit_link_input(&mut self, ch: Option<&str>, cx: &mut Context<Self>) {
        match ch {
            Some(c) => self.link_name_input.push_str(c),
            None => {
                self.link_name_input.pop();
            }
        }
        cx.notify();
    }

    /// Update the rail's linked-dir set after a mutation so the marker
    /// appears/disappears without waiting for a catalog refresh.
    fn mark_linked(&mut self, root: &str, linked: bool, cx: &mut Context<Self>) {
        if let Some(app) = self.app.as_ref().and_then(|w| w.upgrade()) {
            let root = root.to_string();
            app.update(cx, |app, cx| {
                app.linked_gen += 1;
                if linked {
                    app.linked_dirs.insert(root);
                } else {
                    app.linked_dirs.remove(&root);
                }
                cx.notify();
            });
        }
    }

    /// A link op is in flight. `"mutate"` covers link/save/create/unlink
    /// — one guard for all of them so concurrent choices can't interleave
    /// server-side writes for the same root.
    pub fn link_busy(&self, key: &str) -> bool {
        self.link_inflight.contains(key)
    }

    /// Drop link state — handoff/relocate/sheet-close paths share it.
    fn clear_link(&mut self) {
        self.link = None;
        self.link_candidates = None;
        self.link_gen += 1;
        self.link_inflight.clear();
    }

    /// Picker mutation in flight (`create`, `select:{id}`, `unlink`,
    /// `load_more`) — the sheet dims its controls until it settles.
    pub fn picker_busy(&self, key: &str) -> bool {
        self.picker_inflight.contains(key)
    }

    /// A `session/relocate` request is in flight.
    pub fn relocate_busy(&self) -> bool {
        self.relocate_inflight
    }

    /// `vibeCode/projects/open` — fresh picker view for the sheet.
    /// `picker_gen` drops a response that loses the race (the sheet was
    /// closed and reopened, or another open superseded it).
    fn load_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_gen += 1;
        let gen = self.picker_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.projects_open(&sid, "teleport").await;
            let stale_pid = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.picker_gen != gen || !view.cloud_open {
                    // A picker that opened after its generation died still
                    // exists server-side — hand the id back so the caller
                    // can cancel the suspended workflow.
                    return match resp {
                        Ok(r) => Some(r.picker_id),
                        Err(_) => None,
                    };
                }
                match resp {
                    Ok(r) => {
                        view.picker_id = Some(r.picker_id);
                        view.picker = Some(r.view);
                        // resolvedProjectId marks a previously-linked
                        // project — preselect it so teleport binds it.
                        view.picker_selected = r
                            .resolved_project_id
                            .and_then(|pid| {
                                view.picker
                                    .as_ref()?
                                    .state
                                    .projects
                                    .iter()
                                    .find(|p| p.project_id == pid)
                                    .cloned()
                            })
                            .or_else(|| view.picker.as_ref()?.context.saved_link.clone().map(
                                |link| VibeCodeProject {
                                    project_id: link.project_id.clone(),
                                    name: link.project_name.clone(),
                                    repositories: vec![VibeCodeRepository {
                                        repo_url: link.repo_url.clone(),
                                        default_branch: None,
                                    }],
                                    is_read_only: false,
                                },
                            ));
                    }
                    Err(e) => view.error = Some(format!("projects read failed: {e}")),
                }
                cx.notify();
                None
            });
            if let Ok(Some(pid)) = stale_pid {
                let _ = conn.projects_cancel(&sid, &pid).await;
            }
        })
        .detach();
    }

    /// `vibeCode/projects/loadMore` — page the project list; the
    /// returned view replaces the snapshot wholesale.
    pub fn picker_load_more(&mut self, cx: &mut Context<Self>) {
        let Some(picker_id) = self.picker_id.clone() else {
            return;
        };
        if !self.picker_inflight.insert("load_more".to_string()) {
            return;
        }
        self.picker_gen += 1;
        let gen = self.picker_gen;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.projects_load_more(&sid, &picker_id).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                // Release the key when the same picker still owns it —
                // a reply that lost the gen race to a mutation still
                // finished this picker's page request. A mismatched id
                // means close_picker already cleared the keys and a new
                // picker may reuse "load_more".
                if view.picker_id.as_deref() == Some(picker_id.as_str()) {
                    view.picker_inflight.remove("load_more");
                }
                if view.picker_gen != gen {
                    // The key release must redraw even on a stale reply —
                    // the load-more control only shows when the key is
                    // free.
                    cx.notify();
                    return;
                }
                match resp {
                    Ok(r) => view.picker = Some(r.view),
                    Err(e) => view.error = Some(format!("projects page failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `vibeCode/projects/select` — choose the project a later
    /// `teleport/start` binds to. Selections serialize (one in flight at
    /// a time): two row clicks can't race a stale reply into overwriting
    /// the newest choice.
    pub fn picker_select(&mut self, project_id: String, cx: &mut Context<Self>) {
        let Some(picker_id) = self.picker_id.clone() else {
            return;
        };
        if !self.picker_inflight.insert("select".to_string()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.projects_select(&sid, &picker_id, &project_id).await;
            let _ = this.update(cx, |view, cx| {
                // The reply must land on the picker it was issued
                // against — a reopened picker has a new id, and an
                // old picker's reply must not overwrite it (or release
                // the new picker's inflight key).
                if view.session_id() != sid || view.picker_id.as_deref() != Some(picker_id.as_str())
                {
                    return;
                }
                view.picker_inflight.remove("select");
                match resp {
                    Ok(r) => {
                        // The applied view supersedes any in-flight
                        // read — invalidate pending loadMore replies.
                        view.picker_gen += 1;
                        view.picker = Some(r.view);
                        view.picker_selected = Some(r.project);
                    }
                    Err(e) => view.error = Some(format!("project select failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `vibeCode/projects/unlink` — drop the saved repo↔project link.
    pub fn picker_unlink(&mut self, cx: &mut Context<Self>) {
        let Some(picker_id) = self.picker_id.clone() else {
            return;
        };
        if !self.picker_inflight.insert("unlink".to_string()) {
            return;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.projects_unlink(&sid, &picker_id).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.picker_id.as_deref() != Some(picker_id.as_str())
                {
                    return;
                }
                view.picker_inflight.remove("unlink");
                match resp {
                    Ok(r) => {
                        view.picker_gen += 1;
                        view.picker = Some(r.view);
                        view.picker_selected = None;
                    }
                    Err(e) => view.error = Some(format!("project unlink failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Sheet text inputs — `None` backspaces, `Some` appends.
    pub fn edit_project_input(&mut self, ch: Option<&str>, cx: &mut Context<Self>) {
        match ch {
            Some(c) => self.project_input.push_str(c),
            None => {
                self.project_input.pop();
            }
        }
        cx.notify();
    }

    pub fn edit_relocate_input(&mut self, ch: Option<&str>, cx: &mut Context<Self>) {
        match ch {
            Some(c) => self.relocate_input.push_str(c),
            None => {
                self.relocate_input.pop();
            }
        }
        cx.notify();
    }

    /// `vibeCode/projects/create` — new cloud project from the sheet's
    /// name input on the repo's default branch.
    pub fn picker_create(&mut self, cx: &mut Context<Self>) {
        let name = self.project_input.trim().to_string();
        let Some(picker_id) = self.picker_id.clone() else {
            return;
        };
        if name.is_empty() || !self.picker_inflight.insert("create".to_string()) {
            return;
        }
        let branch = self
            .picker
            .as_ref()
            .and_then(|v| v.git.default_branch.clone().or(v.git.branch.clone()))
            .unwrap_or_else(|| "main".to_string());
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.projects_create(&sid, &picker_id, &name, &branch).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid || view.picker_id.as_deref() != Some(picker_id.as_str())
                {
                    return;
                }
                view.picker_inflight.remove("create");
                match resp {
                    Ok(r) => {
                        view.picker_gen += 1;
                        view.picker = Some(r.view);
                        view.picker_selected = Some(r.project);
                        view.project_input.clear();
                    }
                    Err(e) => view.error = Some(format!("project create failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `vibeCode/teleport/start` — push this session to the selected
    /// cloud project. `operationId` is client-chosen so a superseded
    /// run's events can't bleed into a newer one.
    pub fn teleport_begin(&mut self, project_id: String, cx: &mut Context<Self>) {
        let Some(picker_id) = self.picker_id.clone() else {
            return;
        };
        if self.teleport.as_ref().is_some_and(|t| !t.settled()) {
            return;
        }
        let operation_id = format!(
            "op-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        self.teleport = Some(TeleportState {
            operation_id: operation_id.clone(),
            phase: "starting",
            push: None,
            url: None,
            error: None,
            cancel_pending: false,
        });
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn
                .teleport_start(&sid, &picker_id, &operation_id, &project_id)
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                let Some(t) = view.teleport.as_mut() else {
                    return;
                };
                if t.operation_id != operation_id {
                    return;
                }
                match resp {
                    Ok(r) => {
                        // The server's operation id is authoritative.
                        t.operation_id = r.operation_id;
                    }
                    Err(e) => {
                        t.phase = "failed";
                        t.error = Some(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `vibeCode/teleport/push/respond` — answer the `push_required`
    /// gate (push unpushed commits or decline).
    pub fn teleport_push_answer(&mut self, approved: bool, cx: &mut Context<Self>) {
        let Some(t) = self.teleport.as_mut() else {
            return;
        };
        if t.push.is_none() {
            return;
        }
        let gate = t.push.take();
        t.phase = if approved { "pushing" } else { "declining" };
        let op = t.operation_id.clone();
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.teleport_push_respond(&sid, &op, approved).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                if let Err(e) = resp {
                    if let Some(t) = view.teleport.as_mut() {
                        if t.operation_id == op && !t.settled() {
                            // The server still awaits the gate answer —
                            // restore it so the user can retry instead
                            // of being left with only dismissal.
                            t.push = gate;
                            t.phase = "push required";
                            view.error = Some(format!("push answer failed: {e}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `vibeCode/teleport/cancel` — abort the in-flight run.
    pub fn teleport_cancel(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.teleport.as_ref() else {
            return;
        };
        if t.settled() {
            self.teleport = None;
            cx.notify();
            return;
        }
        if t.cancel_pending {
            return;
        }
        let op = t.operation_id.clone();
        if let Some(t) = self.teleport.as_mut() {
            t.cancel_pending = true;
        }
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.teleport_cancel(&sid, &op).await;
            let _ = this.update(cx, |view, cx| {
                if view.session_id() != sid {
                    return;
                }
                // The response must apply to the run it cancelled — a
                // late reply for a superseded operation must not erase
                // (or error) the run that's current now.
                match resp {
                    Ok(r) if r.cancelled => {
                        if view
                            .teleport
                            .as_ref()
                            .is_some_and(|t| t.operation_id == op)
                        {
                            view.teleport = None;
                        }
                    }
                    Ok(_) => {
                        if let Some(t) = view.teleport.as_mut() {
                            if t.operation_id == op {
                                t.cancel_pending = false;
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(t) = view.teleport.as_mut() {
                            if t.operation_id == op {
                                t.cancel_pending = false;
                                t.error = Some(format!("cancel failed: {e}"));
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `session/relocate` — move this session's cwd; the returned state
    /// goes through `adopt` so a stale snapshot can't roll back the
    /// projection.
    pub fn relocate(&mut self, cx: &mut Context<Self>) {
        let cwd = self.relocate_input.trim().to_string();
        if cwd.is_empty() || self.relocate_inflight {
            return;
        }
        self.relocate_inflight = true;
        let conn = self.conn.clone();
        let sid = self.session_id().to_string();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let resp = conn.session_relocate(&sid, &cwd).await;
            let _ = this.update(cx, |view, cx| {
                // A reply that outlived a handoff must not clear a NEW
                // relocate's busy flag — check ownership first.
                if view.session_id() != sid {
                    return;
                }
                view.relocate_inflight = false;
                match resp {
                    Ok(r) => {
                        // adopt() refuses a stale snapshot — resync then.
                        if !view.projection.adopt(r.state) {
                            view.resync(cx);
                        }
                        // cwd moved: the picker + workspace reads are
                        // bound to the old checkout — reopen the picker
                        // against the new root and refresh workspace-
                        // scoped trust/worktrees/settings.
                        let sid2 = view.session_id().to_string();
                        view.close_picker(&sid2, cx);
                        view.worktrees.clear();
                        view.repo_branch = None;
                        view.config_fields.clear();
                        view.trust_dismissed = false;
                        view.check_trust(cx);
                        view.load_worktrees(cx);
                        view.clear_link();
                        // The rail row's cwd is stale now — update it so
                        // the linked-dir marker recomputes against the new
                        // checkout without waiting for a catalog refresh.
                        if let Some(app) = view.app.as_ref().and_then(|w| w.upgrade()) {
                            let new_cwd = view
                                .projection
                                .state
                                .session
                                .cwd
                                .clone()
                                .unwrap_or_else(|| cwd.clone());
                            let sid3 = sid.clone();
                            app.update(cx, |app, cx| {
                                if let Some(row) =
                                    app.sessions.iter_mut().find(|s| s.id == sid3)
                                {
                                    row.cwd = Some(new_cwd);
                                }
                                cx.notify();
                            });
                        }
                        if view.cloud_open {
                            view.load_picker(cx);
                            view.load_link(cx);
                        }
                        if view.settings_open {
                            view.load_settings(cx);
                        }
                    }
                    Err(e) => view.error = Some(format!("relocate failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// `vibeCode/teleport/event` — advance the tracked run's phase.
    /// Events for an untracked operation id are ignored.
    fn on_teleport_event(&mut self, event: TeleportEvent) {
        let Some(t) = self.teleport.as_mut() else {
            return;
        };
        if t.operation_id != event.operation_id() {
            return;
        }
        match event {
            TeleportEvent::SummarizingContext { .. } => t.phase = "summarizing context",
            TeleportEvent::CheckingGit { .. } => t.phase = "checking git",
            TeleportEvent::PushRequired {
                unpushed_count,
                branch_not_pushed,
                ..
            } => {
                t.phase = "push required";
                t.push = Some((unpushed_count, branch_not_pushed));
            }
            TeleportEvent::Pushing { .. } => {
                t.phase = "pushing";
                t.push = None;
            }
            TeleportEvent::StartingWorkflow { .. } => t.phase = "starting cloud workflow",
            TeleportEvent::Complete { url, .. } => {
                t.phase = "complete";
                t.url = Some(url);
                t.push = None;
            }
            TeleportEvent::Failed { error, .. } => {
                t.phase = "failed";
                t.error = Some(error.message);
                t.push = None;
            }
        }
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
                        view.settle_settings_write(cx);
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
                        view.settle_settings_write(cx);
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
                // The read raced a relocate — it describes the OLD cwd
                // and must not overwrite the new workspace's status.
                if view.projection.state.session.cwd != cwd {
                    return;
                }
                if let Ok(status) = result {
                    // Clear a stale banner too — after a relocate the
                    // new cwd may be trusted where the old one wasn't.
                    view.trust = if status.status == "untrusted" {
                        status.details
                    } else {
                        None
                    };
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
                // Non-watermarked lifecycle channel — route before the
                // projection so a resync window can't swallow it.
                if method == "vibeCode/teleport/event" {
                    if let Ok(p) =
                        serde_json::from_value::<TeleportEventParams>(params.clone())
                    {
                        self.on_teleport_event(p.event);
                    }
                }
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
            self.review_diff_sel = None;
            self.review_state_gen += 1;
            self.review_diff_gen += 1;
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
            self.loops_gen += 1;
            self.worktrees_gen += 1;
            // Cloud state is session-scoped too — cancel the old picker
            // against the OLD session id, drop teleport tracking, and
            // reopen the picker for the handed-off session when the
            // sheet is up.
            self.close_picker(&old, cx);
            self.teleport = None;
            self.relocate_inflight = false;
            self.clear_link();
            if self.cloud_open {
                self.load_picker(cx);
                self.load_link(cx);
            }
            if self.settings_open {
                self.load_worktrees(cx);
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
                            // supersedes the projection wholesale — and
                            // rewound-away edits must leave the review
                            // sheet, which a same-session replace can't
                            // reach through the notification refresh.
                            if !view.projection.adopt(resp.state) {
                                view.resync(cx);
                            }
                            view.review = None;
                            view.review_diff = None;
                            view.review_diff_sel = None;
                            view.review_state_gen += 1;
                            view.review_diff_gen += 1;
                            if view.review_open {
                                view.load_review(cx);
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
