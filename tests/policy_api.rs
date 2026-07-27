//! HTTP contract coverage for the deterministic workflow policy engine.

use ai_agent_bridge::policy;
use serde_json::{json, Value};

async fn spawn() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, policy::router()).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn explain_returns_versioned_consensus_decision_for_restricted_data() {
    let base = spawn().await;
    let response = reqwest::Client::new()
        .post(format!("{base}/workflow-policy/explain"))
        .json(&json!({
            "task_risk": "high",
            "data_sensitivity": "restricted",
            "required_capabilities": ["rust"],
            "requires_repository_write": true,
            "providers": [
                provider("codex", "codex"),
                provider("claude", "claude"),
                provider("gemini", "gemini")
            ]
        }))
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["decision"]["mode"], "consensus");
    assert_eq!(body["decision"]["require_human_approval"], true);
    assert_eq!(body["decision"]["require_fiducia_lease"], true);
    assert_eq!(
        body["decision"]["selected_providers"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

fn provider(agent_key: &str, kind: &str) -> Value {
    json!({
        "agent_key": agent_key,
        "kind": kind,
        "model": format!("{kind}-test"),
        "available": true,
        "capabilities": ["rust"],
        "trusted_for_restricted": true,
        "health_score_bps": 9500,
        "p95_latency_ms": 1000,
        "estimated_cost_micro_usd": 100000
    })
}
