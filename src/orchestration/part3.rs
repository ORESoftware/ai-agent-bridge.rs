fn select_candidates(
    agents: &[Agent],
    requested_keys: &[String],
    requested_kinds: &[AgentKind],
    required_capabilities: &[String],
) -> BridgeResult<Vec<Agent>> {
    let by_key = agents
        .iter()
        .map(|agent| (agent.agent_key.clone(), agent.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();

    if !requested_keys.is_empty() {
        for requested in requested_keys {
            let key = normalize_agent_key(requested)?;
            if !seen.insert(key.clone()) {
                continue;
            }
            let agent = by_key
                .get(&key)
                .cloned()
                .ok_or_else(|| BridgeError::AgentNotFound(key.clone()))?;
            if !required_capabilities.is_empty()
                && !agent_has_capabilities(&agent, required_capabilities)
            {
                return Err(BridgeError::BadRequest(format!(
                    "agent '{}' does not advertise all required capabilities",
                    agent.agent_key
                )));
            }
            selected.push(agent);
        }
    } else {
        for agent in by_key.values() {
            if agent.kind == AgentKind::Human {
                continue;
            }
            if !requested_kinds.is_empty() && !requested_kinds.contains(&agent.kind) {
                continue;
            }
            if !agent_has_capabilities(agent, required_capabilities) {
                continue;
            }
            selected.push(agent.clone());
        }
    }

    if selected.is_empty() {
        return Err(BridgeError::BadRequest(
            "no registered agents match the workflow selection".into(),
        ));
    }
    Ok(selected)
}

fn build_assignments(
    mode: WorkflowMode,
    candidates: &[Agent],
    worker_count: Option<usize>,
    explicit_reviewer: Option<&str>,
) -> BridgeResult<Vec<WorkflowAssignment>> {
    let mut assignments = Vec::new();

    match mode {
        WorkflowMode::Single => {
            if worker_count.is_some_and(|count| count != 1) {
                return Err(BridgeError::BadRequest(
                    "single mode requires worker_count=1".into(),
                ));
            }
            let worker = candidates.first().ok_or_else(|| {
                BridgeError::BadRequest("single mode requires one matching agent".into())
            })?;
            assignments.push(WorkflowAssignment {
                ordinal: 0,
                agent_key: worker.agent_key.clone(),
                role: AssignmentRole::Worker,
                phase: 0,
            });
        }
        WorkflowMode::Sequential => {
            let count = normalized_worker_count(worker_count, candidates.len().min(3), 1)?;
            ensure_candidate_count(candidates, count, "sequential")?;
            for (ordinal, worker) in candidates.iter().take(count).enumerate() {
                assignments.push(WorkflowAssignment {
                    ordinal,
                    agent_key: worker.agent_key.clone(),
                    role: AssignmentRole::Worker,
                    phase: ordinal,
                });
            }
        }
        WorkflowMode::Competitive => {
            let count = normalized_worker_count(worker_count, candidates.len().min(3), 2)?;
            ensure_candidate_count(candidates, count, "competitive")?;
            for (ordinal, worker) in candidates.iter().take(count).enumerate() {
                assignments.push(WorkflowAssignment {
                    ordinal,
                    agent_key: worker.agent_key.clone(),
                    role: AssignmentRole::Worker,
                    phase: 0,
                });
            }
        }
        WorkflowMode::Consensus => {
            let reviewer = if let Some(reviewer) = explicit_reviewer {
                reviewer.to_string()
            } else {
                if candidates.len() < 3 {
                    return Err(BridgeError::BadRequest(
                        "consensus mode requires at least two workers and one reviewer".into(),
                    ));
                }
                let default_workers = worker_count.unwrap_or((candidates.len() - 1).min(3));
                candidates
                    .get(default_workers)
                    .ok_or_else(|| {
                        BridgeError::BadRequest(
                            "consensus mode needs one candidate reserved as reviewer".into(),
                        )
                    })?
                    .agent_key
                    .clone()
            };
            let max_workers = if explicit_reviewer.is_some() {
                candidates.len()
            } else {
                candidates.len().saturating_sub(1)
            };
            let count = normalized_worker_count(worker_count, max_workers.min(3), 2)?;
            ensure_candidate_count(candidates, count, "consensus")?;
            for (ordinal, worker) in candidates.iter().take(count).enumerate() {
                assignments.push(WorkflowAssignment {
                    ordinal,
                    agent_key: worker.agent_key.clone(),
                    role: AssignmentRole::Worker,
                    phase: 0,
                });
            }
            if assignments.iter().any(|item| item.agent_key == reviewer) {
                return Err(BridgeError::BadRequest(
                    "consensus reviewer must be distinct from every worker".into(),
                ));
            }
            assignments.push(WorkflowAssignment {
                ordinal: assignments.len(),
                agent_key: reviewer,
                role: AssignmentRole::Reviewer,
                phase: 1,
            });
        }
    }

    if assignments.len() > MAX_ASSIGNMENTS {
        return Err(BridgeError::CapacityExceeded {
            what: "workflow assignments",
            limit: MAX_ASSIGNMENTS,
        });
    }
    Ok(assignments)
}

fn normalized_worker_count(
    requested: Option<usize>,
    default: usize,
    minimum: usize,
) -> BridgeResult<usize> {
    let count = requested.unwrap_or(default);
    if count < minimum {
        return Err(BridgeError::BadRequest(format!(
            "workflow mode requires at least {minimum} worker(s)"
        )));
    }
    if count > MAX_ASSIGNMENTS {
        return Err(BridgeError::CapacityExceeded {
            what: "workflow assignments",
            limit: MAX_ASSIGNMENTS,
        });
    }
    Ok(count)
}

fn ensure_candidate_count(candidates: &[Agent], count: usize, mode: &str) -> BridgeResult<()> {
    if candidates.len() < count {
        return Err(BridgeError::BadRequest(format!(
            "{mode} mode requested {count} worker(s), but only {} matching agent(s) are registered",
            candidates.len()
        )));
    }
    Ok(())
}

fn build_file_lease_requirement(
    repository: Option<&str>,
    paths: &[String],
    required: bool,
    ttl_ms: Option<u64>,
) -> BridgeResult<Option<FileLeaseRequirement>> {
    let has_repository = repository.is_some_and(|value| !value.trim().is_empty());
    let has_paths = !paths.is_empty();
    if !has_repository && !has_paths && !required {
        return Ok(None);
    }
    let repository = normalize_repository(repository.unwrap_or_default())?;
    if paths.is_empty() {
        return Err(BridgeError::BadRequest(
            "file-lease workflows require at least one exact repository-relative path".into(),
        ));
    }
    let mut normalized_paths = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let path = normalize_repo_path(path)?;
        if seen.insert(path.clone()) {
            normalized_paths.push(path);
        }
    }
    let ttl_ms = ttl_ms
        .unwrap_or(DEFAULT_LEASE_TTL_MS)
        .clamp(MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS);
    Ok(Some(FileLeaseRequirement {
        repository,
        paths: normalized_paths,
        required: true,
        ttl_ms,
        acquire_path: "/file-leases/acquire".to_string(),
        release_path: "/file-leases/release".to_string(),
    }))
}

fn normalize_repository(value: &str) -> BridgeResult<String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err(BridgeError::BadRequest("repository is required".into()));
    }
    if value.len() > MAX_REPOSITORY_BYTES {
        return Err(BridgeError::PayloadTooLarge {
            what: "repository",
            limit: MAX_REPOSITORY_BYTES,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(BridgeError::BadRequest(
            "repository contains control characters".into(),
        ));
    }
    Ok(value.to_string())
}

fn normalize_repo_path(value: &str) -> BridgeResult<String> {
    let value = value.trim().trim_matches('/');
    if value.is_empty() {
        return Err(BridgeError::BadRequest("file-lease path is required".into()));
    }
    if value.len() > MAX_PATH_BYTES {
        return Err(BridgeError::PayloadTooLarge {
            what: "file-lease path",
            limit: MAX_PATH_BYTES,
        });
    }
    if value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || value.contains('\\')
    {
        return Err(BridgeError::BadRequest(format!(
            "file-lease path '{value}' is not canonical and repository-relative"
        )));
    }
    Ok(value.to_string())
}

fn normalize_agent_key(value: &str) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest("agent_key is required".into()));
    }
    if value.len() > 120 {
        return Err(BridgeError::PayloadTooLarge {
            what: "agent_key",
            limit: 120,
        });
    }
    Ok(value.to_string())
}

