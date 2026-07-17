//! Transition tests driven by the real payloads captured in Phase 0
//! (docs/payloads). Session/agent ids below are the ones in those fixtures.

use agentdeck_daemon::state::{Status, Store};
use serde_json::{json, Value};
use std::time::Duration;

const ROOT_FG: &str = "097177f4-db9c-4cbd-b942-664dc6733cac"; // foreground-subagent session
const AGENT_FG: &str = "a48ec88ddb26aeff3";
const ROOT_BG: &str = "9a584be0-ccaf-4fd3-930b-68bee4b41540"; // background + nested session
const AGENT_BG: &str = "ad1a5833a3115f5d1"; // general-purpose, background
const AGENT_NESTED: &str = "af1003f94e383f3ae"; // Explore, launched by AGENT_BG

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../docs/payloads/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn apply_fixtures(store: &mut Store, names: &[&str]) {
    for name in names {
        store.apply(&fixture(name));
    }
}

#[test]
fn root_session_lifecycle() {
    let mut store = Store::default();

    assert!(store.apply(&fixture("SessionStart-0006")));
    assert_eq!(store.get(ROOT_FG).unwrap().status, Status::Idle);
    assert_eq!(
        store.get(ROOT_FG).unwrap().cwd.as_deref(),
        Some("/home/user/agentdeck")
    );

    store.apply(&fixture("UserPromptSubmit-0007"));
    assert_eq!(store.get(ROOT_FG).unwrap().status, Status::Working);

    store.apply(&fixture("Stop-0012"));
    assert_eq!(store.get(ROOT_FG).unwrap().status, Status::Done);

    store.apply(&fixture("SessionEnd-0013"));
    assert!(store.is_empty());
}

#[test]
fn foreground_subagent_lifecycle_and_metrics() {
    let mut store = Store::default();
    apply_fixtures(
        &mut store,
        &["SessionStart-0006", "UserPromptSubmit-0007", "PreToolUse-0008"],
    );

    store.apply(&fixture("SubagentStart-0009"));
    let agent = store.get(AGENT_FG).unwrap();
    assert_eq!(agent.status, Status::Working);
    assert_eq!(agent.parent.as_deref(), Some(ROOT_FG));
    assert_eq!(agent.agent_type.as_deref(), Some("general-purpose"));

    store.apply(&fixture("SubagentStop-0010"));
    assert_eq!(store.get(AGENT_FG).unwrap().status, Status::Done);

    // Foreground completion: PostToolUse[Agent] carries the full metrics.
    store.apply(&fixture("PostToolUse-0011"));
    let agent = store.get(AGENT_FG).unwrap();
    assert_eq!(agent.tokens, Some(14145));
    assert_eq!(agent.duration_ms, Some(9717));
    assert_eq!(agent.tool_calls, Some(2));
    assert_eq!(agent.model.as_deref(), Some("claude-haiku-4-5-20251001"));

    // SessionEnd removes the whole tree, children included.
    store.apply(&fixture("SessionEnd-0013"));
    assert!(store.is_empty());
}

#[test]
fn nested_subagent_gets_true_parent_edge() {
    let mut store = Store::default();
    apply_fixtures(
        &mut store,
        &[
            "SessionStart-0034",
            "UserPromptSubmit-0035",
            "PreToolUse-0036",
            "SubagentStart-0037",  // root launches general-purpose (background)
            "PostToolUse-0038",    // async_launched, no metrics
            "PreToolUse-0040",     // inside AGENT_BG: launches Explore
            "SubagentStart-0041",  // Explore starts (payload only names the session)
            "PostToolUse-0042",    // proves AGENT_BG -> AGENT_NESTED edge
        ],
    );

    // session_id-only parenting would put the Explore agent at depth 1;
    // the PostToolUse[Agent] fired inside AGENT_BG upgrades the edge.
    assert_eq!(
        store.get(AGENT_NESTED).unwrap().parent.as_deref(),
        Some(AGENT_BG)
    );
    assert_eq!(store.get(AGENT_BG).unwrap().parent.as_deref(), Some(ROOT_BG));
    // async_launched must not fabricate metrics.
    assert_eq!(store.get(AGENT_BG).unwrap().tokens, None);
}

#[test]
fn task_notification_backfills_background_agent_metrics() {
    let mut store = Store::default();
    apply_fixtures(
        &mut store,
        &[
            "SessionStart-0034",
            "UserPromptSubmit-0035",
            "SubagentStart-0037",
            "SubagentStop-0043",
        ],
    );
    assert_eq!(store.get(AGENT_BG).unwrap().tokens, None);

    // Background agents never get a metrics-bearing PostToolUse; the
    // <task-notification> re-wake prompt is the only channel.
    store.apply(&fixture("UserPromptSubmit-0044"));
    let agent = store.get(AGENT_BG).unwrap();
    assert_eq!(agent.tokens, Some(14473));
    assert_eq!(agent.tool_calls, Some(3));
    assert_eq!(agent.duration_ms, Some(16042));
    // The task-notification is a machine turn but still a root turn.
    assert_eq!(store.get(ROOT_BG).unwrap().status, Status::Working);
}

