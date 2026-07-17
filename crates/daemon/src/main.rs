use agentdeck_daemon::server::{App, Config};

#[tokio::main]
async fn main() {
    let config = Config::from_env();
    let app = App::spawn(config);
    // Loopback only (spec §5): hook payloads carry cwd, prompts and file
    // paths — they must not be reachable off the machine.
    let addr = "127.0.0.1:4747";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!(
        "agentdeck daemon on http://{addr} (hook: POST /hook, state: GET /state, sse: GET /events; stale ttl {}s, done ttl {}s)",
        config.stale_ttl.as_secs(),
        config.done_ttl.as_secs()
    );
    axum::serve(listener, app.router()).await.unwrap();
}
