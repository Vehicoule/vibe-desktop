//! Root view: session rail + selected session + new-session sheet.

use std::path::PathBuf;

use gpui::{AppContext as _, AsyncApp, Context, Entity, FocusHandle, WeakEntity};
use vibe_protocol::client::{ClientResult, Connection};
use vibe_protocol::models::*;

use crate::{host, session::SessionView};

/// Where sessions come from: the real server or the dev fixture.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Server,
    Fixture,
}

pub struct VibeApp {
    pub backend: Backend,
    pub sessions: Vec<PublicSession>,
    pub open: Vec<Entity<SessionView>>,
    /// Session ids of `open` views, kept in parallel — reading view
    /// entities in `open_session` would reenter a SessionView that is
    /// mid-update (e.g. `request_fork` → `open_session` → panic).
    pub open_ids: Vec<String>,
    pub selected: usize,
    pub new_session_open: bool,
    pub new_cwd: String,
    pub new_focus: FocusHandle,
    /// Session id whose rail menu is open.
    pub rail_menu: Option<String>,
    /// Session id being renamed inline in the rail.
    pub renaming: Option<String>,
    pub rename_text: String,
    pub rename_focus: FocusHandle,
    pub status: String,
    pub spawning: bool,
    /// Rail includes archived sessions (and offers unarchive) when set.
    pub show_archived: bool,
    /// `include_archived` filter of the catalog request currently in
    /// flight — also the re-entrancy guard. A toggle during the flight
    /// re-runs the refresh once it lands.
    catalog_inflight: Option<bool>,
    /// A refresh requested while `catalog_inflight` was occupied —
    /// replayed when that request lands (e.g. post-install retry while
    /// the startup request is still running).
    catalog_retry_pending: bool,
    /// Directory paths bound to remote projects (`projectLinks/list`) —
    /// rail rows inside one get a link marker.
    pub linked_dirs: std::collections::HashSet<String>,
    /// Bumped by in-app link mutations (session sheet's `mark_linked`).
    /// A catalog refresh only applies its fetched `linked_dirs` when the
    /// gen it started under is still current — otherwise its older
    /// snapshot would erase the mutation's marker.
    pub linked_gen: u64,
    /// Managed `vibe-app-server` state — probed at boot and after
    /// install/update ops.
    pub vibe_dist: crate::vibe_dist::VibeDist,
    /// Serializes dist ops (probe/install/update) — one at a time.
    dist_inflight: bool,
    /// Window focus — notifications only fire for sessions you're not
    /// looking at (background tab, or window unfocused entirely).
    pub window_active: bool,
    /// Bumped when a relocate updates a rail row's cwd locally. A
    /// refresh that started before the bump fetched the old cwd — it
    /// re-fires after applying so the stale row can't stick.
    pub cwd_gen: u64,
}

enum AttachKind {
    Resume(String),
    Continue,
    Start { cwd: Option<String> },
}

/// Catalog-level session op issued from the rail menu. Runs on a fresh
/// server connection (these are host-level methods, not session-bound).
#[derive(Clone)]
pub enum RailOp {
    Pin(bool),
    Archive(bool),
    Rename(String),
    Delete,
}

