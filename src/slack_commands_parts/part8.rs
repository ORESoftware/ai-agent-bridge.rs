#[cfg(test)]
mod command_integration_tests {
    use std::{fs, path::PathBuf, sync::Arc, time::Duration};

    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use tokio::{net::TcpListener, sync::Mutex};
    use uuid::Uuid;

    use super::*;

    #[derive(Clone, Default)]
    struct MockState {
        views: Arc<Mutex<Vec<Value>>>,
        messages: Arc<Mutex<Vec<Value>>>,
        workflows: Arc<Mutex<Vec<Value>>>,
        jobs: Arc<Mutex<Vec<Value>>>,
        idempotency_keys: Arc<Mutex<Vec<String>>>,
    }

    async fn history() -> Json<Value> {
        Json(json!({
            "ok": true,
            "messages": [
                {"user": "U6", "ts": "1006.000001", "text": "message-6"},
                {"user": "U5", "ts": "1005.000001", "text": "message-5"},
                {"bot_id": "B1", "ts": "1004.500001", "text": "ignore-bot"},
                {"user": "U4", "ts": "1004.000001", "text": "message-4"},
                {"user": "U3", "ts": "1003.000001", "text": "message-3"},
                {"user": "U2", "ts": "1002.000001", "text": "message-2"},
                {"user": "U1", "ts": "1001.000001", "text": "message-1"}
            ]
        }))
    }

    async fn usergroups() -> Json<Value> {
        Json(json!({"ok": true, "usergroups": []}))
    }

    async fn open_view(State(state): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
        state.views.lock().await.push(body);
        Json(json!({"ok": true}))
    }

    async fn post_message(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.messages.lock().await.push(body);
        Json(json!({"ok": true, "ts": "2000.000001"}))
    }

    async fn create_workflow(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let agent = body["agent_keys"][0]
            .as_str()
            .expect("single agent key")
            .to_string();
        state.workflows.lock().await.push(body);
        Json(json!({
            "workflow": {
                "plan": {
                    "id": "workflow-test-1",
                    "assignments": [{"agent_key": agent}]
                }
            }
        }))
    }

    async fn create_job(
        State(state): State<MockState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let key = headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .expect("idempotency key")
            .to_string();
        state.idempotency_keys.lock().await.push(key);
        state.jobs.lock().await.push(body);
        (
            StatusCode::ACCEPTED,
            Json(json!({"job": {"id": "job-test-1"}})),
        )
    }

