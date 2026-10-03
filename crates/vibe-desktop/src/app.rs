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
    pub selected: usize,
    pub new_session_open: bool,
    pub new_cwd: String,
    pub new_focus: FocusHandle,
    pub status: String,
    pub spawning: bool,
    catalog_spawned: bool,
}

enum AttachKind {
    Resume(String),
    Continue,
    Start { cwd: Option<String> },
}

impl VibeApp {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let new_focus = cx.focus_handle();
        let mut app = Self {
            backend,
            sessions: Vec::new(),
            open: Vec::new(),
            selected: 0,
            new_session_open: false,
            new_cwd: std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            new_focus,
            status: "starting…".into(),
            spawning: false,
            catalog_spawned: false,
        };
        app.refresh_sessions(cx);
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

    /// Catalog connection (never attaches a session): serves `session/list`.
    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        if self.catalog_spawned {
            return;
        }
        self.catalog_spawned = true;
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
                    conn.session_list(SessionListParams {
                        include_archived: false,
                        ..Default::default()
                    })
                    .await
                })
                .await
                .map_err(|_| vibe_protocol::client::ClientError::Closed)
                .and_then(|r| r);
            let _ = this.update(cx, |app, cx| {
                match out {
                    Ok(list) => {
                        app.sessions = list.items;
                        app.sort_sessions();
                        app.status = format!("{} session(s)", app.sessions.len());
                    }
                    Err(e) => {
                        app.status = format!("catalog: {e}");
                    }
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
        if let Some(idx) = self
            .open
            .iter()
            .position(|s| s.read(cx).session_id() == session_id)
        {
            self.selected = idx;
            cx.notify();
            return;
        }
        self.attach(AttachKind::Resume(session_id.to_string()), cx);
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
                        let view =
                            cx.new(|cx| SessionView::attached(conn, state, cx.focus_handle(), cx));
                        app.open.push(view);
                        app.selected = app.open.len() - 1;
                        app.status = format!("attached {title}");
                        if !app.sessions.iter().any(|s| s.id == session_id) {
                            app.sessions
                                .push(app.open[app.selected].read(cx).session().clone());
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
