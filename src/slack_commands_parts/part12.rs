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

fn observable_provider(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "anthropic",
        Provider::Chatgpt => "openai",
    }
}

fn observable_capability(capability: RequestedCapability) -> &'static str {
    match capability {
        RequestedCapability::ReadOnly => "read_only",
        RequestedCapability::LinearWrite => "linear_write",
        RequestedCapability::RepositoryWrite => "repository_write",
    }
}

fn observable_task_created_event(
    config: &Config,
    request: &RunRequest,
    resolved: &crate::slack_project_bindings::ResolvedProjectContext,
    workflow_id: &str,
) -> Value {
    let agent = config.agent_key(request.provider);
    let event = json!({
        "schema_version": OBSERVABLE_EVENT_SCHEMA_VERSION,
        "event_id": deterministic_uuid("slack-task-created-event", &request.run_id),
        "idempotency_key": format!("slack-task-created:{}", request.run_id),
        "occurred_at": request.occurred_at,
        "source": {
            "agent_id": agent,
            "provider": observable_provider(request.provider),
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
            "requested_capability": observable_capability(request.capability)
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
            source_key: "slash:workspace-private:channel-private:user-private:trigger-private".into(),
            occurred_at: "2026-08-03T11:20:00.000Z".into(),
            provider: Provider::Chatgpt,
            team_id: "workspace-private".into(),
            channel_id: "channel-private".into(),
            user_id: "user-private".into(),
            prompt: "private prompt text must never enter the observable event".into(),
            action: "implement".into(),
            repository: Some("ORESoftware/ai-agent-bridge.rs".into()),
            linear_issue: Some("DEN-1061".into()),
            capability: RequestedCapability::RepositoryWrite,
            context_messages: 5,
        }
    }

    fn config() -> Config {
        Config {
            host: "127.0.0.1".parse().expect("loopback IP"),
            port: 8151,
            signing_secret: "test-signing-secret".into(),
            bot_token: "test-bot-secret".into(),
            registry_path: PathBuf::from("/tmp/registry.json"),
            state_dir: PathBuf::from("/tmp/slack-command-state"),
            bridge_url: "http://127.0.0.1:8142/".into(),
            bridge_bearer: Some("test-bridge-secret".into()),
            coordinator_url: "http://127.0.0.1:8160/".into(),
            coordinator_bearer: Some("test-coordinator-secret".into()),
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
        crate::slack_project_bindings::ResolvedProjectContext {
            workspace_id: "workspace-private".into(),
            channel_id: "channel-private".into(),
            linear_team_id: "team-1".into(),
            linear_team_key: "DEN".into(),
            linear_project_id: "project-1".into(),
            repository: "ORESoftware/ai-agent-bridge.rs".into(),
            agent_mode: AgentMode::Chatgpt,
            write_policy: WritePolicy::DraftPullRequest,
            budget_policy: crate::slack_project_bindings::BudgetPolicy {
                max_concurrent_runs: 1,
                max_runtime_secs: 900,
                max_tokens: 20_000,
                max_spend_cents: 500,
                max_retries: 1,
            },
            issue: Some(crate::slack_project_bindings::LinearIssueRef {
                identifier: "DEN-1061".into(),
                team_key: "DEN".into(),
                number: 1061,
            }),
        }
    }

    fn contains_forbidden_key(value: &Value) -> bool {
        const FORBIDDEN: [&str; 10] = [
            "prompt",
            "context",
            "source_key",
            "workspace_id",
            "channel_id",
            "user_id",
            "team_id",
            "chain_of_thought",
            "hidden_reasoning",
            "scratchpad",
        ];
        match value {
            Value::Object(object) => object.iter().any(|(key, child)| {
                FORBIDDEN.contains(&key.as_str()) || contains_forbidden_key(child)
            }),
            Value::Array(values) => values.iter().any(contains_forbidden_key),
            _ => false,
        }
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
        assert_eq!(first["occurred_at"], "2026-08-03T11:20:00.000Z");
        assert_eq!(first["correlation"]["run_id"], request().run_id);
    }

    #[test]
    fn projection_excludes_private_inputs_credentials_and_reasoning_fields() {
        let event = observable_task_created_event(&config(), &request(), &resolved(), "workflow-1");
        assert!(!contains_forbidden_key(&event));
        let encoded = serde_json::to_string(&event).expect("serialize event");
        for forbidden in [
            "private prompt text",
            "workspace-private",
            "channel-private",
            "user-private",
            "trigger-private",
            "test-bot-secret",
            "test-bridge-secret",
            "test-coordinator-secret",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "leaked forbidden fragment: {forbidden}"
            );
        }
    }

    #[test]
    fn private_prompt_and_principal_changes_do_not_change_projection() {
        let original = request();
        let mut changed = original.clone();
        changed.source_key = "slash:other-workspace:other-channel:other-user:other-trigger".into();
        changed.team_id = "other-workspace".into();
        changed.channel_id = "other-channel".into();
        changed.user_id = "other-user".into();
        changed.prompt = "completely different private prompt".into();
        let first = observable_task_created_event(&config(), &original, &resolved(), "workflow-1");
        let second = observable_task_created_event(&config(), &changed, &resolved(), "workflow-1");
        assert_eq!(first, second);
    }

    #[test]
    fn provider_and_capability_mapping_is_explicit() {
        let mut claude = request();
        claude.provider = Provider::Claude;
        claude.capability = RequestedCapability::ReadOnly;
        let event = observable_task_created_event(&config(), &claude, &resolved(), "workflow-1");
        assert_eq!(event["source"]["provider"], "anthropic");
        assert_eq!(event["source"]["agent_id"], "claude-fable-5");
        assert_eq!(event["payload"]["requested_capability"], "read_only");
    }

    #[test]
    fn deterministic_ids_are_rfc4122_version_five_shape() {
        let value = deterministic_uuid("domain", "value");
        let parts = value.split('-').collect::<Vec<_>>();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('5'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
        assert_eq!(value, deterministic_uuid("domain", "value"));
        assert_ne!(value, deterministic_uuid("domain", "other"));
    }

    #[test]
    fn event_and_payload_remain_inside_contract_limits() {
        let event = observable_task_created_event(&config(), &request(), &resolved(), "workflow-1");
        assert!(
            serde_json::to_vec(&event).expect("serialize event").len()
                <= OBSERVABLE_EVENT_MAX_BYTES
        );
        assert!(
            serde_json::to_vec(&event["payload"])
                .expect("serialize payload")
                .len()
                <= OBSERVABLE_PAYLOAD_MAX_BYTES
        );
    }
}
