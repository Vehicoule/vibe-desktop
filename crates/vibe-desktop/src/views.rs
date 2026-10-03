//! Render code for the app: rail, session timeline, composer, callback cards,
//! status bar. Kept as `impl` blocks on the entities + small helpers.

use gpui::{
    div, hsla, prelude::FluentBuilder as _, px, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
};
use vibe_protocol::models::*;

use crate::app::{RailOp, VibeApp};
use crate::session::{CallbackAnswer, SessionView};
use crate::theme::{self, c};

fn text(s: impl Into<String>, size: f32, color: gpui::Hsla) -> gpui::Div {
    div().text_color(color).text_size(px(size)).child(s.into())
}

fn badge(label: &str, color: u32) -> gpui::Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .bg(c(color))
        .text_color(gpui::white())
        .text_size(px(10.0))
        .child(label.to_string())
}

fn pixel_mark() -> gpui::Div {
    // Mistral pixel-mosaic mark: a small block grid.
    let cells: [[u32; 3]; 3] = [
        [theme::SUNSET_DEEP, theme::SUNSET, theme::AMBER],
        [theme::SUNSET, theme::AMBER, 0xf2b01e],
        [theme::AMBER, 0xf2b01e, 0xf7d154],
    ];
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .children(cells.iter().map(|row| {
            div()
                .flex()
                .gap(px(1.0))
                .children(row.iter().map(|&col| theme::pixel(7.0, col)))
        }))
}

fn ghost_button(id: &'static str, label: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(11.0))
        .text_color(c(theme::INK_SOFT))
        .hover(|s| s.bg(c(theme::IVORY_DEEP)).text_color(c(theme::INK)))
        .child(label.to_string())
}

fn accent_button(id: &'static str, label: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(c(theme::SUNSET))
        .text_color(gpui::white())
        .text_size(px(12.0))
        .hover(|s| s.bg(c(theme::SUNSET_DEEP)))
        .child(label.to_string())
}

fn approval_button(
    cb_id: &str,
    choice: &str,
    label: &str,
    accent: bool,
    cx: &mut Context<SessionView>,
) -> gpui::Stateful<gpui::Div> {
    let cb = cb_id.to_string();
    let decision = choice.to_string();
    div()
        .id(gpui::ElementId::Name(
            format!("choice-{cb}-{choice}").into(),
        ))
        .px_3()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .text_size(px(11.5))
        .when(accent, |d| d.bg(c(theme::SUNSET)).text_color(gpui::white()))
        .when(!accent, |d| {
            d.bg(c(theme::IVORY_DEEP)).text_color(c(theme::INK))
        })
        .on_click(cx.listener(move |v, _e, _w, cx| {
            v.answer_callback(
                &cb,
                CallbackAnswer::Approval(ApprovalDecision::of(&decision)),
                cx,
            );
        }))
        .child(label.to_string())
}

impl Render for VibeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rail = self.render_rail(cx);
        let main = self.render_main(cx);
        div()
            .flex()
            .size_full()
            .bg(c(theme::IVORY))
            .text_color(c(theme::INK))
            .child(rail)
            .child(main)
    }
}

impl VibeApp {
    fn render_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let sessions = self.sessions.clone();
        let open_ids: Vec<String> = self
            .open
            .iter()
            .map(|s| s.read(cx).session_id().to_string())
            .collect();

