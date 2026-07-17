//! agentdeck tray shell.
//!
//! The Rust side owns the daemon connection: it consumes the SSE stream at
//! 127.0.0.1:4747/events, keeps the tray icon in sync with the aggregate
//! state (the tray must be live even with the popover closed), and forwards
//! snapshots to the webview as `state` events. The webview never talks to the
//! daemon directly — no CORS, one source of truth.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

const DAEMON_EVENTS_URL: &str = "http://127.0.0.1:4747/events";
const TRAY_ID: &str = "agentdeck-tray";

/// Set by `--window`: the window is the primary surface (no usable tray, e.g.
/// WSLg), so losing focus must not hide it — there'd be no way back.
static WINDOW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct Cache(Mutex<Value>);

fn unreachable_state() -> Value {
    json!({ "connected": false, "aggregate": "unreachable", "nodes": [] })
}

#[tauri::command]
fn get_state(cache: State<'_, Cache>) -> Value {
    cache.0.lock().expect("cache lock").clone()
}

const HOOK_URL: &str = "http://127.0.0.1:4747/hook";

/// Merge agentdeck's hook config into a settings.json value, preserving
/// everything else. Idempotent: our entries are recognized by URL and
/// replaced, never duplicated; other tools' hooks are untouched.
fn merge_hooks(settings: &mut Value) -> Result<(), String> {
    let obj = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or("settings.json \"hooks\" is not an object")?;

    let is_ours = |entry: &Value| -> bool {
        entry["hooks"].as_array().is_some_and(|hs| {
            hs.iter().any(|h| {
                h["url"].as_str().is_some_and(|u| u.contains(HOOK_URL))
                    || h["command"].as_str().is_some_and(|c| c.contains(HOOK_URL))
            })
        })
    };

    let http_events: [(&str, Option<&str>); 9] = [
        ("SubagentStart", None),
        ("SubagentStop", None),
        ("UserPromptSubmit", None),
        ("Stop", None),
        ("StopFailure", None),
        ("Notification", None),
        ("PostToolUse", Some("Agent")),
        ("PreToolUse", Some("Agent")),
        ("SessionEnd", None),
    ];
    for (event, matcher) in http_events {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        let arr = list
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} is not an array"))?;
        arr.retain(|e| !is_ours(e));
        let mut entry = json!({ "hooks": [{ "type": "http", "url": HOOK_URL, "timeout": 5 }] });
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        arr.push(entry);
    }

    // SessionStart does not support type:"http" (verified empirically,
    // docs/OBSERVED.md) — curl the payload through a command hook instead.
    let curl = format!(
        "curl -s --max-time 1 -X POST -H 'Content-Type: application/json' \
         --data-binary @- {HOOK_URL} > /dev/null 2>&1; exit 0"
    );
    let list = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    let arr = list
        .as_array_mut()
        .ok_or("hooks.SessionStart is not an array")?;
    arr.retain(|e| !is_ours(e));
    arr.push(json!({ "hooks": [{ "type": "command", "command": curl, "timeout": 5 }] }));
    Ok(())
}

#[tauri::command]
fn install_hooks() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "cannot resolve $HOME".to_string())?;
    let path = std::path::Path::new(&home).join(".claude").join("settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut settings: Value =
        serde_json::from_str(&text).map_err(|e| format!("settings.json is not valid JSON: {e}"))?;
    if path.exists() {
        let backup = path.with_file_name("settings.json.agentdeck-backup");
        std::fs::copy(&path, &backup).map_err(|e| format!("backup failed: {e}"))?;
    }
    merge_hooks(&mut settings)?;
    let out = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, out).map_err(|e| format!("cannot write settings.json: {e}"))?;
    Ok("Hooks installed (backup: settings.json.agentdeck-backup). New sessions will report in.".to_string())
}

/// 32×32 antialiased disc in the status color; hollow ring when the daemon
/// is unreachable. Built at runtime — no asset files to ship.
fn status_icon(aggregate: &str) -> Image<'static> {
    let rgb: [u8; 3] = match aggregate {
        "blocked" => [0xf5, 0x9e, 0x0b],
        "working" => [0x3b, 0x82, 0xf6],
        "error" => [0xef, 0x44, 0x44],
        "done" => [0x22, 0xc5, 0x5e],
        "idle" => [0x9c, 0xa3, 0xaf],
        _ => [0x9c, 0xa3, 0xaf], // unreachable: gray ring
    };
    let hollow = !matches!(aggregate, "blocked" | "working" | "error" | "done" | "idle");
    const S: usize = 32;
    let (center, radius) = ((S as f32 - 1.0) / 2.0, 12.0_f32);
    let mut px = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            let alpha = if hollow {
                // 3px ring
                (radius + 0.5 - d).clamp(0.0, 1.0) * (d - (radius - 3.5)).clamp(0.0, 1.0)
            } else {
                (radius + 0.5 - d).clamp(0.0, 1.0)
            };
            let i = (y * S + x) * 4;
            px[i] = rgb[0];
            px[i + 1] = rgb[1];
            px[i + 2] = rgb[2];
            px[i + 3] = (alpha * 255.0) as u8;
        }
    }
    Image::new_owned(px, S as u32, S as u32)
}

fn tray_tooltip(state: &Value) -> String {
    if state["connected"] != json!(true) {
        return "agentdeck — daemon unreachable".to_string();
    }
    let nodes = state["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        return "agentdeck — no agents".to_string();
    }
    let count = |s: &str| nodes.iter().filter(|n| n["status"] == json!(s)).count();
    let mut parts = Vec::new();
    for status in ["blocked", "working", "error", "done", "idle"] {
        let c = count(status);
        if c > 0 {
            parts.push(format!("{c} {status}"));
        }
    }
    format!("agentdeck — {}", parts.join(", "))
}

