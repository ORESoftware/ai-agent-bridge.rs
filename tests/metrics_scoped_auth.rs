//! Full-stack scoped-auth coverage for the public Prometheus endpoint.

mod common;

use ai_agent_bridge::http;
use ai_agent_bridge::workflow_security::{self, WorkflowSecurity};
use axum::middleware::from_fn_with_state;

#[tokio::test]
async fn metrics_remain_public_when_scoped_adapter_auth_is_enabled() {
    let state = common::state();
    let security = WorkflowSecurity::from_json(
        Some("operator-secret".into()),
        r#"{
          "credentials": [
            {
              "token_id": "codex-read-v1",
              "token": "codex-scoped-secret",
              "agent_key": "codex",
              "scopes": ["agent:read"]
            }
          ]
        }"#,
        state.config.max_http_body_bytes,
    )
    .unwrap();
    let app =
        http::router(state.clone()).layer(from_fn_with_state(security, workflow_security::enforce));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let metrics = reqwest::get(format!("http://{address}/metrics"))
        .await
        .unwrap();
    assert!(metrics.status().is_success());
    let text = metrics.text().await.unwrap();
    assert!(text.contains("ai_agent_bridge_build_info"));

    let agents = reqwest::get(format!("http://{address}/agents"))
        .await
        .unwrap();
    assert_eq!(agents.status(), reqwest::StatusCode::UNAUTHORIZED);
}
