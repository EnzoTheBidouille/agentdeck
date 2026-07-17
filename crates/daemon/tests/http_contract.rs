//! Endpoint contract tests (spec §5, §11): /hook always answers 204 with an
//! EMPTY body — a non-empty 2xx body would be injected into Claude's context
//! and is a high-severity bug.

use agentdeck_daemon::server::{App, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::time::Duration;
use tower::ServiceExt;

fn fixture_body(name: &str) -> String {
    let path = format!(
        "{}/../../docs/payloads/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn post_hook(body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/hook")
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

async fn assert_204_empty(app: &App, body: &str) {
    let response = app.router().oneshot(post_hook(body.to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        bytes.is_empty(),
        "hook response body MUST be empty, got: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

/// Wait until the async consumer has applied whatever was posted.
async fn wait_for_nodes(app: &App, want: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if app.store.read().await.len() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never reached {want} nodes"));
}

#[tokio::test]
async fn hook_responds_204_empty_and_state_updates() {
    let app = App::spawn(Config::default());
    assert_204_empty(&app, &fixture_body("SessionStart-0006")).await;
    assert_204_empty(&app, &fixture_body("SubagentStart-0009")).await;
    wait_for_nodes(&app, 2).await;

    let response = app
        .router()
        .oneshot(Request::builder().uri("/state").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot["aggregate"], "working");
    assert_eq!(snapshot["nodes"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn hook_survives_garbage_and_keeps_working() {
    let app = App::spawn(Config::default());
    assert_204_empty(&app, "not json at all {{{").await;
    assert_204_empty(&app, "").await;
    assert_204_empty(&app, r#"{"hook_event_name": 42, "session_id": null}"#).await;
    // Still alive and processing after garbage:
    assert_204_empty(&app, &fixture_body("SessionStart-0006")).await;
    wait_for_nodes(&app, 1).await;
}

#[tokio::test]
async fn sse_sends_snapshot_on_connect() {
    let app = App::spawn(Config::default());
    assert_204_empty(&app, &fixture_body("SessionStart-0006")).await;
    wait_for_nodes(&app, 1).await;

    let response = app
        .router()
        .oneshot(Request::builder().uri("/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("no SSE frame within 2s")
        .expect("stream ended")
        .expect("stream error");
    let text = String::from_utf8_lossy(first.data_ref().unwrap());
    assert!(text.contains("event: state"), "got: {text}");
    assert!(text.contains("\"aggregate\":\"idle\""), "got: {text}");
}
