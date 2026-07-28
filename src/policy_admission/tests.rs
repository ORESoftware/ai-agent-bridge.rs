mod tests {
    use crate::config::Config;
    use crate::embed::Embedder;
    use crate::orchestration::{WorkflowAssignment, WorkflowPlan};
    use crate::policy::{
        DataSensitivity, ProviderCandidate, RequestedBudget, TaskRisk,
    };
    use crate::types::{AgentKind, now_ts};

    use super::*;

    async fn state_with_plan(mode: WorkflowMode) -> Arc<AppState> {
        let config = Config::in_memory();
        let embedder = Embedder::new(
            config.embed_dim,
            None,
            "local".into(),
            None,
            config.max_embedding_response_bytes,
        );
        let state = AppState::new(config, embedder).unwrap();
        let workflow_id = "workflow-test";
        let channel = format!("workflow-{workflow_id}");
        state
            .create_or_get_channel(&channel, "test workflow", "coordinator")
            .await
            .unwrap();
        let plan = WorkflowPlan {
            version: 1,
            id: workflow_id.into(),
            channel: channel.clone(),
            title: "Test workflow".into(),
            prompt: "Produce a result".into(),
            mode,
            created_by: "coordinator".into(),
            created_at: now_ts(),
            assignments: vec![WorkflowAssignment {
                ordinal: 0,
                agent_key: "codex".into(),
                role: crate::orchestration::AssignmentRole::Worker,
                phase: 0,
            }],
            file_lease: None,
            required_capabilities: vec!["rust".into()],
            meta: json!({}),
        };
        state
            .set_context(
                &channel,
                WORKFLOW_PLAN_CONTEXT_KEY,
                serde_json::to_value(plan).unwrap(),
                "coordinator",
            )
            .unwrap();
        state
    }

    fn request(max_cost_micro_usd: u64) -> AdmitReq {
        AdmitReq {
            requested_by: "coordinator".into(),
            approved_by: None,
            override_reason: None,
            policy_request: PolicyRequest {
                task_risk: TaskRisk::Low,
                data_sensitivity: DataSensitivity::Internal,
                requested_mode: Some(WorkflowMode::Single),
                required_capabilities: vec!["rust".into()],
                requires_repository_write: false,
                expected_duration_ms: 0,
                requested_budget: RequestedBudget {
                    max_cost_micro_usd: Some(max_cost_micro_usd),
                    ..RequestedBudget::default()
                },
                providers: vec![ProviderCandidate {
                    agent_key: "codex".into(),
                    kind: AgentKind::Codex,
                    model: "codex-test".into(),
                    available: true,
                    capabilities: vec!["rust".into()],
                    trusted_for_restricted: false,
                    health_score_bps: 10_000,
                    p95_latency_ms: 10,
                    estimated_cost_micro_usd: 0,
                    max_context_tokens: 100_000,
                }],
            },
        }
    }

    #[tokio::test]
    async fn duplicate_admission_is_idempotent_and_insert_only() {
        let state = state_with_plan(WorkflowMode::Single).await;
        let (first, created) =
            create_admission(&state, "workflow-test", request(100)).unwrap();
        assert!(created);
        let (second, created) =
            create_admission(&state, "workflow-test", request(1)).unwrap();
        assert!(!created);
        assert_eq!(first.policy.budget.max_cost_micro_usd, 100);
        assert_eq!(second.policy.budget.max_cost_micro_usd, 100);
    }

    #[tokio::test]
    async fn one_unit_overage_exhausts_without_accepting_the_delta() {
        let state = state_with_plan(WorkflowMode::Single).await;
        create_admission(&state, "workflow-test", request(100)).unwrap();
        let accepted = record_usage(
            &state,
            "workflow-test",
            "codex",
            UsageDelta {
                cost_micro_usd: 100,
                provider_calls: 1,
                concurrency: 1,
                ..UsageDelta::default()
            },
        )
        .unwrap();
        assert_eq!(accepted.usage.cost_micro_usd, 100);
        assert_eq!(accepted.status, AdmissionStatus::Active);

        let error = record_usage(
            &state,
            "workflow-test",
            "codex",
            UsageDelta {
                cost_micro_usd: 1,
                ..UsageDelta::default()
            },
        )
        .unwrap_err();
        let exhausted = error.admission.unwrap();
        assert_eq!(exhausted.status, AdmissionStatus::Exhausted);
        assert_eq!(exhausted.usage.cost_micro_usd, 100);
        assert_eq!(
            exhausted.last_rejected_delta.unwrap().cost_micro_usd,
            1
        );
        assert!(record_usage(
            &state,
            "workflow-test",
            "codex",
            UsageDelta::default()
        )
        .is_err());
    }

    #[tokio::test]
    async fn policy_assignment_mismatch_is_denied_without_storage() {
        let state = state_with_plan(WorkflowMode::Single).await;
        let mut request = request(100);
        request.policy_request.providers[0].agent_key = "claude".into();
        assert!(create_admission(&state, "workflow-test", request).is_err());
        assert!(load_admission(&state, "workflow-test").unwrap().is_none());
    }
}
