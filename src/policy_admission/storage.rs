#[derive(Debug)]
struct AdmissionFailure {
    status: StatusCode,
    code: &'static str,
    message: String,
    admission: Option<Box<AdmissionRecord>>,
}

impl AdmissionFailure {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            admission: None,
        }
    }

    fn with_admission(mut self, admission: AdmissionRecord) -> Self {
        self.admission = Some(Box::new(admission));
        self
    }

    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": self.code,
                "message": self.message,
                "admission": self.admission,
            })),
        )
            .into_response()
    }
}

type AdmissionResult<T> = Result<T, AdmissionFailure>;

fn admission_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load_plan(state: &AppState, workflow_id: &str) -> BridgeResult<WorkflowPlan> {
    let channel = format!("workflow-{workflow_id}");
    let entry = state
        .get_context_key_internal(&channel, WORKFLOW_PLAN_CONTEXT_KEY)?
        .ok_or_else(|| BridgeError::BadRequest("workflow plan is missing".into()))?;
    serde_json::from_value(entry.value)
        .map_err(|_| BridgeError::BadRequest("workflow plan is invalid".into()))
}

fn load_admission(state: &AppState, workflow_id: &str) -> BridgeResult<Option<AdmissionRecord>> {
    let channel = format!("workflow-{workflow_id}");
    state
        .get_context_key_internal(&channel, ADMISSION_CONTEXT_KEY)?
        .map(|entry| {
            serde_json::from_value(entry.value)
                .map_err(|_| BridgeError::BadRequest("workflow admission is invalid".into()))
        })
        .transpose()
}

fn persist_admission(state: &AppState, admission: &AdmissionRecord) -> BridgeResult<()> {
    state.set_context_internal(
        &format!("workflow-{}", admission.workflow_id),
        ADMISSION_CONTEXT_KEY,
        serde_json::to_value(admission)
            .map_err(|_| BridgeError::BadRequest("admission is not serializable".into()))?,
        &admission.requested_by,
    )?;
    Ok(())
}

fn create_admission(
    state: &AppState,
    workflow_id: &str,
    request: AdmitReq,
) -> AdmissionResult<(AdmissionRecord, bool)> {
    let _guard = admission_lock().lock();
    if let Some(existing) = load_admission(state, workflow_id).map_err(domain_failure)? {
        return Ok((existing, false));
    }
    let plan = load_plan(state, workflow_id).map_err(domain_failure)?;
    validate_request_matches_plan(&plan, &request.policy_request)?;

    let decision = policy::evaluate(&request.policy_request).map_err(|message| {
        AdmissionFailure::new(StatusCode::BAD_REQUEST, "invalid_policy_request", message)
    })?;
    if !decision.allowed {
        return Err(AdmissionFailure::new(
            StatusCode::CONFLICT,
            "policy_denied",
            decision
                .denial_reason
                .clone()
                .unwrap_or_else(|| "policy denied managed execution".into()),
        ));
    }
    if decision.execution_target != ExecutionTarget::StandardWorkflow {
    return Err(AdmissionFailure::new(
        StatusCode::CONFLICT,
        "specialized_executor_required",
        "policy requires the blind-competition or adversarial-review executor; standard workflow admission is not permitted",
    ));
}
if decision.mode != plan.mode {
        return Err(AdmissionFailure::new(
            StatusCode::CONFLICT,
            "policy_mode_escalated",
            "policy requires a different workflow mode; create a matching workflow plan",
        ));
    }

    let planned_agents = plan
        .assignments
        .iter()
        .map(|assignment| assignment.agent_key.clone())
        .collect::<BTreeSet<_>>();
    let selected_agents = decision
        .selected_providers
        .iter()
        .map(|provider| provider.agent_key.clone())
        .collect::<BTreeSet<_>>();
    if planned_agents != selected_agents {
        return Err(AdmissionFailure::new(
            StatusCode::CONFLICT,
            "policy_assignment_mismatch",
            "workflow assignments do not exactly match the policy-selected providers",
        ));
    }

    let requested_by = normalize_actor(&request.requested_by)?;
    let approved_by = request
        .approved_by
        .as_deref()
        .map(normalize_actor)
        .transpose()?;
    if decision.require_human_approval && approved_by.is_none() {
        return Err(AdmissionFailure::new(
            StatusCode::CONFLICT,
            "human_approval_required",
            "the policy decision requires an explicit approver",
        ));
    }
    let override_reason = request
        .override_reason
        .as_deref()
        .map(normalize_reason)
        .transpose()?;
    let now = now_ts();
    let admission = AdmissionRecord {
        version: 1,
        workflow_id: workflow_id.to_string(),
        requested_by,
        approved_by,
        override_reason,
        policy: AdmissionPolicySnapshot {
            policy_version: decision.policy_version.to_string(),
            mode: decision.mode,
            selected_agent_keys: selected_agents.into_iter().collect(),
            budget: decision.budget,
            require_human_approval: decision.require_human_approval,
            require_fiducia_lease: decision.require_fiducia_lease,
            reasons: decision.reasons,
        },
        status: AdmissionStatus::Active,
        usage: UsageTotals::default(),
        created_at: now.clone(),
        updated_at: now,
        terminal_reason: None,
        last_rejected_delta: None,
    };
    persist_admission(state, &admission).map_err(domain_failure)?;
    Ok((admission, true))
}

