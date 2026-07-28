fn validate_request(request: &PolicyRequest) -> Result<(), String> {
    if request.providers.is_empty() {
        return Err("providers must contain at least one candidate".into());
    }
    if request.providers.len() > MAX_PROVIDER_CANDIDATES {
        return Err(format!(
            "providers exceeds the maximum of {MAX_PROVIDER_CANDIDATES}"
        ));
    }
    if request.required_capabilities.len() > MAX_CAPABILITIES {
        return Err(format!(
            "required_capabilities exceeds the maximum of {MAX_CAPABILITIES}"
        ));
    }

    let mut keys = BTreeSet::new();
    for provider in &request.providers {
        let agent_key = provider.agent_key.trim();
        if agent_key.is_empty() || agent_key.len() > MAX_AGENT_KEY_BYTES {
            return Err(format!(
                "provider agent_key must be 1..={MAX_AGENT_KEY_BYTES} bytes"
            ));
        }
        if !keys.insert(agent_key.to_string()) {
            return Err(format!("duplicate provider agent_key '{agent_key}'"));
        }
        let model = provider.model.trim();
        if model.is_empty() || model.len() > MAX_MODEL_BYTES {
            return Err(format!(
                "provider model must be 1..={MAX_MODEL_BYTES} bytes"
            ));
        }
        if provider.health_score_bps > 10_000 {
            return Err(format!(
                "provider '{agent_key}' health_score_bps must be <= 10000"
            ));
        }
        normalize_capabilities(&provider.capabilities)?;
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
        WorkflowMode::Sequential => 1,
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

fn total_estimated_cost(providers: &[&ProviderCandidate]) -> u64 {
    providers.iter().fold(0u64, |total, provider| {
        total.saturating_add(provider.estimated_cost_micro_usd)
    })
}

fn degradation_behavior(
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