/// THE anti-zombie test (spec §11): a SubagentStart with no SubagentStop must
/// not outlive the TTL.
#[tokio::test(start_paused = true)]
async fn reaper_purges_zombie_subagent() {
    let stale = Duration::from_secs(300);
    let done = Duration::from_secs(60);
    let mut store = Store::default();
    apply_fixtures(&mut store, &["SessionStart-0006", "SubagentStart-0009"]);

    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(!store.purge(stale, done), "nothing should be purged before TTL");
    assert!(store.get(AGENT_FG).is_some());

    tokio::time::advance(Duration::from_secs(101)).await; // total 301s silent
    assert!(store.purge(stale, done));
    assert!(store.is_empty(), "zombie subagent (and stale root) must be gone");
}

#[tokio::test(start_paused = true)]
async fn reaper_expires_done_children_but_keeps_live_root() {
    let stale = Duration::from_secs(300);
    let done = Duration::from_secs(60);
    let mut store = Store::default();
    apply_fixtures(
        &mut store,
        &["SessionStart-0006", "SubagentStart-0009", "SubagentStop-0010"],
    );

    tokio::time::advance(Duration::from_secs(61)).await;
    // Keep the root alive with a fresh event.
    store.apply(&fixture("UserPromptSubmit-0007"));
    assert!(store.purge(stale, done));
    assert!(store.get(AGENT_FG).is_none(), "Done child expires after done_ttl");
    assert!(store.get(ROOT_FG).is_some(), "root stays until SessionEnd");
}

#[test]
fn internal_subagent_stop_with_empty_type_is_ignored() {
    let mut store = Store::default();
    // Observed payload: agent_type "", no SubagentStart ever fired for it.
    assert!(!store.apply(&fixture("SubagentStop-0065")));
    assert!(store.is_empty());
}

#[test]
fn subagent_stop_without_start_creates_done_node() {
    let mut store = Store::default();
    // Real agent type but its Start was missed: register it as Done so the
    // done_ttl reaps it, rather than dropping evidence.
    assert!(store.apply(&fixture("SubagentStop-0046")));
    assert_eq!(store.get(AGENT_NESTED).unwrap().status, Status::Done);
}

#[test]
fn ghost_session_end_is_a_noop() {
    let mut store = Store::default();
    assert!(!store.apply(&fixture("SessionEnd-0050")));
    assert!(store.is_empty());
}

#[test]
fn error_and_blocked_statuses() {
    let mut store = Store::default();
    apply_fixtures(&mut store, &["SessionStart-0052", "UserPromptSubmit-0053"]);
    store.apply(&fixture("StopFailure-0054"));
    assert_eq!(
        store
            .get("4c101925-e94e-43ba-905c-a3f0fd3b3940")
            .unwrap()
            .status,
        Status::Error
    );

    let mut store = Store::default();
    apply_fixtures(&mut store, &["SessionStart-0056", "UserPromptSubmit-0057"]);
    store.apply(&fixture("Notification-0058")); // permission_prompt
    assert_eq!(
        store
            .get("2221611f-bb00-482e-bab7-39872827d065")
            .unwrap()
            .status,
        Status::Blocked
    );
}

#[test]
fn aggregate_is_worst_status() {
    let mut store = Store::default();
    assert_eq!(store.aggregate(), Status::Idle);

    apply_fixtures(&mut store, &["SessionStart-0006", "SubagentStart-0009"]);
    assert_eq!(store.aggregate(), Status::Working);

    // Subagent needs input -> Blocked dominates Working.
    store.apply(&json!({
        "hook_event_name": "Notification",
        "session_id": ROOT_FG,
        "agent_id": AGENT_FG,
        "notification_type": "agent_needs_input",
        "message": "Agent needs your input",
    }));
    assert_eq!(store.aggregate(), Status::Blocked);
}

#[test]
fn subagent_prompt_never_creates_a_node() {
    let mut store = Store::default();
    // Ghost-session trap (spec §7): UserPromptSubmit with agent_id must not
    // create anything, even though we never saw this agent.
    assert!(!store.apply(&json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "some-session",
        "agent_id": "some-agent",
        "prompt": "hi",
    })));
    assert!(store.is_empty());
}

#[test]
fn malformed_payloads_never_panic() {
    let mut store = Store::default();
    for v in [
        json!({}),
        json!(null),
        json!("garbage"),
        json!([1, 2, 3]),
        json!({"hook_event_name": 42}),
        json!({"hook_event_name": "SubagentStart"}), // no session_id
        json!({"hook_event_name": "SubagentStart", "session_id": "s"}), // no agent_id
        json!({"hook_event_name": "TotallyNewEvent", "session_id": "s", "future_field": {"x": 1}}),
        json!({"hook_event_name": "PostToolUse", "session_id": "s", "tool_name": "Agent", "tool_response": "not-an-object"}),
    ] {
        assert!(!store.apply(&v), "must be a rejected no-op: {v}");
    }
    assert!(store.is_empty());
}