fn record_usage(
    state: &AppState,
    workflow_id: &str,
    updated_by: &str,
    delta: UsageDelta,
) -> AdmissionResult<AdmissionRecord> {
    let _guard = admission_lock().lock();
    let mut admission = load_admission(state, workflow_id)
        .map_err(domain_failure)?
        .ok_or_else(|| {
            AdmissionFailure::new(
                StatusCode::CONFLICT,
                "admission_required",
                "managed provider work requires an active admission",
            )
        })?;
    require_active(&admission)?;
    let updated_by = normalize_actor(updated_by)?;
    if !admission
        .policy
        .selected_agent_keys
        .iter()
        .any(|agent| agent == &updated_by)
    {
        return Err(AdmissionFailure::new(
            StatusCode::FORBIDDEN,
            "admission_actor_not_selected",
            "usage may be reported only by a policy-selected provider",
        ));
    }

    let attempted = attempted_totals(&admission.usage, &delta).ok_or_else(|| {
        exhaust(
            state,
            admission.clone(),
            delta.clone(),
            "usage counter overflow",
        )
    })?;
    if let Some(reason) = budget_violation(&admission.policy.budget, &attempted) {
        return Err(exhaust(state, admission, delta, reason));
    }

    admission.usage = attempted;
    admission.updated_at = now_ts();
    admission.last_rejected_delta = None;
    persist_admission(state, &admission).map_err(domain_failure)?;
    Ok(admission)
}

fn transition(
    state: &AppState,
    workflow_id: &str,
    updated_by: &str,
    status: AdmissionStatus,
    reason: Option<&str>,
) -> AdmissionResult<AdmissionRecord> {
    let _guard = admission_lock().lock();
    let mut admission = load_admission(state, workflow_id)
        .map_err(domain_failure)?
        .ok_or_else(|| {
            AdmissionFailure::new(
                StatusCode::CONFLICT,
                "admission_required",
                "workflow admission is missing",
            )
        })?;
    require_active(&admission)?;
    let actor = normalize_actor(updated_by)?;
    if actor != admission.requested_by
        && !admission
            .policy
            .selected_agent_keys
            .iter()
            .any(|agent| agent == &actor)
    {
        return Err(AdmissionFailure::new(
            StatusCode::FORBIDDEN,
            "admission_actor_not_authorized",
            "actor is not authorized to transition this admission",
        ));
    }
    admission.status = status;
    admission.updated_at = now_ts();
    admission.terminal_reason = reason.map(normalize_reason).transpose()?;
    persist_admission(state, &admission).map_err(domain_failure)?;
    Ok(admission)
}

