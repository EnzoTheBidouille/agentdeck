//! In-memory session/subagent tree, driven by raw hook payloads.
//!
//! Everything here is defensive: payloads are untrusted JSON, events arrive
//! out of order (SubagentStop before SubagentStart, SessionEnd for sessions
//! never seen — all observed, see docs/OBSERVED.md), and missed events are
//! expected. `apply` never panics; unknown shapes are dropped.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Ordered by severity: `max()` over nodes gives the tray aggregate
/// (Blocked > Working > Error > Done > Idle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Done,
    Error,
    Working,
    Blocked,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    /// None = root (session). For subagents this is the session_id, upgraded
    /// to the launching agent's id when a PostToolUse[Agent] proves nesting.
    pub parent: Option<String>,
    pub agent_type: Option<String>,
    pub cwd: Option<String>,
    pub status: Status,
    pub started_at_ms: u64,
    pub started: Instant,
    pub last_event: Instant,
    pub tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub tool_calls: Option<u64>,
    pub model: Option<String>,
}

impl Node {
    fn new(id: &str, parent: Option<&str>, status: Status) -> Self {
        let now = Instant::now();
        Node {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            agent_type: None,
            cwd: None,
            status,
            started_at_ms: epoch_ms(),
            started: now,
            last_event: now,
            tokens: None,
            duration_ms: None,
            tool_calls: None,
            model: None,
        }
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Non-empty string field helper: Claude Code emits `agent_type: ""` for
/// internal agents — treat empty as absent.
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

#[derive(Default)]
pub struct Store {
    nodes: HashMap<String, Node>,
}

impl Store {
    pub fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn upsert_root(&mut self, session_id: &str, cwd: Option<&str>, status: Status) {
        let node = self
            .nodes
            .entry(session_id.to_string())
            .or_insert_with(|| Node::new(session_id, None, status));
        node.status = status;
        node.last_event = Instant::now();
        if let Some(cwd) = cwd {
            node.cwd = Some(cwd.to_string());
        }
    }

    fn touch(&mut self, id: &str, status: Option<Status>) -> bool {
        match self.nodes.get_mut(id) {
            Some(node) => {
                node.last_event = Instant::now();
                if let Some(s) = status {
                    node.status = s;
                }
                true
            }
            None => false,
        }
    }

    /// Apply one raw hook payload. Returns true if state changed.
    pub fn apply(&mut self, event: &Value) -> bool {
        let Some(ev) = str_field(event, "hook_event_name") else {
            return false;
        };
        let Some(session_id) = str_field(event, "session_id") else {
            return false;
        };
        let agent_id = str_field(event, "agent_id");
        let agent_type = str_field(event, "agent_type");
        let cwd = str_field(event, "cwd");

        match ev {
            "SessionStart" => {
                self.upsert_root(session_id, cwd, Status::Idle);
                if let Some(model) = str_field(event, "model") {
                    if let Some(n) = self.nodes.get_mut(session_id) {
                        n.model = Some(model.to_string());
                    }
                }
                true
            }
            "UserPromptSubmit" => {
                match agent_id {
                    // Defensive (ghost-session trap, spec §7): never create a
                    // node for a subagent-side prompt.
                    Some(aid) => self.touch(aid, Some(Status::Working)),
                    None => {
                        self.upsert_root(session_id, cwd, Status::Working);
                        // Background-agent completion re-wakes the root with a
                        // <task-notification> prompt that carries the only
                        // metrics we'll ever get for async agents.
                        if let Some(prompt) = str_field(event, "prompt") {
                            self.enrich_from_task_notification(prompt);
                        }
                        true
                    }
                }
            }
            "SubagentStart" => {
                let Some(aid) = agent_id else { return false };
                self.upsert_root(session_id, cwd, Status::Working);
                let node = self
                    .nodes
                    .entry(aid.to_string())
                    .or_insert_with(|| Node::new(aid, Some(session_id), Status::Working));
                node.status = Status::Working;
                node.last_event = Instant::now();
                node.agent_type = agent_type.map(str::to_string).or(node.agent_type.take());
                node.cwd = cwd.map(str::to_string).or(node.cwd.take());
                true
            }
            "SubagentStop" => {
                let Some(aid) = agent_id else { return false };
                if self.touch(aid, Some(Status::Done)) {
                    return true;
                }
                // Stop without Start: create only if it looks like a real
                // agent (empty agent_type = internal system agent, observed).
                match agent_type {
                    Some(at) => {
                        let mut node = Node::new(aid, Some(session_id), Status::Done);
                        node.agent_type = Some(at.to_string());
                        node.cwd = cwd.map(str::to_string);
                        self.nodes.insert(aid.to_string(), node);
                        true
                    }
                    None => false,
                }
            }
            "Stop" => {
                let key = agent_id.unwrap_or(session_id);
                match agent_id {
                    Some(_) => self.touch(key, Some(Status::Done)),
                    None => {
                        self.upsert_root(session_id, cwd, Status::Done);
                        true
                    }
                }
            }
            "StopFailure" => {
                let key = agent_id.unwrap_or(session_id);
                match agent_id {
                    Some(_) => self.touch(key, Some(Status::Error)),
                    None => {
                        self.upsert_root(session_id, cwd, Status::Error);
                        true
                    }
                }
            }
            "Notification" => {
                let status = match str_field(event, "notification_type") {
                    Some("permission_prompt" | "idle_prompt" | "agent_needs_input") => {
                        Some(Status::Blocked)
                    }
                    Some("agent_completed") => Some(Status::Done),
                    _ => None, // unknown type: touch only
                };
                match agent_id {
                    Some(aid) => self.touch(aid, status),
                    None => {
                        if status.is_some() || self.nodes.contains_key(session_id) {
                            self.upsert_root(session_id, cwd, Status::Idle);
                            if let (Some(s), Some(n)) = (status, self.nodes.get_mut(session_id)) {
                                n.status = s;
                            }
                            true
                        } else {
                            false
                        }
                    }
                }
            }
            "PreToolUse" => {
                if str_field(event, "tool_name") != Some("Agent") {
                    return false;
                }
                self.touch(agent_id.unwrap_or(session_id), Some(Status::Working))
            }
            "PostToolUse" => {
                if str_field(event, "tool_name") != Some("Agent") {
                    return false;
                }
                let launcher = agent_id.unwrap_or(session_id);
                let mut changed = self.touch(launcher, Some(Status::Working));
                if let Some(resp) = event.get("tool_response") {
                    changed |= self.apply_agent_response(launcher, resp);
                }
                changed
            }
            "SessionEnd" => self.remove_tree(session_id),
            _ => false,
        }
    }

    /// PostToolUse[Agent] tool_response: proves the parent edge (launcher →
    /// tool_response.agentId) and, for foreground completions, carries the
    /// full metrics (spec §8).
    fn apply_agent_response(&mut self, launcher: &str, resp: &Value) -> bool {
        let Some(child_id) = str_field(resp, "agentId") else {
            return false;
        };
        if child_id == launcher {
            return false;
        }
        let completed = str_field(resp, "status") == Some("completed");
        if !self.nodes.contains_key(child_id) {
            // Child already gone (reaped / stopped long ago): don't resurrect.
            return false;
        }
        let launcher_exists = self.nodes.contains_key(launcher);
        let node = self.nodes.get_mut(child_id).expect("checked above");
        if launcher_exists {
            node.parent = Some(launcher.to_string());
        }
        node.last_event = Instant::now();
        if let Some(at) = str_field(resp, "agentType") {
            node.agent_type = Some(at.to_string());
        }
        if let Some(m) = str_field(resp, "resolvedModel") {
            node.model = Some(m.to_string());
        }
        if completed {
            node.status = Status::Done;
            node.tokens = resp.get("totalTokens").and_then(Value::as_u64).or(node.tokens);
            node.duration_ms = resp
                .get("totalDurationMs")
                .and_then(Value::as_u64)
                .or(node.duration_ms);
            node.tool_calls = resp
                .get("totalToolUseCount")
                .and_then(Value::as_u64)
                .or(node.tool_calls);
        }
        true
    }

    /// Best-effort parse of the `<task-notification>` block a background
    /// agent's completion injects as a root prompt (docs/OBSERVED.md):
    /// `<task-id>ID</task-id>` + `<usage><subagent_tokens>N</subagent_tokens>
    /// <tool_uses>N</tool_uses><duration_ms>N</duration_ms></usage>`.
    fn enrich_from_task_notification(&mut self, prompt: &str) {
        if !prompt.contains("<task-notification>") {
            return;
        }
        let Some(task_id) = extract_tag(prompt, "task-id") else {
            return;
        };
        let Some(node) = self.nodes.get_mut(task_id) else {
            return;
        };
        node.last_event = Instant::now();
        if let Some(t) = extract_tag(prompt, "subagent_tokens").and_then(|s| s.parse().ok()) {
            node.tokens = Some(t);
        }
        if let Some(t) = extract_tag(prompt, "tool_uses").and_then(|s| s.parse().ok()) {
            node.tool_calls = Some(t);
        }
        if let Some(t) = extract_tag(prompt, "duration_ms").and_then(|s| s.parse().ok()) {
            node.duration_ms = Some(t);
        }
    }

    /// Remove a node and every transitive descendant.
    pub fn remove_tree(&mut self, root: &str) -> bool {
        if !self.nodes.contains_key(root) {
            return false;
        }
        let mut doomed: Vec<String> = vec![root.to_string()];
        let mut i = 0;
        while i < doomed.len() {
            let parent = doomed[i].clone();
            for (id, node) in &self.nodes {
                if node.parent.as_deref() == Some(parent.as_str()) && !doomed.contains(id) {
                    doomed.push(id.clone());
                }
            }
            i += 1;
        }
        for id in doomed {
            self.nodes.remove(&id);
        }
        true
    }

    /// The reaper's net for missed events (spec §7 — mandatory):
    /// - any node silent longer than `stale_ttl` is purged (with its subtree);
    /// - finished (Done/Error) *subagent* nodes are purged after `done_ttl`
    ///   so they linger briefly then vanish; roots stay until SessionEnd.
    /// - orphans (parent no longer present) are purged immediately.
    pub fn purge(&mut self, stale_ttl: Duration, done_ttl: Duration) -> bool {
        let doomed: Vec<String> = self
            .nodes
            .values()
            .filter(|n| {
                let silent = n.last_event.elapsed();
                let orphaned = n
                    .parent
                    .as_deref()
                    .is_some_and(|p| !self.nodes.contains_key(p));
                let finished_child = n.parent.is_some()
                    && matches!(n.status, Status::Done | Status::Error)
                    && silent >= done_ttl;
                silent >= stale_ttl || orphaned || finished_child
            })
            .map(|n| n.id.clone())
            .collect();
        let mut changed = false;
        for id in doomed {
            changed |= self.remove_tree(&id);
        }
        changed
    }

    pub fn aggregate(&self) -> Status {
        self.nodes
            .values()
            .map(|n| n.status)
            .max()
            .unwrap_or(Status::Idle)
    }

    /// Full state as one JSON value — what SSE pushes and /state serves.
    pub fn snapshot(&self) -> Value {
        let mut nodes: Vec<&Node> = self.nodes.values().collect();
        nodes.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms).then(a.id.cmp(&b.id)));
        serde_json::json!({
            "aggregate": self.aggregate(),
            "nodes": nodes.iter().map(|n| serde_json::json!({
                "id": n.id,
                "parent": n.parent,
                "agent_type": n.agent_type,
                "cwd": n.cwd,
                "status": n.status,
                "started_at_ms": n.started_at_ms,
                "age_ms": n.started.elapsed().as_millis() as u64,
                "tokens": n.tokens,
                "duration_ms": n.duration_ms,
                "tool_calls": n.tool_calls,
                "model": n.model,
            })).collect::<Vec<_>>(),
        })
    }
}

fn extract_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}
