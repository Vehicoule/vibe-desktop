//! Process host: owns a tokio runtime for `vibe-app-server` child processes.
//! gpui runs its own executor; protocol Connections are created on the tokio
//! runtime (which spawns their reader/writer tasks) and then driven from gpui
//! executor futures — the wire channels are executor-agnostic.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use vibe_protocol::client::{ClientResult, Connection};

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Resolve the `vibe-app-server` binary:
/// `VIBE_APP_SERVER` env → managed install → `~/.local/bin` → PATH.
pub fn server_binary() -> PathBuf {
    if let Ok(p) = std::env::var("VIBE_APP_SERVER") {
        return PathBuf::from(p);
    }
    if let Some(root) = crate::vibe_dist::dist_root() {
        let managed = crate::vibe_dist::managed_binary(&root);
        if managed.exists() {
            return managed;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = Path::new(&home).join(".local/bin/vibe-app-server");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("vibe-app-server")
}

/// Resolve the dev fixture binary: `VIBE_FIXTURE` env → sibling cargo target.
pub fn fixture_binary() -> PathBuf {
    if let Ok(p) = std::env::var("VIBE_FIXTURE") {
        return PathBuf::from(p);
    }
    for candidate in [
        "target/debug/vibe-fixture",
        "../target/debug/vibe-fixture",
        "vibe-fixture",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("vibe-fixture")
}

pub async fn spawn_connection(program: &Path, cwd: Option<&Path>) -> ClientResult<Connection> {
    Connection::spawn_with_args(program, &[], cwd).await
}
