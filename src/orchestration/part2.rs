fn workflow_context_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Insert a workflow-owned context record without replacing an existing value.
///
/// The lock closes duplicate-submission races within the single authoritative
/// bridge process. A future state-layer reservation should additionally reject
/// generic context writes using the `workflow.*` namespace.
fn insert_workflow_context(
    state: &AppState,
    slug: &str,
    key: &str,
    value: serde_json::Value,
    updated_by: &str,
) -> BridgeResult<ContextEntry> {
    let _guard = workflow_context_lock().lock();
    if state.get_context_key(slug, key)?.is_some() {
        return Err(BridgeError::BadRequest(format!(
            "workflow context key '{key}' already exists"
        )));
    }
    state.set_context(slug, key, value, updated_by)
}

fn load_plan(state: &AppState, workflow_id: &str) -> BridgeResult<WorkflowPlan> {
    let channel = workflow_channel(workflow_id)?;
    load_plan_by_channel(state, &channel)
}

fn load_plan_by_channel(state: &AppState, channel: &str) -> BridgeResult<WorkflowPlan> {
    let entry = state
        .get_context_key(channel, PLAN_CONTEXT_KEY)?
        .ok_or_else(|| BridgeError::BadRequest(format!("workflow plan missing in '{channel}'")))?;
    serde_json::from_value(entry.value)
        .map_err(|_| BridgeError::BadRequest(format!("workflow plan in '{channel}' is invalid")))
}

fn workflow_view(state: &AppState, plan: &WorkflowPlan) -> BridgeResult<WorkflowView> {
    let submissions = load_submissions(state, &plan.channel)?;
    let status = workflow_status(plan, &submissions);
    Ok(WorkflowView {
        plan: plan.clone(),
        status,
        submissions,
    })
}

fn load_submissions(state: &AppState, channel: &str) -> BridgeResult<Vec<WorkflowSubmission>> {
    let mut submissions = state
        .get_context(channel)?
        .into_iter()
        .filter_map(parse_submission)
        .collect::<Vec<_>>();
    submissions.sort_by_key(|submission| submission.assignment_ordinal);
    Ok(submissions)
}

fn parse_submission(entry: ContextEntry) -> Option<WorkflowSubmission> {
    entry
        .key
        .strip_prefix(SUBMISSION_CONTEXT_PREFIX)
        .and_then(|_| serde_json::from_value(entry.value).ok())
}