/// Publish a new state everywhere: cache (for get_state), webview, tray.
fn publish(app: &AppHandle, state: Value) {
    let aggregate = state["aggregate"].as_str().unwrap_or("unreachable").to_string();
    if let Some(cache) = app.try_state::<Cache>() {
        *cache.0.lock().expect("cache lock") = state.clone();
    }
    let _ = app.emit("state", &state);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(status_icon(&aggregate)));
        let _ = tray.set_tooltip(Some(tray_tooltip(&state)));
    }
}

fn wrap_snapshot(snapshot: Value) -> Value {
    json!({
        "connected": true,
        "aggregate": snapshot.get("aggregate").cloned().unwrap_or(json!("idle")),
        "nodes": snapshot.get("nodes").cloned().unwrap_or(json!([])),
    })
}

/// Consume the daemon's SSE stream forever; on any failure mark the state
/// unreachable and retry every 3s. Daemon restarts are transparent.
async fn sse_loop(app: AppHandle) {
    eprintln!("agentdeck: sse loop starting ({DAEMON_EVENTS_URL})");
    let client = reqwest::Client::new();
    let mut was_connected = true; // log the first failure
    loop {
        let connected = async {
            let resp = match client.get(DAEMON_EVENTS_URL).send().await {
                Ok(r) => r,
                Err(e) => {
                    if was_connected {
                        eprintln!("agentdeck: daemon unreachable: {e}");
                    }
                    return None;
                }
            };
            if !resp.status().is_success() {
                if was_connected {
                    eprintln!("agentdeck: daemon answered {}", resp.status());
                }
                return None;
            }
            eprintln!("agentdeck: connected to daemon");
            let mut buf = String::new();
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.ok()?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // SSE frames are blank-line separated; each carries one full
                // snapshot on a single `data:` line (daemon serializes flat).
                while let Some(pos) = buf.find("\n\n") {
                    let frame: String = buf.drain(..pos + 2).collect();
                    for line in frame.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            if let Ok(snap) = serde_json::from_str::<Value>(data) {
                                publish(&app, wrap_snapshot(snap));
                            }
                        }
                    }
                }
            }
            Some(())
        }
        .await;
        was_connected = connected.is_some();
        publish(&app, unreachable_state());
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn toggle_popover(app: &AppHandle, click_position: Option<tauri::PhysicalPosition<f64>>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    if let Some(pos) = click_position {
        let size = window.outer_size().unwrap_or(tauri::PhysicalSize::new(380, 460));
        // Near the tray click, clamped so the popover stays on screen.
        let (mut x, mut y) = (pos.x - size.width as f64 / 2.0, pos.y + 8.0);
        if let Ok(Some(monitor)) = window.current_monitor() {
            let m = monitor.size();
            x = x.clamp(0.0, (m.width.saturating_sub(size.width)) as f64);
            y = y.clamp(0.0, (m.height.saturating_sub(size.height)) as f64);
        }
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_foreign_hooks_and_is_idempotent() {
        let mut settings = json!({
            "model": "claude-fable-5[1m]",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "python3 gate.py" }] }
                ]
            }
        });
        merge_hooks(&mut settings).unwrap();
        let once = settings.clone();
        merge_hooks(&mut settings).unwrap();
        assert_eq!(settings, once, "second install must change nothing");

        // Foreign hook survived, ours was appended.
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "python3 gate.py");
        assert_eq!(pre[1]["matcher"], "Agent");
        assert_eq!(pre[1]["hooks"][0]["url"], HOOK_URL);
        // Unrelated top-level settings untouched.
        assert_eq!(settings["model"], "claude-fable-5[1m]");
        // SessionStart got the curl command hook, not http.
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["type"], "command");
        // All nine http events present.
        for ev in ["SubagentStart", "SubagentStop", "UserPromptSubmit", "Stop",
                   "StopFailure", "Notification", "PostToolUse", "SessionEnd"] {
            assert!(settings["hooks"][ev].as_array().unwrap().iter().any(
                |e| e["hooks"][0]["url"] == HOOK_URL), "missing {ev}");
        }
    }

    #[test]
    fn merge_rejects_malformed_settings() {
        assert!(merge_hooks(&mut json!([])).is_err());
        assert!(merge_hooks(&mut json!({ "hooks": "nope" })).is_err());
        // Must not have partially modified anything before erroring matters
        // little here (caller re-reads), but it must never panic.
    }
}

fn main() {
    tauri::Builder::default()
        .manage(Cache(Mutex::new(unreachable_state())))
        .invoke_handler(tauri::generate_handler![get_state, install_hooks])
        .setup(|app| {
            let quit = MenuItem::with_id(app, "quit", "Quit agentdeck", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(status_icon("unreachable"))
                .tooltip("agentdeck — starting…")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        position,
                        ..
                    } = event
                    {
                        toggle_popover(tray.app_handle(), Some(position));
                    }
                })
                .build(app)?;

            tauri::async_runtime::spawn(sse_loop(app.handle().clone()));

            // Escape hatch for environments with flaky tray support (WSLg):
            // `agentdeck-app --window` opens the popover straight away.
            if std::env::args().any(|a| a == "--window") {
                WINDOW_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Popover semantics: closing or losing focus hides, never quits.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            WindowEvent::Focused(false) => {
                if !WINDOW_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running agentdeck");
}
