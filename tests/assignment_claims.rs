//! Distributed assignment-claim submission fencing tests.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ai_agent_bridge::assignment_claims::AssignmentClaimPolicy;
use ai_agent_bridge::types::{now_ts, Agent, AgentKind};
use ai_agent_bridge::{assignment_claims, orchestration};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

const CLAIM_REPOSITORY: &str = "fiducia-cloud/ai-agent-assignment-claims";

#[derive(Clone)]
struct MockState {
    current_token: Arc<AtomicU64>,
    calls: Arc<Mutex<Vec<Value>>>,
}

async fn renew_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(
        headers
            .get("x-internal-auth")
            .and_then(|value| value.to_str().ok()),
        Some("test-internal-secret")
    );
    state.calls.lock().unwrap().push(body.clone());
    let token = body["fencing_token"].as_u64().unwrap_or_default();
    let owner = body["agent_key"].as_str().unwrap_or_default();
    if token != state.current_token.load(Ordering::SeqCst) || owner != "runner/pod-b" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"stale_or_wrong_owner"})),
        )
            .into_response();
    }
    Json(json!({
        "result": {
            "output": {
                "renewed": true,
                "fencing_token": token,
                "expires_at_ms": 999999
            }
        }
    }))
    .into_response()
}

async fn spawn() -> (String, MockState) {
    let mock = MockState {
        current_token: Arc::new(AtomicU64::new(43)),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let control_addr = control_listener.local_addr().unwrap();
    let control_app = Router::new()
        .route("/v1/file-leases/renew", post(renew_handler))
        .with_state(mock.clone());
    tokio::spawn(async move {
        let _ = axum::serve(control_listener, control_app).await;
    });

    let mut config = common::base_config();
    config.control_plane_url = Some(format!("http://{control_addr}"));
    config.control_plane_secret = Some("test-internal-secret".into());
    let state = common::state_with(config);
    state
        .register_agent(Agent {
            agent_key: "worker".into(),
            display_name: "Worker".into(),
            kind: AgentKind::Codex,
            host: None,
            meta: json!({"capabilities":["rust"]}),
            registered_at: now_ts(),
        })
        .unwrap();
    let policy = AssignmentClaimPolicy::new(
        state.clone(),
        true,
        CLAIM_REPOSITORY.into(),
        60_000,
    )
    .unwrap();
    let app = orchestration::router(state)
        .layer(from_fn_with_state(policy, assignment_claims::enforce));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), mock)
}

async fn create_workflow(base: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{base}/workflows"))
        .json(&json!({
            "title":"Distributed claim test",
            "prompt":"Produce one result",
            "mode":"single",
            "created_by":"coordinator",
            "agent_keys":["worker"],
            "required_capabilities":["rust"]
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.json::<Value>().await.unwrap();
    body["workflow"]["plan"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn submission(workflow_id: &str, token: u64, ordinal: usize) -> Value {
    json!({
        "agent_key":"worker",
        "content":"candidate result",
        "meta":{
            "assignment_claim":{
                "repository":CLAIM_REPOSITORY,
                "paths":[format!("workflows/{workflow_id}/assignments/{ordinal}")],
                "owner":"runner/pod-b",
                "instance_id":"pod-b",
                "assignment_ordinal":ordinal,
                "fencing_token":token,
                "ttl_ms":60000
            }
        }
    })
}

#[tokio::test]
async fn stale_replica_cannot_submit_after_successor_gets_new_token() {
    let (base, mock) = spawn().await;
    let workflow_id = create_workflow(&base).await;
    let client = reqwest::Client::new();

    let stale = client
        .post(format!("{base}/workflows/{workflow_id}/submissions"))
        .json(&submission(&workflow_id, 42, 0))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);

    let successor = client
        .post(format!("{base}/workflows/{workflow_id}/submissions"))
        .json(&submission(&workflow_id, 43, 0))
        .send()
        .await
        .unwrap();
    assert!(successor.status().is_success());
    let body = successor.json::<Value>().await.unwrap();
    assert_eq!(body["workflow"]["submissions"].as_array().unwrap().len(), 1);
    assert_eq!(body["workflow"]["submissions"][0]["meta"]["assignment_claim"]["fencing_token"], 43);

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1]["repository"], CLAIM_REPOSITORY);
    assert_eq!(
        calls[1]["paths"],
        json!([format!("workflows/{workflow_id}/assignments/0")])
    );
}

#[tokio::test]
async fn missing_or_mismatched_claim_is_rejected_without_authority_call() {
    let (base, mock) = spawn().await;
    let workflow_id = create_workflow(&base).await;
    let client = reqwest::Client::new();

    let missing = client
        .post(format!("{base}/workflows/{workflow_id}/submissions"))
        .json(&json!({"agent_key":"worker","content":"no claim","meta":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::CONFLICT);

    let mismatched = client
        .post(format!("{base}/workflows/{workflow_id}/submissions"))
        .json(&submission(&workflow_id, 43, 9))
        .send()
        .await
        .unwrap();
    assert_eq!(mismatched.status(), StatusCode::BAD_REQUEST);
    assert!(mock.calls.lock().unwrap().is_empty());
}
