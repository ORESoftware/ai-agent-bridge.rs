//! Prometheus endpoint and bounded-cardinality contract tests.

mod common;

use ai_agent_bridge::metrics;
use ai_agent_bridge::types::{Agent, AgentKind, Role};
use serde_json::json;

#[tokio::test]
async fn metrics_endpoint_exposes_valid_bounded_text_without_identity_labels() {
    let state = common::state();
    state
        .register_agent(Agent {
            agent_key: "sensitive-agent-key".into(),
            display_name: "Sensitive Agent".into(),
            kind: AgentKind::Codex,
            host: None,
            meta: json!({}),
            registered_at: String::new(),
        })
        .unwrap();
    state
        .create_or_get_channel(
            "sensitive-channel-name",
            "sensitive repository discussion",
            "sensitive-agent-key",
        )
        .await
        .unwrap();
    state
        .post_message(
            "sensitive-channel-name",
            "sensitive-agent-key",
            Role::Assistant,
            "sensitive message body",
            json!({"repository":"secret/repository"}),
        )
        .unwrap();

    let base = common::spawn_http(state).await;
    let response = reqwest::get(format!("{base}/metrics")).await.unwrap();
    assert!(response.status().is_success());
    assert!(response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain")));
    let text = response.text().await.unwrap();

    for expected in [
        "# TYPE ai_agent_bridge_build_info gauge",
        "# TYPE ai_agent_bridge_http_requests_total counter",
        "# TYPE ai_agent_bridge_tcp_connections_active gauge",
        "# TYPE ai_agent_bridge_messages_total counter",
        "# TYPE ai_agent_bridge_file_leases_active gauge",
        "# TYPE ai_agent_bridge_persistence_info gauge",
        "# TYPE ai_agent_bridge_control_plane_requests_total counter",
        "ai_agent_bridge_agents 1",
        "ai_agent_bridge_channels 1",
        "ai_agent_bridge_messages_retained 1",
    ] {
        assert!(text.contains(expected), "missing metric: {expected}");
    }

    for forbidden in [
        "sensitive-agent-key",
        "sensitive-channel-name",
        "sensitive repository discussion",
        "sensitive message body",
        "secret/repository",
    ] {
        assert!(
            !text.contains(forbidden),
            "sensitive value leaked: {forbidden}"
        );
    }
}

#[tokio::test]
async fn registry_records_bounded_http_message_and_lease_error_outcomes() {
    let state = common::state();
    let started = metrics::global().http_started();
    metrics::global().http_finished(started, 429);
    metrics::global().http_capacity_rejected();
    metrics::global().observe_message(std::time::Duration::from_millis(2), 0, 3);
    metrics::global().observe_bridge_error(
        &ai_agent_bridge::error::BridgeError::StaleFencingToken("opaque-id".into()),
    );

    let text = metrics::global().render(&state);
    assert!(text.contains("ai_agent_bridge_http_requests_total{status_class=\"4xx\"}"));
    assert!(text.contains("ai_agent_bridge_http_rejected_total{reason=\"capacity\"}"));
    assert!(text.contains("ai_agent_bridge_messages_total{result=\"no_subscribers\"}"));
    assert!(
        text.contains("ai_agent_bridge_file_lease_errors_total{reason=\"stale_fencing_token\"}")
    );
    assert!(!text.contains("opaque-id"));
}
