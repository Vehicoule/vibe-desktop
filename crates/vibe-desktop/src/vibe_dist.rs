//! Managed `vibe-app-server` distribution (DESIGN.md M5): the app owns a
//! private `uv tool install mistral-vibe` under its data dir so the server
//! can install and update independently of app releases.
//!
//! Resolution order for the server binary:
//! `VIBE_APP_SERVER` env → managed install → `~/.local/bin` → PATH.
//!
//! `uv` itself comes from `VIBE_DESKTOP_UV` env → PATH. The tool dir is
//! isolated (`UV_TOOL_DIR`/`UV_TOOL_BIN_DIR` under the dist root) so the
//! managed copy never collides with the user's own uv tools.

use std::path::{Path, PathBuf};

/// Distribution lifecycle — `Missing` prompts the managed-install CTA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VibeDist {
    /// No server binary resolved anywhere.
    Missing,
    /// uv isn't available to perform an install/upgrade.
    NoUv,
    Installed { version: String, managed: bool },
    /// Latest version on PyPI is newer than the installed one.
    UpdateAvailable { installed: String, latest: String },
    Installing,
    Updating,
    /// `upgrade` remembers which op failed so retry re-runs it, not the
    /// other one.
    Failed { upgrade: bool, error: String },
}

impl VibeDist {
    pub fn label(&self) -> String {
        match self {
            Self::Missing => "vibe: not installed".into(),
            Self::NoUv => "vibe: uv required".into(),
            Self::Installed { version, managed } => {
                format!("vibe {version}{}", if *managed { "" } else { " (system)" })
            }
            Self::UpdateAvailable { installed, latest } => {
                format!("vibe {installed} → {latest}")
            }
            Self::Installing => "vibe: installing…".into(),
            Self::Updating => "vibe: updating…".into(),
            Self::Failed { error, .. } => format!("vibe: {error}"),
        }
    }
}

/// Root of the managed distribution: `$XDG_DATA_HOME/vibe-desktop/vibe`,
/// `~/.local/share/vibe-desktop/vibe`, or the platform data dir
/// (`%LOCALAPPDATA%` on Windows, where HOME/XDG are unset);
/// `VIBE_DESKTOP_DIST` overrides (tests point it at a tempdir). `None`
/// when no per-user data dir resolves — deliberately never a shared
/// `/tmp` path, where another local user could plant a server binary
/// we'd then execute.
pub fn dist_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VIBE_DESKTOP_DIST") {
        return Some(PathBuf::from(p));
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return Some(Path::new(&x).join("vibe-desktop/vibe"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        return Some(Path::new(&h).join(".local/share/vibe-desktop/vibe"));
    }
    dirs::data_local_dir().map(|d| d.join("vibe-desktop").join("vibe"))
}

/// `vibe-app-server` inside the managed tool env.
/// uv lays tools out as `<UV_TOOL_DIR>/<name>/bin/<exe>` on Unix and
/// `<UV_TOOL_DIR>/<name>/Scripts/<exe>.exe` on Windows (venv layout).
pub fn managed_binary(root: &Path) -> PathBuf {
    managed_binary_on(root, cfg!(windows))
}

fn managed_binary_on(root: &Path, windows: bool) -> PathBuf {
    if windows {
        root.join("tool/mistral-vibe/Scripts/vibe-app-server.exe")
    } else {
        root.join("tool/mistral-vibe/bin/vibe-app-server")
    }
}

/// uv binary: `VIBE_DESKTOP_UV` env → PATH. `None` when uv is absent.
pub fn uv_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VIBE_DESKTOP_UV") {
        return Some(PathBuf::from(p));
    }
    which("uv")
}

/// PATH search. Anything `server_binary` may spawn must come through
/// this resolver — the returned path carries its extension (CreateProcess
/// only auto-appends .exe, and .cmd/.bat need it for std's cmd /c
/// wrapping).
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    which_in(name, std::env::split_paths(&path))
}