fn workflow_status(plan: &WorkflowPlan, submissions: &[WorkflowSubmission]) -> WorkflowStatus {
    let submitted_ordinals = submissions
        .iter()
        .map(|submission| submission.assignment_ordinal)
        .collect::<BTreeSet<_>>();
    let submitted_agents = plan
        .assignments
        .iter()
        .filter(|assignment| submitted_ordinals.contains(&assignment.ordinal))
        .map(|assignment| assignment.agent_key.clone())
        .collect::<Vec<_>>();

    let workers = plan
        .assignments
        .iter()
        .filter(|assignment| assignment.role == AssignmentRole::Worker)
        .collect::<Vec<_>>();
    let pending_workers = workers
        .iter()
        .filter(|assignment| !submitted_ordinals.contains(&assignment.ordinal))
        .copied()
        .collect::<Vec<_>>();
    let any_submitted = !submitted_ordinals.is_empty();

    match plan.mode {
        WorkflowMode::Sequential => {
            if let Some(next) = pending_workers.first() {
                WorkflowStatus {
                    stage: if any_submitted {
                        WorkflowStage::Running
                    } else {
                        WorkflowStage::Ready
                    },
                    current_agent_key: Some(next.agent_key.clone()),
                    submitted_agents,
                    pending_agents: pending_workers
                        .iter()
                        .map(|assignment| assignment.agent_key.clone())
                        .collect(),
                }
            } else {
                WorkflowStatus {
                    stage: WorkflowStage::Completed,
                    current_agent_key: None,
                    submitted_agents,
                    pending_agents: Vec::new(),
                }
            }
        }
        WorkflowMode::Consensus => {
            if !pending_workers.is_empty() {
                WorkflowStatus {
                    stage: if any_submitted {
                        WorkflowStage::Running
                    } else {
                        WorkflowStage::Ready
                    },
                    current_agent_key: None,
                    submitted_agents,
                    pending_agents: pending_workers
                        .iter()
                        .map(|assignment| assignment.agent_key.clone())
                        .collect(),
                }
            } else if let Some(reviewer) = plan
                .assignments
                .iter()
                .find(|assignment| assignment.role == AssignmentRole::Reviewer)
            {
                if submitted_ordinals.contains(&reviewer.ordinal) {
                    WorkflowStatus {
                        stage: WorkflowStage::Completed,
                        current_agent_key: None,
                        submitted_agents,
                        pending_agents: Vec::new(),
                    }
                } else {
                    WorkflowStatus {
                        stage: WorkflowStage::AwaitingReview,
                        current_agent_key: Some(reviewer.agent_key.clone()),
                        submitted_agents,
                        pending_agents: vec![reviewer.agent_key.clone()],
                    }
                }
            } else {
                WorkflowStatus {
                    stage: WorkflowStage::Completed,
                    current_agent_key: None,
                    submitted_agents,
                    pending_agents: Vec::new(),
                }
            }
        }
        WorkflowMode::Single | WorkflowMode::Competitive => {
            if pending_workers.is_empty() {
                WorkflowStatus {
                    stage: WorkflowStage::Completed,
                    current_agent_key: None,
                    submitted_agents,
                    pending_agents: Vec::new(),
                }
            } else {
                WorkflowStatus {
                    stage: if any_submitted {
                        WorkflowStage::Running
                    } else {
                        WorkflowStage::Ready
                    },
                    current_agent_key: if plan.mode == WorkflowMode::Single {
                        pending_workers
                            .first()
                            .map(|assignment| assignment.agent_key.clone())
                    } else {
                        None
                    },
                    submitted_agents,
                    pending_agents: pending_workers
                        .iter()
                        .map(|assignment| assignment.agent_key.clone())
                        .collect(),
                }
            }
        }
    }
}

fn validate_submission_turn(
    plan: &WorkflowPlan,
    assignment: &WorkflowAssignment,
    submissions: &[WorkflowSubmission],
) -> BridgeResult<()> {
    let submitted = submissions
        .iter()
        .map(|submission| submission.assignment_ordinal)
        .collect::<BTreeSet<_>>();

    if submitted.contains(&assignment.ordinal) {
        return Err(BridgeError::BadRequest(format!(
            "assignment {} has already submitted",
            assignment.ordinal
        )));
    }

    match plan.mode {
        WorkflowMode::Sequential => {
            let first_missing = plan
                .assignments
                .iter()
                .filter(|candidate| candidate.role == AssignmentRole::Worker)
                .find(|candidate| !submitted.contains(&candidate.ordinal));
            if first_missing.map(|candidate| candidate.ordinal) != Some(assignment.ordinal) {
                let expected = first_missing
                    .map(|candidate| candidate.agent_key.as_str())
                    .unwrap_or("none");
                return Err(BridgeError::BadRequest(format!(
                    "sequential workflow is waiting for agent '{expected}'"
                )));
            }
        }
        WorkflowMode::Consensus => match assignment.role {
            AssignmentRole::Worker => {
                let reviewer_submitted = plan.assignments.iter().any(|candidate| {
                    candidate.role == AssignmentRole::Reviewer
                        && submitted.contains(&candidate.ordinal)
                });
                if reviewer_submitted {
                    return Err(BridgeError::BadRequest(
                        "worker submissions are closed after the reviewer submitted".into(),
                    ));
                }
            }
            AssignmentRole::Reviewer => {
                let pending_worker = plan.assignments.iter().any(|candidate| {
                    candidate.role == AssignmentRole::Worker
                        && !submitted.contains(&candidate.ordinal)
                });
                if pending_worker {
                    return Err(BridgeError::BadRequest(
                        "reviewer must wait for every worker submission".into(),
                    ));
                }
            }
        },
        WorkflowMode::Single | WorkflowMode::Competitive => {
            if assignment.role != AssignmentRole::Worker {
                return Err(BridgeError::BadRequest(
                    "reviewer assignments are only valid in consensus mode".into(),
                ));
            }
        }
    }
    Ok(())
}
