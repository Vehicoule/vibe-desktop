# vibe-desktop — design

A desktop client for [Mistral Vibe](https://github.com/mistralai/mistral-vibe) with feature parity to the CLI and Vibe Code Web: local sessions against your own checkout, cloud sessions in remote sandboxes, and moving sessions between them.

**Non-goal:** reimplementing the agent. The app is a client of `vibe-app-server`, the official harness boundary shipped inside `mistral-vibe`. Every official surface (Textual TUI, `vibe-acp`, programmatic mode) is a client of it; we are the fourth. Parity is structural — a capability lands in the server, the app renders it.

## Stack

- **Rust + [gpui](https://gpui.rs)** (`gpui` + `gpui_platform` from crates.io; pre-1.0 — pin a version, bump deliberately). Targets macOS, Linux, Windows.
- **tokio + serde_json** for the NDJSON JSON-RPC 2.0 wire. `vibe/cli-rust` in mistral-vibe is a prior art thin client in the same language over the same protocol.
- **Audio (voice)**: mic capture + speaker playback are client-owned per ADR-0009. `cpal` for I/O; the server provides `narration/summarize` for TTS text.

## Architecture

```
┌────────────────────────── vibe-desktop (gpui app) ──────────────────────────┐
│  UI layer: session rail · timeline · inspector · approvals · settings       │
│  Protocol client: typed models, NDJSON codec, watermark/gap reducer         │
│  Process manager:                                                           │
│    catalog proc  ── stdio ──► vibe-app-server  (never attached; list/meta)  │
│    session proc ── stdio ──► vibe-app-server  (one per attached session)    │
│    session proc ── stdio ──► vibe-app-server  (one per attached session)    │
└─────────────────────────────────────────────────────────────────────────────┘
```

- One server process owns one attached root session → tab per session, spawn per tab. A separate never-attached process serves `session/list` and other passive calls (ADR-0009).
- ClientTools capabilities (`filesystem/read|write`, `terminal`) are server→client requests the app answers: render managed terminals and file access in-app rather than letting the server touch the host directly. Declare capabilities honestly — only what the UI implements.

### Wire contract (from ADR-0009 + protocol models)

- camelCase `ProtocolModel` fields; StrEnums lowercase; ids `client-N` for client→server requests.
- Lifecycle: `initialize` (ClientInfo + capabilities) → `initialized` notification → passive calls or `session/start|resume|continue` attach → live notifications + server→client requests.
- Notifications: `session/snapshot|updated|compacted|contextCleared|childSessionUpdated|statsUpdated`, `history/entryAdded|entryUpdated`, `turn/started|completed|queueUpdated|retrying`, `runtime/updated`; handoff notifications carry a replacement `PublicSessionState` adopted atomically.
- History entries: `message|reasoning|effect|callback|checkpoint|notice`. Effect kinds: `tool|shell|file_edit|file_search|file_read|todo|file_write|user_question|web_search|web_fetch|skill|subagent|worktree|process`. Effect states: `pending|running|blocked|completed|failed|cancelled|skipped`.
- `JsonPatchOperation`: `add|append|replace|remove|test` on RFC-6901 paths; `append` is the string-suffix streaming op.
- Event watermark: ignore ≤ watermark, accept exactly +1, gap → `session/read` resync.
- Callbacks: `approval|user_input|connector_auth`; `callback/call` response is a delivery ack — semantic answer goes back via client `callback/respond`. Approval decisions: `approve|approve_for_session|approve_permanently|deny|cancel_turn`.
- SessionStatus: `idle|running|blocked|failed|archived`.

## Server distribution (vibe is installed by the app, updated independently)

`vibe-app-server` is a console script in the `mistral-vibe` PyPI package — there is **no standalone release asset** as of v2.25.8 (the `vibe-*.zip` bundles ship the `vibe` TUI binary only; `vibe-app-server.spec` exists in-repo but isn't published).

Primary path:
1. App manages a `uv` binary (or uses `python-build-standalone`) in its own data dir → `uv tool install mistral-vibe==<pinned>` into an app-owned location → spawns `vibe-app-server` from there.
2. Update check: query PyPI for latest `mistral-vibe`, compare to installed, offer/perform upgrade **without an app release**. Updates are per-machine state (`~/.local/share/vibe-desktop/vibe/`).
3. Fallback: if the user already has a working `vibe-app-server` on PATH/newer, allow "use system install" with a version check against `runtime/read`.

Supply-chain (per mistral-vibe's own rules): downloads verified by hash keyed to version+arch; GitHub release assets expose `digest` in the API where used; hard-fail on unregistered versions. Never curl|sh.

## Feature parity matrix

Every row: what exists upstream → which protocol calls → the desktop surface. "—" = server-side, app only renders.

### Sessions & timeline (the core)

| Feature | Upstream | Protocol | Desktop surface |
|---|---|---|---|
| New/resume/continue session | `vibe`, `vibe --resume` | `session/start|resume|continue`, `session/read` | Session rail + new-session sheet (cwd, model, agent, branch/worktree) |
| Session list / pin / archive / rename / title / markAsSeen / delete | `vibe --resume` picker | `session/list|pin|archive|rename|title|markAsSeen|delete` | Rail with context menu, pins on top, archive filter |
| Live timeline | TUI chat | `history/entryAdded|entryUpdated`, patches | Timeline: messages, collapsible reasoning, effect cards with live output, checkpoints, notices |
| Streaming text | TUI streaming | `append` patch op | Incremental render |
| Turn state / retry / runtime | TUI status line | `turn/started|completed|retrying`, `runtime/updated`, `session/statsUpdated` | Status bar: model, agent, tokens, cache-hit, cost, status pill |
| Interrupt / steer | Esc / type-during-run | `session/turn/interrupt`, `session/turn/steer`, `session/shellCommand` | Esc key, steer input while running |
| Turn queue | queued prompts | `session/turn/enqueue`, `session/turn/queue`, `remove|replace|steer|resume` | Queue panel: reorder/edit/consume, ADR-0013 semantics |
| Approvals | permission prompts | `callback/call` → `callback/respond` (approve/…/cancel_turn) | Modal cards; remember choices per scope |
| `ask_user_question` | questions | `user_question` callback | Option picker inline in timeline |
| fork / rewind / compact | `/fork`, `/rewind`, `/compact` | `session/fork`, `session/rewind(+read)`, `session/compact`, `session/context/*` | Timeline checkpoint actions; rewind preview before apply |
| Session history / journal | `vibe --history` | `session/history/get|list|clear` | History browser |
| Slash commands | `/…` | `session/shellCommand`, skills catalog | Command palette with completion; busy-safe subset per ADR-0012 |
| Subagents/child sessions | task tool | `session/childSessionUpdated`, `subagent` effects | Nested cards / child tabs |
| Multi-session | — (one TUI per terminal) | proc-per-session | Tabs + per-tab rail state; badge on `blocked` |
| Voice mode | `/voice` | client-owned I/O + `narration/summarize` | Push-to-talk input, narration playback (ADR-0017 shape) |

### Code & project surfaces

| Feature | Upstream | Protocol | Desktop surface |
|---|---|---|---|
| Review diffs | `/review` | `review/state|approve|baseline|hunks|revert|turnDiff` | Dedicated diff view: per-hunk approve/revert |
| Worktrees | worktree tool | `session/git/worktrees*`, `worktree` effects | Worktree switcher, limit/prune/reap controls |
| Trust folder | trust prompt | `workspace/trust*` | Trust dialog on first open; status in settings |
| Prompt prep (@files) | `@` mentions | `workspace/prompt/prepare` | Composer autocomplete |
| File read/write effects | tools | `file_*` effects, `clientTool/*` | Inline file cards; open-in-editor |
| Loops (scheduled) | `/loop` | `loops/create|list|delete|clear` | Loops panel |

### Extension surfaces (settings area)

| Feature | Upstream | Protocol | Desktop surface |
|---|---|---|---|
| Config editor | `/config` browser (v2.23.2) | `config/read|write|reload|schema|fields|model|proxy` | Searchable settings browser with per-layer origin — parity with the new TUI config |
| Model + agent pickers | `/model`, `/agents` | `config/model`, `agents/list|install|uninstall` | Pickers in status bar + settings |
| Skills | `/skills` | `skills/catalog|installed|detail|import|convertLocal|remove|versions|setAlias|setEnabled|setLatest|setVersion|updates` | Skills manager: enable/disable, versions, updates |
| Plugins | plugins | `plugins/read|info|reload|catalog` | Plugins panel |
| MCP | `/mcp` | `mcp/read|add|login|logout|refresh|toggle`, `mcp_catalog/*` | MCP manager + OAuth login flow |
| Connectors | connectors | `connectors/read|refresh|auth`, `connector_catalog/*`, `connector_auth` callback | Connector cards with auth state |
| Project links | link local↔remote | `projectLinks/*` | Link indicator on sessions; manage in settings |

### Cloud (Vibe Code Web parity)

| Feature | Upstream | Protocol | Desktop surface |
|---|---|---|---|
| Remote projects | web app | `vibeCode/projects/create|select|open|recover|loadMore|cancel|unlink` | Projects section in rail; picker UI |
| Teleport local→cloud | web | `vibeCode/teleport/start|cancel|push/respond` (+ progress events) | "Send to cloud" action with staged progress (Summarizing→Git→Push→Workflow→Done) |
| Relocate | — | `session/relocate` | Move session local↔remote |
| GitHub repo picker | web | `VibeCodeProject/Repository/PickerView/GitInfo` models | Repo selector, branch, git state |

### Account & diagnostics

`account/read`, `identity/read`, `stats/read`, `diagnostics/list`, `logs/read`, `events/read`, `feedback/*`, `telemetry/record` → account menu, stats footer, diagnostics viewer, feedback sheet.

### Desktop-only extras

OS notifications when a session goes `blocked`, dock badge, global hotkey to summon, drag-drop file/image attachments into composer, native menus, auto-update of the app itself.

## Look

Mistral's system: warm ivory surfaces, the orange→red→yellow sunset accents, pixel-block motif, editorial type with mono for structural text. Keep it a tool, not a chat toy:

- **Left rail** — sessions (pinned first) + cloud projects + status footer (account, update dot).
- **Center** — timeline + composer (slash palette, @-mentions, voice, queue/steer affordances).
- **Right inspector** (collapsible) — diffs, files touched, turn queue, session stats.
- Empty states + loading use the pixel mosaic; accents reserved for actions and status — sunset gradient for "running", red for approvals attention, amber for blocked.

## Milestones

1. **M1 — local core**: spawn/attach, session rail, live timeline (messages + reasoning + effect cards), approvals + user-question, interrupt/steer, status bar. Fake-server fixture for CI determinism.
2. **M2 — session ops**: turn queue UI, pin/archive/rename/fork/rewind/compact, history browser, trust dialog, voice (narration playback + mic input).
3. **M3 — settings & extensions**: config browser, model/agent pickers, skills/plugins/MCP/connectors managers, review diff view, worktrees, loops.
4. **M4 — cloud**: projects rail, teleport flow, relocate, remote session drive.
5. **M5 — ship**: packaging per-OS, managed-vibe installer + updater, notifications, signing/notarization.

## Open questions

- Voice scope detail: CLI voice does push-to-talk + narration; confirm we mirror that (not wake-word).
- Upstream contribution: `vibe-app-server` standalone asset doesn't ship yet — open an issue/PR upstream to publish it? Would simplify our update channel to GitHub assets.
- gpui version pinning strategy (`*` is required today — bump cadence + lock).
- Windows terminal parity: Vibe targets UNIX primarily; managed PTY (`clientTool/terminal`) on Windows needs verification.
- Repo visibility (private for now) and CI runners for 3-OS builds.
