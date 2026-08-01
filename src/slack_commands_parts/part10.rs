const OBSERVABLE_EVENT_SCHEMA_VERSION: &str = "1.0";
const OBSERVABLE_EVENT_MAX_BYTES: usize = 65_536;
const OBSERVABLE_PAYLOAD_MAX_BYTES: usize = 32_768;

fn deterministic_uuid(domain: &str, value: &str) -> String {
    let digest = Sha256::digest(format!("{domain}:{value}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn observable_task_created_event(
    config: &Config,
    request: &RunRequest,
    resolved: &crate::slack_project_bindings::ResolvedProjectContext,
    workflow_id: &str,
) -> Value {
    let agent = config.agent_key(request.provider);
    let provider = match request.provider {
        Provider::Claude => "anthropic",
        Provider::Chatgpt => "openai",
    };
    let idempotency_key = format!("slack-task-created:{}", request.run_id);
    let event = json!({
        "schema_version": OBSERVABLE_EVENT_SCHEMA_VERSION,
        "event_id": deterministic_uuid("slack-task-created-event", &request.run_id),
        "idempotency_key": idempotency_key,
        "occurred_at": request.occurred_at,
        "source": {
            "agent_id": agent,
            "provider": provider,
            "model": agent,
            "instance_id": "alex-main-agent",
            "metadata": {
                "surface": "slack_slash_command",
                "action": request.action,
                "write_policy": write_policy(resolved.write_policy)
            }
        },
        "correlation": {
            "correlation_id": deterministic_uuid("slack-run-correlation", &request.run_id),
            "server_id": "ai-agent-coordinator",
            "session_id": request.run_id,
            "run_id": request.run_id,
            "task_id": request.run_id
        },
        "kind": "task_created",
        "payload_classification": "internal",
        "redaction_state": "sanitized",
        "evidence_references": [],
        "delivery": {
            "transport": "http",
            "delivery_id": format!("coordinator-job:{}", request.run_id),
            "attempt": 1,
            "ack_requested": true
        },
        "payload": {
            "repository": resolved.repository,
            "linear_project_id": resolved.linear_project_id,
            "linear_run_project_id": config.linear_run_project_id,
            "linear_issue": resolved.issue.as_ref().map(|issue| issue.identifier.as_str()),
            "bridge_workflow_id": workflow_id,
            "requested_capability": match request.capability {
                RequestedCapability::ReadOnly => "read_only",
                RequestedCapability::LinearWrite => "linear_write",
                RequestedCapability::RepositoryWrite => "repository_write"
            }
        }
    });
    debug_assert!(
        serde_json::to_vec(&event).is_ok_and(|encoded| encoded.len() <= OBSERVABLE_EVENT_MAX_BYTES)
    );
    debug_assert!(
        serde_json::to_vec(&event["payload"])
            .is_ok_and(|encoded| encoded.len() <= OBSERVABLE_PAYLOAD_MAX_BYTES)
    );
    event
}

#[cfg(test)]
mod observable_event_contract_tests {
    use super::*;

    fn request() -> RunRequest {
        RunRequest {
            run_id: "ores-0123456789abcdef01234567".into(),
            source_key: "slash:T-private:C-private:U-private:trigger-private".into(),
            occurred_at: "2026-08-01T22:50:00.000Z".into(),
            provider: Provider::Chatgpt,
            team_id: "T-private".into(),
            channel_id: "C-private".into(),
            user_id: "U-private".into(),
            prompt: "top secret prompt text must never enter the observable event".into(),
            action: "implement".into(),
            repository: Some("ORESoftware/ai-agent-bridge.rs".into()),
            linear_issue: Some("DEN-1061".into()),
            capability: RequestedCapability::RepositoryWrite,
            context_messages: 5,
        }
    }

    fn config() -> Config {
        Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 8151,
            signing_secret: "test-signing-secret".into(),
            bot_token: "xoxb-sensitive-bot-token".into(),
            registry_path: PathBuf::from("/tmp/registry.json"),
            state_dir: PathBuf::from("/tmp/slack-command-state"),
            bridge_url: "http://127.0.0.1:8142/".into(),
            bridge_bearer: Some("sensitive-bridge-bearer".into()),
            coordinator_url: "http://127.0.0.1:8160/".into(),
            coordinator_bearer: Some("sensitive-coordinator-bearer".into()),
            slack_api_base_url: "http://127.0.0.1:8170/api/".into(),
            claude_agent: "claude-fable-5".into(),
            chatgpt_agent: "gpt-5.6-sol".into(),
            linear_run_project_id: DEFAULT_LINEAR_RUN_PROJECT.into(),
            context_messages: 5,
            dry_run: true,
            max_concurrent_runs: 1,
        }
    }

    fn resolved() -> crate::slack_project_bindings::ResolvedProjectContext {
        serde_json::from_value(json!({
            "workspace_id": "T-private",
            "channel_id": "C-private",
            "linear_team_id": "team-1",
            "linear_project_id": "project-1",
            "repository": "ORESoftware/ai-agent-bridge.rs",
            "agent_mode": "chatgpt",
            "capability": "repository_write",
            "write_policy": "draft_pull_request",
            "budget_policy": {
                "max_concurrent_runs": 1,
                "max_runtime_seconds": 900,
                "max_input_tokens": 10000,
                "max_output_tokens": 10000,
                "max_spend_cents": 500,
                "max_retries": 1
            },
            "issue": {
                "identifier": "DEN-1061",
                "team_key": "DEN"
            }
        }))
        .expect("valid resolved project fixture")
    }

    #[test]
    fn projection_is_deterministic_and_v1_shaped() {
        let first = observable_task_created_event(&config(), &request(), &resolved(), "workflow-1");
        let second = observable_task_created_event(&config(), &request(), &resolved(), "workflow-1");
        assert_eq!(first, second);
        assert_eq!(first["schema_version"], OBSERVABLE_EVENT_SCHEMA_VERSION);
        assert_eq!(first["kind"], "task_created");
        assert_eq!(first["redaction_state"], "sanitized");
        assert_eq!(first["delivery"]["transport"], "http");
        assert_eq!(first["delivery"]["ack_requested"], true);
        assert_eq!(first["occurred_at"], "2026-08-01T22:50:00.000Z");
        assert_eq!(first["correlation"]["run_id"], request().run_id);
    }

    #[test]
    fn projection_excludes_prompt_context_principals_and_credentials() {
        let event = observable_task_created_event(&config(), &request(), &resolved(), "workflow-1");
        let encoded = serde_json::to_string(&event).unwrap();
        for forbidden in [
            "top secret prompt",
            "T-private",
            "C-private",
            "U-private",
            "trigger-private",
            "xoxb-sensitive",
            "sensitive-bridge-bearer",
            "sensitive-coordinator-bearer",
            "source_key",
            "prompt",
            "context",
            "user_id",
            "channel_id",
            "team_id",
            "chain_of_thought",
            "hidden_reasoning",
        ] {
            assert!(!encoded.contains(forbidden), "leaked forbidden fragment: {forbidden}");
        }
        assert!(encoded.len() <= OBSERVABLE_EVENT_MAX_BYTES);
        assert!(serde_json::to_vec(&event["payload"]).unwrap().len() <= OBSERVABLE_PAYLOAD_MAX_BYTES);
    }

    #[test]
    fn deterministic_ids_are_rfc4122_version_five_shape() {
        let value = deterministic_uuid("domain", "value");
        let parts = value.split('-').collect::<Vec<_>>();
        assert_eq!(parts.iter().map(|part| part.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('5'));
        assert!(matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')));
        assert_eq!(value, deterministic_uuid("domain", "value"));
        assert_ne!(value, deterministic_uuid("domain", "other"));
    }
}
