fn validate_request(request: &PolicyRequest) -> Result<(), String> {
    if request.providers.is_empty() {
        return Err("providers must contain at least one candidate".into());
    }
    if request.providers.len() > MAX_PROVIDER_CANDIDATES {
        return Err(format!(
            "providers exceeds the maximum of {MAX_PROVIDER_CANDIDATES}"
        ));
    }
    if request.required_agent_keys.len() > MAX_REQUIRED_PROVIDER_KEYS {
        return Err(format!(
            "required_agent_keys exceeds the maximum of {MAX_REQUIRED_PROVIDER_KEYS}"
        ));
    }
    if request.required_capabilities.len() > MAX_CAPABILITIES {
        return Err(format!(
            "required_capabilities exceeds the maximum of {MAX_CAPABILITIES}"
        ));
    }

    let required_keys = normalize_agent_keys(&request.required_agent_keys)?;
    let reviewer_key = request
        .required_reviewer_agent_key
        .as_deref()
        .map(normalize_agent_key)
        .transpose()?;

    let mut keys = BTreeSet::new();
    for provider in &request.providers {
        let agent_key = normalize_agent_key(&provider.agent_key)?;
        if !keys.insert(agent_key.clone()) {
            return Err(format!("duplicate provider agent_key '{agent_key}'"));
        }
        let model = provider.model.trim();
        if model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control)
        {
            return Err(format!(
                "provider model must be 1..={MAX_MODEL_BYTES} printable bytes"
            ));
        }
        if provider.historical_quality_bps > 10_000 {
            return Err(format!(
                "provider '{agent_key}' historical_quality_bps must be <= 10000"
            ));
        }
        if provider.health_score_bps > 10_000 {
            return Err(format!(
                "provider '{agent_key}' health_score_bps must be <= 10000"
            ));
        }
        if provider.recent_error_rate_bps > 10_000 {
            return Err(format!(
                "provider '{agent_key}' recent_error_rate_bps must be <= 10000"
            ));
        }
        normalize_capabilities(&provider.capabilities)?;
    }

    for required in &required_keys {
        if !keys.contains(required) {
            return Err(format!(
                "required provider '{required}' is absent from providers"
            ));
        }
    }
    if let Some(reviewer) = reviewer_key {
        if !keys.contains(&reviewer) {
            return Err(format!(
                "required reviewer '{reviewer}' is absent from providers"
            ));
        }
    }
    Ok(())
}

fn normalize_capabilities(values: &[String]) -> Result<BTreeSet<String>, String> {
    if values.len() > MAX_CAPABILITIES {
        return Err(format!(
            "capability list exceeds the maximum of {MAX_CAPABILITIES}"
        ));
    }
    let mut normalized = BTreeSet::new();
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty()
            || value.len() > MAX_CAPABILITY_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(format!("invalid capability '{value}'"));
        }
        normalized.insert(value);
    }
    Ok(normalized)
}

fn normalize_agent_keys(values: &[String]) -> Result<BTreeSet<String>, String> {
    let mut normalized = BTreeSet::new();
    for value in values {
        let value = normalize_agent_key(value)?;
        if !normalized.insert(value.clone()) {
            return Err(format!("duplicate required agent_key '{value}'"));
        }
    }
    Ok(normalized)
}

fn normalize_agent_key(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_AGENT_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "provider agent_key must be 1..={MAX_AGENT_KEY_BYTES} printable bytes"
        ));
    }
    Ok(value.to_string())
}

fn effective_risk(request: &PolicyRequest, capabilities: &BTreeSet<String>) -> TaskRisk {
    let security_sensitive = capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "security" | "secrets" | "authentication" | "authorization" | "cryptography"
        )
    });
    let mut risk = request.task_risk;
    if security_sensitive {
        risk = risk.max(TaskRisk::High);
    }
    if request.requires_repository_write {
        risk = risk.max(TaskRisk::Medium);
    }
    if request.data_sensitivity == DataSensitivity::Restricted {
        risk = risk.max(TaskRisk::Critical);
    }
    risk
}

fn minimum_mode(
    risk: TaskRisk,
    sensitivity: DataSensitivity,
    requires_repository_write: bool,
) -> WorkflowMode {
    if sensitivity == DataSensitivity::Restricted || risk >= TaskRisk::High {
        WorkflowMode::Consensus
    } else if risk == TaskRisk::Medium && requires_repository_write {
        WorkflowMode::Sequential
    } else {
        WorkflowMode::Single
    }
}

fn protocol_required_mode(protocol: CoordinationProtocol) -> WorkflowMode {
    match protocol {
        CoordinationProtocol::Direct => WorkflowMode::Single,
        CoordinationProtocol::SequentialHandoff => WorkflowMode::Sequential,
        CoordinationProtocol::IndependentCandidates => WorkflowMode::Competitive,
        CoordinationProtocol::BlindCandidatesWithReviewerReveal
        | CoordinationProtocol::ReviewerConsensus
        | CoordinationProtocol::AdversarialReviewRequired => WorkflowMode::Consensus,
    }
}

fn default_protocol(mode: WorkflowMode) -> CoordinationProtocol {
    match mode {
        WorkflowMode::Single => CoordinationProtocol::Direct,
        WorkflowMode::Sequential => CoordinationProtocol::SequentialHandoff,
        WorkflowMode::Competitive => CoordinationProtocol::IndependentCandidates,
        WorkflowMode::Consensus => CoordinationProtocol::ReviewerConsensus,
    }
}

