fn blind_context_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn insert_blind_context(
    state: &AppState,
    channel: &str,
    key: &str,
    value: serde_json::Value,
    updated_by: &str,
) -> BridgeResult<()> {
    let _guard = blind_context_lock().lock();
    if state.get_context_key(channel, key)?.is_some() {
        return Err(BridgeError::BadRequest(format!(
            "blind competition context key '{key}' already exists"
        )));
    }
    state.set_context(channel, key, value, updated_by)?;
    Ok(())
}

fn load_plan(state: &AppState, workflow_id: &str) -> BridgeResult<BlindCompetitionPlan> {
    let channel = blind_channel(workflow_id)?;
    let entry = state
        .get_context_key(&channel, BLIND_PLAN_CONTEXT_KEY)?
        .ok_or_else(|| {
            BridgeError::BadRequest(format!("blind competition plan missing in '{channel}'"))
        })?;
    serde_json::from_value(entry.value).map_err(|_| {
        BridgeError::BadRequest(format!("blind competition plan in '{channel}' is invalid"))
    })
}

fn load_submissions(state: &AppState, channel: &str) -> BridgeResult<Vec<BlindSubmission>> {
    let mut submissions = state
        .get_context(channel)?
        .into_iter()
        .filter_map(|entry| {
            entry
                .key
                .strip_prefix(BLIND_SUBMISSION_CONTEXT_PREFIX)
                .and_then(|_| serde_json::from_value::<BlindSubmission>(entry.value).ok())
        })
        .collect::<Vec<_>>();
    submissions.sort_by_key(|submission| submission.assignment_ordinal);
    Ok(submissions)
}

fn load_reveal(state: &AppState, channel: &str) -> BridgeResult<Option<BlindReveal>> {
    state
        .get_context_key(channel, BLIND_REVEAL_CONTEXT_KEY)?
        .map(|entry| {
            serde_json::from_value(entry.value).map_err(|_| {
                BridgeError::BadRequest(format!(
                    "blind competition reveal in '{channel}' is invalid"
                ))
            })
        })
        .transpose()
}

fn build_view(
    state: &AppState,
    plan: &BlindCompetitionPlan,
    viewer_agent_key: Option<&str>,
) -> BridgeResult<BlindCompetitionView> {
    let all_submissions = load_submissions(state, &plan.channel)?;
    let reveal = load_reveal(state, &plan.channel)?;
    let revealed = reveal.is_some();
    let visible_submissions = if revealed {
        all_submissions.clone()
    } else if let Some(viewer) = viewer_agent_key {
        all_submissions
            .iter()
            .filter(|submission| submission.agent_key == viewer)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let all_workers_submitted = all_submissions.len() == plan.workers.len();
    let stage = if revealed {
        BlindCompetitionStage::Revealed
    } else if all_workers_submitted {
        BlindCompetitionStage::ReadyToReveal
    } else {
        BlindCompetitionStage::Collecting
    };
    let reviewer_can_reveal = !revealed
        && all_workers_submitted
        && viewer_agent_key == Some(plan.reviewer_agent_key.as_str());

    Ok(BlindCompetitionView {
        plan: plan.clone(),
        stage,
        submission_count: all_submissions.len(),
        hidden_submission_count: all_submissions
            .len()
            .saturating_sub(visible_submissions.len()),
        revealed,
        reviewer_can_reveal,
        submissions: visible_submissions,
        reveal,
    })
}

fn blind_channel(workflow_id: &str) -> BridgeResult<String> {
    let workflow_id = workflow_id.trim();
    if workflow_id.is_empty()
        || workflow_id.len() > 64
        || !workflow_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(BridgeError::BadRequest(
            "invalid blind competition id".into(),
        ));
    }
    Ok(format!("blind-workflow-{workflow_id}"))
}

fn normalize_agent_key(value: &str) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest("agent_key is required".into()));
    }
    if value.len() > MAX_AGENT_KEY_BYTES || value.chars().any(char::is_control) {
        return Err(BridgeError::BadRequest(format!(
            "agent_key must be 1..={MAX_AGENT_KEY_BYTES} printable bytes"
        )));
    }
    Ok(value.to_string())
}

fn normalize_required_text(
    value: &str,
    name: &'static str,
    limit: usize,
) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest(format!("{name} is required")));
    }
    if value.len() > limit {
        return Err(BridgeError::PayloadTooLarge { what: name, limit });
    }
    Ok(value.to_string())
}

fn validate_json_size(
    value: &serde_json::Value,
    limit: usize,
    what: &'static str,
) -> BridgeResult<()> {
    let size = serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if size > limit {
        return Err(BridgeError::PayloadTooLarge { what, limit });
    }
    Ok(())
}

fn registered_agents_by_key(state: &AppState) -> BTreeMap<String, Agent> {
    state
        .list_agents()
        .into_iter()
        .map(|agent| (agent.agent_key.clone(), agent))
        .collect()
}

fn viewer_key(viewer: Option<&Extension<AuthenticatedAdapter>>) -> Option<&str> {
    viewer.map(|Extension(identity)| identity.agent_key.as_str())
}
