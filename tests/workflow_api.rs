//! HTTP integration coverage for first-class multi-model workflows.

mod common;

use ai_agent_bridge::{http, orchestration};
use serde_json::{json, Value};

async fn spawn() -> String {
    let state = common::state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http::router(state.clone()).merge(orchestration::router(state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn post(client: &reqwest::Client, url: String, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client.post(url).json(&body).send().await.unwrap();
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn competitive_workflow_accepts_independent_immutable_submissions() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    for (agent_key, kind) in [("codex-rust", "codex"), ("gemini-rust", "gemini")] {
        let (status, _) = post(
            &client,
            format!("{base}/agents/register"),
            json!({
                "agent_key": agent_key,
                "kind": kind,
                "meta": {"capabilities": ["rust", "review"]}
            }),
        )
        .await;
        assert!(status.is_success());
    }

    let (status, created) = post(
        &client,
        format!("{base}/workflows"),
        json!({
            "title": "Compare Rust lease designs",
            "prompt": "Propose an independently reasoned implementation.",
            "created_by": "coordinator",
            "mode": "competitive",
            "agent_kinds": ["codex", "gemini"],
            "required_capabilities": ["rust"],
            "worker_count": 2,
            "repository": "ORESoftware/ai-agent-bridge.rs",
            "paths": ["src/orchestration.rs"],
            "require_file_leases": true
        }),
    )
    .await;
    assert!(status.is_success(), "{created}");
    assert_eq!(
        created["workflow"]["plan"]["assignments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(created["workflow"]["status"]["stage"], "ready");

    let workflow_id = created["workflow"]["plan"]["id"].as_str().unwrap();
    let submit_url = format!("{base}/workflows/{workflow_id}/submissions");

    let (status, first) = post(
        &client,
        submit_url.clone(),
        json!({"agent_key": "codex-rust", "content": "proposal A"}),
    )
    .await;
    assert!(status.is_success(), "{first}");
    assert_eq!(first["workflow"]["status"]["stage"], "running");

    let (duplicate_status, duplicate) = post(
        &client,
        submit_url.clone(),
        json!({"agent_key": "codex-rust", "content": "overwrite attempt"}),
    )
    .await;
    assert_eq!(duplicate_status.as_u16(), 400, "{duplicate}");

    let (status, completed) = post(
        &client,
        submit_url,
        json!({"agent_key": "gemini-rust", "content": "proposal B"}),
    )
    .await;
    assert!(status.is_success(), "{completed}");
    assert_eq!(completed["workflow"]["status"]["stage"], "completed");
    assert_eq!(
        completed["workflow"]["submissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