fn selected_protocol(
    mode: WorkflowMode,
    requested: CoordinationProtocol,
) -> CoordinationProtocol {
    let requested_mode = protocol_required_mode(requested);
    if mode_rank(requested_mode) < mode_rank(mode) {
        default_protocol(mode)
    } else {
        requested
    }
}

fn execution_target(protocol: CoordinationProtocol) -> ExecutionTarget {
    match protocol {
        CoordinationProtocol::BlindCandidatesWithReviewerReveal => {
            ExecutionTarget::BlindCompetition
        }
        CoordinationProtocol::AdversarialReviewRequired => ExecutionTarget::AdversarialReview,
        CoordinationProtocol::Direct
        | CoordinationProtocol::SequentialHandoff
        | CoordinationProtocol::IndependentCandidates
        | CoordinationProtocol::ReviewerConsensus => ExecutionTarget::StandardWorkflow,
    }
}

fn mode_rank(mode: WorkflowMode) -> u8 {
    match mode {
        WorkflowMode::Single => 0,
        WorkflowMode::Sequential => 1,
        WorkflowMode::Competitive => 2,
        WorkflowMode::Consensus => 3,
    }
}

fn safer_mode(requested: WorkflowMode, required: WorkflowMode) -> WorkflowMode {
    if mode_rank(requested) >= mode_rank(required) {
        requested
    } else {
        required
    }
}

fn minimum_provider_count(mode: WorkflowMode) -> usize {
    match mode {
        WorkflowMode::Single => 1,
        WorkflowMode::Sequential => 2,
        WorkflowMode::Competitive => 2,
        WorkflowMode::Consensus => 3,
    }
}

fn desired_provider_count(mode: WorkflowMode, available: usize) -> usize {
    match mode {
        WorkflowMode::Single => available.min(1),
        WorkflowMode::Sequential => available.min(3),
        WorkflowMode::Competitive => available.min(3),
        WorkflowMode::Consensus => available.min(4),
    }
}

fn highest_feasible_mode(available: usize) -> Option<WorkflowMode> {
    if available >= minimum_provider_count(WorkflowMode::Consensus) {
        Some(WorkflowMode::Consensus)
    } else if available >= minimum_provider_count(WorkflowMode::Competitive) {
        Some(WorkflowMode::Competitive)
    } else if available >= minimum_provider_count(WorkflowMode::Single) {
        Some(WorkflowMode::Single)
    } else {
        None
    }
}

fn total_estimated_cost(providers: &[&ProviderCandidate]) -> u64 {
    providers.iter().fold(0u64, |total, provider| {
        total.saturating_add(provider.estimated_cost_micro_usd)
    })
}

fn minimum_context_tokens(providers: &[&ProviderCandidate]) -> u64 {
    providers
        .iter()
        .filter_map(|provider| (provider.max_context_tokens > 0).then_some(provider.max_context_tokens))
        .min()
        .unwrap_or(0)
}

fn default_degradation_behavior(
    risk: TaskRisk,
    sensitivity: DataSensitivity,
    requires_repository_write: bool,
) -> DegradationBehavior {
    if sensitivity == DataSensitivity::Restricted || risk == TaskRisk::Critical {
        DegradationBehavior::FailClosed
    } else if risk >= TaskRisk::High || requires_repository_write {
        DegradationBehavior::QueueUntilRequiredProvidersAreAvailable
    } else if risk == TaskRisk::Medium {
        DegradationBehavior::FallbackToSingleWithHumanApproval
    } else {
        DegradationBehavior::ReduceProviderCount
    }
}

fn degradation_rank(value: DegradationBehavior) -> u8 {
    match value {
        DegradationBehavior::ReduceProviderCount => 0,
        DegradationBehavior::FallbackToSingleWithHumanApproval => 1,
        DegradationBehavior::QueueUntilRequiredProvidersAreAvailable => 2,
        DegradationBehavior::FailClosed => 3,
    }
}

fn effective_degradation_behavior(
    default: DegradationBehavior,
    requested: Option<DegradationBehavior>,
    has_required_provider: bool,
    specialized_executor: bool,
) -> DegradationBehavior {
    let mut selected = requested
        .filter(|requested| degradation_rank(*requested) > degradation_rank(default))
        .unwrap_or(default);
    if (has_required_provider || specialized_executor)
        && degradation_rank(selected)
            < degradation_rank(DegradationBehavior::QueueUntilRequiredProvidersAreAvailable)
    {
        selected = DegradationBehavior::QueueUntilRequiredProvidersAreAvailable;
    }
    selected
}

fn exclusion_reason(
    provider: &ProviderCandidate,
    required_capabilities: &BTreeSet<String>,
    sensitivity: DataSensitivity,
) -> Result<Option<ProviderExclusionReason>, String> {
    if !provider.available {
        return Ok(Some(ProviderExclusionReason::Unavailable));
    }
    match provider.availability {
        ProviderAvailability::Disabled => {
            return Ok(Some(ProviderExclusionReason::Disabled));
        }
        ProviderAvailability::Outage => {
            return Ok(Some(ProviderExclusionReason::Outage));
        }
        ProviderAvailability::Available | ProviderAvailability::Degraded => {}
    }
    if sensitivity == DataSensitivity::Restricted && !provider.trusted_for_restricted {
        return Ok(Some(ProviderExclusionReason::RestrictedTrustRequired));
    }
    let capabilities = normalize_capabilities(&provider.capabilities)?;
    if !required_capabilities
        .iter()
        .all(|required| capabilities.contains(required))
    {
        return Ok(Some(ProviderExclusionReason::MissingCapability));
    }
    Ok(None)
}