/// Split out from `which` so tests inject PATH entries without mutating
/// env (parallel tests share it).
fn which_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in dirs {
        for n in exe_candidates(name, cfg!(windows)) {
            let p = dir.join(n);
            if runnable(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Spawnability gate for `which` candidates. Unix requires an execute
/// bit: a resolved path bypasses PATH search at spawn time, so letting
/// a non-executable file through would hard-fail where a bare name
/// would have execvp-skipped to a working later entry.
#[cfg(unix)]
fn runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// No execute bit on Windows — the extension is the contract.
#[cfg(not(unix))]
fn runnable(p: &Path) -> bool {
    p.is_file()
}

/// Windows executables carry an extension; try the common script/exe
/// forms instead of PATHEXT parsing (covers .cmd shims too).
fn exe_candidates(name: &str, windows: bool) -> Vec<String> {
    if windows {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    }
}

/// `~/.local/bin/<name>` — uv's tool-bin dir is home-relative on every
/// OS (`%USERPROFILE%\.local\bin` on Windows). Exe suffix on Windows.
pub fn local_tool_binary(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    Some(dirs::home_dir()?.join(".local/bin").join(exe))
}

/// Parse `mistral-vibe 1.2.3` (uv tool list output line) → `1.2.3`.
pub fn parse_tool_version(list_output: &str) -> Option<String> {
    for line in list_output.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("mistral-vibe") {
            if let Some(v) = it.next() {
                return Some(v.trim_start_matches('v').to_string());
            }
        }
    }
    None
}

/// Parse PyPI JSON → `info.version`.
pub fn parse_pypi_latest(body: &serde_json::Value) -> Option<String> {
    body["info"]["version"].as_str().map(str::to_string)
}

/// One version bump: `a` newer than `b`? Semver-ish compare on
/// dot-separated numerics — pre-release suffixes compare equal to the
/// bare number (a managed pin only tracks released versions).
pub fn newer_than(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().max(y.len()) {
        match x.get(i).copied().unwrap_or(0).cmp(&y.get(i).copied().unwrap_or(0)) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            _ => {}
        }
    }
    false
}

/// env for every managed-uv call — isolates the tool env + bin dir.
fn managed_env(root: &Path) -> Vec<(String, String)> {
    vec![
        ("UV_TOOL_DIR".into(), root.join("tool").display().to_string()),
        ("UV_TOOL_BIN_DIR".into(), root.join("bin").display().to_string()),
        // Deterministic builds: managed installs resolve against PyPI only.
        ("UV_NO_CONFIG".into(), "1".into()),
    ]
}

/// Installed managed version, if the managed tool env exists.
pub async fn installed_version(uv: &Path, root: &Path) -> Option<String> {
    let out = tokio::process::Command::new(uv)
        .args(["tool", "list"])
        .envs(managed_env(root))
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tool_version(&String::from_utf8_lossy(&out.stdout))
}

/// Latest published version — PyPI JSON API.
pub async fn latest_version() -> Option<String> {
    let body = reqwest::get("https://pypi.org/pypi/mistral-vibe/json")
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?;
    parse_pypi_latest(&body)
}

/// `uv tool install mistral-vibe` into the managed env.
///
/// Pins the release PyPI reports (the same one `probe` would offer) —
/// installs resolve a named release instead of floating `latest`.
/// `--force` repairs a tool entry whose binary went missing. uv itself
/// verifies artifacts against index metadata; there is no
/// per-artifact hash pinning for `uv tool install`. `pin` overrides
/// the resolved version (tests inject it to stay offline).
pub async fn install(uv: &Path, root: &Path, pin: Option<&str>) -> Result<String, String> {
    let spec = match pin {
        Some(v) => format!("mistral-vibe=={v}"),
        None => latest_version()
            .await
            .map(|v| format!("mistral-vibe=={v}"))
            .unwrap_or_else(|| "mistral-vibe".to_string()),
    };
    run_uv(uv, root, &["tool", "install", "--force", &spec]).await?;
    installed_version(uv, root)
        .await
        .ok_or_else(|| "install finished but mistral-vibe is not listed".to_string())
}

/// Update to the newest pinned release — a pinned `install --force`,
/// NOT `uv tool upgrade`: the install's stored `==` requirement would
/// pin the upgrade resolution to the installed version forever. The
/// force-install replaces both the env and the stored requirement.
pub async fn upgrade(uv: &Path, root: &Path, pin: Option<&str>) -> Result<String, String> {
    install(uv, root, pin).await
}

async fn run_uv(uv: &Path, root: &Path, args: &[&str]) -> Result<(), String> {
    let out = tokio::process::Command::new(uv)
        .args(args)
        .envs(managed_env(root))
        .output()
        .await
        .map_err(|e| format!("{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Resolve the state in one pass, mirroring `server_binary`'s order —
/// the rail row describes the binary the app would actually run:
/// env override → managed install → system → nothing.
pub async fn probe(check_latest: bool) -> VibeDist {
    // `VIBE_APP_SERVER` wins outright in `server_binary`: the app never
    // runs the managed copy, so don't offer updates for it.
    if std::env::var("VIBE_APP_SERVER").is_ok() {
        return VibeDist::Installed {
            version: "unknown".into(),
            managed: false,
        };
    }
    let root = dist_root();
    let uv = uv_binary();
    let managed_version = match (&uv, &root) {
        (Some(u), Some(r)) => installed_version(u, r).await,
        _ => None,
    };
    // A runnable managed binary reports installed even without uv — uv
    // only gates install/update, not the probe. A `tool list` entry
    // whose binary is gone does NOT count: that's a broken install the
    // install CTA repairs via `--force`.
    if let Some(root) = &root {
        if managed_binary(root).is_file() {
            let installed = managed_version.unwrap_or_else(|| "unknown".into());
            if check_latest && installed != "unknown" {
                if let Some(latest) = latest_version().await {
                    if newer_than(&latest, &installed) {
                        return VibeDist::UpdateAvailable { installed, latest };
                    }
                }
            }
            return VibeDist::Installed {
                version: installed,
                managed: true,
            };
        }
    }
    // A system/PATH server needs no per-user data dir — check it before
    // the managed-root failure so the row describes a runnable server.
    let system_present = local_tool_binary("vibe-app-server")
        .map(|p| p.exists())
        .unwrap_or(false)
        || which("vibe-app-server").is_some();
    if system_present {
        return VibeDist::Installed {
            version: "unknown".into(),
            managed: false,
        };
    }
    if root.is_none() {
        return VibeDist::Failed {
            upgrade: false,
            error: "no per-user data directory resolved".into(),
        };
    }
    if uv.is_none() {
        return VibeDist::NoUv;
    }
    VibeDist::Missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_version_finds_mistral_vibe() {
        assert_eq!(
            parse_tool_version("mistral-vibe 0.4.2\nruff 0.6.1\n"),
            Some("0.4.2".to_string())
        );
        assert_eq!(parse_tool_version("ruff 0.6.1\n"), None);
    }

    /// Windows takes the Scripts/*.exe venv layout; Unix bin/ — both
    /// branches testable without a Windows box.
    #[test]
    fn managed_binary_layout_per_os() {
        let root = Path::new("/root");
        assert_eq!(
            managed_binary_on(root, true),
            root.join("tool/mistral-vibe/Scripts/vibe-app-server.exe")
        );
        assert_eq!(
            managed_binary_on(root, false),
            root.join("tool/mistral-vibe/bin/vibe-app-server")
        );
    }

    /// A non-executable PATH entry must not shadow a runnable one
    /// later in PATH (the resolved path bypasses PATH search, so
    /// `which` itself has to skip it — execvp semantics).
    #[cfg(unix)]
    #[test]
    fn which_skips_non_executable_candidates() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vibe-which-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dead = dir.join("old");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let bad = dead.join("vibe-app-server");
        let good = bin.join("vibe-app-server");
        // Default create mode has no exec bit (0644 under a sane umask).
        std::fs::write(&bad, b"").unwrap();
        std::fs::write(&good, b"").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(which_in("vibe-app-server", vec![dead, bin]), Some(good));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows PATH resolution must cover real exe + script shims —
    /// anything probe accepts has to be spawnable via the same resolver.
    #[test]
    fn exe_candidates_cover_windows_forms() {
        assert_eq!(
            exe_candidates("vibe-app-server", true),
            vec![
                "vibe-app-server.exe",
                "vibe-app-server.cmd",
                "vibe-app-server.bat",
                "vibe-app-server"
            ]
        );
        assert_eq!(exe_candidates("uv", false), vec!["uv"]);
    }

    #[test]
    fn parse_pypi_latest_reads_info_version() {
        let body = serde_json::json!({"info": {"version": "0.4.3"}, "releases": {}});
        assert_eq!(parse_pypi_latest(&body), Some("0.4.3".to_string()));
    }

    #[test]
    fn newer_than_compares_dotted_numerics() {
        assert!(newer_than("0.5.0", "0.4.9"));
        assert!(newer_than("1.0.0", "0.99.9"));
        assert!(!newer_than("0.4.2", "0.4.2"));
        assert!(!newer_than("0.4.2", "0.4.3"));
        assert!(newer_than("0.4.10", "0.4.9"));
    }

    /// A scripted `uv` stand-in: `tool install/upgrade` create the managed
    /// binary + version file; `tool list` echoes it back. Exercises the
    /// real command/env path (`install` → `upgrade` → `installed_version`)
    /// without network or a real uv.
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_install_and_upgrade_via_fake_uv() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vibe-dist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let uv = dir.join("fake-uv");
        std::fs::write(
            &uv,
            r#"#!/bin/sh
# $UV_TOOL_DIR/mistral-vibe/VER holds the installed version (bumped on upgrade).
tool="$UV_TOOL_DIR/mistral-vibe"
ver_file="$tool/VER"
case "$1 $2" in
  "tool install")
    # install --force mistral-vibe==<v> → the pinned version lands;
    # an unpinned spec installs the fake's default.
    mkdir -p "$tool/bin"
    v="${4##*==}"; [ "$v" = "$4" ] && v="0.4.0"
    echo "$v" > "$ver_file"; touch "$tool/bin/vibe-app-server" ;;
  "tool list")
    [ -f "$ver_file" ] && echo "mistral-vibe $(cat "$ver_file")" || true ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();

        let root = dir.join("dist");
        std::fs::create_dir_all(&root).unwrap();

        // install → managed binary + version land (pinned spec, no
        // network).
        let v = install(&uv, &root, Some("0.4.0")).await.unwrap();
        assert_eq!(v, "0.4.0");
        assert!(managed_binary(&root).is_file());
        assert_eq!(installed_version(&uv, &root).await.as_deref(), Some("0.4.0"));

        // upgrade = pinned reinstall at the newer pin (not
        // `uv tool upgrade`, which would stay pinned to the stored
        // `==` requirement).
        let v = upgrade(&uv, &root, Some("0.4.9")).await.unwrap();
        assert_eq!(v, "0.4.9");
        assert_eq!(installed_version(&uv, &root).await.as_deref(), Some("0.4.9"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
