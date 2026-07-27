//! HTTP integration coverage for scoped workflow adapter authentication.

use ai_agent_bridge::workflow_security::{self, WorkflowSecurity};
use axum::extract::Request;
use axum::http::header;
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

async fn spawn() -> String {
    let security = WorkflowSecurity::from_json(
        Some("global-admin-secret".into()),
        r#"{
          "credentials": [
            {
              "token_id":"codex-v2",
              "token":"codex-adapter-secret",
              "agent_key":"codex",
              "scopes":["workflow:submit"]
            }
          ]
        }"#,
        65_536,
    )
    .unwrap();
    let app = Router::new()
        .route("/workflows/{id}/submissions", post(echo_auth))
        .route("/channels/{slug}/context", post(ok))
        .layer(from_fn_with_state(security, workflow_security::enforce));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn echo_auth(request: Request) -> impl IntoResponse {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    Json(json!({ "ok": true, "authorization": authorization }))
}

async fn ok() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

#[tokio::test]
async fn scoped_submission_is_bound_to_adapter_identity_and_rewritten_for_inner_auth() {
    let base = spawn().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/workflows/wf/submissions"))
        .bearer_auth("codex-adapter-secret")
        .json(&json!({ "agent_key": "codex", "content": "done" }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["authorization"], "Bearer global-admin-secret");

    let denied = client
        .post(format!("{base}/workflows/wf/submissions"))
        .bearer_auth("codex-adapter-secret")
        .json(&json!({ "agent_key": "claude", "content": "forged" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 403);
}

#[tokio::test]
async fn generic_http_context_cannot_write_reserved_workflow_namespace() {
    let base = spawn().await;
    let response = reqwest::Client::new()
        .post(format!("{base}/channels/workflow-wf/context"))
        .bearer_auth("global-admin-secret")
        .json(&json!({
            "key": "workflow.plan.v1",
            "value": {"forged": true},
            "updated_by": "operator"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}
