//! Shared harness for the integration tests: an in-memory bridge bound to
//! OS-assigned ports on localhost.
#![allow(dead_code)] // each test file uses a subset of these helpers

use std::sync::Arc;

use ai_agent_bridge::config::Config;
use ai_agent_bridge::embed::Embedder;
use ai_agent_bridge::state::AppState;
use ai_agent_bridge::workflow_security::WorkflowSecurity;
use ai_agent_bridge::{http, tcp};
use tokio::net::TcpListener;

/// A base config with a unique inbox dir (so claude-inbox tests don't collide).
pub fn base_config() -> Config {
    let mut cfg = Config::in_memory();
    cfg.inbox_dir = unique_tmp_dir();
    cfg
}

pub fn state() -> Arc<AppState> {
    state_with(base_config())
}

pub fn state_with(cfg: Config) -> Arc<AppState> {
    let embedder = Embedder::new(
        cfg.embed_dim,
        None,
        "local".into(),
        None,
        cfg.max_embedding_response_bytes,
    );
    AppState::new(cfg, embedder).unwrap()
}

pub fn unique_tmp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("aab-test-{nanos}-{n}"))
}

/// Boot the HTTP server on a free port; returns its base URL.
pub async fn spawn_http(state: Arc<AppState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http::router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Boot the TCP server on a free port; returns its address.
pub async fn spawn_tcp(state: Arc<AppState>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let security = WorkflowSecurity::from_json(
        state.config.api_auth_bearer.clone(),
        r#"{"credentials":[]}"#,
        state.config.max_http_body_bytes,
    )
    .unwrap();
    tokio::spawn(async move {
        let _ = tcp::serve(state, listener, security).await;
    });
    addr
}