        let mut list = div()
            .id("session-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .gap_1()
            .p_2();
        for (i, s) in sessions.iter().enumerate() {
            let is_open = open_ids.contains(&s.id);
            let status_color = match s.status.label() {
                "running" => theme::SUNSET,
                "blocked" => theme::AMBER,
                "failed" => theme::RED,
                _ => theme::EDGE,
            };
            let mut row = div().flex().flex_col();
            let sid2 = s.id.clone();
            let mut item = div()
                .id(gpui::ElementId::Name(format!("session-{i}").into()))
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .bg(if is_open {
                    c(theme::CARD)
                } else {
                    gpui::transparent_white()
                })
                .hover(|s| s.bg(c(theme::CARD)))
                .on_click(cx.listener(move |app, _e, _w, cx| app.open_session(&sid2, cx)));
            let pinned = s.pinned_at.is_some();
            if self.renaming.as_deref() == Some(s.id.as_str()) {
                // Inline rename field replaces the title row.
                let field = div()
                    .track_focus(&self.rename_focus)
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(c(theme::IVORY))
                    .border_1()
                    .border_color(c(theme::SUNSET))
                    .text_size(px(12.0))
                    .on_key_down(cx.listener(|app, e: &gpui::KeyDownEvent, _w, cx| {
                        app.on_rename_key(e, cx);
                    }))
                    .child(if self.rename_text.is_empty() {
                        text("title…", 12.0, c(theme::INK_FAINT))
                    } else {
                        text(format!("{}▏", self.rename_text), 12.0, c(theme::INK))
                    });
                item = item.child(field);
            } else {
                let menu_sid = s.id.clone();
                item = item.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(6.0)).h(px(6.0)).rounded_sm().bg(c(status_color)))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(12.0))
                                .child(s.display_title()),
                        )
                        .when(pinned, |d| d.child(text("·", 12.0, c(theme::AMBER))))
                        .child(
                            div()
                                .id(gpui::ElementId::Name(format!("menu-{i}").into()))
                                .px_1()
                                .rounded_sm()
                                .text_color(c(theme::INK_FAINT))
                                .hover(|s| s.bg(c(theme::IVORY_DEEP)).text_color(c(theme::INK)))
                                .on_click(cx.listener(move |app, _e, _w, cx| {
                                    app.rail_menu =
                                        if app.rail_menu.as_deref() == Some(menu_sid.as_str()) {
                                            None
                                        } else {
                                            Some(menu_sid.clone())
                                        };
                                    cx.stop_propagation();
                                    cx.notify();
                                }))
                                .child(text("⋯", 12.0, c(theme::INK_FAINT))),
                        ),
                );
            }
            row = row.child(item);
            if self.rail_menu.as_deref() == Some(s.id.as_str()) {
                row = row.child(self.render_rail_menu(s, cx));
            }
            list = list.child(row);
        }

        let continue_btn = ghost_button("continue", "↩ continue last")
            .on_click(cx.listener(|app, _e, _w, cx| app.continue_session(cx)));
        let new_btn =
            accent_button("new-session", "+ new").on_click(cx.listener(|app, _e, window, cx| {
                app.new_session_open = !app.new_session_open;
                window.focus(&app.new_focus);
                cx.notify();
            }));

        div()
            .w(px(250.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(c(theme::PAPER))
            .border_r_1()
            .border_color(c(theme::EDGE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(c(theme::EDGE))
                    .child(pixel_mark())
                    .child(text("vibe", 15.0, c(theme::INK)))
                    .child(div().flex_1())
                    .child(new_btn),
            )
            .child(div().px_2().py_1().child(continue_btn))
            .child(
                div()
                    .px_2()
                    .flex()
                    .items_center()
                    .child(
                        ghost_button("refresh", "⟳ refresh")
                            .on_click(cx.listener(|app, _e, _w, cx| app.refresh_sessions(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        ghost_button(
                            "toggle-archived",
                            if self.show_archived {
                                "☑ archived"
                            } else {
                                "☐ archived"
                            },
                        )
                        .on_click(cx.listener(|app, _e, _w, cx| app.toggle_archived(cx))),
                    ),
            )
            .child(list)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(c(theme::EDGE))
                    .child(text(self.status.clone(), 10.5, c(theme::INK_FAINT))),
            )
    }

    /// Action rows under a rail item: pin/unpin, rename, archive, delete.
    fn render_rail_menu(&self, s: &PublicSession, cx: &mut Context<Self>) -> impl IntoElement {
        let sid = s.id.clone();
        let row = |label: &'static str, op: RailOp| {
            let sid = sid.clone();
            div()
                .id(gpui::ElementId::Name(format!("{label}-{sid}").into()))
                .px_3()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(c(theme::INK_SOFT))
                .hover(|d| d.bg(c(theme::IVORY_DEEP)).text_color(c(theme::INK)))
                .on_click(cx.listener(move |app, _e, _w, cx| {
                    app.rail_op(&sid, op.clone(), cx);
                }))
                .child(label)
        };
        let title = s.display_title();
        let sid_rename = s.id.clone();
        div()
            .ml_4()
            .flex()
            .flex_col()
            .gap_0p5()
            .py_1()
            .child(if s.pinned_at.is_some() {
                row("unpin", RailOp::Pin(false))
            } else {
                row("pin", RailOp::Pin(true))
            })
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("rename-{}", s.id).into()))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .text_color(c(theme::INK_SOFT))
                    .hover(|d| d.bg(c(theme::IVORY_DEEP)).text_color(c(theme::INK)))
                    .on_click(cx.listener(move |app, _e, window, cx| {
                        app.begin_rename(&sid_rename, &title, cx);
                        window.focus(&app.rename_focus);
                    }))
                    .child("rename…"),
            )
            .child(if s.archived_at.is_some() {
                row("unarchive", RailOp::Archive(false))
            } else {
                row("archive", RailOp::Archive(true))
            })
            .child(row("delete", RailOp::Delete))
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut main = div().flex_1().h_full().flex().flex_col().overflow_hidden();

        // Tab strip for open sessions.
        if !self.open.is_empty() {
            let mut tabs = div()
                .flex()
                .gap_1()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(c(theme::EDGE))
                .bg(c(theme::PAPER));
            for (i, s) in self.open.iter().enumerate() {
                let title = s.read(cx).session().display_title();
                let active = i == self.selected;
                tabs = tabs.child(
                    div()
                        .id(gpui::ElementId::Name(format!("tab-{i}").into()))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .bg(if active {
                            c(theme::SUNSET_WASH)
                        } else {
                            gpui::transparent_white()
                        })
                        .text_color(if active {
                            c(theme::SUNSET_DEEP)
                        } else {
                            c(theme::INK_SOFT)
                        })
                        .hover(|s| s.bg(c(theme::SUNSET_WASH)))
                        .on_click(cx.listener(move |app, _e, _w, cx| {
                            app.selected = i;
                            cx.notify();
                        }))
                        .child(title),
                );
            }
            main = main.child(tabs);
        }

        match self.selected_view() {
            Some(view) => {
                main = main.child(div().flex_1().overflow_hidden().child(view.clone()));
            }
            None => {
                main = main.child(
                    div().flex_1().flex().items_center().justify_center().child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_3()
                            .child(pixel_mark())
                            .child(text("vibe desktop", 18.0, c(theme::INK)))
                            .child(text(
                                "open a session on the left, or start a new one",
                                12.5,
                                c(theme::INK_FAINT),
                            )),
                    ),
                );
            }
        }

        if self.new_session_open {
            let field = div()
                .track_focus(&self.new_focus)
                .px_3()
                .py_2()
                .rounded_md()
                .bg(c(theme::PAPER))
                .border_1()
                .border_color(c(theme::EDGE))
                .text_size(px(13.0))
                .min_w(px(320.0))
                .on_key_down(cx.listener(|app, e: &gpui::KeyDownEvent, _w, cx| {
                    app.on_new_field_key(e, cx);
                }))
                .child(if self.new_cwd.is_empty() {
                    text("/path/to/project", 13.0, c(theme::INK_FAINT))
                } else {
                    text(format!("{}▏", self.new_cwd), 13.0, c(theme::INK))
                });
            main = main.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(hsla(0., 0., 0., 0.3))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_5()
                            .rounded_lg()
                            .bg(c(theme::IVORY))
                            .border_1()
                            .border_color(c(theme::EDGE))
                            .shadow_lg()
                            .child(text("new session", 14.0, c(theme::INK)))
                            .child(text("working directory", 11.0, c(theme::INK_FAINT)))
                            .child(field)
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .justify_end()
                                    .child(ghost_button("cancel-new", "cancel").on_click(
                                        cx.listener(|app, _e, _w, cx| {
                                            app.new_session_open = false;
                                            cx.notify();
                                        }),
                                    ))
                                    .child(accent_button("start-new", "start").on_click(
                                        cx.listener(|app, _e, _w, cx| app.start_new_session(cx)),
                                    )),
                            ),
                    ),
            );
        }
        main
    }

    fn on_new_field_key(&mut self, e: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        match e.keystroke.key.as_str() {
            "backspace" => {
                self.new_cwd.pop();
            }
            "enter" => self.start_new_session(cx),
            "escape" => self.new_session_open = false,
            _ => {
                if let Some(ch) = &e.keystroke.key_char {
                    self.new_cwd.push_str(ch);
                }
            }
        }
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Session view
// ---------------------------------------------------------------------------

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocused {
            self.autofocused = true;
            window.focus(&self.composer_focus);
        }
        let session = self.projection.state.session.clone();
        let status_color = match session.status.label() {
            "running" => theme::SUNSET,
            "blocked" => theme::AMBER,
            "failed" => theme::RED,
            "idle" => theme::GREEN,
            _ => theme::BLUEGREY,
        };
        let mut entries = div()
            .id("entries")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .gap_2()
            .px_4()
            .py_3();
        if self.projection.state.history_before_cursor.is_some() {
            let label = if self.loading_earlier {
                "loading…"
            } else {
                "↑ load earlier messages"
            };
            entries = entries.child(
                div().flex().justify_center().child(
                    ghost_button("load-earlier", label)
                        .on_click(cx.listener(|v, _e, _w, cx| v.load_earlier(cx))),
                ),
            );
        }
        for entry in self.projection.history() {
            entries = entries.child(self.render_entry(entry, cx));
        }

        // Pre-create other-input focus handles for open user-input
        // callbacks so the &self card renderer can track them.
        let other_keys: Vec<(String, usize)> = self
            .projection
            .state
            .active_callbacks
            .iter()
            .filter_map(|e| match e {
                PublicHistoryEntry::Callback {
                    callback_id,
                    detail: CallbackDetail::UserInput(input),
                    ..
                } => input.request.as_ref().map(|req| {
                    req.questions
                        .iter()
                        .enumerate()
                        .filter(|(_, q)| !q.hide_other)
                        .map(|(qi, _)| (callback_id.clone(), qi))
                        .collect::<Vec<_>>()
                }),
                _ => None,
            })
            .flatten()
            .collect();
        for (cb, qi) in other_keys {
            self.other_focus_handle(&cb, qi, cx);
        }

        let open_callbacks = self.open_callbacks();
        let callback_area = if open_callbacks.is_empty() {
            None
        } else {
            let mut col = div().flex().flex_col().gap_2().px_4().pb_2();
            for cb in open_callbacks {
                col = col.child(self.render_callback_card(cb, cx));
            }
            Some(col)
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                // header
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(c(theme::EDGE))
                    .bg(c(theme::PAPER))
                    .child(badge(session.status.label(), status_color))
                    .child(text(session.display_title(), 13.0, c(theme::INK)))
                    .child(div().flex_1())
                    .child(text(
                        session
                            .model
                            .clone()
                            .unwrap_or_else(|| "default model".into()),
                        11.0,
                        c(theme::INK_FAINT),
                    ))
                    .child(
                        ghost_button("compact", "compact")
                            .on_click(cx.listener(|v, _e, _w, cx| v.compact(cx))),
                    )
                    .child(
                        ghost_button("fork", "fork").on_click(cx.listener(|v, _e, _w, cx| {
                            v.request_fork(cx);
                        })),
                    )
                    .child(
                        ghost_button("stop", "■").on_click(cx.listener(|v, _e, _w, cx| {
                            v.interrupt(cx);
                        })),
                    ),
            )
            .when_some(self.render_trust_banner(cx), |d, b| d.child(b))
            .child(entries)
            .when_some(callback_area, |d, area| d.child(area))
            .when_some(self.render_queue_strip(cx), |d, q| d.child(q))
            .when_some(self.render_rewind_sheet(cx), |d, s| d.child(s))
            .child(self.render_composer(cx))
            .child(self.render_status_bar(cx))
    }
}

