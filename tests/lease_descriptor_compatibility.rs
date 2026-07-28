//! Compatibility lease IDs backed by durable authoritative Fiducia descriptors.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ai_agent_bridge::types::{now_ts, Agent, AgentKind};
use ai_agent_bridge::{http, lease_descriptors, lease_renewal};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Acquire,
    Renew,
    Release,
}

type RecordedCall = (Operation, Option<String>, Value);
type RecordedCalls = Arc<Mutex<Vec<RecordedCall>>>;

#[derive(Clone)]
struct MockState {
    calls: RecordedCalls,
    active_token: Arc<AtomicU64>,
}

async fn acquire_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record(&state, Operation::Acquire, &headers, body);
    let expires_at_ms = chrono::Utc::now().timestamp_millis() + 60_000;
    Json(json!({
        "result": {
            "output": {
                "acquired": true,
                "fencing_token": state.active_token.load(Ordering::SeqCst),
                "expires_at_ms": expires_at_ms
            }
        }
    }))
    .into_response()
}

async fn renew_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record(&state, Operation::Renew, &headers, body.clone());
    let token = body["fencing_token"].as_u64().unwrap_or_default();
    if token != state.active_token.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"stale_fencing_token"})),
        )
            .into_response();
    }
    let expires_at_ms = chrono::Utc::now().timestamp_millis() + 90_000;
    Json(json!({
        "result": {
            "output": {
                "renewed": true,
                "fencing_token": token,
                "expires_at_ms": expires_at_ms
            }
        }
    }))
    .into_response()
}

async fn release_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record(&state, Operation::Release, &headers, body.clone());
    let token = body["fencing_token"].as_u64().unwrap_or_default();
    if token != state.active_token.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"stale_fencing_token"})),
        )
            .into_response();
    }
    Json(json!({"released":true,"fencing_token":token})).into_response()
}

fn record(state: &MockState, operation: Operation, headers: &HeaderMap, body: Value) {
    let internal_auth = headers
        .get("x-internal-auth")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    state
        .calls
        .lock()
        .unwrap()
        .push((operation, internal_auth, body));
}

async fn spawn() -> (String, Arc<ai_agent_bridge::state::AppState>, MockState) {
    let mock = MockState {
        calls: Arc::new(Mutex::new(Vec::new())),
        active_token: Arc::new(AtomicU64::new(42)),
    };
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control_listener.local_addr().unwrap();
    let control_app = Router::new()
        .route("/v1/file-leases/acquire", post(acquire_handler))
        .route("/v1/file-leases/renew", post(renew_handler))
        .route("/v1/file-leases/release", post(release_handler))
        .with_state(mock.clone());
    tokio::spawn(async move {
        let _ = axum::serve(control_listener, control_app).await;
    });

    let mut config = common::base_config();
    config.control_plane_url = Some(format!("http://{control_addr}"));
    config.control_plane_secret = Some("descriptor-test-secret".into());
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
    let address = listener.local_addr().unwrap();
    let app = http::router(state.clone())
        .layer(from_fn_with_state(state.clone(), lease_renewal::intercept))
        .layer(from_fn_with_state(
            state.clone(),
            lease_descriptors::intercept,
        ));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), state, mock)
}

async fn acquire(client: &reqwest::Client, base: &str) -> Value {
    let response = client
        .post(format!("{base}/file-leases"))
        .json(&json!({
            "repository":"owner/repo/",
            "path":"src/./lib.rs",
            "agent_key":"codex",
            "ttl_ms":30000,
            "purpose":"compatibility test",
            "meta":{"ticket":"DEN-490"}
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json::<Value>().await.unwrap()
}

#[tokio::test]
async fn acquisition_persists_exact_descriptor_and_renewal_reuses_it() {
    let (base, _state, mock) = spawn().await;
    let client = reqwest::Client::new();
    let acquired = acquire(&client, &base).await;
    let lease = &acquired["compatibility_lease"];
    let lease_id = lease["id"].as_str().unwrap();
    assert_eq!(lease["repository"], "owner/repo");
    assert_eq!(lease["path"], "src/lib.rs");
    assert_eq!(lease["fencing_token"], 42);

    let renewed = client
        .post(format!("{base}/file-leases/{lease_id}/renew"))
        .json(&json!({
            "agent_key":"codex",
            "fencing_token":42,
            "ttl_ms":45000
        }))
        .send()
        .await
        .unwrap();
    assert!(renewed.status().is_success());
    let body = renewed.json::<Value>().await.unwrap();
    assert_eq!(body["compatibility_lease"]["id"], lease_id);
    assert_eq!(body["compatibility_lease"]["ttl_ms"], 45000);
    assert_eq!(body["compatibility_lease"]["fencing_token"], 42);

    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, Operation::Acquire);
    assert_eq!(calls[1].0, Operation::Renew);
    assert_eq!(calls[1].1.as_deref(), Some("descriptor-test-secret"));
    assert_eq!(calls[1].2["repository"], "owner/repo");
    assert_eq!(calls[1].2["paths"], json!(["src/lib.rs"]));
    assert_eq!(calls[1].2["agent_key"], "codex");
    assert_eq!(calls[1].2["fencing_token"], 42);
}

#[tokio::test]
async fn forged_owner_and_stale_token_are_rejected_before_authority_access() {
    let (base, _, mock) = spawn().await;
    let client = reqwest::Client::new();
    let acquired = acquire(&client, &base).await;
    let lease_id = acquired["compatibility_lease"]["id"].as_str().unwrap();
    let calls_before = mock.calls.lock().unwrap().len();

    let wrong_owner = client
        .post(format!("{base}/file-leases/{lease_id}/renew"))
        .json(&json!({
            "agent_key":"claude",
            "fencing_token":42,
            "ttl_ms":30000
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_owner.status(), StatusCode::FORBIDDEN);

    let stale = client
        .post(format!("{base}/file-leases/{lease_id}/renew"))
        .json(&json!({
            "agent_key":"codex",
            "fencing_token":41,
            "ttl_ms":30000
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(mock.calls.lock().unwrap().len(), calls_before);
}

#[tokio::test]
async fn release_tombstones_the_descriptor_and_blocks_later_renewal() {
    let (base, _state, mock) = spawn().await;
    let client = reqwest::Client::new();
    let acquired = acquire(&client, &base).await;
    let lease_id = acquired["compatibility_lease"]["id"].as_str().unwrap();

    let released = client
        .post(format!("{base}/file-leases/{lease_id}/release"))
        .json(&json!({
            "agent_key":"codex",
            "fencing_token":42,
            "ttl_ms":30000
        }))
        .send()
        .await
        .unwrap();
    assert!(released.status().is_success());
    let released_body = released.json::<Value>().await.unwrap();
    assert_eq!(released_body["compatibility_lease"]["status"], "released");

    let renewal = client
        .post(format!("{base}/file-leases/{lease_id}/renew"))
        .json(&json!({
            "agent_key":"codex",
            "fencing_token":42,
            "ttl_ms":30000
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(renewal.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(operation, _, _)| *operation == Operation::Renew)
            .count(),
        0
    );
}
