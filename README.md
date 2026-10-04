# vibe-desktop

Desktop client for [Mistral Vibe](https://github.com/mistralai/mistral-vibe) — local and cloud sessions, built with Rust + [gpui](https://gpui.rs).

The app is a client of `vibe-app-server`, the official harness boundary inside `mistral-vibe`; the agent, tools, permissions, and cloud backend all live server-side.

See [DESIGN.md](DESIGN.md) for the architecture and the feature-parity matrix against the CLI and Vibe Code Web.

## Build & run

```sh
cargo build --workspace        # build all crates
cargo test  --workspace        # unit + fixture E2E tests
cargo run  -p vibe-desktop -- --fixture   # run against the scripted fake server (no API key needed)
cargo run  -p vibe-desktop                 # run against a real `vibe-app-server` on PATH
```

Requires Rust 1.85+ and (Linux) a working Vulkan driver for gpui — `mesa-vulkan-drivers` (llvmpipe) or a hardware driver. Rendering under old software-Mesa stacks (e.g. llvmpipe/LLVM 15 on Ubuntu 22.04 + KWin/X11) is a known-bad combination: windows open but presents never land. Real GPUs and current Mesa work.

### vibe runtime

The app manages its own `vibe-app-server`: with `uv` on PATH it installs `mistral-vibe` into an isolated tool env under `~/.local/share/vibe-desktop/vibe` (`VIBE_DESKTOP_DIST` overrides), and the rail footer shows the installed version plus an update action when PyPI has a newer release — so the server upgrades without an app release. Resolution order: `VIBE_APP_SERVER` env → managed install → `~/.local/bin` → PATH.

## Crates

- `crates/vibe-protocol` — typed JSON-RPC client (`Connection`), wire models, JSON-Patch + session projection (watermark/gap-recovery semantics from the app-server ADR).
- `crates/vibe-fixture` — scripted fake `vibe-app-server` for deterministic tests and credential-free development.
- `crates/vibe-desktop` — the gpui app (session rail, timeline, approvals, composer, status bar).

## Notes

- `Cargo.lock` pins `libc = 0.2.189`: `xattr 0.2.3` (via `gpui_http_client`) still uses `libc::ENOATTR`, removed from libc on Linux in 0.2.190. Revisit when `xattr` releases a fix.
- Status: M5 in progress — see DESIGN.md milestones.
