//! Shared harness for the integration tests: an in-memory bridge bound to
//! OS-assigned ports on localhost.
#![allow(dead_code)] // each test file uses a subset of these helpers

use std::sync::Arc;

use ai_agent_bridge::config::Config;
use ai_agent_bridge::embed::Embedder;
use ai_agent_bridge::state::AppState;
use ai_agent_bridge::{http, tcp};
use tokio::net::TcpListener;

pub fn state() -> Arc<AppState> {
    let cfg = Config::in_memory();
    let embedder = Embedder::new(cfg.embed_dim, "local-hash-v1".into(), None, "local".into(), None);
    AppState::new(cfg, embedder)
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
    tokio::spawn(async move {
        let _ = tcp::serve(state, listener).await;
    });
    addr
}
