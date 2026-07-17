# OBSERVED — Phase 0 discovery results

Captured 2026-07-17 against **Claude Code v2.1.199** on Linux (WSL2), using the
`discover` binary (`crates/daemon/src/bin/discover.rs`) and HTTP hooks registered in
`~/.claude/settings.json` for: `SubagentStart`, `SubagentStop`, `UserPromptSubmit`,
`Stop`, `StopFailure`, `Notification`, `PostToolUse` (matcher `Agent`),
`PreToolUse` (matcher `Agent`, discovery only), `SessionEnd`, plus a `command`+curl
hook for `SessionStart`.

Raw corpus: `docs/payloads/*.json`, numbered in arrival order. Scenarios exercised:
plain turn, foreground subagent, two parallel subagents, background subagent that
itself spawned a nested subagent, API error (bogus model), interactive permission
prompt, interactive idle. All via headless `claude -p` except the two interactive
ones (driven through a pty with `script(1)`).

---

## Headline answers to the spec's open questions

1. **`SubagentStart` DOES carry `agent_id` and `agent_type`** (`SubagentStart-0009`).
   `session_id` stays the parent session's. The spec's linkage rule
   (`key = agent_id || session_id`) works.
2. **`agent_id`/`agent_type` are present on `PreToolUse`/`PostToolUse` fired inside a
   subagent** (`PreToolUse-0040`, `PostToolUse-0042`), absent at root. Bonus: this
   makes **true nesting reconstructible** — see "Nested subagents" below.
3. **`UserPromptSubmit` never carried `agent_id`** in the corpus (12 samples).
   Subagent turns do not emit `UserPromptSubmit` at all in v2.1.199 — the ghost-session
   trap of the spec (#33049) did not reproduce. Keep the defensive rule anyway.
4. **Foreground subagent completion delivers full metrics** on `PostToolUse[Agent]`
   (`PostToolUse-0011`): `totalTokens`, `totalDurationMs`, `totalToolUseCount`,
   `resolvedModel`, full `usage`, plus an undocumented `toolStats` breakdown.
5. **Background (`async_launched`) is the common case** — Haiku launched agents in
   background even unprompted (session `e85d01f6`). The immediate `tool_response`
   has no metrics, but metrics ARE recoverable later: see "Background subagents".

---

## Fields observed, per event

Common to (nearly) all events: `hook_event_name`, `session_id`, `transcript_path`,
`cwd`, `prompt_id`. `permission_mode` is present on `UserPromptSubmit`, `Stop`,
`SubagentStop`, `PreToolUse`, `PostToolUse` — **absent** on `SessionStart`,
`SessionEnd`, `SubagentStart`, `Notification`, `StopFailure`.

| Event | Extra fields observed (n = samples) |
|---|---|
| `SessionStart` (7) | `source` (`"startup"` in all samples), `model` (1/7 — optional!) |
| `UserPromptSubmit` (12) | `prompt` |
| `PreToolUse` (6) | `tool_name`, `tool_input`, `tool_use_id`; `agent_id`+`agent_type` iff inside a subagent (2/6) |
| `PostToolUse` (6) | same as PreToolUse plus `tool_response`, `duration_ms` |
| `SubagentStart` (6) | `agent_id`, `agent_type` — **nothing else**. No prompt, no parent pointer, no tools list |
| `SubagentStop` (8) | `agent_id`, `agent_type` (**can be `""`**), `agent_transcript_path`, `last_assistant_message` (7/8 — can be missing), `background_tasks`, `session_crons`, `stop_hook_active` |
| `Stop` (10) | `last_assistant_message`, `background_tasks`, `session_crons`, `stop_hook_active` |
| `StopFailure` (1) | `error` (observed `"model_not_found"`), `last_assistant_message`, `effort` |
| `Notification` (2) | `notification_type` (observed `"permission_prompt"`, `"idle_prompt"`), `message` |
| `SessionEnd` (9) | `reason` (observed only `"other"` — even for killed interactive sessions and normal `-p` exits) |

## Lifecycle sequences observed

Plain `-p` session (`db2f1744`, seq 0002–0005):
`SessionStart(startup)` → `UserPromptSubmit` → `Stop` → `SessionEnd(other)`.

Foreground subagent (`097177f4`, 0006–0013):
`UserPromptSubmit` → `PreToolUse[Agent]` → `SubagentStart` → `SubagentStop` →
`PostToolUse[Agent]` (`status: "completed"`, full metrics) → `Stop` → `SessionEnd`.
Note ordering: **`SubagentStop` arrives before the parent's `PostToolUse`**.

Background subagent (`9a584be0`, 0034–0049):
`PreToolUse[Agent]` → `SubagentStart` → `PostToolUse[Agent]`
(`status: "async_launched"`, no metrics) → root `Stop` (root turn ends while agent
runs) → …agent works… → `SubagentStop` → **`UserPromptSubmit` with a
`<task-notification>` prompt** (the harness re-wakes the root) → `Stop`.
One task-notification turn per completed background agent.

## Background subagents (`async_launched`)

Immediate `tool_response` fields (`PostToolUse-0038`): `agentId`, `agentType`?,
`description`, `prompt`, `resolvedModel`, `status: "async_launched"`,
`isAsync: true`, `outputFile`, `canReadOutputFile`. **No usage/duration/token
fields** — as the spec warned.

**However**: the task-notification `UserPromptSubmit` prompt embeds
`<task-id>` (= agentId) and
`<usage><subagent_tokens>N</subagent_tokens><tool_uses>N</tool_uses><duration_ms>N</duration_ms></usage>`
(`UserPromptSubmit-0044`). Parsing that XML-ish block gives tokens / tool calls /
duration for background agents too. Hacky but it is the only metrics channel for
the async case; treat as best-effort enrichment.

## Nested subagents

A subagent can spawn its own subagents. Observed (`9a584be0`): root → general-purpose
(background) → Explore. **`SubagentStart` of the nested agent carries only the root
`session_id`** — no parent-agent field. Flat `session_id` parenting would misplace
nested agents at depth 1.

The real edge comes from the tool events: `PreToolUse`/`PostToolUse[Agent]` fired
*inside* agent X carry `agent_id = X`, and the `PostToolUse.tool_response.agentId`
is the child (`PostToolUse-0042`: `agent_id: ad1a…` launched `agentId: af1003…`).
So: parent of a new subagent = `agent_id` of the surrounding `PostToolUse[Agent]`
(or root if absent). `prompt_id` is NOT a reliable parent signal — it tags the
originating root turn, and on `SubagentStop` it reflects whatever turn was active
when the stop was processed.

## Robustness facts (all observed, all must not crash the daemon)

- `SubagentStop` with `agent_type: ""`, empty `last_assistant_message`, and **no
  preceding `SubagentStart`** (`SubagentStop-0065` — apparently an internal system
  agent). Upsert-or-ignore; never assume Start precedes Stop.
- `SessionStart` with no subsequent events (0001), and `SessionEnd` for a session
  never seen before (0050). Ghost sessions exist on both ends.
- Two sessions can interleave arbitrarily; ordering guarantees hold only per session
  (and not even fully there — see SubagentStop-before-PostToolUse above).
- `Stop` and `SubagentStop` carry `background_tasks`; a `SubagentStop` can list
  *itself* as still `running` in that array (`SubagentStop-0043`).
- Hook config **hot-reloads**: hooks installed mid-session applied to an
  already-running session's subagents (0051 came from the session that installed
  the hooks).

