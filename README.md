# agentdeck

A local dashboard for Claude Code agents: see at a glance which agents are running,
blocked, or done — **including subagents**, which pane-based tools structurally
cannot see. Spec: `docs/SPEC.md`. Observed hook behavior (ground truth): `docs/OBSERVED.md`.

## Status

- ✅ **Phase 0** — discovery: real hook payloads captured in `docs/payloads/`, written up in `docs/OBSERVED.md`
- ✅ **Phase 1** — daemon: `POST /hook` sink, in-memory session/subagent tree, reaper, SSE. Verified against fixtures *and* live Claude Code sessions (incl. nested subagents + per-agent token metrics)
- ✅ **Phase 2** — Tauri v2 tray app + React tree; tray icon = aggregate state, popover = live tree
- ✅ **Phase 3** — per-subagent tokens/duration in the UI, in-app "Install hooks" button

## Run

```sh
cargo run --bin agentdeck-daemon      # 1. daemon, listens on 127.0.0.1:4747
cargo run -p agentdeck-app            # 2. tray app (left-click tray = popover, right-click = quit)
cargo run -p agentdeck-app -- --window  # WSLg/no-tray fallback: open the window directly
```

Hooks: click **Install hooks** in the app footer, or run `python3 scripts/install-hooks.py`.
Both are idempotent and preserve hooks from other tools (a backup is written to
`~/.claude/settings.json.agentdeck-backup`).

The Rust shell owns the daemon connection (SSE) and drives the tray icon even with
the popover closed; the webview receives snapshots as Tauri `state` events. Rebuild
order matters: the frontend is embedded at compile time, so `npm --prefix ui run build`
before `cargo build -p agentdeck-app` when the UI changed.

- `POST /hook` — hook sink (always `204`, empty body — the contract, see `docs/SPEC.md` §5)
- `GET /state` — current tree snapshot (JSON)
- `GET /events` — SSE, one full snapshot per state change

Config: `AGENTDECK_TTL_SECS` (stale reap, default 300), `AGENTDECK_DONE_TTL_SECS`
(done-subagent linger, default 60).

Daemon down = zero impact on Claude Code (hook failures are non-blocking, verified).

## Test

```sh
cargo test
```

Fixtures in `docs/payloads/` are real captured payloads (Claude Code v2.1.199) and
double as the test corpus. The suite covers the state machine, the anti-zombie
reaper, nesting edges, and the empty-204 endpoint contract.

## Layout

- `crates/daemon` — the daemon (separate crate from the Tauri shell on purpose; see spec §12)
  - `src/bin/discover.rs` — Phase 0 payload dump server
- `crates/app` — Tauri v2 tray shell (SSE client, tray icon, popover window, hook installer)
- `ui/` — React + TypeScript popover frontend (Vite)
- `docs/` — spec, observations, payload corpus
- `scripts/install-hooks.py` — CLI twin of the in-app hook installer
