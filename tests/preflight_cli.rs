use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;

const TEST_BEARER: &str = "preflight-test-secret-that-must-not-leak";

async fn authenticated_agents(headers: HeaderMap) -> StatusCode {
    match headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if value == format!("Bearer {TEST_BEARER}") => StatusCode::OK,
        _ => StatusCode::UNAUTHORIZED,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_binary_emits_json_only_and_never_prints_the_bearer() {
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP witness");
    let http_address = http_listener.local_addr().expect("HTTP address");
    let app = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/agents", get(authenticated_agents));
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

    let output =
        std::process::Command::new(env!("CARGO_BIN_EXE_fiducia-ai-agent-bridge-preflight"))
            .args([
                "--base-url",
                &format!("http://{http_address}"),
                "--tcp-port",
                &tcp_port.to_string(),
                "--timeout-seconds",
                "2",
            ])
            .env("FIDUCIA_BRIDGE_PREFLIGHT_BEARER", TEST_BEARER)
            .output()
            .expect("run preflight binary");

    tcp_task.await.expect("TCP witness task");
    http_task.abort();
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(output.status.success(), "stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON-only stdout");
    assert_eq!(report["ok"], true);
    assert_eq!(report["diagnosis"], "ready");
    assert!(!stdout.contains(TEST_BEARER));
    assert!(!stderr.contains(TEST_BEARER));
    assert!(stderr.is_empty(), "successful preflight keeps stderr empty");
}
