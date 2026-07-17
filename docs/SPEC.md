# agentdeck — Technical Spec v1

> Working title. A local dashboard for Claude Code agents: see at a glance which agents are running, which are blocked, which are done — including subagents, which pane-based tools structurally cannot see.

> **Status note (2026-07-17):** Phase 0 is done. Where this document and
> `docs/OBSERVED.md` disagree, OBSERVED wins (per §3). Key deltas: nesting is
> reconstructible via `PostToolUse[Agent]`; background agents' metrics come from
> task-notification prompts; `Notification` field is `notification_type`;
> `StopFailure` field is `error`.

---

## 0. Context and rationale

Agent-aware multiplexers (herdr, cmux) and Warp surface state **per pane**. But Claude Code subagents (the `Agent` tool) run **in-process**: no PTY, no pane. They are therefore invisible to those tools by construction.

agentdeck listens to Claude Code's **HTTP hooks** and reconstructs the real `session → subagents` tree, with status, duration and cost per node.

**What this is not:** a multiplexer. No pane management, no "click to jump to the terminal" (Warp exposes neither IPC nor a focus URI — permanently out of scope on the Warp side).

---

## 1. Scope, v1

### In scope
- Local daemon receiving hook events over HTTP
- In-memory state: a session/subagent tree with status
- Menubar (tray) UI rendering the tree in real time
- Tray badge = worst aggregate state (blocked > working > idle)

### Out of scope for v1 (but the architecture must not preclude them)
- Persistence / history / database
- VPS hosting, multi-user, auth
- Actions back to the agent (reply, approve, cancel)
- Any agent other than Claude Code

### Non-negotiable
- **Zero impact on agent latency.** The hook sits in Claude Code's critical path.
- **Zero persistence.** State is ephemeral by nature: sessions die with the process.

---

## 2. Stack

| Layer | Choice | Why |
|---|---|---|
| Daemon | Rust + `axum` + `tokio` | Performance on the critical path, and the Rust learning goal |
| State | `Arc<RwLock<HashMap<...>>>` + `tokio::sync::broadcast` | No DB, no persistence |
| Shell app | Tauri v2 (tray/menubar) | Native, light, not Electron |
| Frontend | React + TypeScript | Known stack; it's just a tree to render |
| UI transport | SSE or Tauri events | Push, not polling |

The daemon must be a **separate crate** from the Tauri shell (`crates/daemon`, `crates/app`). That boundary is the seam for a possible hosted v2. Do not merge them.

---

## 3. Phase 0 — Discovery (do this FIRST)

