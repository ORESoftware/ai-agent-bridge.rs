mod tests {
    use super::*;

    fn provider(agent_key: &str, trusted: bool, cost: u64) -> ProviderCandidate {
        ProviderCandidate {
            agent_key: agent_key.into(),
            kind: AgentKind::Codex,
            model: "test-model".into(),
            available: true,
            capabilities: vec!["rust".into(), "security".into()],
            trusted_for_restricted: trusted,
            health_score_bps: 9_500,
            p95_latency_ms: 1_000,
            estimated_cost_micro_usd: cost,
            max_context_tokens: 100_000,
        }
    }

    fn request(providers: Vec<ProviderCandidate>) -> PolicyRequest {
        PolicyRequest {
            task_risk: TaskRisk::Low,
            data_sensitivity: DataSensitivity::Internal,
            requested_mode: None,
            required_capabilities: vec!["rust".into()],
            requires_repository_write: false,
            expected_duration_ms: 0,
            requested_budget: RequestedBudget::default(),
            providers,
        }
    }

    #[test]
    fn low_risk_defaults_to_one_provider() {
        let decision = evaluate(&request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
        ]))
        .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Single);
        assert_eq!(decision.selected_providers.len(), 1);
        assert!(!decision.require_human_approval);
    }

    #[test]
    fn security_capability_raises_risk_and_requires_consensus() {
        let mut request = request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
            provider("gemini", false, 100_000),
        ]);
        request.required_capabilities = vec!["security".into()];
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Consensus);
        assert!(decision.require_human_approval);
        assert_eq!(decision.selected_providers.len(), 3);
        assert_eq!(
            decision.selected_providers.last().unwrap().role,
            ProviderRole::Reviewer
        );
    }

    #[test]
    fn restricted_data_fails_closed_without_three_trusted_providers() {
        let mut request = request(vec![
            provider("codex", true, 100_000),
            provider("claude", true, 100_000),
            provider("gemini", false, 100_000),
        ]);
        request.data_sensitivity = DataSensitivity::Restricted;
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Consensus);
        assert_eq!(
            decision.degradation_behavior,
            DegradationBehavior::FailClosed
        );
    }

    #[test]
    fn requested_budget_is_a_hard_ceiling() {
        let mut request = request(vec![provider("codex", false, 500_000)]);
        request.requested_budget.max_cost_micro_usd = Some(100_000);
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert_eq!(decision.budget.max_cost_micro_usd, 100_000);
    }

    #[test]
    fn expected_duration_above_ceiling_is_denied() {
        let mut request = request(vec![provider("codex", false, 100_000)]);
        request.expected_duration_ms = 700_000;
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert!(decision
            .denial_reason
            .as_deref()
            .unwrap()
            .contains("wall-clock"));
    }
}