    async fn spawn_mock() -> (String, MockState) {
        let state = MockState::default();
        let app = Router::new()
            .route("/api/conversations.history", get(history))
            .route("/api/usergroups.list", get(usergroups))
            .route("/api/views.open", post(open_view))
            .route("/api/chat.postMessage", post(post_message))
            .route("/workflows", post(create_workflow))
            .route("/v1/jobs", post(create_job))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), state)
    }

    fn unique_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fiducia-slack-command-{label}-{}",
            Uuid::new_v4()
        ))
    }

    fn write_registry() -> PathBuf {
        let path = unique_path("registry.json");
        let document = json!({
            "schema_version": 1,
            "bindings": [{
                "workspace_id": "T1",
                "channel_id": "C1",
                "linear_team_id": "team-uuid",
                "linear_team_key": "DEN",
                "linear_project_id": "project-uuid",
                "default_repository": "ORESoftware/ai-agent-bridge.rs",
                "repository_allowlist": [
                    "ORESoftware/ai-agent-bridge.rs",
                    "ORESoftware/ai-agent-coordinator.rs"
                ],
                "default_agent_mode": "chatgpt",
                "allowed_agent_modes": ["claude", "chatgpt"],
                "allowed_user_ids": ["U1"],
                "allowed_user_group_ids": [],
                "write_policy": "draft_pull_request",
                "budget_policy": {
                    "max_concurrent_runs": 2,
                    "max_runtime_secs": 600,
                    "max_tokens": 100000,
                    "max_spend_cents": 500,
                    "max_retries": 2
                },
                "updated_by": "U1",
                "updated_at": "2026-08-01T12:00:00Z"
            }]
        });
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        path
    }

    async fn spawn_command_service(mock_base: &str) -> (String, PathBuf) {
        let state_dir = unique_path("state");
        let config = Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 8151,
            signing_secret: "integration-signing-secret".into(),
            bot_token: "xoxb-integration-test".into(),
            registry_path: write_registry(),
            state_dir: state_dir.clone(),
            bridge_url: format!("{mock_base}/"),
            bridge_bearer: None,
            coordinator_url: format!("{mock_base}/"),
            coordinator_bearer: None,
            slack_api_base_url: format!("{mock_base}/api/"),
            claude_agent: "claude-fable-5".into(),
            chatgpt_agent: "gpt-5.6-sol".into(),
            linear_run_project_id: DEFAULT_LINEAR_RUN_PROJECT.into(),
            context_messages: 5,
            dry_run: false,
            max_concurrent_runs: 4,
        };
        let app = Arc::new(App::new(config).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(app)).await.unwrap();
        });
        (format!("http://{address}"), state_dir)
    }

    fn signed_headers(body: &str, timestamp: i64) -> HeaderMap {
        let mut mac = HmacSha256::new_from_slice(b"integration-signing-secret").unwrap();
        mac.update(b"v0:");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b":");
        mac.update(body.as_bytes());
        let signature = mac.finalize().into_bytes();
        let encoded = signature
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            timestamp.to_string().parse().unwrap(),
        );
        headers.insert(
            "x-slack-signature",
            format!("v0={encoded}").parse().unwrap(),
        );
        headers.insert(
            "content-type",
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        headers
    }

    async fn wait_for_count(values: &Arc<Mutex<Vec<Value>>>, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if values.lock().await.len() >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("mock call deadline");
    }

    #[tokio::test]
    async fn signed_direct_command_dispatches_exactly_once_with_five_human_messages() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, _) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let body = "command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Implement+DEN-1041&trigger_id=trigger-1";
        let timestamp = Utc::now().timestamp();

        let response = client
            .post(format!("{service_base}/slack/commands/ores-chatgpt"))
            .headers(signed_headers(body, timestamp))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let accepted = response.json::<Value>().await.unwrap();
        assert!(accepted["text"].as_str().unwrap().contains("Accepted ChatGPT run"));

        wait_for_count(&mock.messages, 1).await;
        assert_eq!(mock.workflows.lock().await.len(), 1);
        assert_eq!(mock.jobs.lock().await.len(), 1);
        assert_eq!(mock.idempotency_keys.lock().await.len(), 1);

        let jobs = mock.jobs.lock().await;
        let messages = jobs[0]["payload"]["context"]["messages"]
            .as_array()
            .unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["text"], "message-2");
        assert_eq!(messages[4]["text"], "message-6");
        assert_eq!(jobs[0]["payload"]["context"]["trust"], "untrusted_channel_context");
        assert_eq!(jobs[0]["payload"]["routing"]["linear_issue"], "DEN-1041");
        drop(jobs);

        let duplicate = client
            .post(format!("{service_base}/slack/commands/ores-chatgpt"))
            .headers(signed_headers(body, timestamp))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate.status(), reqwest::StatusCode::OK);
        let duplicate = duplicate.json::<Value>().await.unwrap();
        assert!(duplicate["text"].as_str().unwrap().contains("already accepted"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(mock.workflows.lock().await.len(), 1);
        assert_eq!(mock.jobs.lock().await.len(), 1);
        assert_eq!(mock.messages.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn empty_command_opens_the_reviewed_modal_defaults() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, _) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let body = "command=%2Fores-claude&team_id=T1&channel_id=C1&user_id=U1&text=&trigger_id=trigger-modal";
        let response = client
            .post(format!("{service_base}/slack/commands/ores-claude"))
            .headers(signed_headers(body, Utc::now().timestamp()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        wait_for_count(&mock.views, 1).await;

        let views = mock.views.lock().await;
        let blocks = views[0]["view"]["blocks"].as_array().unwrap();
        assert_eq!(views[0]["view"]["callback_id"], CALLBACK_ID);
        assert_eq!(blocks[0]["block_id"], "task");
        assert_eq!(blocks[1]["block_id"], "action");
        assert_eq!(blocks[2]["block_id"], "repository");
        assert_eq!(blocks[4]["block_id"], "write_scope");
        assert_eq!(blocks[5]["block_id"], "context_messages");
        assert_eq!(
            blocks[5]["element"]["initial_option"]["value"],
            "5"
        );
        assert_eq!(
            blocks[4]["element"]["initial_option"]["value"],
            "draft_pull_request"
        );
    }

    #[tokio::test]
    async fn unauthorized_channel_is_rejected_before_run_claim() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, state_dir) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let body = "command=%2Fores-chatgpt&team_id=T1&channel_id=C2&user_id=U1&text=Implement+DEN-1041&trigger_id=trigger-denied";
        let response = client
            .post(format!("{service_base}/slack/commands/ores-chatgpt"))
            .headers(signed_headers(body, Utc::now().timestamp()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(fs::read_dir(state_dir).unwrap().next().is_none());
        assert!(mock.workflows.lock().await.is_empty());
        assert!(mock.jobs.lock().await.is_empty());
        assert!(mock.messages.lock().await.is_empty());
    }

    #[tokio::test]
    async fn stale_or_tampered_signatures_are_rejected() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, state_dir) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let body = "command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Implement+DEN-1041&trigger_id=trigger-auth";
        let stale = Utc::now().timestamp() - 600;
        let response = client
            .post(format!("{service_base}/slack/commands/ores-chatgpt"))
            .headers(signed_headers(body, stale))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

        let response = client
            .post(format!("{service_base}/slack/commands/ores-chatgpt"))
            .headers(signed_headers(body, Utc::now().timestamp()))
            .body(format!("{body}+tampered"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(fs::read_dir(state_dir).unwrap().next().is_none());
        assert!(mock.workflows.lock().await.is_empty());
    }
}
