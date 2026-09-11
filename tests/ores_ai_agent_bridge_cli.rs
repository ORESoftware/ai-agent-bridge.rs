use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const TEST_BEARER: &str = "ores-bridge-test-secret-that-must-not-leak";
const SERVICE_ID: &str = "com.ores.ai-agent-bridge";

#[derive(Clone, Default)]
struct WitnessState {
    posted: Arc<Mutex<Option<Value>>>,
}

fn is_authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        == Some(TEST_BEARER)
}

fn reject_unless_authorized(headers: &HeaderMap) -> Option<Response> {
    if is_authorized(headers) {
        None
    } else {
        Some(StatusCode::UNAUTHORIZED.into_response())
    }
}

async fn agents(headers: HeaderMap) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    Json(json!({ "ok": true, "agents": [] })).into_response()
}

async fn register(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    Json(json!({
        "ok": true,
        "agent": {
            "agent_key": body["agent_key"].clone(),
            "display_name": body["display_name"].clone(),
            "kind": body["kind"].clone(),
            "meta": body["meta"].clone()
        }
    }))
    .into_response()
}

async fn resolve(headers: HeaderMap) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    Json(json!({
        "ok": true,
        "channel": {
            "slug": "com-ores-ai-agent-bridge-connectivity-smoke",
            "topic": "com.ores.ai-agent-bridge connectivity smoke"
        },
        "score": 1.0,
        "created": false
    }))
    .into_response()
}

async fn join(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    Json(json!({
        "ok": true,
        "member": {
            "agent_key": body["agent_key"].clone(),
            "role": body["role"].clone()
        },
        "channel": {
            "slug": "com-ores-ai-agent-bridge-connectivity-smoke"
        },
        "newly_joined": true
    }))
    .into_response()
}

async fn post_message(
    State(state): State<WitnessState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    let message = json!({
        "id": "message-7",
        "channel": "com-ores-ai-agent-bridge-connectivity-smoke",
        "seq": 7,
        "from": body["from"].clone(),
        "role": "user",
        "content": body["content"].clone(),
        "meta": body["meta"].clone(),
        "created_at": "2026-08-08T00:00:00.000Z"
    });
    *state.posted.lock().expect("posted-message lock") = Some(message.clone());
    Json(json!({ "ok": true, "message": message })).into_response()
}

async fn history(State(state): State<WitnessState>, headers: HeaderMap) -> Response {
    if let Some(response) = reject_unless_authorized(&headers) {
        return response;
    }
    let messages = state
        .posted
        .lock()
        .expect("posted-message lock")
        .clone()
        .into_iter()
        .collect::<Vec<_>>();
    Json(json!({ "ok": true, "messages": messages })).into_response()
}

#[test]
fn help_names_the_logical_service_without_offering_a_bearer_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ores-ai-agent-bridge"))
        .arg("--help")
        .output()
        .expect("run ORES bridge client help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains(SERVICE_ID));
    assert!(stdout.contains("ORES_AI_AGENT_BRIDGE_BEARER"));
    assert!(!stdout.contains("--bearer"));
}

#[test]
fn command_line_bearers_are_rejected_without_echoing_the_secret() {
    let secret = "cli-secret-that-must-not-appear";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ores-ai-agent-bridge"))
        .args(["probe", "--bearer", secret])
        .env_remove("ORES_AI_AGENT_BRIDGE_BEARER")
        .env_remove("FIDUCIA_BRIDGE_PREFLIGHT_BEARER")
        .env_remove("API_AUTH_BEARER")
        .output()
        .expect("run ORES bridge client with forbidden bearer flag");

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stdout.is_empty());
    assert!(stderr.contains("unsupported argument"));
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
}

#[test]
fn url_credentials_are_rejected_before_network_io_and_redacted() {
    let secret = "url-secret-that-must-not-appear";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ores-ai-agent-bridge"))
        .args([
            "probe",
            "--base-url",
            &format!("http://operator:{secret}@127.0.0.1:9"),
            "--tcp-port",
            "9",
            "--timeout-seconds",
            "1",
        ])
        .env_remove("ORES_AI_AGENT_BRIDGE_BEARER")
        .env_remove("FIDUCIA_BRIDGE_PREFLIGHT_BEARER")
        .env_remove("API_AUTH_BEARER")
        .output()
        .expect("run ORES bridge client with URL credentials");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let report: Value = serde_json::from_str(&stdout).expect("JSON-only stdout");
    assert_eq!(report["ok"].as_bool(), Some(false));
    assert_eq!(report["service_id"], SERVICE_ID);
    assert_eq!(report["diagnosis"], "connection_or_contract_failure");
    assert!(report["message"]
        .as_str()
        .is_some_and(|message| message.contains("credentials must come from the bearer")));
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
    assert!(stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_smoke_command_proves_connect_resolve_post_and_read_back() {
    let witness = WitnessState::default();
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP witness");
    let http_address = http_listener.local_addr().expect("HTTP address");
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                Json(json!({
                    "service": "ai-agent-bridge",
                    "transports": {
                        "http": "REST + SSE",
                        "tcp": "newline-delimited JSON"
                    }
                }))
            }),
        )
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/agents", get(agents))
        .route("/agents/register", post(register))
        .route("/channels/resolve", post(resolve))
        .route("/channels/{slug}/join", post(join))
        .route("/channels/{slug}/messages", post(post_message).get(history))
        .with_state(witness.clone());
    let http_task = tokio::spawn(async move {
        axum::serve(http_listener, app)
            .await
            .expect("serve HTTP witness");
    });

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind TCP witness");
    let tcp_port = tcp_listener.local_addr().expect("TCP address").port();
    let tcp_task = tokio::spawn(async move {
        let _ = tcp_listener.accept().await.expect("accept TCP witness");
    });

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ores-ai-agent-bridge"))
        .args([
            "smoke",
            "--base-url",
            &format!("http://{http_address}"),
            "--tcp-port",
            &tcp_port.to_string(),
            "--timeout-seconds",
            "2",
        ])
        .env("ORES_AI_AGENT_BRIDGE_BEARER", TEST_BEARER)
        .output()
        .expect("run ORES bridge smoke");

    tcp_task.await.expect("TCP witness task");
    http_task.abort();
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        output.status.success(),
        "stderr: {stderr}; stdout: {stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).expect("JSON-only stdout");
    assert_eq!(report["ok"].as_bool(), Some(true));
    assert_eq!(report["service_id"], SERVICE_ID);
    assert_eq!(report["mode"], "smoke");
    assert_eq!(report["identity"]["wire_service"], "ai-agent-bridge");
    assert_eq!(report["smoke"]["joined"].as_bool(), Some(true));
    assert_eq!(report["smoke"]["sequence"].as_u64(), Some(7));
    assert_eq!(report["smoke"]["read_back"].as_bool(), Some(true));
    assert!(!stdout.contains(TEST_BEARER));
    assert!(!stderr.contains(TEST_BEARER));
    assert!(stderr.is_empty(), "successful smoke keeps stderr empty");

    let posted = witness
        .posted
        .lock()
        .expect("posted-message lock")
        .clone()
        .expect("smoke posted a message");
    assert_eq!(posted["meta"]["service_id"], SERVICE_ID);
    assert_eq!(posted["meta"]["smoke"].as_bool(), Some(true));
}
