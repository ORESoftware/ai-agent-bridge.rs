#[cfg(test)]
mod coordinator_idempotency_http_contract_tests {
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
    struct LiveMockState {
        workflows: Arc<Mutex<Vec<Value>>>,
        jobs: Arc<Mutex<Vec<Value>>>,
        messages: Arc<Mutex<Vec<Value>>>,
    }

    async fn history() -> Json<Value> {
        Json(json!({
            "ok": true,
            "messages": [
                {"user": "U5", "ts": "1005.000001", "text": "message-5"},
                {"user": "U4", "ts": "1004.000001", "text": "message-4"},
                {"bot_id": "B1", "ts": "1003.500001", "text": "ignore-bot"},
                {"user": "U3", "ts": "1003.000001", "text": "message-3"},
                {"user": "U2", "ts": "1002.000001", "text": "message-2"},
                {"user": "U1", "ts": "1001.000001", "text": "message-1"}
            ]
        }))
    }

    async fn usergroups() -> Json<Value> {
        Json(json!({"ok": true, "usergroups": []}))
    }

    async fn create_workflow(
        State(state): State<LiveMockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let agent = body["created_by"]
            .as_str()
            .expect("workflow created_by")
            .to_string();
        state.workflows.lock().await.push(body);
        Json(json!({
            "ok": true,
            "workflow": {
                "plan": {
                    "id": "workflow-live-1",
                    "assignments": [{"agent_key": agent}]
                }
            }
        }))
    }

    async fn create_job(
        State(state): State<LiveMockState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let idempotency_key = headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let payload_run_id = body["payload"]["run_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        state.jobs.lock().await.push(json!({
            "idempotency_key": idempotency_key,
            "body": body
        }));
        if idempotency_key != payload_run_id
            || !idempotency_key.starts_with("ores-")
            || idempotency_key.len() != 29
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid_slack_run_idempotency_key"})),
            );
        }
        (
            StatusCode::CREATED,
            Json(json!({"job": {"id": "job-live-1"}})),
        )
    }

    async fn post_message(
        State(state): State<LiveMockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.messages.lock().await.push(body);
        Json(json!({"ok": true, "ts": "2000.000001"}))
    }

    async fn spawn_mock() -> (String, LiveMockState) {
        let state = LiveMockState::default();
        let app = Router::new()
            .route("/api/conversations.history", get(history))
            .route("/api/usergroups.list", get(usergroups))
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
            "fiducia-slack-idempotency-http-{label}-{}",
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
                "repository_allowlist": ["ORESoftware/ai-agent-bridge.rs"],
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
                "updated_at": "2026-08-04T12:00:00Z"
            }]
        });
        fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        path
    }

    async fn spawn_command_service(
        mock_base: &str,
    ) -> (String, PathBuf, PathBuf) {
        let state_dir = unique_path("state");
        let registry_path = write_registry();
        let config = Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 8151,
            signing_secret: "idempotency-http-signing-secret".into(),
            bot_token: "test-bot-token".into(),
            registry_path: registry_path.clone(),
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
            max_concurrent_runs: 8,
        };
        let app = Arc::new(App::new(config).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(app)).await.unwrap();
        });
        (format!("http://{address}"), state_dir, registry_path)
    }

    fn signed_headers(body: &str, timestamp: i64) -> HeaderMap {
        let mut mac = HmacSha256::new_from_slice(b"idempotency-http-signing-secret").unwrap();
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

    async fn wait_for_dispatch(state: &LiveMockState) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.jobs.lock().await.len() == 1
                    && state.workflows.lock().await.len() == 1
                    && state.messages.lock().await.len() == 1
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("live Slack dispatch deadline");
    }

    #[tokio::test]
    async fn live_command_uses_exact_run_id_for_coordinator_idempotency() {
        let (mock_base, state) = spawn_mock().await;
        let (service_base, state_dir, registry_path) =
            spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let body = "command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Implement+DEN-1231+exact+idempotency&trigger_id=trigger-live";
        let expected_run_id = run_id("slash:T1:C1:U1:trigger-live");

        let response = client
            .post(format!(
                "{service_base}/slack/commands/ores-chatgpt"
            ))
            .headers(signed_headers(body, Utc::now().timestamp()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let accepted = response.json::<Value>().await.unwrap();
        assert!(accepted["text"]
            .as_str()
            .unwrap()
            .contains(&format!("Accepted ChatGPT run `{expected_run_id}`")));

        wait_for_dispatch(&state).await;

        let jobs = state.jobs.lock().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["idempotency_key"], expected_run_id);
        assert_eq!(jobs[0]["body"]["task_type"], "slack_agent_run");
        assert_eq!(
            jobs[0]["body"]["payload"]["run_id"],
            jobs[0]["idempotency_key"]
        );
        assert_eq!(
            jobs[0]["body"]["payload"]["observable_event"]["correlation"]["run_id"],
            expected_run_id
        );
        assert!(!jobs[0]["idempotency_key"]
            .as_str()
            .unwrap()
            .starts_with("slack-command:"));
        drop(jobs);

        let workflows = state.workflows.lock().await;
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0]["meta"]["run_id"], expected_run_id);
        drop(workflows);

        let messages = state.messages.lock().await;
        assert_eq!(messages.len(), 1);
        let text = messages[0]["text"].as_str().unwrap();
        assert!(text.contains("ChatGPT work dispatched"));
        assert!(text.contains(&expected_run_id));
        assert!(text.contains("job-live-1"));
        assert!(text.contains("workflow-live-1"));
        drop(messages);

        let duplicate = client
            .post(format!(
                "{service_base}/slack/commands/ores-chatgpt"
            ))
            .headers(signed_headers(body, Utc::now().timestamp()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate.status(), reqwest::StatusCode::OK);
        let duplicate = duplicate.json::<Value>().await.unwrap();
        assert!(duplicate["text"]
            .as_str()
            .unwrap()
            .contains(&format!("Run `{expected_run_id}` was already accepted.")));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(state.jobs.lock().await.len(), 1);
        assert_eq!(state.workflows.lock().await.len(), 1);
        assert_eq!(state.messages.lock().await.len(), 1);

        fs::remove_file(registry_path).unwrap();
        fs::remove_dir_all(state_dir).unwrap();
    }
}
