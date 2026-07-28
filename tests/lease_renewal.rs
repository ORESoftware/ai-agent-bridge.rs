//! External Fiducia renewal proxy and failure-semantics coverage.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_agent_bridge::types::{now_ts, Agent, AgentKind};
use ai_agent_bridge::{http, lease_renewal};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

type RecordedCall = (Option<String>, Value);
type RecordedCalls = Arc<Mutex<Vec<RecordedCall>>>;

#[derive(Clone, Copy)]
enum MockMode {
    Success,
    Stale,
    WrongOwner,
    Redirect,
    Oversized,
    Timeout,
}

#[derive(Clone)]
struct MockState {
    mode: MockMode,
    calls: RecordedCalls,
    redirect_hits: Arc<AtomicU64>,
}

async fn renew_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let internal_auth = headers
        .get("x-internal-auth")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    state.calls.lock().unwrap().push((internal_auth, body));
    match state.mode {
        MockMode::Success => Json(json!({
            "result": {
                "output": {
                    "renewed": true,
                    "fencing_token": 42,
                    "expires_at_ms": 999999
                }
            }
        }))
        .into_response(),
        MockMode::Stale => (
            StatusCode::CONFLICT,
            Json(json!({"error":"stale_fencing_token"})),
        )
            .into_response(),
        MockMode::WrongOwner => (
            StatusCode::CONFLICT,
            Json(json!({"error":"lease_owner_mismatch"})),
        )
            .into_response(),
        MockMode::Redirect => (
            StatusCode::TEMPORARY_REDIRECT,
            [(axum::http::header::LOCATION, "/redirect-target")],
            Json(json!({"redirected":true})),
        )
            .into_response(),
        MockMode::Oversized => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            format!("\"{}\"", "x".repeat(1_048_577)),
        )
            .into_response(),
        MockMode::Timeout => {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            Json(json!({"result":{"output":{"renewed":true,"fencing_token":42}}})).into_response()
        }
    }
}

async fn redirect_target(State(state): State<MockState>) -> Response {
    state.redirect_hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({"unexpected":true})).into_response()
}

async fn spawn(mode: MockMode, timeout_secs: u64) -> (String, MockState) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let redirect_hits = Arc::new(AtomicU64::new(0));
    let mock_state = MockState {
        mode,
        calls,
        redirect_hits,
    };
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control_listener.local_addr().unwrap();
    let control_app = Router::new()
        .route("/v1/file-leases/renew", post(renew_handler))
        .route("/redirect-target", post(redirect_target))
        .with_state(mock_state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(control_listener, control_app).await;
    });

    let mut config = common::base_config();
    config.control_plane_url = Some(format!("http://{control_addr}"));
    config.control_plane_secret = Some("test-internal-secret".into());
    config.control_plane_timeout_secs = timeout_secs;
    let state = common::state_with(config);
    state
        .register_agent(Agent {
            agent_key: "codex".into(),
            display_name: "Codex".into(),
            kind: AgentKind::Codex,
            host: None,
            meta: json!({}),
            registered_at: now_ts(),
        })
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app =
        http::router(state.clone()).layer(from_fn_with_state(state, lease_renewal::intercept));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), mock_state)
}

fn renewal_body() -> Value {
    json!({
        "repository": "owner/repo",
        "paths": ["src/lib.rs", "src/main.rs"],
        "agent_key": "codex",
        "fencing_token": 42,
        "ttl_ms": 30000
    })
}

#[tokio::test]
async fn canonical_renewal_preserves_exact_union_auth_and_token() {
    let (base, mock) = spawn(MockMode::Success, 2).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/file-leases/renew"))
        .json(&renewal_body())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["result"]["output"]["renewed"], true);
    assert_eq!(body["result"]["output"]["fencing_token"], 42);

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.as_deref(), Some("test-internal-secret"));
    assert_eq!(calls[0].1, renewal_body());
}

#[tokio::test]
async fn legacy_renewal_path_uses_the_same_authoritative_contract() {
    let (base, _) = spawn(MockMode::Success, 2).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/file-leases/legacy-id/renew"))
        .json(&renewal_body())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

#[tokio::test]
async fn stale_token_and_wrong_owner_conflicts_are_preserved() {
    for mode in [MockMode::Stale, MockMode::WrongOwner] {
        let (base, _) = spawn(mode, 2).await;
        let response = reqwest::Client::new()
            .post(format!("{base}/file-leases/renew"))
            .json(&renewal_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (base, mock) = spawn(MockMode::Redirect, 2).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .post(format!("{base}/file-leases/renew"))
        .json(&renewal_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(mock.redirect_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn oversized_and_timed_out_control_plane_responses_fail_closed() {
    let (base, _) = spawn(MockMode::Oversized, 2).await;
    let oversized = reqwest::Client::new()
        .post(format!("{base}/file-leases/renew"))
        .json(&renewal_body())
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::BAD_GATEWAY);
    let body = oversized.json::<Value>().await.unwrap();
    assert_eq!(body["error"], "control_plane_error");

    let (base, _) = spawn(MockMode::Timeout, 1).await;
    let timeout = reqwest::Client::new()
        .post(format!("{base}/file-leases/renew"))
        .json(&renewal_body())
        .send()
        .await
        .unwrap();
    assert_eq!(timeout.status(), StatusCode::BAD_GATEWAY);
    let body = timeout.json::<Value>().await.unwrap();
    assert_eq!(body["error"], "control_plane_error");
    assert!(body["message"]
        .as_str()
        .unwrap_or_default()
        .contains("timed out"));
}

#[tokio::test]
async fn unregistered_agents_are_rejected_before_control_plane_access() {
    let (base, mock) = spawn(MockMode::Success, 2).await;
    let mut body = renewal_body();
    body["agent_key"] = Value::String("unknown".into());
    let response = reqwest::Client::new()
        .post(format!("{base}/file-leases/renew"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(mock.calls.lock().unwrap().is_empty());
}