## Divergences from the official docs (docs re-checked 2026-07-17)

The docs/SDK reference claims, for current versions:
- `SubagentStart` includes `agent_prompt` and `agent_tools` — **not observed** (v2.1.199).
- `Notification` field is `type` — observed field is **`notification_type`**.
- `StopFailure` field is `error_type` — observed field is **`error`**.
- "All events support `type: "http"`", including `SessionStart` — **false as observed**:
  a `type: "http"` hook on `SessionStart` silently never fired (tested twice);
  the curl `command` hook works. The spec's §6 constraint stands.
- Docs also confirm: `PermissionRequest` hooks do not fire in `-p` mode; HTTP hook
  non-2xx / timeout / connection failure are non-blocking; a 2xx **text body is
  injected as context** (hence the empty-204 contract).

Newer event names exist (`PostToolUseFailure`, `PermissionRequest`, `TeammateIdle`,
`CwdChanged`, …) — not needed for v1, but the event set is confirmed to be moving.

## Not captured (and why)

- `Notification` with `agent_needs_input` / `agent_completed` — needs an interactive
  session with background agents left unattended; not reproducible headlessly here.
  Handle by matching on `notification_type` string, tolerate unknown values.
- `SessionEnd` reasons other than `"other"` (`clear`, `logout`, …).
- `Setup` (fires only with `--init`/`--maintenance`; irrelevant to v1).
- Subagent-level `Notification` (permission request *inside* a subagent): docs say
  `agent_id` is populated on hooks inside subagents; unverified for Notification.

## Spec corrections (observations win)

1. §4 data model: add parent-edge source = `PostToolUse[Agent].agent_id` (nesting);
   `session_id`-only parenting is the fallback, not the whole story.
2. §7: `UserPromptSubmit` with a `<task-notification>` prompt is a *machine* turn —
   root goes `Working` again; fine, but don't treat it as human input. No
   subagent-side `UserPromptSubmit` observed, so the "never create on
   UserPromptSubmit with agent_id" rule stays as pure defense.
3. §8: metrics for background agents come from the task-notification prompt, not
   from `PostToolUse`. `PostToolUse[Agent]` with metrics only happens for
   foreground (`run_in_background: false`) launches.
4. §8: `tool_response.toolStats` (bash/edit/read/search counts, lines added/removed)
   exists and is free — worth surfacing later.
