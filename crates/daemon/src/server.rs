//! HTTP surface.
//!
//! POST /hook   — hook sink. ALWAYS 204 with an empty body, immediately: any
//!                2xx text body would be injected into Claude's context
//!                (spec §5). Parsing/state happen off the response path.
//! GET  /state  — current snapshot (UI bootstrap, debugging).
//! GET  /events — SSE: one snapshot on connect, then one per state change.

use crate::state::Store;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct App {
    pub store: Arc<RwLock<Store>>,
    events_tx: mpsc::UnboundedSender<Bytes>,
    bcast_tx: broadcast::Sender<String>,
}

#[derive(Clone, Copy)]
pub struct Config {
    pub stale_ttl: Duration,
    pub done_ttl: Duration,
    pub reap_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stale_ttl: Duration::from_secs(300),
            done_ttl: Duration::from_secs(60),
            reap_interval: Duration::from_secs(15),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let secs = |var: &str, default: Duration| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(default)
        };
        let d = Config::default();
        Config {
            stale_ttl: secs("AGENTDECK_TTL_SECS", d.stale_ttl),
            done_ttl: secs("AGENTDECK_DONE_TTL_SECS", d.done_ttl),
            reap_interval: d.reap_interval,
        }
    }
}

impl App {
    /// Build the app and spawn its two background tasks (event consumer,
    /// reaper). State only ever mutates in those tasks.
    pub fn spawn(config: Config) -> App {
        let store = Arc::new(RwLock::new(Store::default()));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Bytes>();
        let (bcast_tx, _) = broadcast::channel::<String>(64);

        let app = App {
            store: store.clone(),
            events_tx,
            bcast_tx: bcast_tx.clone(),
        };

        // Consumer: parse + apply off the hook response path.
        {
            let store = store.clone();
            let bcast_tx = bcast_tx.clone();
            tokio::spawn(async move {
                while let Some(body) = events_rx.recv().await {
                    let value = match serde_json::from_slice::<serde_json::Value>(&body) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("dropping unparseable payload: {e}");
                            continue;
                        }
                    };
                    let mut store = store.write().await;
                    if store.apply(&value) {
                        let _ = bcast_tx.send(store.snapshot().to_string());
                    }
                }
            });
        }

        // Reaper: the mandatory net for missed events (spec §7).
        {
            let store = store.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(config.reap_interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let mut store = store.write().await;
                    if store.purge(config.stale_ttl, config.done_ttl) {
                        let _ = bcast_tx.send(store.snapshot().to_string());
                    }
                }
            });
        }

        app
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/hook", post(hook))
            .route("/state", get(state))
            .route("/events", get(events))
            .with_state(self.clone())
    }
}

async fn hook(State(app): State<App>, body: Bytes) -> StatusCode {
    // No parsing, no locks, no I/O here — hand off and answer.
    let _ = app.events_tx.send(body);
    StatusCode::NO_CONTENT
}

async fn state(State(app): State<App>) -> impl IntoResponse {
    Json(app.store.read().await.snapshot())
}

async fn events(State(app): State<App>) -> impl IntoResponse {
    let initial = app.store.read().await.snapshot().to_string();
    let updates = BroadcastStream::new(app.bcast_tx.subscribe())
        // A lagged receiver just skips intermediate snapshots — each message
        // is a full snapshot, so dropping some loses nothing.
        .filter_map(|msg| msg.ok());
    let stream = tokio_stream::once(initial)
        .chain(updates)
        .map(|snap| Ok::<Event, Infallible>(Event::default().event("state").data(snap)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
