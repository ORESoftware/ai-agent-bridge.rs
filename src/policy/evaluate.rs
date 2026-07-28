pub fn evaluate(request: &PolicyRequest) -> Result<PolicyDecision, String> {
    validate_request(request)?;

    let required_capabilities = normalize_capabilities(&request.required_capabilities)?;
    let effective_risk = effective_risk(request, &required_capabilities);
    let profile = budget_profile(effective_risk);
    let budget = clamp_budget(&request.requested_budget, profile);

    let mut reasons = Vec::new();
    if effective_risk > request.task_risk {
        reasons.push(
            "risk elevated by repository-write or security-sensitive capability policy".into(),
        );
    }

    let mut eligible = request
        .providers
        .iter()
        .filter(|provider| provider.available)
        .filter(|provider| {
            request.data_sensitivity != DataSensitivity::Restricted
                || provider.trusted_for_restricted
        })
        .filter_map(|provider| {
            let capabilities = normalize_capabilities(&provider.capabilities).ok()?;
            required_capabilities
                .iter()
                .all(|required| capabilities.contains(required))
                .then_some(provider)
        })
        .collect::<Vec<_>>();

    eligible.sort_by(|left, right| {
        Reverse(left.health_score_bps.min(10_000))
            .cmp(&Reverse(right.health_score_bps.min(10_000)))
            .then(left.p95_latency_ms.cmp(&right.p95_latency_ms))
            .then(
                left.estimated_cost_micro_usd
                    .cmp(&right.estimated_cost_micro_usd),
            )
            .then(left.agent_key.cmp(&right.agent_key))
    });

    let required_mode = minimum_mode(
        effective_risk,
        request.data_sensitivity,
        request.requires_repository_write,
    );
    let requested_mode = request.requested_mode.unwrap_or(required_mode);
    let mut mode = safer_mode(requested_mode, required_mode);
    if mode != requested_mode {
        reasons.push(format!(
            "requested mode {:?} was raised to {:?} by policy",
            requested_mode, mode
        ));
    }

    let degradation_behavior = degradation_behavior(
        effective_risk,
        request.data_sensitivity,
        request.requires_repository_write,
    );
    let mut require_human_approval = effective_risk >= TaskRisk::High
        || request.data_sensitivity == DataSensitivity::Restricted
        || request.requires_repository_write && effective_risk >= TaskRisk::Medium;

    let max_selectable = usize::from(budget.max_providers).min(eligible.len());
    let desired = desired_provider_count(mode, max_selectable);
    let minimum = minimum_provider_count(mode);

    if desired < minimum {
        let restricted_or_critical = request.data_sensitivity == DataSensitivity::Restricted
            || effective_risk == TaskRisk::Critical;
        if restricted_or_critical {
            return Ok(denied_decision(
                mode,
                budget,
                require_human_approval,
                request.requires_repository_write,
                degradation_behavior,
                reasons,
                format!(
                    "mode {:?} requires at least {minimum} eligible providers; \
                     {desired} are available within budget",
                    mode,
                ),
            ));
        }

        mode = if desired >= 2 {
            WorkflowMode::Competitive
        } else {
            WorkflowMode::Single
        };
        require_human_approval = true;
        reasons.push(format!(
            "provider availability/budget reduced execution to {:?}; human approval is required",
            mode
        ));
    }

    let selected_count = desired_provider_count(mode, max_selectable);
    let mut selected = eligible
        .into_iter()
        .take(selected_count)
        .collect::<Vec<_>>();

    while total_estimated_cost(&selected) > budget.max_cost_micro_usd
        && selected.len() > minimum_provider_count(mode)
    {
        selected.pop();
        reasons.push("dropped one provider to remain within the hard cost ceiling".into());
    }

    if selected.len() < minimum_provider_count(mode)
        || total_estimated_cost(&selected) > budget.max_cost_micro_usd
    {
        return Ok(denied_decision(
            mode,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            reasons,
            "eligible provider estimates exceed the hard cost ceiling".into(),
        ));
    }

    if request.expected_duration_ms > 0 && request.expected_duration_ms > budget.max_wall_clock_ms {
        let denial_reason = format!(
            "expected duration {} ms exceeds the hard wall-clock ceiling {} ms",
            request.expected_duration_ms, budget.max_wall_clock_ms
        );
        return Ok(denied_decision(
            mode,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            reasons,
            denial_reason,
        ));
    }

    let selected_providers = selected
        .iter()
        .enumerate()
        .map(|(ordinal, provider)| SelectedProvider {
            ordinal,
            agent_key: provider.agent_key.trim().to_string(),
            kind: provider.kind,
            model: provider.model.trim().to_string(),
            role: if mode == WorkflowMode::Consensus && ordinal + 1 == selected.len() {
                ProviderRole::Reviewer
            } else {
                ProviderRole::Worker
            },
            estimated_cost_micro_usd: provider.estimated_cost_micro_usd,
        })
        .collect::<Vec<_>>();

    reasons.push(format!(
        "selected {} provider(s) using policy {}",
        selected_providers.len(),
        POLICY_VERSION
    ));

    Ok(PolicyDecision {
        policy_version: POLICY_VERSION,
        allowed: true,
        mode,
        selected_providers,
        budget,
        require_human_approval,
        require_fiducia_lease: request.requires_repository_write,
        degradation_behavior,
        reasons,
        denial_reason: None,
    })
}