impl VibeApp {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let new_focus = cx.focus_handle();
        let mut app = Self {
            backend,
            sessions: Vec::new(),
            open: Vec::new(),
            open_ids: Vec::new(),
            selected: 0,
            new_session_open: false,
            new_cwd: dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .display()
                .to_string(),
            new_focus,
            rail_menu: None,
            renaming: None,
            rename_text: String::new(),
            rename_focus: cx.focus_handle(),
            status: "starting…".into(),
            spawning: false,
            show_archived: false,
            catalog_inflight: None,
            catalog_retry_pending: false,
            linked_dirs: std::collections::HashSet::new(),
            linked_gen: 0,
            vibe_dist: crate::vibe_dist::VibeDist::Missing,
            dist_inflight: false,
            window_active: false,
            cwd_gen: 0,
        };
        app.refresh_sessions(cx);
        // The dist row manages the REAL server — meaningless under the
        // fixture backend, which never resolves it.
        if backend == Backend::Server {
            app.probe_dist(true, cx);
        }
        app
    }

    fn program(&self) -> PathBuf {
        match self.backend {
            Backend::Server => host::server_binary(),
            Backend::Fixture => host::fixture_binary(),
        }
    }

    fn client_info() -> ClientInfo {
        ClientInfo {
            name: "vibe-desktop".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Vibe Desktop".into()),
            entrypoint: "desktop".into(),
            terminal_emulator: "unknown".into(),
        }
    }

    fn capabilities() -> ClientCapabilities {
        ClientCapabilities {
            callback_kinds: vec!["approval".into(), "user_input".into()],
            client_tools: vec![],
            disabled_notifications: vec![],
        }
    }

    /// Re-probe the managed vibe distribution (`check_latest` adds the
    /// PyPI version check — cheap but network-bound, so only callers that
    /// want the update badge set it).
    pub fn probe_dist(&mut self, check_latest: bool, cx: &mut Context<Self>) {
        if self.dist_inflight {
            return;
        }
        self.dist_inflight = true;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // probe drives tokio child processes + reqwest — it must run
            // on the host runtime, not gpui's executor (tokio::process
            // panics without a reactor).
            let state = host::runtime()
                .spawn(async move { crate::vibe_dist::probe(check_latest).await })
                .await
                .unwrap_or_else(|_| crate::vibe_dist::VibeDist::Failed {
                    upgrade: false,
                    error: "dist probe failed".into(),
                });
            let _ = this.update(cx, |app, cx| {
                app.dist_inflight = false;
                // An op outcome (in-flight or failed) owns the label —
                // don't clobber it with a probe that started earlier.
                if !matches!(
                    app.vibe_dist,
                    crate::vibe_dist::VibeDist::Installing
                        | crate::vibe_dist::VibeDist::Updating
                        | crate::vibe_dist::VibeDist::Failed { .. }
                ) {
                    app.vibe_dist = state;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Managed install or upgrade — uv on the host runtime; the dist label
    /// shows progress until the post-probe lands.
    pub fn vibe_dist_op(&mut self, upgrade: bool, cx: &mut Context<Self>) {
        if self.dist_inflight {
            return;
        }
        self.dist_inflight = true;
        self.vibe_dist = if upgrade {
            crate::vibe_dist::VibeDist::Updating
        } else {
            crate::vibe_dist::VibeDist::Installing
        };
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let out = host::runtime()
                .spawn(async move {
                    let Some(uv) = crate::vibe_dist::uv_binary() else {
                        return Err("uv not found — install it from astral.sh/uv".to_string());
                    };
                    let Some(root) = crate::vibe_dist::dist_root() else {
                        return Err("no per-user data directory resolved".to_string());
                    };
                    std::fs::create_dir_all(&root).map_err(|e| format!("{e}"))?;
                    if upgrade {
                        crate::vibe_dist::upgrade(&uv, &root, None).await
                    } else {
                        crate::vibe_dist::install(&uv, &root, None).await
                    }
                })
                .await
                .unwrap_or_else(|_| Err("dist task failed".into()));
            let _ = this.update(cx, |app, cx| {
                app.dist_inflight = false;
                match out {
                    Ok(version) => {
                        app.vibe_dist = crate::vibe_dist::VibeDist::Installed {
                            version,
                            managed: true,
                        };
                        cx.notify();
                        if !upgrade {
                            // The new binary may finally resolve
                            // `server_binary` — retry the catalog so
                            // existing sessions appear.
                            app.refresh_sessions(cx);
                        }
                        // Fresh probe so a post-install update badge
                        // appears. Only on success — a Failed state must
                        // keep its error + retry action.
                        app.probe_dist(true, cx);
                    }
                    Err(e) => {
                        app.vibe_dist = crate::vibe_dist::VibeDist::Failed {
                            upgrade,
                            error: e,
                        };
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// OS notification for a session event the user probably missed —
    /// suppressed while the window is focused on that session, and by
    /// `VIBE_DESKTOP_NO_NOTIFY` (CI/headless runs). Caller supplies the
    /// id + title: this runs inside another view's update, so it must
    /// not read any session entity (`open_ids` is the reentrancy-safe
    /// cache for exactly this).
    pub fn notify_attention(&mut self, sid: &str, title: &str, what: &str) {
        if std::env::var_os("VIBE_DESKTOP_NO_NOTIFY").is_some() {
            return;
        }
        let on_selected = self.open_ids.get(self.selected).map(String::as_str) == Some(sid);
        if self.window_active && on_selected {
            return;
        }
        let _ = notify_rust::Notification::new()
            .appname("vibe desktop")
            .summary(title)
            .body(what)
            .show();
    }

    /// Catalog connection (never attaches a session): serves `session/list`.
    /// Re-entrant-safe: one catalog request at a time; the completion
    /// re-fires when `show_archived` changed under it.
    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        if self.catalog_inflight.is_some() {
            // Don't drop the request — the in-flight call may carry a
            // stale server path (e.g. pre-install), so queue a replay.
            self.catalog_retry_pending = true;
            return;
        }
        let include_archived = self.show_archived;
        let linked_gen_at_start = self.linked_gen;
        let cwd_gen_at_start = self.cwd_gen;
        self.catalog_inflight = Some(include_archived);
        let program = self.program();
        self.status = format!("connecting to {}…", program.display());
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // Process spawn needs a tokio reactor — run the whole call on RT.
            let out = host::runtime()
                .spawn(async move {
                    let conn = host::spawn_connection(&program, None).await?;
                    conn.initialize(Self::client_info(), Self::capabilities())
                        .await?;
                    // Follow the cursor to the end — session catalogs are
                    // unbounded. The seen-set guards a server that repeats a
                    // cursor (which would otherwise loop forever).
                    let mut items = Vec::new();
                    let mut cursor: Option<String> = None;
                    let mut seen = std::collections::HashSet::new();
                    loop {
                        let page = conn
                            .session_list(SessionListParams {
                                cursor: cursor.take(),
                                include_archived,
                                ..Default::default()
                            })
                            .await?;
                        items.extend(page.items);
                        match page.next_cursor {
                            Some(c) if seen.insert(c.clone()) => cursor = Some(c),
                            _ => break,
                        }
                    }
                    // Session-less: every local dir bound to a remote
                    // project (the rail's link markers).
                    let linked = conn
                        .project_links_list()
                        .await
                        .map(|r| {
                            r.projects
                                .into_iter()
                                .flat_map(|p| p.local_links)
                                .map(|l| l.directory_path)
                                .collect::<std::collections::HashSet<_>>()
                        })
                        .unwrap_or_default();
                    Ok((items, linked))
                })
                .await
                .map_err(|_| vibe_protocol::client::ClientError::Closed)
                .and_then(|r| r);
            let _ = this.update(cx, |app, cx| {
                app.catalog_inflight = None;
                match out {
                    Ok((items, linked)) => {
                        app.sessions = items;
                        // The fetched links snapshot predates any mutation
                        // made while the refresh was in flight — keep the
                        // newer optimistic set in that case.
                        if app.linked_gen == linked_gen_at_start {
                            app.linked_dirs = linked;
                        }
                        app.sort_sessions();
                        app.status = format!("{} session(s)", app.sessions.len());
                    }
                    Err(e) => {
                        app.status = format!("catalog: {e}");
                    }
                }
                // The archived toggle flipped while this request was in
                // flight — reload with the filter the user now expects.
                // A retry queued mid-flight (post-install catalog
                // recovery) replays too.
                if app.show_archived != include_archived || app.catalog_retry_pending {
                    app.catalog_retry_pending = false;
                    app.refresh_sessions(cx);
                } else if app.cwd_gen != cwd_gen_at_start {
                    // A relocate landed mid-refresh — the fetched rows
                    // still carry the old cwd. Refetch so the linked
                    // marker reflects the new checkout.
                    app.refresh_sessions(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sort_sessions(&mut self) {
        self.sessions.sort_by(|a, b| {
            b.pinned_at
                .is_some()
                .cmp(&a.pinned_at.is_some())
                .then(b.updated_at.cmp(&a.updated_at))
        });
    }

    pub fn open_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(idx) = self.open_ids.iter().position(|s| s == session_id) {
            self.selected = idx;
            cx.notify();
            return;
        }
        self.attach(AttachKind::Resume(session_id.to_string()), cx);
    }

    /// A view's session id changed (compact/fork handoff replaces the session).
    pub fn sync_session_id(&mut self, old: &str, new: &str) {
        if let Some(idx) = self.open_ids.iter().position(|s| s == old) {
            self.open_ids[idx] = new.to_string();
        }
    }

    pub fn continue_session(&mut self, cx: &mut Context<Self>) {
        self.attach(AttachKind::Continue, cx);
    }

    pub fn start_new_session(&mut self, cx: &mut Context<Self>) {
        let cwd = self.new_cwd.trim().to_string();
        self.new_session_open = false;
        self.attach(
            AttachKind::Start {
                cwd: (!cwd.is_empty()).then_some(cwd),
            },
            cx,
        );
    }

    /// Run a catalog session op (pin/archive/rename/delete) on a throwaway
    /// connection, then reflect it in the rail list and any open view.
    pub fn rail_op(&mut self, session_id: &str, op: RailOp, cx: &mut Context<Self>) {
        self.rail_menu = None;
        self.renaming = None;
        let session_id = session_id.to_string();
        let op_sid = session_id.clone();
        let program = self.program();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result: ClientResult<RailOp> = host::runtime()
                .spawn(async move {
                    let conn = host::spawn_connection(&program, None).await?;
                    conn.initialize(Self::client_info(), Self::capabilities())
                        .await?;
                    match &op {
                        RailOp::Pin(p) => conn.session_pin(&op_sid, *p).await?,
                        RailOp::Archive(a) => conn.session_archive(&op_sid, *a).await?,
                        RailOp::Rename(t) => conn.session_rename(&op_sid, t).await?,
                        RailOp::Delete => {
                            conn.request(
                                "session/delete",
                                serde_json::json!({"sessionId": op_sid}),
                            )
                            .await?;
                        }
                    }
                    Ok(op)
                })
                .await
                .map_err(|_| vibe_protocol::client::ClientError::Closed)
                .and_then(|r| r);
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok(op) => app.apply_rail_op(&session_id, op, cx),
                    Err(e) => app.status = format!("{e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Toggle archived rows in the rail and reload the catalog.
    pub fn toggle_archived(&mut self, cx: &mut Context<Self>) {
        self.show_archived = !self.show_archived;
        self.refresh_sessions(cx);
    }

    /// Reflect a completed rail op in the catalog row and any open view's
    /// projection — the wire ops don't echo back over other connections.
    fn apply_rail_op(&mut self, session_id: &str, op: RailOp, cx: &mut Context<Self>) {
        let stamp = Some(1_700_000_000);
        // Archiving hides the row only while the rail shows unarchived
        // sessions; with archived rows visible it stays, marked archived.
        let hide_row = matches!(op, RailOp::Delete)
            || (matches!(op, RailOp::Archive(true)) && !self.show_archived);
        if hide_row {
            self.sessions.retain(|s| s.id != session_id);
        } else if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            match &op {
                RailOp::Pin(p) => s.pinned_at = if *p { stamp } else { None },
                RailOp::Archive(a) => {
                    s.archived_at = if *a { stamp } else { None };
                    // Turn status is orthogonal: a live Running/Blocked
                    // keeps its activeTurnId (steer/interrupt still work);
                    // only Idle↔Archived translate.
                    s.status = match (&s.status, a) {
                        (PublicSessionStatus::Archived, false) => PublicSessionStatus::Idle,
                        (PublicSessionStatus::Idle, true) => PublicSessionStatus::Archived,
                        (other, _) => other.clone(),
                    };
                }
                RailOp::Rename(t) => s.title = Some(t.clone()),
                RailOp::Delete => {}
            }
        }
        // Mirror onto an attached view's session so the header stays true.
        for (i, id) in self.open_ids.iter().enumerate() {
            if id == session_id {
                if let Some(view) = self.open.get(i) {
                    view.update(cx, |v, cx| {
                        let s = &mut v.projection.state.session;
                        match &op {
                            RailOp::Pin(p) => s.pinned_at = if *p { stamp } else { None },
                            RailOp::Archive(a) => {
                                s.archived_at = if *a { stamp } else { None };
                                s.status = match (&s.status, a) {
                                    (PublicSessionStatus::Archived, false) => {
                                        PublicSessionStatus::Idle
                                    }
                                    (PublicSessionStatus::Idle, true) => {
                                        PublicSessionStatus::Archived
                                    }
                                    (other, _) => other.clone(),
                                };
                            }
                            RailOp::Rename(t) => s.title = Some(t.clone()),
                            RailOp::Delete => {}
                        }
                        cx.notify();
                    });
                }
            }
        }
        self.sort_sessions();
    }

    /// Begin an inline rename in the rail for `session_id`.
    pub fn begin_rename(&mut self, session_id: &str, current: &str, cx: &mut Context<Self>) {
        self.rail_menu = None;
        self.renaming = Some(session_id.to_string());
        self.rename_text = current.to_string();
        cx.notify();
    }

    pub fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.renaming.take() else {
            return;
        };
        let title = self.rename_text.trim().to_string();
        self.rename_text.clear();
        if !title.is_empty() {
            self.rail_op(&sid, RailOp::Rename(title), cx);
        }
        cx.notify();
    }

    pub fn on_rename_key(&mut self, e: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        match e.keystroke.key.as_str() {
            "backspace" => {
                self.rename_text.pop();
            }
            "enter" => self.commit_rename(cx),
            "escape" => {
                self.renaming = None;
                self.rename_text.clear();
            }
            _ => {
                if let Some(ch) = &e.keystroke.key_char {
                    self.rename_text.push_str(ch);
                }
            }
        }
        cx.notify();
    }

    /// Spawn a fresh app-server process, handshake, attach, make a view.
    /// One process per attached session (ADR-0009).
    fn attach(&mut self, kind: AttachKind, cx: &mut Context<Self>) {
        if self.spawning {
            return;
        }
        self.spawning = true;
        self.status = "attaching…".into();
        cx.notify();
        let program = self.program();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // Spawn + handshake on the tokio runtime so the connection's
            // reader/writer tasks land on its threads; after that the
            // connection is driven fine from the gpui executor.
            let spawned = host::runtime()
                .spawn(async move {
                    let conn = host::spawn_connection(&program, None).await?;
                    conn.initialize(Self::client_info(), Self::capabilities())
                        .await?;
                    Ok::<_, vibe_protocol::client::ClientError>(conn)
                })
                .await;
            let result: ClientResult<(Connection, PublicSessionState)> = async {
                let conn = spawned.map_err(|_| vibe_protocol::client::ClientError::Closed)??;
                let state = match kind {
                    AttachKind::Resume(id) => {
                        conn.session_resume(&id, AgentConfig::default(), 200)
                            .await?
                    }
                    AttachKind::Continue => {
                        conn.session_continue(AgentConfig::default(), 200).await?
                    }
                    AttachKind::Start { cwd } => {
                        conn.session_start(SessionStartParams {
                            agent_config: AgentConfig {
                                cwd,
                                ..Default::default()
                            },
                            history_limit: 200,
                            idempotency_key: None,
                            kind: None,
                        })
                        .await?
                    }
                };
                Ok((conn, state))
            }
            .await;
            let _ = this.update(cx, |app, cx| {
                app.spawning = false;
                match result {
                    Ok((conn, state)) => {
                        let session_id = state.session.id.clone();
                        let title = state.session.display_title();
                        let summary = state.session.clone();
                        let view =
                            cx.new(|cx| SessionView::attached(conn, state, cx.focus_handle(), cx));
                        let app_weak = cx.entity().downgrade();
                        view.update(cx, |v, cx| {
                            v.app = Some(app_weak);
                            v.check_trust(cx);
                        });
                        app.open.push(view);
                        app.open_ids.push(session_id.clone());
                        app.selected = app.open.len() - 1;
                        app.status = format!("attached {title}");
                        if !app.sessions.iter().any(|s| s.id == session_id) {
                            app.sessions.push(summary);
                            app.sort_sessions();
                        }
                    }
                    Err(e) => {
                        app.status = format!("attach failed: {e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn selected_view(&self) -> Option<&Entity<SessionView>> {
        self.open.get(self.selected)
    }
}
