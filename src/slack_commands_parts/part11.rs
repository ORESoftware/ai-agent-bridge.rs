#[cfg(test)]
mod alias_http_contract_tests {
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
        messages: Arc<Mutex<Vec<Value>>>,
        workflows: Arc<Mutex<Vec<Value>>>,
        jobs: Arc<Mutex<Vec<Value>>>,
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

    async fn post_message(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.messages.lock().await.push(body);
        Json(json!({"ok": true, "ts": "2000.000001"}))
    }

    async fn unexpected_workflow(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        state.workflows.lock().await.push(body);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "dry_run_must_not_create_workflow"})),
        )
    }

    async fn unexpected_job(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        state.jobs.lock().await.push(body);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "dry_run_must_not_create_job"})),
        )
    }

    async fn spawn_mock() -> (String, MockState) {
        let state = MockState::default();
        let app = Router::new()
            .route("/api/conversations.history", get(history))
            .route("/api/usergroups.list", get(usergroups))
            .route("/api/chat.postMessage", post(post_message))
            .route("/workflows", post(unexpected_workflow))
            .route("/v1/jobs", post(unexpected_job))
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
            "fiducia-slack-alias-http-{label}-{}",
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
                "updated_at": "2026-08-03T12:00:00Z"
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
            signing_secret: "alias-http-signing-secret".into(),
            bot_token: "test-bot-token".into(),
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
            dry_run: true,
            max_concurrent_runs: 8,
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
        let mut mac = HmacSha256::new_from_slice(b"alias-http-signing-secret").unwrap();
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

    async fn wait_for_messages(messages: &Arc<Mutex<Vec<Value>>>, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if messages.lock().await.len() >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("dry-run Slack message deadline");
    }

    #[tokio::test]
    async fn all_six_reviewed_commands_use_only_the_two_canonical_http_routes() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, _) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();
        let cases = [
            ("%2Fores-claude", "/slack/commands/ores-claude", "Claude"),
            ("%2Fx-claude", "/slack/commands/ores-claude", "Claude"),
            ("%2Fmy-claude", "/slack/commands/ores-claude", "Claude"),
            (
                "%2Fores-chatgpt",
                "/slack/commands/ores-chatgpt",
                "ChatGPT",
            ),
            (
                "%2Fx-chatgpt",
                "/slack/commands/ores-chatgpt",
                "ChatGPT",
            ),
            (
                "%2Fmy-chatgpt",
                "/slack/commands/ores-chatgpt",
                "ChatGPT",
            ),
        ];

        for (index, (command, endpoint, provider)) in cases.into_iter().enumerate() {
            let body = format!(
                "command={command}&team_id=T1&channel_id=C1&user_id=U1&text=alias-case-{index}&trigger_id=trigger-{index}"
            );
            let response = client
                .post(format!("{service_base}{endpoint}"))
                .headers(signed_headers(&body, Utc::now().timestamp()))
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let accepted = response.json::<Value>().await.unwrap();
            assert!(accepted["text"]
                .as_str()
                .unwrap()
                .contains(&format!("Accepted {provider} run")));
        }

        wait_for_messages(&mock.messages, 6).await;
        let messages = mock.messages.lock().await;
        assert_eq!(messages.len(), 6);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["text"]
                    .as_str()
                    .unwrap()
                    .contains("Dry-run Claude task"))
                .count(),
            3
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["text"]
                    .as_str()
                    .unwrap()
                    .contains("Dry-run ChatGPT task"))
                .count(),
            3
        );
        for message in messages.iter() {
            assert_eq!(message["channel"], "C1");
            assert!(message["text"]
                .as_str()
                .unwrap()
                .contains("No coordinator, bridge, Linear, or GitHub write was performed."));
        }
        drop(messages);
        assert!(mock.workflows.lock().await.is_empty());
        assert!(mock.jobs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn provider_mismatches_and_alias_paths_fail_before_a_run_claim() {
        let (mock_base, mock) = spawn_mock().await;
        let (service_base, state_dir) = spawn_command_service(&mock_base).await;
        let client = reqwest::Client::new();

        for (command, endpoint) in [
            ("%2Fx-chatgpt", "/slack/commands/ores-claude"),
            ("%2Fmy-claude", "/slack/commands/ores-chatgpt"),
        ] {
            let body = format!(
                "command={command}&team_id=T1&channel_id=C1&user_id=U1&text=must-not-run&trigger_id=mismatch"
            );
            let response = client
                .post(format!("{service_base}{endpoint}"))
                .headers(signed_headers(&body, Utc::now().timestamp()))
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
            let denied = response.json::<Value>().await.unwrap();
            assert_eq!(denied["text"], "Invalid slash command payload.");
        }

        let alias_body = "command=%2Fx-claude&team_id=T1&channel_id=C1&user_id=U1&text=must-not-run&trigger_id=alias-path";
        let alias_path = client
            .post(format!("{service_base}/slack/commands/x-claude"))
            .headers(signed_headers(alias_body, Utc::now().timestamp()))
            .body(alias_body)
            .send()
            .await
            .unwrap();
        assert_eq!(alias_path.status(), reqwest::StatusCode::NOT_FOUND);

        let generic_path = client
            .post(format!("{service_base}/slack"))
            .headers(signed_headers(alias_body, Utc::now().timestamp()))
            .body(alias_body)
            .send()
            .await
            .unwrap();
        assert_eq!(generic_path.status(), reqwest::StatusCode::NOT_FOUND);

        let wrong_method = client
            .get(format!("{service_base}/slack/commands/ores-claude"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            wrong_method.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(fs::read_dir(state_dir).unwrap().next().is_none());
        assert!(mock.messages.lock().await.is_empty());
        assert!(mock.workflows.lock().await.is_empty());
        assert!(mock.jobs.lock().await.is_empty());
    }
}