**Do not model before seeing the data.** The `SubagentStart` input schema is undocumented (anthropics/claude-code#19170), and the event set is actively expanding — the official reference (https://code.claude.com/docs/en/hooks) is the only canonical source. Assume nothing from memory.

### Task
1. A minimal binary: `axum`, `POST /hook`, `println!("{}", body)`, respond `204`.
2. Register an HTTP hook for **every** event listed in §6 in `~/.claude/settings.json`.
3. Trigger: a normal session, a `Task` (subagent), a subagent requesting permission, an error, several subagents in parallel.
4. Dump each payload to `docs/payloads/<event>.json`.

### Expected output
A `docs/OBSERVED.md` documenting, per event: which fields are actually present, which are absent, and in particular **whether `SubagentStart` carries `agent_id`**.

**The rest of this spec is contingent on that phase.** If observations contradict this document, observations win — update the spec.

> ✅ Done. See `docs/OBSERVED.md`. `SubagentStart` carries `agent_id` and `agent_type`.

---

## 4. Data model

```rust
enum Status {
    Working,    // running
    Blocked,    // waiting on user input / approval
    Done,       // last turn completed successfully
    Error,      // last turn ended in error
    Idle,       // alive, nothing in flight
}

struct Node {
    id: String,             // session_id (root) or agent_id (child)
    parent: Option<String>, // None = root
    agent_type: Option<String>, // "Explore", "general-purpose", "my-plugin:reviewer"…
    cwd: Option<String>,
    status: Status,
    started_at: Instant,
    last_event_at: Instant,
    // filled on completion, see §8
    tokens: Option<u64>,
    duration_ms: Option<u64>,
    tool_calls: Option<u32>,
    model: Option<String>,
}
```

### Linkage rule (the core of the whole thing)
- `agent_id` and `agent_type` are present **only** when the hook fires inside a subagent.
- `session_id` remains the **parent's**, shared across all its subagents.

Therefore:

```
key    = agent_id.unwrap_or(session_id)
parent = if agent_id.is_some() { Some(session_id) } else { None }
```

That's it. The tree falls out for free.

> **Amended by observation:** that rule flattens *nested* subagents to depth 1.
> The true parent edge is available: `PreToolUse`/`PostToolUse[Agent]` events fired
> inside agent P carry `agent_id = P`, and `tool_response.agentId` is the child C.
> On that event, set `C.parent = P`. `session_id` parenting is the fallback.

---

## 5. Endpoint contract

`POST /hook` — a single endpoint. Claude Code deduplicates HTTP hooks by URL; dispatch happens server-side on `hook_event_name`.

### Hard rules

1. **Respond `204 No Content`, always, immediately.**
   A 2xx with a **text** body is injected into Claude's context. A stray `"OK"` would pollute every turn of every agent. Empty body. Full stop.

2. **Process asynchronously.** The handler parses, sends to a channel, responds. No work in the handler. No blocking locks, no I/O.

3. **Never block, never panic.** Unknown or malformed payload → log and drop. The next event must still get through.

4. Bind to `127.0.0.1:4747` only. Not `0.0.0.0`.

### Safety net
On the Claude Code side, connection failures, timeouts and non-2xx responses are all **non-blocking** errors: execution continues. Daemon down = zero impact on agents. So the daemon can be killed and restarted freely during development.

---

## 6. Hook configuration

Target: `~/.claude/settings.json` (scope = all projects).

```json
{
  "hooks": {
    "SubagentStart":   [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "SubagentStop":    [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "UserPromptSubmit":[{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "Stop":            [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "StopFailure":     [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "Notification":    [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "PostToolUse":     [{ "matcher": "Agent", "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "PreToolUse":      [{ "matcher": "Agent", "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }],
    "SessionEnd":      [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:4747/hook", "timeout": 5 }] }]
  }
}
```

> `PreToolUse[Agent]` added post-Phase-0: it is the nesting edge + "still working" signal.

### Constraints to be aware of
- **`SessionStart` and `Setup` do not support `type: "http"`** — only `command` and `mcp_tool`. *(Empirically re-confirmed in Phase 0: an http hook on `SessionStart` silently never fires, despite docs claiming otherwise.)* To capture session start: a command hook that `curl`s with `--max-time 1` to the same endpoint. Treat it as an isolated special case, not as the general pattern.
- `UserPromptSubmit` has a default timeout of **30s** (vs 600 elsewhere). We force `timeout: 5` everywhere anyway.
- `Stop`, `UserPromptSubmit` and `Notification` accept no matcher — they always fire.
- `SubagentStart` / `SubagentStop` match on **agent type** (`general-purpose`, `Explore`, `Plan`, custom names, or plugin-scoped `^my-plugin:reviewer$`). Omit the matcher → all of them.
- A `Stop` hook declared **inside** a subagent is automatically converted to `SubagentStop`.

The app must offer an **"Install hooks"** button that merges this config into `~/.claude/settings.json` without clobbering what's there (hooks from other tools must survive). `/hooks` inside Claude Code lets you verify the resulting config.

---

## 7. State machine

| Event | Effect |
|---|---|
| `UserPromptSubmit` | upsert root node → `Working` (also fired by `<task-notification>` machine turns) |
| `SubagentStart` | **create** child node → `Working` |
| `SubagentStop` | node → `Done` with a TTL (ignore if unknown node with empty `agent_type`) |
| `Notification` (`notification_type` ∈ `permission_prompt`, `idle_prompt`, `agent_needs_input`) | node → `Blocked` |
| `Notification` (`agent_completed`) | node → `Done` |
| `Stop` | node → `Done` |
| `StopFailure` | node → `Error` |
| `PreToolUse` / `Agent` | touch node; parent edge candidate |
| `PostToolUse` / `Agent` | enrich child node with metrics (§8) + set true parent edge |
| `SessionStart` | upsert root → `Idle` |
| `SessionEnd` | remove the root and **all** its descendants |

### ⚠️ The ghost-session trap
Subagents fire `UserPromptSubmit` and `PreToolUse`, which leads external tools to register a new session — but a subagent that finishes does **not** fire `Stop`. Result: zombie nodes that never die (anthropics/claude-code#33049).

*(Phase 0 note: subagent-side `UserPromptSubmit` did not reproduce on v2.1.199,
but `SubagentStop` without `SubagentStart` did, and so did `SessionEnd` without
`SessionStart`. The defensive rules stay.)*

**Rules that follow:**
- A subagent's lifecycle is **exclusively** `SubagentStart` → `SubagentStop`. Nothing else creates one (exception: metrics-bearing `PostToolUse[Agent]` may upsert, since it proves existence).
- **Never** create a node on `UserPromptSubmit` when `agent_id` is present.
- **A reaper is mandatory**: a periodic task purging any node whose `last_event_at` exceeds a TTL (5 min default, configurable). This is the net for missed events, not a nice-to-have.

---

## 8. Per-subagent metrics

On `PostToolUse` with `matcher: "Agent"`, the `tool_response` field carries, for a subagent that completed in the **foreground**:

`agentId`, `status`, `content`, `resolvedModel`, `totalTokens`, `totalDurationMs`, `totalToolUseCount`, a detailed `usage` object (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`) and a `toolStats` breakdown.

→ Correlate via `agentId` and enrich the node. **Cost and duration per subagent is the product's differentiator.** Neither Warp nor herdr surfaces it.

**Caution:** as of Claude Code v2.1.198 subagents run in the background by default, and the tool then returns immediately with `status: "async_launched"` and **no usage fields** (only `agentId`, `description`, `prompt`, `outputFile`, `resolvedModel`, `isAsync`, `canReadOutputFile`). Handle both cases.

**Background-agent metrics (Phase 0 finding):** when a background agent completes, the parent session receives a `UserPromptSubmit` whose `prompt` is a `<task-notification>` block containing `<task-id>` (= agentId) and `<usage><subagent_tokens>…</subagent_tokens><tool_uses>…</tool_uses><duration_ms>…</duration_ms></usage>`. Parse it (best-effort) to enrich the node.

---

## 9. UI

**Form factor: tray/menubar.** The point is to glance, not to open an app.

- **Tray icon** = aggregate state, priority `Blocked > Working > Error > Done > Idle`. This is the primary signal: it must be legible peripherally, not a 6px badge in a corner.
- **Popover** = the tree. Roots are sessions (label: basename of `cwd`). Indented children are subagents (label: `agent_type`).
- Per node: status dot, elapsed time, and where available tokens + duration.
- **Explicit empty state**: distinguish "no agents running" from "daemon unreachable". Both happen; they do not mean the same thing.
- No sound, no desktop notifications in v1 — Claude Code and the terminal already do that.

---

## 10. Phases

| Phase | Content | Done when |
|---|---|---|
| **0** | Discovery, `docs/OBSERVED.md` | ✅ Real payloads are documented |
| **1** | Daemon: endpoint, state, reaper, SSE | `curl`ing fixture payloads produces correct state over SSE |
| **2** | Tauri + tray + React tree | A real `Task` appears and disappears cleanly |
| **3** | Metrics (§8), hook installer (§6) | Tokens visible per subagent |

Each phase is usable on its own. Do not start phase 1 before phase 0 is written up.

---

## 11. Testing

- **Fixtures**: the phase 0 payloads become the test corpus. Each event → one file → one transition test.
- **Explicit anti-zombie test**: replay a `SubagentStart` with no `SubagentStop` → assert the reaper purges it. This is the most likely bug in the project; it earns a dedicated test from phase 1.
- **Robustness test**: payloads with unknown fields, missing fields, invalid JSON → no panic, still `204`.
- **Contract regression test**: the response body is empty. A non-empty body is a high-severity bug (it pollutes Claude's context).

---

## 12. Notes for later (do not implement in v1)

If this ever goes multi-user with a VPS relay: the local daemon stays the hook entry point and **sanitizes before forwarding**. Payloads carry `cwd`, file paths, `last_assistant_message` and prompt text. Only `session_id`, `agent_id`, `agent_type`, status and timestamps would ever go upstream.

Two reasons to keep the daemon local even in that scenario: (1) HTTP hooks have **no** `async` field — it exists only for command hooks — so a remote endpoint puts a network round-trip in the critical path of every event; (2) raw data must not leave the machine.

That's why the daemon is a separate crate from day one.

---

## 13. Sources

- Hooks reference (canonical, re-verify — the event set moves): https://code.claude.com/docs/en/hooks
- Ghost sessions / subagents not firing `Stop`: anthropics/claude-code#33049
- `SubagentStart` schema undocumented: anthropics/claude-code#19170
- Background on `agent_id` linkage: anthropics/claude-code#7881
