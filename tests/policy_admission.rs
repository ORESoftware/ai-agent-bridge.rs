//! HTTP contract coverage for durable workflow policy admission and accounting.

mod common;

use ai_agent_bridge::types::{now_ts, Agent, AgentKind};
use ai_agent_bridge::{orchestration, policy_admission};
use serde_json::{json, Value};

async fn spawn() -> String {
    let state = common::state();
    state
        .register_agent(Agent {
            agent_key: "codex".into(),
            display_name: "Codex".into(),
            kind: AgentKind::Codex,
            host: None,
            meta: json!({"capabilities":["rust"]}),
            registered_at: now_ts(),
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = orchestration::router(state.clone()).merge(policy_admission::router(state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

async fn create_workflow(client: &reqwest::Client, base: &str) -> String {
    let response = client
        .post(format!("{base}/workflows"))
        .json(&json!({
            "title":"Admission test",
            "prompt":"Produce one bounded result",
            "mode":"single",
            "created_by":"coordinator",
            "agent_keys":["codex"],
            "required_capabilities":["rust"]
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.json::<Value>().await.unwrap();
    body["workflow"]["plan"]["id"].as_str().unwrap().to_string()
}

fn policy_request(max_cost_micro_usd: u64) -> Value {
    json!({
        "task_risk":"low",
        "data_sensitivity":"internal",
        "requested_mode":"single",
        "required_capabilities":["rust"],
        "requires_repository_write":false,
        "requested_budget":{"max_cost_micro_usd":max_cost_micro_usd},
        "providers":[{
            "agent_key":"codex",
            "kind":"codex",
            "model":"codex-test",
            "available":true,
            "capabilities":["rust"],
            "health_score_bps":10000,
            "estimated_cost_micro_usd":0
        }]
    })
}

#[tokio::test]
async fn admission_is_insert_only_and_one_unit_overage_is_terminal() {
    let base = spawn().await;
    let client = reqwest::Client::new();
    let workflow_id = create_workflow(&client, &base).await;

    let admitted = client
        .post(format!("{base}/workflows/{workflow_id}/admission"))
        .json(&json!({
            "requested_by":"runner/pod-0",
            "policy_request":policy_request(100)
        }))
        .send()
        .await
        .unwrap();
    assert!(admitted.status().is_success());
    let admitted = admitted.json::<Value>().await.unwrap();
    assert_eq!(admitted["created"], true);
    assert_eq!(admitted["admission"]["status"], "active");
    assert_eq!(
        admitted["admission"]["policy"]["budget"]["max_cost_micro_usd"],
        100
    );

    let duplicate = client
        .post(format!("{base}/workflows/{workflow_id}/admission"))
        .json(&json!({
            "requested_by":"runner/pod-0",
            "policy_request":policy_request(1)
        }))
        .send()
        .await
        .unwrap();
    assert!(duplicate.status().is_success());
    let duplicate = duplicate.json::<Value>().await.unwrap();
    assert_eq!(duplicate["created"], false);
    assert_eq!(
        duplicate["admission"]["policy"]["budget"]["max_cost_micro_usd"],
        100
    );

    let accepted = client
        .post(format!("{base}/workflows/{workflow_id}/admission/usage"))
        .json(&json!({
            "updated_by":"runner/pod-0",
            "provider_agent_key":"codex",
            "delta":{"cost_micro_usd":100,"provider_calls":1,"concurrency":1}
        }))
        .send()
        .await
        .unwrap();
    assert!(accepted.status().is_success());

    let exhausted = client
        .post(format!("{base}/workflows/{workflow_id}/admission/usage"))
        .json(&json!({
            "updated_by":"runner/pod-0",
            "provider_agent_key":"codex",
            "delta":{"cost_micro_usd":1}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(exhausted.status(), reqwest::StatusCode::CONFLICT);
    let exhausted = exhausted.json::<Value>().await.unwrap();
    assert_eq!(exhausted["error"], "admission_exhausted");
    assert_eq!(exhausted["admission"]["status"], "exhausted");
    assert_eq!(exhausted["admission"]["usage"]["cost_micro_usd"], 100);
    assert_eq!(
        exhausted["admission"]["last_rejected_delta"]["cost_micro_usd"],
        1
    );

    let late = client
        .post(format!("{base}/workflows/{workflow_id}/admission/usage"))
        .json(&json!({
            "updated_by":"runner/pod-0",
            "provider_agent_key":"codex",
            "delta":{}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(late.status(), reqwest::StatusCode::CONFLICT);
}

#[tokio::test]
async fn runner_and_provider_identities_are_checked_separately() {
    let base = spawn().await;
    let client = reqwest::Client::new();
    let workflow_id = create_workflow(&client, &base).await;
    let admitted = client
        .post(format!("{base}/workflows/{workflow_id}/admission"))
        .json(&json!({
            "requested_by":"runner/pod-0",
            "policy_request":policy_request(100)
        }))
        .send()
        .await
        .unwrap();
    assert!(admitted.status().is_success());

    let wrong_runner = client
        .post(format!("{base}/workflows/{workflow_id}/admission/usage"))
        .json(&json!({
            "updated_by":"runner/pod-1",
            "provider_agent_key":"codex",
            "delta":{"provider_calls":1,"concurrency":1}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_runner.status(), reqwest::StatusCode::FORBIDDEN);

    let wrong_provider = client
        .post(format!("{base}/workflows/{workflow_id}/admission/usage"))
        .json(&json!({
            "updated_by":"runner/pod-0",
            "provider_agent_key":"claude",
            "delta":{"provider_calls":1,"concurrency":1}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_provider.status(), reqwest::StatusCode::FORBIDDEN);
}