fn validate_request_matches_plan(
    plan: &WorkflowPlan,
    request: &PolicyRequest,
) -> AdmissionResult<()> {
    if request.requested_mode != Some(plan.mode) {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "policy_mode_mismatch",
            "policy requested_mode must exactly match the workflow mode",
        ));
    }
    let requires_repository_write = plan
        .file_lease
        .as_ref()
        .is_some_and(|lease| lease.required);
    if request.requires_repository_write != requires_repository_write {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "policy_lease_mismatch",
            "policy repository-write flag must match the workflow lease requirement",
        ));
    }
    let planned_capabilities = plan
        .required_capabilities
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let requested_capabilities = request
        .required_capabilities
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if planned_capabilities != requested_capabilities {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "policy_capability_mismatch",
            "policy capabilities must exactly match the workflow plan",
        ));
    }
    let planned_agents = plan
        .assignments
        .iter()
        .map(|assignment| assignment.agent_key.as_str())
        .collect::<HashSet<_>>();
    let candidate_agents = request
        .providers
        .iter()
        .map(|provider| provider.agent_key.as_str())
        .collect::<HashSet<_>>();
    if planned_agents != candidate_agents {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "policy_candidate_mismatch",
            "policy candidates must exactly match workflow assignments",
        ));
    }
    Ok(())
}

fn attempted_totals(current: &UsageTotals, delta: &UsageDelta) -> Option<UsageTotals> {
    Some(UsageTotals {
        input_tokens: current.input_tokens.checked_add(delta.input_tokens)?,
        output_tokens: current.output_tokens.checked_add(delta.output_tokens)?,
        cost_micro_usd: current.cost_micro_usd.checked_add(delta.cost_micro_usd)?,
        retries: current.retries.checked_add(delta.retries)?,
        provider_calls: current.provider_calls.checked_add(delta.provider_calls)?,
        elapsed_ms: current.elapsed_ms.checked_add(delta.elapsed_ms)?,
        peak_concurrency: current.peak_concurrency.max(delta.concurrency),
    })
}

fn budget_violation(budget: &BudgetLimits, usage: &UsageTotals) -> Option<&'static str> {
    if usage.input_tokens > budget.max_input_tokens {
        return Some("input token budget exhausted");
    }
    if usage.output_tokens > budget.max_output_tokens {
        return Some("output token budget exhausted");
    }
    if usage.cost_micro_usd > budget.max_cost_micro_usd {
        return Some("cost budget exhausted");
    }
    if usage.retries > u64::from(budget.max_retries) {
        return Some("retry budget exhausted");
    }
    if usage.peak_concurrency > budget.max_concurrency {
        return Some("concurrency budget exhausted");
    }
    if usage.elapsed_ms > budget.max_wall_clock_ms {
        return Some("wall-clock budget exhausted");
    }
    let max_calls = u64::from(budget.max_providers)
        .saturating_mul(u64::from(budget.max_rounds))
        .saturating_mul(u64::from(budget.max_retries).saturating_add(1));
    if usage.provider_calls > max_calls {
        return Some("provider call budget exhausted");
    }
    None
}

fn exhaust(
    state: &AppState,
    mut admission: AdmissionRecord,
    delta: UsageDelta,
    reason: impl Into<String>,
) -> AdmissionFailure {
    let reason = reason.into();
    admission.status = AdmissionStatus::Exhausted;
    admission.updated_at = now_ts();
    admission.terminal_reason = Some(reason.clone());
    admission.last_rejected_delta = Some(delta);
    if let Err(error) = persist_admission(state, &admission) {
        return domain_failure(error);
    }
    AdmissionFailure::new(StatusCode::CONFLICT, "admission_exhausted", reason)
        .with_admission(admission)
}

fn require_active(admission: &AdmissionRecord) -> AdmissionResult<()> {
    if admission.status == AdmissionStatus::Active {
        Ok(())
    } else {
        Err(AdmissionFailure::new(
            StatusCode::CONFLICT,
            "admission_terminal",
            format!("admission is {:?}", admission.status),
        )
        .with_admission(admission.clone()))
    }
}

fn normalize_actor(value: &str) -> AdmissionResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_ACTOR_BYTES || value.chars().any(char::is_control) {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "invalid_admission_actor",
            "admission actor must be 1-120 printable bytes",
        ));
    }
    Ok(value.to_string())
}

fn normalize_reason(value: &str) -> AdmissionResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_REASON_BYTES || value.chars().any(char::is_control) {
        return Err(AdmissionFailure::new(
            StatusCode::BAD_REQUEST,
            "invalid_admission_reason",
            "admission reason must be 1-2048 printable bytes",
        ));
    }
    Ok(value.to_string())
}

fn domain_failure(error: BridgeError) -> AdmissionFailure {
    AdmissionFailure::new(
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        error.code(),
        error.to_string(),
    )
}