fn normalize_required_text(value: &str, name: &'static str, limit: usize) -> BridgeResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest(format!("{name} is required")));
    }
    if value.len() > limit {
        return Err(BridgeError::PayloadTooLarge { what: name, limit });
    }
    Ok(value.to_string())
}

fn normalize_capabilities(values: &[String]) -> BridgeResult<Vec<String>> {
    let mut capabilities = BTreeSet::new();
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(BridgeError::BadRequest(
                "required capabilities cannot contain empty values".into(),
            ));
        }
        if value.len() > 120 || value.chars().any(char::is_control) {
            return Err(BridgeError::BadRequest(format!(
                "invalid required capability '{value}'"
            )));
        }
        capabilities.insert(value);
    }
    Ok(capabilities.into_iter().collect())
}

fn agent_has_capabilities(agent: &Agent, required: &[String]) -> bool {
    if required.is_empty() {
        return true;
    }
    let advertised = agent
        .meta
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    required.iter().all(|value| advertised.contains(value))
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

fn workflow_channel(workflow_id: &str) -> BridgeResult<String> {
    let workflow_id = workflow_id.trim();
    if workflow_id.is_empty()
        || workflow_id.len() > 64
        || !workflow_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(BridgeError::BadRequest("invalid workflow id".into()));
    }
    Ok(format!("workflow-{workflow_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(key: &str, kind: AgentKind, capabilities: &[&str]) -> Agent {
        Agent {
            agent_key: key.to_string(),
            display_name: key.to_string(),
            kind,
            host: None,
            meta: json!({ "capabilities": capabilities }),
            registered_at: now_ts(),
        }
    }

    fn submission(assignment: &WorkflowAssignment) -> WorkflowSubmission {
        WorkflowSubmission {
            workflow_id: "wf".into(),
            assignment_ordinal: assignment.ordinal,
            agent_key: assignment.agent_key.clone(),
            role: assignment.role,
            content: "done".into(),
            meta: json!({}),
            submitted_at: now_ts(),
        }
    }

    fn plan(mode: WorkflowMode, assignments: Vec<WorkflowAssignment>) -> WorkflowPlan {
        WorkflowPlan {
            version: 1,
            id: "wf".into(),
            channel: "workflow-wf".into(),
            title: "test".into(),
            prompt: "test".into(),
            mode,
            created_by: "coordinator".into(),
            created_at: now_ts(),
            assignments,
            file_lease: None,
            required_capabilities: Vec::new(),
            meta: json!({}),
        }
    }

    #[test]
    fn agent_kind_filters_include_new_providers() {
        let agents = vec![
            agent("codex", AgentKind::Codex, &["rust"]),
            agent("gemini", AgentKind::Gemini, &["rust", "review"]),
            agent("kimi", AgentKind::Kimi, &["review"]),
            agent("qwen", AgentKind::Qwen, &["rust"]),
        ];
        let selected = select_candidates(
            &agents,
            &[],
            &[AgentKind::Gemini, AgentKind::Qwen],
            &["rust".into()],
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|item| item.agent_key.as_str())
                .collect::<Vec<_>>(),
            vec!["gemini", "qwen"]
        );
    }

    #[test]
    fn single_mode_always_assigns_exactly_one_worker() {
        let agents = vec![
            agent("claude", AgentKind::Claude, &[]),
            agent("codex", AgentKind::Codex, &[]),
        ];
        let assignments = build_assignments(WorkflowMode::Single, &agents, None, None).unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].agent_key, "claude");
        assert_eq!(assignments[0].role, AssignmentRole::Worker);
    }

    #[test]
    fn sequential_mode_has_strictly_increasing_phases() {
        let agents = vec![
            agent("claude", AgentKind::Claude, &[]),
            agent("codex", AgentKind::Codex, &[]),
            agent("gemini", AgentKind::Gemini, &[]),
        ];
        let assignments =
            build_assignments(WorkflowMode::Sequential, &agents, Some(3), None).unwrap();
        assert_eq!(
            assignments
                .iter()
                .map(|item| item.phase)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let workflow = plan(WorkflowMode::Sequential, assignments.clone());
        let initial = workflow_status(&workflow, &[]);
        assert_eq!(initial.current_agent_key.as_deref(), Some("claude"));
        let after_first = workflow_status(&workflow, &[submission(&assignments[0])]);
        assert_eq!(after_first.current_agent_key.as_deref(), Some("codex"));
    }

    #[test]
    fn competitive_mode_requires_multiple_workers() {
        let agents = vec![agent("codex", AgentKind::Codex, &[])];
        let error =
            build_assignments(WorkflowMode::Competitive, &agents, None, None).unwrap_err();
        assert!(error.to_string().contains("at least 2"));
    }

    #[test]
    fn consensus_reserves_a_distinct_reviewer() {
        let agents = vec![
            agent("claude", AgentKind::Claude, &[]),
            agent("codex", AgentKind::Codex, &[]),
            agent("gemini", AgentKind::Gemini, &[]),
        ];
        let assignments =
            build_assignments(WorkflowMode::Consensus, &agents, Some(2), None).unwrap();
        assert_eq!(assignments.len(), 3);
        assert_eq!(assignments[2].role, AssignmentRole::Reviewer);
        assert_eq!(assignments[2].agent_key, "gemini");
    }

    #[test]
    fn consensus_reviewer_waits_for_all_workers() {
        let agents = vec![
            agent("claude", AgentKind::Claude, &[]),
            agent("codex", AgentKind::Codex, &[]),
            agent("gemini", AgentKind::Gemini, &[]),
        ];
        let assignments =
            build_assignments(WorkflowMode::Consensus, &agents, Some(2), None).unwrap();
        let workflow = plan(WorkflowMode::Consensus, assignments.clone());
        let reviewer = assignments.last().unwrap();
        let error = validate_submission_turn(&workflow, reviewer, &[]).unwrap_err();
        assert!(error.to_string().contains("wait for every worker"));
        let worker_submissions = vec![submission(&assignments[0]), submission(&assignments[1])];
        validate_submission_turn(&workflow, reviewer, &worker_submissions).unwrap();
        let status = workflow_status(&workflow, &worker_submissions);
        assert_eq!(status.stage, WorkflowStage::AwaitingReview);
        assert_eq!(status.current_agent_key.as_deref(), Some("gemini"));
    }

    #[test]
    fn lease_paths_reject_traversal_and_become_required() {
        assert!(build_file_lease_requirement(
            Some("ORESoftware/repo"),
            &["src/../secret".into()],
            true,
            None,
        )
        .is_err());
        let requirement = build_file_lease_requirement(
            Some("ORESoftware/repo"),
            &["src/lib.rs".into()],
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(requirement.required);
        assert_eq!(requirement.ttl_ms, DEFAULT_LEASE_TTL_MS);
    }
}