impl SessionView {
    fn render_entry(&self, entry: &PublicHistoryEntry, cx: &mut Context<Self>) -> impl IntoElement {
        match entry {
            PublicHistoryEntry::Message { role, content, .. } => {
                let is_user = role == "user";
                let body = content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let rewind_entry = entry.clone();
                div()
                    .max_w(if is_user { px(560.0) } else { px(720.0) })
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .bg(if is_user {
                        c(theme::CARD)
                    } else {
                        c(theme::PAPER)
                    })
                    .border_1()
                    .border_color(c(theme::EDGE))
                    .when(is_user, |d| d.ml_auto().border_color(c(theme::SUNSET_WASH)))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(div().flex_1().child(text(body, 13.0, c(theme::INK))))
                            .when(is_user, |d| {
                                d.child(
                                    div()
                                        .id(gpui::ElementId::Name(
                                            format!("rw-{}", entry.id().unwrap_or("")).into(),
                                        ))
                                        .px_1p5()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_color(c(theme::INK_FAINT))
                                        .hover(|s| {
                                            s.bg(c(theme::SUNSET_WASH))
                                                .text_color(c(theme::SUNSET_DEEP))
                                        })
                                        .on_click(cx.listener(move |v, _e, _w, cx| {
                                            v.open_rewind(&rewind_entry, cx);
                                        }))
                                        .child(text("↺", 12.0, c(theme::INK_FAINT))),
                                )
                            }),
                    )
            }
            PublicHistoryEntry::Reasoning { text: t, base, .. } => {
                let in_progress = base.generation_status == "in_progress";
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(c(theme::IVORY_DEEP))
                    .border_l_2()
                    .border_color(c(theme::AMBER))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(text("thinking", 10.5, c(theme::AMBER)))
                            .when(in_progress, |d| d.child(badge("…", theme::AMBER))),
                    )
                    .when(!t.is_empty(), |d| {
                        d.child(text(t.clone(), 11.5, c(theme::INK_SOFT)))
                    })
            }
            PublicHistoryEntry::Effect {
                title,
                detail,
                state,
                ..
            } => self.render_effect(title, detail, state),
            PublicHistoryEntry::Callback { .. } => {
                // Open callbacks render in the card area; settled ones as a note.
                div()
            }
            PublicHistoryEntry::Checkpoint { kind, message, .. } => div()
                .px_3()
                .py_1p5()
                .rounded_md()
                .bg(c(theme::IVORY_DEEP))
                .child(text(
                    format!(
                        "checkpoint · {kind}{}",
                        message
                            .as_ref()
                            .map(|m| format!(" — {m}"))
                            .unwrap_or_default()
                    ),
                    11.0,
                    c(theme::INK_FAINT),
                )),
            PublicHistoryEntry::Notice { level, message, .. } => {
                let color = match level.as_str() {
                    "error" => theme::RED,
                    "warning" => theme::AMBER,
                    _ => theme::BLUEGREY,
                };
                div()
                    .px_3()
                    .py_1p5()
                    .child(text(format!("• {message}"), 11.0, c(color)))
            }
            PublicHistoryEntry::Unknown => div(),
        }
    }

    fn render_effect(&self, title: &str, detail: &EffectDetail, state: &EffectState) -> gpui::Div {
        let (label, color) = match state {
            EffectState::Pending => ("pending", theme::BLUEGREY),
            EffectState::Running { .. } => ("running", theme::SUNSET),
            EffectState::Blocked { .. } => ("approval", theme::AMBER),
            EffectState::Completed { .. } => ("done", theme::GREEN),
            EffectState::Failed { .. } => ("failed", theme::RED),
            EffectState::Cancelled { .. } => ("cancelled", theme::BLUEGREY),
            EffectState::Skipped { .. } => ("skipped", theme::BLUEGREY),
            EffectState::Unknown => ("?", theme::BLUEGREY),
        };
        let summary = detail
            .display
            .as_ref()
            .map(|d| d.summary.clone())
            .unwrap_or_else(|| title.to_string());
        let output = match state {
            EffectState::Running { output_text }
            | EffectState::Blocked { output_text, .. }
            | EffectState::Completed { output_text, .. }
            | EffectState::Cancelled { output_text, .. } => Some(output_text.clone()),
            EffectState::Failed {
                output_text, error, ..
            } => Some(format!("{output_text}\n{}", error.message)),
            _ => None,
        }
        .filter(|s| !s.is_empty());

        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(c(theme::PAPER))
            .border_1()
            .border_color(c(theme::EDGE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(badge(label, color))
                    .child(text(detail.kind.clone(), 10.5, c(theme::INK_FAINT)))
                    .child(text(summary, 12.0, c(theme::INK))),
            )
            .when_some(output, |d, out| {
                d.child(
                    div()
                        .mt_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(c(theme::IVORY_DEEP))
                        .child(text(out, 11.0, c(theme::INK_SOFT))),
                )
            })
    }

    fn render_callback_card(
        &self,
        entry: &PublicHistoryEntry,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let PublicHistoryEntry::Callback {
            callback_id,
            title,
            detail,
            ..
        } = entry
        else {
            return div();
        };
        let cb_id = callback_id.clone();
        match detail {
            CallbackDetail::Approval(approval) => {
                let summary = approval
                    .effect
                    .as_ref()
                    .and_then(|e| e.display.as_ref())
                    .map(|d| d.summary.clone())
                    .unwrap_or_else(|| title.clone());
                let perms = approval
                    .required_permissions
                    .iter()
                    .map(|p| p.label.clone())
                    .collect::<Vec<_>>()
                    .join("\n");

                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .bg(c(theme::SUNSET_WASH))
                    .border_1()
                    .border_color(c(theme::SUNSET))
                    .child(text(
                        format!("approval needed · {summary}"),
                        12.5,
                        c(theme::INK),
                    ))
                    .when_some(approval.reason.clone(), |d, r| {
                        d.child(text(r, 11.0, c(theme::INK_SOFT)))
                    })
                    .when(!perms.is_empty(), |d| {
                        d.child(text(perms, 11.0, c(theme::INK_SOFT)))
                    })
                    .child(div().flex().gap_2().flex_wrap().children(
                        approval.choices.iter().filter_map(|choice| {
                            let (label, accent) = match choice.as_str() {
                                "approve" => ("approve", true),
                                "approve_for_session" => ("approve for session", false),
                                "approve_permanently" => ("always allow", false),
                                "deny" => ("deny", false),
                                "cancel_turn" => ("cancel turn", false),
                                _ => return None,
                            };
                            Some(approval_button(&cb_id, choice, label, accent, cx))
                        }),
                    ))
            }
            CallbackDetail::UserInput(input) => {
                let mut col = div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .bg(c(theme::CARD))
                    .border_1()
                    .border_color(c(theme::AMBER))
                    .child(text("question", 12.5, c(theme::INK)));
                if let Some(req) = &input.request {
                    let complete = self.question_complete(&cb_id, req);
                    for (qi, q) in req.questions.iter().enumerate() {
                        let selected = self.selected_options(&cb_id, qi);
                        col = col
                            .child(text(q.question.clone(), 12.0, c(theme::INK)))
                            .child(div().flex().gap_2().flex_wrap().children(
                                q.options.iter().map(|opt| {
                                    let answer = opt.label.clone();
                                    let cb2 = cb_id.clone();
                                    let multi = q.multi_select;
                                    let chosen = selected.contains(&answer);
                                    div()
                                        .id(gpui::ElementId::Name(
                                            format!("opt-{}-{}-{}", cb2, qi, opt.label).into(),
                                        ))
                                        .px_3()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .text_size(px(11.5))
                                        .bg(if chosen {
                                            c(theme::SUNSET)
                                        } else {
                                            c(theme::IVORY_DEEP)
                                        })
                                        .text_color(if chosen {
                                            gpui::white()
                                        } else {
                                            c(theme::INK)
                                        })
                                        .hover(|s| s.bg(c(theme::SUNSET_WASH)))
                                        .on_click(cx.listener(move |v, _e, _w, cx| {
                                            v.select_question_option(&cb2, qi, &answer, multi, cx);
                                        }))
                                        .child(opt.label.clone())
                                }),
                            ));
                        // Free-text "other" answer unless the request hides it.
                        if !q.hide_other {
                            let key = format!("{cb_id}:{qi}");
                            let focus = self.other_focus.get(&key).cloned();
                            let current = self.other_text(&cb_id, qi).to_string();
                            if let Some(focus) = focus {
                                let cb3 = cb_id.clone();
                                let req2 = req.clone();
                                let shown = if current.is_empty() {
                                    text("other…", 11.5, c(theme::INK_FAINT))
                                } else {
                                    text(format!("{current}▏"), 11.5, c(theme::INK))
                                };
                                let input = div()
                                    .track_focus(&focus)
                                    .size_full()
                                    .on_key_down(cx.listener(
                                        move |v, e: &gpui::KeyDownEvent, _w, cx| {
                                            match e.keystroke.key.as_str() {
                                                "backspace" => {
                                                    v.edit_other(&cb3, qi, None, cx);
                                                }
                                                "enter" => {
                                                    if v.question_complete(&cb3, &req2) {
                                                        v.submit_question(&cb3, &req2, cx);
                                                    }
                                                }
                                                _ => {
                                                    if let Some(ch) = &e.keystroke.key_char {
                                                        v.edit_other(&cb3, qi, Some(ch), cx);
                                                    }
                                                }
                                            }
                                        },
                                    ))
                                    .child(shown);
                                col = col.child(
                                    div()
                                        .id(gpui::ElementId::Name(format!("other-{key}").into()))
                                        .cursor_text()
                                        .px_3()
                                        .py_1p5()
                                        .rounded_sm()
                                        .bg(c(theme::IVORY))
                                        .border_1()
                                        .border_color(c(theme::EDGE))
                                        .on_click(cx.listener(move |v, _e, window, _cx| {
                                            window.focus(&v.other_focus[&key]);
                                        }))
                                        .child(input),
                                );
                            }
                        }
                    }
                    let req2 = req.clone();
                    let cb3 = cb_id.clone();
                    col = col.child(
                        div()
                            .flex()
                            .gap_2()
                            .justify_end()
                            .child(ghost_button("cancel-q", "cancel").on_click(cx.listener(
                                move |v, _e, _w, cx| {
                                    v.answer_callback(
                                        &cb_id,
                                        CallbackAnswer::UserInput(UserQuestionResult {
                                            answers: vec![],
                                            cancelled: true,
                                        }),
                                        cx,
                                    );
                                },
                            )))
                            .child(
                                accent_button("submit-q", "submit")
                                    .when(!complete, |d| d.opacity(0.4))
                                    .on_click(cx.listener(move |v, _e, _w, cx| {
                                        if v.question_complete(&cb3, &req2) {
                                            v.submit_question(&cb3, &req2, cx);
                                        }
                                    })),
                            ),
                    );
                }
                col
            }
            CallbackDetail::Unknown => div(),
        }
    }

    /// Queued turns under the timeline: per-item remove + pause/resume.
    fn render_queue_strip(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let queue = &self.projection.state.turn_queue;
        if queue.items.is_empty() && !queue.paused {
            return None;
        }
        let mut strip = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(c(theme::EDGE))
            .bg(c(theme::IVORY_DEEP));
        strip = strip.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(text("queue", 10.5, c(theme::INK_FAINT)))
                .when(queue.paused, |d| {
                    d.child(badge("paused", theme::AMBER)).child(
                        ghost_button("resume-queue", "resume")
                            .on_click(cx.listener(|v, _e, _w, cx| v.queue_resume(cx))),
                    )
                }),
        );
        for item in &queue.items {
            let preview = item
                .entries
                .iter()
                .filter_map(|e| match e {
                    TurnInputEntry::User { content, .. } => content
                        .iter()
                        .filter_map(|b| match b {
                            SessionContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .next(),
                    _ => None,
                })
                .next()
                .unwrap_or_default();
            let item_id = item.id.clone();
            strip = strip.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(c(theme::CARD))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .child(preview),
                    )
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("qrm-{item_id}").into()))
                            .px_1p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(c(theme::INK_FAINT))
                            .hover(|s| s.bg(c(theme::RED)).text_color(gpui::white()))
                            .on_click(cx.listener(move |v, _e, _w, cx| {
                                v.queue_remove(&item_id, cx);
                            }))
                            .child(text("✕", 10.5, c(theme::INK_FAINT))),
                    ),
            );
        }
        Some(strip)
    }

    /// Workspace trust banner — shown once until decided or dismissed.
    fn render_trust_banner(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.trust_dismissed {
            return None;
        }
        let details = self.trust.as_ref()?;
        let files = details.detected_files.join(", ");
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .py_2()
                .bg(c(theme::SUNSET_WASH))
                .border_b_1()
                .border_color(c(theme::SUNSET))
                .child(div().flex_1().child(text(
                    format!("this workspace asks for trust · {files}"),
                    11.5,
                    c(theme::INK),
                )))
                .child(
                    accent_button("trust-cwd", "trust workspace").on_click(cx.listener(
                        |v, _e, _w, cx| {
                            v.trust_decision("trust_cwd", cx);
                        },
                    )),
                )
                .child(
                    ghost_button("trust-dismiss", "not now").on_click(cx.listener(
                        |v, _e, _w, cx| {
                            v.trust_dismissed = true;
                            cx.notify();
                        },
                    )),
                ),
        )
    }

    /// Rewind confirmation sheet pinned above the composer.
    fn render_rewind_sheet(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let dlg = self.rewind.as_ref()?;
        let mut sheet = div()
            .flex()
            .flex_col()
            .gap_2()
            .mx_4()
            .mb_2()
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(c(theme::CARD))
            .border_1()
            .border_color(c(theme::AMBER))
            .child(text("rewind to this message?", 12.5, c(theme::INK)))
            .child(text(dlg.preview.clone(), 11.0, c(theme::INK_SOFT)));
        match dlg.has_changes {
            None if dlg.read_error.is_none() => {
                sheet = sheet.child(text("checking file changes…", 11.0, c(theme::INK_FAINT)));
            }
            None => {}
            Some(true) => {
                sheet = sheet
                    .child(text(
                        format!("file changes will be lost ({})", dlg.paths.join(", ")),
                        11.0,
                        c(theme::RED),
                    ))
                    .child(
                        div()
                            .id("restore-files")
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|v, _e, _w, cx| {
                                if let Some(d) = v.rewind.as_mut() {
                                    d.restore_files = !d.restore_files;
                                }
                                cx.notify();
                            }))
                            .child(text(
                                if dlg.restore_files { "☑" } else { "☐" },
                                12.0,
                                c(theme::INK),
                            ))
                            .child(text("restore the files too", 11.5, c(theme::INK_SOFT))),
                    );
            }
            Some(false) => {}
        }
        if let Some(err) = &dlg.read_error {
            sheet = sheet.child(text(
                format!("file-change preview failed: {err}"),
                11.0,
                c(theme::RED),
            ));
        }
        let mut actions = div().flex().gap_2().justify_end().child(
            ghost_button("cancel-rewind", "cancel")
                .on_click(cx.listener(|v, _e, _w, cx| v.close_rewind(cx))),
        );
        // Rewind only once the preview resolves — truncating blind would
        // hide whether files changed (and a failed read means we can't
        // vouch for the path list either).
        if dlg.has_changes.is_some() {
            actions = actions
                .child(
                    ghost_button("rewind-here", "rewind here")
                        .on_click(cx.listener(|v, _e, _w, cx| v.apply_rewind(true, cx))),
                )
                .child(
                    accent_button("rewind-fork", "rewind into new session")
                        .on_click(cx.listener(|v, _e, _w, cx| v.apply_rewind(false, cx))),
                );
        }
        sheet = sheet.child(actions);
        Some(sheet)
    }

    fn render_composer(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.active_turn_id().is_some();
        let hint = if running {
            "steer the running turn…"
        } else {
            "message vibe — enter to send, ⇧enter to queue"
        };
        let content = if self.composer.is_empty() {
            text(hint, 13.0, c(theme::INK_FAINT))
        } else {
            text(format!("{}▏", self.composer), 13.0, c(theme::INK))
        };
        div()
            .id("composer-wrap")
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(c(theme::EDGE))
            .bg(c(theme::PAPER))
            .on_click(cx.listener(|v, _e, window, _cx| {
                window.focus(&v.composer_focus);
            }))
            .child(
                div()
                    .track_focus(&self.composer_focus)
                    .px_4()
                    .py_2p5()
                    .rounded_lg()
                    .bg(c(theme::IVORY))
                    .border_1()
                    .border_color(c(theme::EDGE))
                    .on_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, _w, cx| {
                        match e.keystroke.key.as_str() {
                            "backspace" => {
                                v.composer.pop();
                            }
                            "enter" => {
                                if e.keystroke.modifiers.shift {
                                    v.enqueue(cx);
                                } else {
                                    v.send_message(cx);
                                }
                            }
                            "escape" => v.interrupt(cx),
                            _ => {
                                if let Some(ch) = &e.keystroke.key_char {
                                    v.composer.push_str(ch);
                                }
                            }
                        }
                        cx.notify();
                    }))
                    .child(content),
            )
    }

    fn render_status_bar(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.projection.state;
        let queue = state.turn_queue.items.len();
        let mut bar = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_1p5()
            .bg(c(theme::IVORY_DEEP))
            .border_t_1()
            .border_color(c(theme::EDGE));
        if let Some(stats) = &self.projection.stats {
            bar = bar.child(text(
                format!(
                    "↑{} ↓{} cached {}",
                    stats.session_prompt_tokens,
                    stats.session_completion_tokens,
                    stats.session_cached_tokens
                ),
                10.5,
                c(theme::INK_FAINT),
            ));
        }
        if queue > 0 {
            bar = bar.child(text(format!("{queue} queued"), 10.5, c(theme::AMBER)));
        }
        if let Some(retry) = &state.retrying {
            bar = bar.child(text(
                format!("retrying: {}", retry.detail),
                10.5,
                c(theme::AMBER),
            ));
        }
        if let Some(err) = &self.error {
            bar = bar.child(text(format!("error: {err}"), 10.5, c(theme::RED)));
        }
        bar.child(div().flex_1()).child(text(
            format!(
                "{} proc · {}",
                self.conn
                    .child_pid()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into()),
                state.session.cwd.clone().unwrap_or_default()
            ),
            10.5,
            c(theme::INK_FAINT),
        ))
    }
}
