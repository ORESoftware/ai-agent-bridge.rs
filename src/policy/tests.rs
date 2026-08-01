mod tests {
    use super::*;

    fn provider(agent_key: &str, trusted: bool, cost: u64) -> ProviderCandidate {
        ProviderCandidate {
            agent_key: agent_key.into(),
            kind: AgentKind::Codex,
            model: "test-model".into(),
            available: true,
            availability: ProviderAvailability::Available,
            capabilities: vec!["rust".into(), "security".into()],
            trusted_for_restricted: trusted,
            historical_quality_bps: 9_000,
            health_score_bps: 9_500,
            recent_error_rate_bps: 100,
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
            requested_protocol: None,
            requested_degradation: None,
            required_agent_keys: Vec::new(),
            required_reviewer_agent_key: None,
            required_capabilities: vec!["rust".into()],
            requires_repository_write: false,
            expected_duration_ms: 0,
            requested_budget: RequestedBudget::default(),
            providers,
        }
    }

    #[test]
    fn low_risk_defaults_to_one_provider_and_explains_no_parallelism() {
        let decision = evaluate(&request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
        ]))
        .unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.policy_version, "2026-07-31.v2");
        assert_eq!(decision.disposition, PolicyDisposition::Execute);
        assert_eq!(decision.mode, WorkflowMode::Single);
        assert_eq!(decision.coordination_protocol, CoordinationProtocol::Direct);
        assert_eq!(decision.execution_target, ExecutionTarget::StandardWorkflow);
        assert_eq!(decision.selected_providers.len(), 1);
        assert!(!decision.require_human_approval);
        assert!(decision.reasons.iter().any(|reason| reason.contains("does not justify parallel")));
    }

    #[test]
    fn historical_quality_health_errors_latency_and_cost_rank_deterministically() {
        let mut lower_quality = provider("a-low-quality", false, 10);
        lower_quality.historical_quality_bps = 8_000;
        lower_quality.health_score_bps = 10_000;
        lower_quality.p95_latency_ms = 1;

        let mut degraded = provider("b-degraded", false, 1);
        degraded.availability = ProviderAvailability::Degraded;
        degraded.historical_quality_bps = 10_000;

        let mut selected = provider("c-selected", false, 1_000);
        selected.historical_quality_bps = 9_900;
        selected.health_score_bps = 9_800;
        selected.recent_error_rate_bps = 50;
        selected.p95_latency_ms = 500;

        let decision = evaluate(&request(vec![lower_quality, degraded, selected])).unwrap();
        assert_eq!(decision.selected_providers[0].agent_key, "c-selected");
    }

    #[test]
    fn repository_write_selects_one_at_a_time_sequential_handoff() {
        let mut request = request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
        ]);
        request.task_risk = TaskRisk::Medium;
        request.requires_repository_write = true;
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Sequential);
        assert_eq!(
            decision.coordination_protocol,
            CoordinationProtocol::SequentialHandoff
        );
        assert_eq!(decision.selected_providers.len(), 2);
        assert_eq!(decision.disposition, PolicyDisposition::RequireHumanApproval);
        assert!(decision.reasons.iter().any(|reason| reason.contains("one at a time")));
    }

    #[test]
    fn security_capability_raises_risk_and_assigns_required_reviewer() {
        let mut request = request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
            provider("gemini", false, 100_000),
        ]);
        request.required_capabilities = vec!["security".into()];
        request.required_reviewer_agent_key = Some("gemini".into());
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Consensus);
        assert_eq!(
            decision.coordination_protocol,
            CoordinationProtocol::ReviewerConsensus
        );
        assert!(decision.require_human_approval);
        assert_eq!(decision.selected_providers.len(), 3);
        assert_eq!(
            decision.selected_providers.last().unwrap().agent_key,
            "gemini"
        );
        assert_eq!(
            decision.selected_providers.last().unwrap().role,
            ProviderRole::Reviewer
        );
    }

    #[test]
    fn required_reviewer_outage_queues_without_implicit_fallback() {
        let mut gemini = provider("gemini", false, 100_000);
        gemini.availability = ProviderAvailability::Outage;
        let mut request = request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
            gemini,
        ]);
        request.task_risk = TaskRisk::High;
        request.required_reviewer_agent_key = Some("gemini".into());
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert_eq!(decision.disposition, PolicyDisposition::Queue);
        assert_eq!(
            decision.degradation_behavior,
            DegradationBehavior::QueueUntilRequiredProvidersAreAvailable
        );
        assert_eq!(
            decision.selection.missing_required_agent_keys,
            vec!["gemini"]
        );
        assert_eq!(decision.excluded_providers[0].reason, ProviderExclusionReason::Outage);
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
        assert_eq!(decision.disposition, PolicyDisposition::Deny);
        assert_eq!(decision.mode, WorkflowMode::Consensus);
        assert_eq!(
            decision.degradation_behavior,
            DegradationBehavior::FailClosed
        );
    }

    #[test]
    fn low_risk_provider_shortage_reduces_to_single_explicitly() {
        let mut request = request(vec![provider("codex", false, 100_000)]);
        request.requested_mode = Some(WorkflowMode::Competitive);
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Single);
        assert_eq!(decision.coordination_protocol, CoordinationProtocol::Direct);
        assert_eq!(
            decision.degradation.as_ref().unwrap().behavior,
            DegradationBehavior::ReduceProviderCount
        );
    }

    #[test]
    fn medium_risk_shortage_requires_approved_single_provider_fallback() {
        let mut request = request(vec![provider("codex", false, 100_000)]);
        request.task_risk = TaskRisk::Medium;
        request.requested_mode = Some(WorkflowMode::Competitive);
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Single);
        assert_eq!(decision.disposition, PolicyDisposition::RequireHumanApproval);
        assert_eq!(
            decision.degradation.as_ref().unwrap().behavior,
            DegradationBehavior::FallbackToSingleWithHumanApproval
        );
    }

    #[test]
    fn critical_policy_refuses_a_weaker_requested_degradation() {
        let mut request = request(vec![provider("codex", true, 100_000)]);
        request.task_risk = TaskRisk::Critical;
        request.requested_mode = Some(WorkflowMode::Consensus);
        request.requested_degradation = Some(DegradationBehavior::ReduceProviderCount);
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert_eq!(decision.disposition, PolicyDisposition::Deny);
        assert_eq!(
            decision.degradation_behavior,
            DegradationBehavior::FailClosed
        );
    }

    #[test]
    fn blind_competition_is_a_distinct_specialized_execution_target() {
        let mut request = request(vec![
            provider("codex", false, 100_000),
            provider("claude", false, 100_000),
            provider("gemini", false, 100_000),
        ]);
        request.requested_protocol =
            Some(CoordinationProtocol::BlindCandidatesWithReviewerReveal);
        request.required_reviewer_agent_key = Some("gemini".into());
        let decision = evaluate(&request).unwrap();
        assert!(decision.allowed);
        assert_eq!(decision.mode, WorkflowMode::Consensus);
        assert_eq!(
            decision.coordination_protocol,
            CoordinationProtocol::BlindCandidatesWithReviewerReveal
        );
        assert_eq!(decision.execution_target, ExecutionTarget::BlindCompetition);
    }

    #[test]
    fn requested_budget_is_a_hard_ceiling() {
        let mut request = request(vec![provider("codex", false, 500_000)]);
        request.requested_budget.max_cost_micro_usd = Some(100_000);
        let decision = evaluate(&request).unwrap();
        assert!(!decision.allowed);
        assert_eq!(decision.disposition, PolicyDisposition::Deny);
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
