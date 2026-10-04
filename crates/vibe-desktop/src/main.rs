//! vibe-desktop — gpui client of `vibe-app-server`.
//!
//! Usage: `vibe-desktop` (real server) or `vibe-desktop --fixture` (scripted
//! fake server for development without credentials).

mod app;
mod host;
mod session;
mod theme;
mod vibe_dist;
mod views;
mod voice;

use app::{Backend, VibeApp};
use gpui::{px, size, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

fn main() {
    let backend = if std::env::args().any(|a| a == "--fixture") {
        Backend::Fixture
    } else {
        Backend::Server
    };

    Application::new().run(move |cx| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("vibe desktop".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    let app = VibeApp::new(backend, cx);
                    cx.observe_window_activation(window, |app: &mut VibeApp, window, cx| {
                        app.window_active = window.is_window_active();
                        cx.notify();
                    })
                    .detach();
                    app
                })
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}
