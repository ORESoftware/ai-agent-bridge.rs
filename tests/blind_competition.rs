//! End-to-end isolation and reviewer-reveal coverage for blind competition.

mod common;

use std::sync::Arc;

use ai_agent_bridge::state::AppState;
use ai_agent_bridge::types::{now_ts, Agent, AgentKind};
use ai_agent_bridge::{blind_competition, http, workflow_security};
use axum::middleware::from_fn_with_state;
use serde_json::{json, Value};

const COORDINATOR_TOKEN: &str = "test-coordinator-credential";
const WORKER_A_TOKEN: &str = "test-worker-a-credential";
const WORKER_B_TOKEN: &str = "test-worker-b-credential";
const REVIEWER_TOKEN: &str = "test-reviewer-credential";
const ADMIN_TOKEN: &str = "test-admin-credential";

async fn spawn() -> (String, Arc<AppState>) {
    let state = common::state();
    for (agent_key, kind) in [
        ("worker-a", AgentKind::Codex),
        ("worker-b", AgentKind::Claude),
        ("reviewer", AgentKind::Gemini),
    ] {
        state
            .register_agent(Agent {
                agent_key: agent_key.into(),
                display_name: agent_key.into(),
                kind,
                host: None,
                meta: json!({ "capabilities": ["rust", "review"] }),
                registered_at: now_ts(),
            })
            .unwrap();
    }

    let security = workflow_security::WorkflowSecurity::from_json(
        Some(ADMIN_TOKEN.into()),
        r#"{
          "credentials": [
            {
              "token_id":"coordinator-v1",
              "token":"test-coordinator-credential",
              "agent_key":"coordinator",
              "scopes":["workflow:create","workflow:read"]
            },
            {
              "token_id":"worker-a-v1",
              "token":"test-worker-a-credential",
              "agent_key":"worker-a",
              "scopes":["workflow:read","workflow:submit"]
            },
            {
              "token_id":"worker-b-v1",
              "token":"test-worker-b-credential",
              "agent_key":"worker-b",
              "scopes":["workflow:read","workflow:submit"]
            },
            {
              "token_id":"reviewer-v1",
              "token":"test-reviewer-credential",
              "agent_key":"reviewer",
              "scopes":["workflow:read"]
            }
          ]
        }"#,
        state.config.max_http_body_bytes,
    )
    .unwrap();

    let app = http::router(state.clone())
        .merge(blind_competition::router(state.clone()))
        .layer(from_fn_with_state(security, workflow_security::enforce));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

async fn post_json(
    client: &reqwest::Client,
    url: String,
    token: &str,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn post_empty(
    client: &reqwest::Client,
    url: String,
    token: &str,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn get_json(
    client: &reqwest::Client,
    url: String,
    token: &str,
) -> (reqwest::StatusCode, Value) {
    let response = client.get(url).bearer_auth(token).send().await.unwrap();
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn candidates_are_hidden_until_the_designated_reviewer_reveals() {
    let (base, state) = spawn().await;
    let client = reqwest::Client::new();

    let (status, created) = post_json(
        &client,
        format!("{base}/blind-workflows"),
        COORDINATOR_TOKEN,
        json!({
            "title": "Independent lease implementation",
            "prompt": "Produce an independent Rust solution from this immutable task.",
            "created_by": "coordinator",
            "worker_agent_keys": ["worker-a", "worker-b"],
            "reviewer_agent_key": "reviewer"
        }),
    )
    .await;
    assert!(status.is_success(), "{created}");
    let workflow_id = created["workflow"]["plan"]["id"].as_str().unwrap();
    let channel = created["workflow"]["plan"]["channel"]
        .as_str()
        .unwrap();
    let workflow_url = format!("{base}/blind-workflows/{workflow_id}");
    let submission_url = format!("{workflow_url}/submissions");
    let reveal_url = format!("{workflow_url}/reveal");

    let (early_status, early) =
        post_empty(&client, reveal_url.clone(), REVIEWER_TOKEN).await;
    assert_eq!(early_status.as_u16(), 400, "{early}");

    let (status, first) = post_json(
        &client,
        submission_url.clone(),
        WORKER_A_TOKEN,
        json!({
            "agent_key": "worker-a",
            "content": "candidate alpha secret",
            "meta": {"tests": ["cargo test"]}
        }),
    )
    .await;
    assert!(status.is_success(), "{first}");
    assert_eq!(first["workflow"]["submissions"].as_array().unwrap().len(), 1);
    assert_eq!(first["workflow"]["hidden_submission_count"], 0);

    let (duplicate_status, duplicate) = post_json(
        &client,
        submission_url.clone(),
        WORKER_A_TOKEN,
        json!({
            "agent_key": "worker-a",
            "content": "overwrite attempt"
        }),
    )
    .await;
    assert_eq!(duplicate_status.as_u16(), 400, "{duplicate}");

    for token in [WORKER_B_TOKEN, REVIEWER_TOKEN, ADMIN_TOKEN] {
        let (status, view) = get_json(&client, workflow_url.clone(), token).await;
        assert!(status.is_success(), "{view}");
        assert_eq!(view["workflow"]["submissions"].as_array().unwrap().len(), 0);
        assert_eq!(view["workflow"]["hidden_submission_count"], 1);
    }

    let (status, second) = post_json(
        &client,
        submission_url,
        WORKER_B_TOKEN,
        json!({
            "agent_key": "worker-b",
            "content": "candidate beta secret"
        }),
    )
    .await;
    assert!(status.is_success(), "{second}");
    assert_eq!(second["workflow"]["stage"], "ready_to_reveal");
    assert_eq!(second["workflow"]["submissions"].as_array().unwrap().len(), 1);
    assert_eq!(second["workflow"]["hidden_submission_count"], 1);

    let history = state.history(channel, None).unwrap();
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(!rendered.contains("candidate alpha secret"));
    assert!(!rendered.contains("candidate beta secret"));

    let (admin_status, admin_reveal) =
        post_empty(&client, reveal_url.clone(), ADMIN_TOKEN).await;
    assert_eq!(admin_status.as_u16(), 401, "{admin_reveal}");

    let (status, revealed) = post_empty(&client, reveal_url, REVIEWER_TOKEN).await;
    assert!(status.is_success(), "{revealed}");
    assert_eq!(revealed["workflow"]["stage"], "revealed");
    let submissions = revealed["workflow"]["submissions"].as_array().unwrap();
    assert_eq!(submissions.len(), 2);
    assert_eq!(submissions[0]["content"], "candidate alpha secret");
    assert_eq!(submissions[1]["content"], "candidate beta secret");

    let (status, worker_view) = get_json(&client, workflow_url, WORKER_A_TOKEN).await;
    assert!(status.is_success(), "{worker_view}");
    assert_eq!(
        worker_view["workflow"]["submissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
