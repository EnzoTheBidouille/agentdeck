//! Phase 0 discovery server.
//!
//! Accepts every hook payload on POST /hook, prints it, and dumps it to
//! docs/payloads/<event>-<seq>.json for later inspection. Always responds
//! 204 No Content with an empty body (a text body would be injected into
//! Claude's context).

use axum::{body::Bytes, http::StatusCode, routing::post, Router};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn dump_dir() -> PathBuf {
    std::env::var("AGENTDECK_DUMP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("docs/payloads"))
}

async fn hook(body: Bytes) -> StatusCode {
    // Nothing but a channel-free spawn here: parse + write happen off the
    // response path so the 204 goes out immediately.
    tokio::spawn(async move {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let (event, pretty) = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => {
                let event = v
                    .get("hook_event_name")
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let pretty = serde_json::to_string_pretty(&v).unwrap_or_default();
                (event, pretty)
            }
            Err(e) => {
                eprintln!("[{seq}] unparseable body ({e}): {:?}", String::from_utf8_lossy(&body));
                ("invalid".to_string(), String::from_utf8_lossy(&body).into_owned())
            }
        };
        println!("=== [{seq}] {event} ===\n{pretty}\n");
        let dir = dump_dir();
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            eprintln!("cannot create {}: {e}", dir.display());
            return;
        }
        let path = dir.join(format!("{event}-{seq:04}.json"));
        if let Err(e) = tokio::fs::write(&path, pretty).await {
            eprintln!("cannot write {}: {e}", path.display());
        }
    });
    StatusCode::NO_CONTENT
}

#[tokio::main]
async fn main() {
    let app = Router::new().route("/hook", post(hook));
    let addr = "127.0.0.1:4747";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("discovery server listening on http://{addr}/hook, dumping to {}", dump_dir().display());
    axum::serve(listener, app).await.unwrap();
}
