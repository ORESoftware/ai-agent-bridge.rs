pub fn evaluate(request: &PolicyRequest) -> Result<PolicyDecision, String> {
    validate_request(request)?;

    let required_capabilities = normalize_capabilities(&request.required_capabilities)?;
    let required_agent_keys = normalize_agent_keys(&request.required_agent_keys)?;
    let required_reviewer_agent_key = request
        .required_reviewer_agent_key
        .as_deref()
        .map(normalize_agent_key)
        .transpose()?;
    let mut required_selection = required_agent_keys.clone();
    if let Some(reviewer) = &required_reviewer_agent_key {
        required_selection.insert(reviewer.clone());
    }

    let effective_risk = effective_risk(request, &required_capabilities);
    let mut reasons = Vec::new();
    if effective_risk > request.task_risk {
        reasons.push(
            "risk elevated by repository-write or security-sensitive capability policy".into(),
        );
    }

    let base_required_mode = minimum_mode(
        effective_risk,
        request.data_sensitivity,
        request.requires_repository_write,
    );
    let default_requested_mode = request.requested_mode.unwrap_or(base_required_mode);
    let requested_protocol = request
        .requested_protocol
        .unwrap_or_else(|| default_protocol(default_requested_mode));
    let protocol_mode = protocol_required_mode(requested_protocol);
    let reviewer_mode = required_reviewer_agent_key
        .as_ref()
        .map(|_| WorkflowMode::Consensus)
        .unwrap_or(WorkflowMode::Single);
    let required_mode = safer_mode(safer_mode(base_required_mode, protocol_mode), reviewer_mode);
    let budget_risk = match required_mode {
        WorkflowMode::Single => effective_risk,
        WorkflowMode::Sequential => effective_risk.max(TaskRisk::Medium),
        WorkflowMode::Competitive | WorkflowMode::Consensus => {
            effective_risk.max(TaskRisk::High)
        }
    };
    let profile = budget_profile(budget_risk);
    let budget = clamp_budget(&request.requested_budget, profile);
    if budget_risk > effective_risk {
        reasons.push(format!(
            "budget profile elevated from {:?} to {:?} because execution mode {:?} requires multiple bounded providers",
            effective_risk, budget_risk, required_mode
        ));
    }
    let mut mode = safer_mode(default_requested_mode, required_mode);
    if mode != default_requested_mode {
        reasons.push(format!(
            "requested mode {:?} was raised to {:?} by policy",
            default_requested_mode, mode
        ));
    }

    let mut protocol = selected_protocol(mode, requested_protocol);
    if protocol != requested_protocol {
        reasons.push(format!(
            "requested protocol {:?} was raised to {:?} to match mode {:?}",
            requested_protocol, protocol, mode
        ));
    }
    let mut target = execution_target(protocol);

    let default_degradation = default_degradation_behavior(
        effective_risk,
        request.data_sensitivity,
        request.requires_repository_write,
    );
    let degradation_behavior = effective_degradation_behavior(
        default_degradation,
        request.requested_degradation,
        !required_selection.is_empty(),
        target != ExecutionTarget::StandardWorkflow,
    );
    if degradation_behavior != default_degradation {
        reasons.push(format!(
            "degradation policy tightened from {:?} to {:?}",
            default_degradation, degradation_behavior
        ));
    }

    let mut excluded_providers = Vec::new();
    let mut eligible = Vec::new();
    for provider in &request.providers {
        match exclusion_reason(
            provider,
            &required_capabilities,
            request.data_sensitivity,
        )? {
            Some(reason) => excluded_providers.push(ProviderExclusion {
                agent_key: provider.agent_key.trim().to_string(),
                reason,
            }),
            None => eligible.push(provider),
        }
    }

    eligible.sort_by(|left, right| {
        left.availability
            .cmp(&right.availability)
            .then(
                Reverse(left.historical_quality_bps.min(10_000))
                    .cmp(&Reverse(right.historical_quality_bps.min(10_000))),
            )
            .then(
                Reverse(left.health_score_bps.min(10_000))
                    .cmp(&Reverse(right.health_score_bps.min(10_000))),
            )
            .then(
                left.recent_error_rate_bps
                    .min(10_000)
                    .cmp(&right.recent_error_rate_bps.min(10_000)),
            )
            .then(left.p95_latency_ms.cmp(&right.p95_latency_ms))
            .then(
                left.estimated_cost_micro_usd
                    .cmp(&right.estimated_cost_micro_usd),
            )
            .then(left.agent_key.cmp(&right.agent_key))
    });

    let eligible_keys = eligible
        .iter()
        .map(|provider| provider.agent_key.trim().to_string())
        .collect::<BTreeSet<_>>();
    let missing_required_agent_keys = required_selection
        .difference(&eligible_keys)
        .cloned()
        .collect::<Vec<_>>();

    let mut require_human_approval = effective_risk >= TaskRisk::High
        || request.data_sensitivity == DataSensitivity::Restricted
        || request.requires_repository_write && effective_risk >= TaskRisk::Medium;
    let mut degradation = None;

    if !missing_required_agent_keys.is_empty() {
        let trigger = format!(
            "required provider(s) unavailable or ineligible: {}",
            missing_required_agent_keys.join(", ")
        );
        reasons.push(trigger.clone());
        let disposition = if degradation_behavior == DegradationBehavior::FailClosed {
            PolicyDisposition::Deny
        } else {
            PolicyDisposition::Queue
        };
        return Ok(non_execute_decision(
            disposition,
            mode,
            protocol,
            target,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            Some(DegradationDecision {
                behavior: degradation_behavior,
                trigger: trigger.clone(),
                from_mode: mode,
                to_mode: None,
                from_protocol: protocol,
                to_protocol: None,
                human_approval_required: require_human_approval,
            }),
            reasons,
            trigger,
            selection_summary(
                default_requested_mode,
                required_mode,
                requested_protocol,
                protocol,
                mode,
                eligible.len(),
                excluded_providers.len(),
                &required_selection,
                missing_required_agent_keys,
                required_reviewer_agent_key,
                budget.max_providers,
            ),
            excluded_providers,
        ));
    }

    let max_selectable = usize::from(budget.max_providers).min(eligible.len());
    let mut minimum = minimum_provider_count(mode).max(required_selection.len());
    let mut desired = desired_provider_count(mode, max_selectable)
        .max(required_selection.len())
        .min(max_selectable);

    if desired < minimum {
        let trigger = format!(
            "mode {:?} requires at least {minimum} eligible providers; {desired} are available within budget",
            mode
        );
        let from_mode = mode;
        let from_protocol = protocol;
        match degradation_behavior {
            DegradationBehavior::FailClosed => {
                return Ok(non_execute_decision(
                    PolicyDisposition::Deny,
                    mode,
                    protocol,
                    target,
                    budget,
                    require_human_approval,
                    request.requires_repository_write,
                    degradation_behavior,
                    Some(DegradationDecision {
                        behavior: degradation_behavior,
                        trigger: trigger.clone(),
                        from_mode,
                        to_mode: None,
                        from_protocol,
                        to_protocol: None,
                        human_approval_required: require_human_approval,
                    }),
                    reasons,
                    trigger,
                    selection_summary(
                        default_requested_mode,
                        required_mode,
                        requested_protocol,
                        protocol,
                        mode,
                        eligible.len(),
                        excluded_providers.len(),
                        &required_selection,
                        Vec::new(),
                        required_reviewer_agent_key,
                        budget.max_providers,
                    ),
                    excluded_providers,
                ));
            }
            DegradationBehavior::QueueUntilRequiredProvidersAreAvailable => {
                return Ok(non_execute_decision(
                    PolicyDisposition::Queue,
                    mode,
                    protocol,
                    target,
                    budget,
                    require_human_approval,
                    request.requires_repository_write,
                    degradation_behavior,
                    Some(DegradationDecision {
                        behavior: degradation_behavior,
                        trigger: trigger.clone(),
                        from_mode,
                        to_mode: None,
                        from_protocol,
                        to_protocol: None,
                        human_approval_required: require_human_approval,
                    }),
                    reasons,
                    trigger,
                    selection_summary(
                        default_requested_mode,
                        required_mode,
                        requested_protocol,
                        protocol,
                        mode,
                        eligible.len(),
                        excluded_providers.len(),
                        &required_selection,
                        Vec::new(),
                        required_reviewer_agent_key,
                        budget.max_providers,
                    ),
                    excluded_providers,
                ));
            }
            DegradationBehavior::FallbackToSingleWithHumanApproval => {
                if max_selectable == 0 {
                    return Ok(non_execute_decision(
                        PolicyDisposition::Deny,
                        mode,
                        protocol,
                        target,
                        budget,
                        true,
                        request.requires_repository_write,
                        degradation_behavior,
                        None,
                        reasons,
                        "no eligible provider is available for the approved single-provider fallback"
                            .into(),
                        selection_summary(
                            default_requested_mode,
                            required_mode,
                            requested_protocol,
                            protocol,
                            mode,
                            eligible.len(),
                            excluded_providers.len(),
                            &required_selection,
                            Vec::new(),
                            required_reviewer_agent_key,
                            budget.max_providers,
                        ),
                        excluded_providers,
                    ));
                }
                mode = WorkflowMode::Single;
                protocol = CoordinationProtocol::Direct;
                target = ExecutionTarget::StandardWorkflow;
                require_human_approval = true;
                reasons.push(
                    "provider shortage selected one provider only; parallel candidates and consensus were refused and explicit human approval is required"
                        .into(),
                );
                degradation = Some(DegradationDecision {
                    behavior: degradation_behavior,
                    trigger,
                    from_mode,
                    to_mode: Some(mode),
                    from_protocol,
                    to_protocol: Some(protocol),
                    human_approval_required: true,
                });
            }
            DegradationBehavior::ReduceProviderCount => {
                let Some(feasible) = highest_feasible_mode(max_selectable) else {
                    return Ok(non_execute_decision(
                        PolicyDisposition::Deny,
                        mode,
                        protocol,
                        target,
                        budget,
                        require_human_approval,
                        request.requires_repository_write,
                        degradation_behavior,
                        None,
                        reasons,
                        "no eligible provider is available".into(),
                        selection_summary(
                            default_requested_mode,
                            required_mode,
                            requested_protocol,
                            protocol,
                            mode,
                            eligible.len(),
                            excluded_providers.len(),
                            &required_selection,
                            Vec::new(),
                            required_reviewer_agent_key,
                            budget.max_providers,
                        ),
                        excluded_providers,
                    ));
                };
                mode = if mode_rank(feasible) > mode_rank(mode) {
                    mode
                } else {
                    feasible
                };
                protocol = default_protocol(mode);
                target = execution_target(protocol);
                reasons.push(format!(
                    "provider shortage reduced execution from {:?} to {:?}; the policy did not silently preserve the stronger mode",
                    from_mode, mode
                ));
                degradation = Some(DegradationDecision {
                    behavior: degradation_behavior,
                    trigger,
                    from_mode,
                    to_mode: Some(mode),
                    from_protocol,
                    to_protocol: Some(protocol),
                    human_approval_required: require_human_approval,
                });
            }
        }
        minimum = minimum_provider_count(mode).max(required_selection.len());
        desired = desired_provider_count(mode, max_selectable)
            .max(required_selection.len())
            .min(max_selectable);
    }

    if desired < minimum {
        return Ok(non_execute_decision(
            PolicyDisposition::Deny,
            mode,
            protocol,
            target,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            degradation,
            reasons,
            "degradation did not produce a feasible provider set".into(),
            selection_summary(
                default_requested_mode,
                required_mode,
                requested_protocol,
                protocol,
                mode,
                eligible.len(),
                excluded_providers.len(),
                &required_selection,
                Vec::new(),
                required_reviewer_agent_key,
                budget.max_providers,
            ),
            excluded_providers,
        ));
    }

    let mut selected = Vec::new();
    for key in &required_selection {
        if let Some(provider) = eligible
            .iter()
            .copied()
            .find(|provider| provider.agent_key.trim() == key)
        {
            selected.push(provider);
        }
    }
    for provider in &eligible {
        if selected.len() >= desired {
            break;
        }
        if !required_selection.contains(provider.agent_key.trim()) {
            selected.push(*provider);
        }
    }

    if let Some(reviewer) = &required_reviewer_agent_key {
        if let Some(index) = selected
            .iter()
            .position(|provider| provider.agent_key.trim() == reviewer)
        {
            let reviewer = selected.remove(index);
            selected.push(reviewer);
        }
    }

    let mut cost_reduced = false;
    while total_estimated_cost(&selected) > budget.max_cost_micro_usd
        && selected.len() > minimum
        && degradation_behavior == DegradationBehavior::ReduceProviderCount
    {
        let removable = selected.iter().rposition(|provider| {
            !required_selection.contains(provider.agent_key.trim())
                && required_reviewer_agent_key
                    .as_deref()
                    .is_none_or(|reviewer| provider.agent_key.trim() != reviewer)
        });
        let Some(index) = removable else {
            break;
        };
        selected.remove(index);
        cost_reduced = true;
    }
    if cost_reduced {
        reasons.push("dropped optional provider(s) to remain within the hard cost ceiling".into());
    }

    if selected.len() < minimum || total_estimated_cost(&selected) > budget.max_cost_micro_usd {
        return Ok(non_execute_decision(
            PolicyDisposition::Deny,
            mode,
            protocol,
            target,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            degradation,
            reasons,
            "eligible provider estimates exceed the hard cost ceiling without a permitted safe reduction"
                .into(),
            selection_summary(
                default_requested_mode,
                required_mode,
                requested_protocol,
                protocol,
                mode,
                eligible.len(),
                excluded_providers.len(),
                &required_selection,
                Vec::new(),
                required_reviewer_agent_key,
                budget.max_providers,
            ),
            excluded_providers,
        ));
    }

    if request.expected_duration_ms > 0 && request.expected_duration_ms > budget.max_wall_clock_ms {
        let denial_reason = format!(
            "expected duration {} ms exceeds the hard wall-clock ceiling {} ms",
            request.expected_duration_ms, budget.max_wall_clock_ms
        );
        return Ok(non_execute_decision(
            PolicyDisposition::Deny,
            mode,
            protocol,
            target,
            budget,
            require_human_approval,
            request.requires_repository_write,
            degradation_behavior,
            degradation,
            reasons,
            denial_reason,
            selection_summary(
                default_requested_mode,
                required_mode,
                requested_protocol,
                protocol,
                mode,
                eligible.len(),
                excluded_providers.len(),
                &required_selection,
                Vec::new(),
                required_reviewer_agent_key,
                budget.max_providers,
            ),
            excluded_providers,
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

    match protocol {
        CoordinationProtocol::Direct => reasons.push(
            "one provider selected because the effective risk does not justify parallel candidates or reviewer consensus"
                .into(),
        ),
        CoordinationProtocol::SequentialHandoff => reasons.push(
            "providers execute one at a time; each handoff receives only bounded accepted prior context"
                .into(),
        ),
        CoordinationProtocol::IndependentCandidates => reasons.push(
            "independent candidates execute without prior peer submissions; no reviewer synthesis is required"
                .into(),
        ),
        CoordinationProtocol::BlindCandidatesWithReviewerReveal => reasons.push(
            "candidate outputs require the blind-competition executor and remain hidden until reviewer reveal"
                .into(),
        ),
        CoordinationProtocol::ReviewerConsensus => reasons.push(
            "independent workers are followed by one reviewer synthesis phase"
                .into(),
        ),
        CoordinationProtocol::AdversarialReviewRequired => reasons.push(
            "execution requires the separate adversarial branch-review protocol; standard workflow admission is not sufficient"
                .into(),
        ),
    }
    reasons.push(
        "providers ranked deterministically by availability, historical quality, health, recent error rate, latency, estimated cost, and agent key"
            .into(),
    );
    reasons.push(format!(
        "selected {} provider(s) using policy {}",
        selected_providers.len(),
        POLICY_VERSION
    ));

    let estimated_wall_clock_ms = if request.expected_duration_ms > 0 {
        request.expected_duration_ms
    } else if mode == WorkflowMode::Sequential {
        selected.iter().fold(0u64, |total, provider| {
            total.saturating_add(provider.p95_latency_ms)
        })
    } else {
        selected
            .iter()
            .map(|provider| provider.p95_latency_ms)
            .max()
            .unwrap_or(0)
    };
    let estimates = PolicyEstimates {
        selected_provider_count: selected.len(),
        estimated_provider_calls: u64::try_from(selected.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(budget.max_rounds)),
        total_estimated_cost_micro_usd: total_estimated_cost(&selected),
        estimated_wall_clock_ms,
        minimum_context_tokens: minimum_context_tokens(&selected),
    };
    let disposition = if require_human_approval {
        PolicyDisposition::RequireHumanApproval
    } else {
        PolicyDisposition::Execute
    };
    let selection = selection_summary(
        default_requested_mode,
        required_mode,
        requested_protocol,
        protocol,
        mode,
        eligible.len(),
        excluded_providers.len(),
        &required_selection,
        Vec::new(),
        required_reviewer_agent_key,
        budget.max_providers,
    );

    Ok(PolicyDecision {
        policy_version: POLICY_VERSION,
        allowed: true,
        disposition,
        mode,
        coordination_protocol: protocol,
        execution_target: target,
        selected_providers,
        excluded_providers,
        selection,
        estimates,
        budget,
        require_human_approval,
        require_fiducia_lease: request.requires_repository_write,
        degradation_behavior,
        degradation,
        reasons,
        denial_reason: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn selection_summary(
    requested_mode: WorkflowMode,
    required_mode: WorkflowMode,
    requested_protocol: CoordinationProtocol,
    selected_protocol: CoordinationProtocol,
    selected_mode: WorkflowMode,
    eligible_provider_count: usize,
    excluded_provider_count: usize,
    required_agent_keys: &BTreeSet<String>,
    missing_required_agent_keys: Vec<String>,
    reviewer_agent_key: Option<String>,
    max_providers: u8,
) -> SelectionSummary {
    let max_selectable = usize::from(max_providers).min(eligible_provider_count);
    SelectionSummary {
        requested_mode,
        required_mode,
        requested_protocol,
        selected_protocol,
        desired_provider_count: desired_provider_count(selected_mode, max_selectable)
            .max(required_agent_keys.len())
            .min(max_selectable),
        minimum_provider_count: minimum_provider_count(selected_mode)
            .max(required_agent_keys.len()),
        eligible_provider_count,
        excluded_provider_count,
        required_agent_keys: required_agent_keys.iter().cloned().collect(),
        missing_required_agent_keys,
        reviewer_agent_key,
    }
}

#[allow(clippy::too_many_arguments)]
fn non_execute_decision(
    disposition: PolicyDisposition,
    mode: WorkflowMode,
    protocol: CoordinationProtocol,
    target: ExecutionTarget,
    budget: BudgetLimits,
    require_human_approval: bool,
    require_fiducia_lease: bool,
    degradation_behavior: DegradationBehavior,
    degradation: Option<DegradationDecision>,
    reasons: Vec<String>,
    denial_reason: String,
    selection: SelectionSummary,
    excluded_providers: Vec<ProviderExclusion>,
) -> PolicyDecision {
    PolicyDecision {
        policy_version: POLICY_VERSION,
        allowed: false,
        disposition,
        mode,
        coordination_protocol: protocol,
        execution_target: target,
        selected_providers: Vec::new(),
        excluded_providers,
        selection,
        estimates: PolicyEstimates {
            selected_provider_count: 0,
            estimated_provider_calls: 0,
            total_estimated_cost_micro_usd: 0,
            estimated_wall_clock_ms: 0,
            minimum_context_tokens: 0,
        },
        budget,
        require_human_approval,
        require_fiducia_lease,
        degradation_behavior,
        degradation,
        reasons,
        denial_reason: Some(denial_reason),
    }
}
