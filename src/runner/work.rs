use std::collections::HashSet;

use serde_json::{json, Value};

use crate::orchestration::{
    AssignmentRole, WorkflowAssignment, WorkflowMode, WorkflowSubmission, WorkflowView,
};
use crate::providers::{ProviderConfig, ProviderProtocol, ProviderRequest};
use crate::types::AgentKind;

const MAX_PRIOR_CONTEXT_BYTES: usize = 512 * 1024;

pub(crate) fn infer_agent_kind(config: &ProviderConfig) -> AgentKind {
    match config.protocol {
        ProviderProtocol::OpenAiResponses => AgentKind::Codex,
        ProviderProtocol::AnthropicMessages => AgentKind::Claude,
        ProviderProtocol::GeminiGenerateContent => AgentKind::Gemini,
        ProviderProtocol::OpenAiCompatibleChat => {
            let name = config.name.to_ascii_lowercase();
            if name.contains("kimi") || name.contains("moonshot") {
                AgentKind::Kimi
            } else if name.contains("qwen") || name.contains("dashscope") {
                AgentKind::Qwen
            } else {
                AgentKind::Other
            }
        }
    }
}

pub(crate) fn protocol_label(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::OpenAiResponses => "open_ai_responses",
        ProviderProtocol::AnthropicMessages => "anthropic_messages",
        ProviderProtocol::GeminiGenerateContent => "gemini_generate_content",
        ProviderProtocol::OpenAiCompatibleChat => "open_ai_compatible_chat",
    }
}

pub(crate) fn eligible_assignment<'a>(
    workflow: &'a WorkflowView,
    agent_key: &str,
) -> Option<&'a WorkflowAssignment> {
    let assignment = workflow
        .plan
        .assignments
        .iter()
        .find(|assignment| assignment.agent_key == agent_key)?;
    if workflow
        .submissions
        .iter()
        .any(|submission| submission.assignment_ordinal == assignment.ordinal)
    {
        return None;
    }
    if !workflow
        .status
        .pending_agents
        .iter()
        .any(|pending| pending == agent_key)
    {
        return None;
    }
    if workflow
        .status
        .current_agent_key
        .as_deref()
        .is_some_and(|current| current != agent_key)
    {
        return None;
    }
    Some(assignment)
}

pub(crate) fn provider_request(
    workflow: &WorkflowView,
    assignment: &WorkflowAssignment,
    max_output_tokens: u32,
) -> ProviderRequest {
    let mut prompt = format!(
        "Workflow: {}\nMode: {:?}\nAssignment: {} ({:?})\n\n{}",
        workflow.plan.title,
        workflow.plan.mode,
        assignment.ordinal,
        assignment.role,
        workflow.plan.prompt
    );
    let prior = prior_submissions(workflow, assignment);
    if !prior.is_empty() {
        prompt.push_str("\n\nPrior accepted submissions:\n");
        let mut retained = 0usize;
        for submission in prior {
            let heading = format!(
                "\n--- agent={} ordinal={} role={:?} ---\n",
                submission.agent_key, submission.assignment_ordinal, submission.role
            );
            let remaining = MAX_PRIOR_CONTEXT_BYTES.saturating_sub(retained);
            if remaining == 0 {
                break;
            }
            append_bounded(&mut prompt, &heading, &mut retained, remaining);
            let remaining = MAX_PRIOR_CONTEXT_BYTES.saturating_sub(retained);
            if remaining == 0 {
                break;
            }
            append_bounded(&mut prompt, &submission.content, &mut retained, remaining);
        }
    }

    let system = match assignment.role {
        AssignmentRole::Worker => {
            "Produce one concrete solution for the assigned workflow. Do not claim that files, branches, tests, or external systems were changed unless the surrounding runner actually performed those operations. Return a self-contained result suitable for a workflow submission."
        }
        AssignmentRole::Reviewer => {
            "Review every prior worker submission, preserve the strongest compatible ideas, identify material failures, and return one synthesized recommendation. Do not invent execution results."
        }
    };
    ProviderRequest {
        prompt,
        max_output_tokens,
        system: Some(system.to_string()),
    }
}

pub(crate) fn success_meta(
    provider: &str,
    model: &str,
    protocol: ProviderProtocol,
    request_id: Option<&str>,
    usage: Value,
    fencing_token: Option<u64>,
) -> Value {
    json!({
        "status": "succeeded",
        "provider": provider,
        "model": model,
        "protocol": protocol_label(protocol),
        "request_id": request_id,
        "usage": usage,
        "fencing_token": fencing_token,
        "managed_by": "fiducia-ai-agent-runner",
    })
}

pub(crate) fn failure_meta(
    provider: &str,
    model: &str,
    protocol: ProviderProtocol,
    error: &str,
    fencing_token: Option<u64>,
) -> Value {
    json!({
        "status": "failed",
        "provider": provider,
        "model": model,
        "protocol": protocol_label(protocol),
        "error": error,
        "fencing_token": fencing_token,
        "managed_by": "fiducia-ai-agent-runner",
    })
}

pub(crate) fn configured_capabilities(
    provider_name: &str,
    capability_map: &Value,
    protocol: ProviderProtocol,
) -> Vec<String> {
    let mut capabilities = capability_map
        .get(provider_name)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    capabilities.push("model-provider".into());
    capabilities.push(protocol_label(protocol).into());
    let mut seen = HashSet::new();
    capabilities.retain(|value| seen.insert(value.clone()));
    capabilities.sort();
    capabilities
}

fn prior_submissions<'a>(
    workflow: &'a WorkflowView,
    assignment: &WorkflowAssignment,
) -> Vec<&'a WorkflowSubmission> {
    let mut submissions = match workflow.plan.mode {
        WorkflowMode::Single | WorkflowMode::Competitive => Vec::new(),
        WorkflowMode::Sequential => workflow
            .submissions
            .iter()
            .filter(|submission| submission.assignment_ordinal < assignment.ordinal)
            .collect(),
        WorkflowMode::Consensus => match assignment.role {
            AssignmentRole::Worker => Vec::new(),
            AssignmentRole::Reviewer => workflow
                .submissions
                .iter()
                .filter(|submission| submission.role == AssignmentRole::Worker)
                .collect(),
        },
    };
    submissions.sort_by_key(|submission| submission.assignment_ordinal);
    submissions
}

fn append_bounded(target: &mut String, value: &str, retained: &mut usize, limit: usize) {
    if value.len() <= limit {
        target.push_str(value);
        *retained = retained.saturating_add(value.len());
        return;
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
    *retained = retained.saturating_add(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::{WorkflowPlan, WorkflowStage, WorkflowStatus, WorkflowSubmission};

    fn assignment(ordinal: usize, role: AssignmentRole, agent: &str) -> WorkflowAssignment {
        WorkflowAssignment {
            ordinal,
            agent_key: agent.into(),
            role,
            phase: if role == AssignmentRole::Reviewer {
                1
            } else {
                0
            },
        }
    }

    fn view(mode: WorkflowMode) -> WorkflowView {
        WorkflowView {
            plan: WorkflowPlan {
                version: 1,
                id: "workflow-1".into(),
                channel: "workflow-workflow-1".into(),
                title: "Test workflow".into(),
                prompt: "Solve it".into(),
                mode,
                created_by: "coordinator".into(),
                created_at: "now".into(),
                assignments: vec![assignment(0, AssignmentRole::Worker, "codex")],
                file_lease: None,
                required_capabilities: Vec::new(),
                meta: json!({}),
            },
            status: WorkflowStatus {
                stage: WorkflowStage::Ready,
                current_agent_key: Some("codex".into()),
                submitted_agents: Vec::new(),
                pending_agents: vec!["codex".into()],
            },
            submissions: Vec::new(),
        }
    }

    fn submission(ordinal: usize, agent: &str, content: &str) -> WorkflowSubmission {
        WorkflowSubmission {
            workflow_id: "workflow-1".into(),
            assignment_ordinal: ordinal,
            agent_key: agent.into(),
            role: AssignmentRole::Worker,
            content: content.into(),
            meta: json!({}),
            submitted_at: "now".into(),
        }
    }

    #[test]
    fn eligibility_requires_pending_unsubmitted_assignment() {
        let mut workflow = view(WorkflowMode::Single);
        assert!(eligible_assignment(&workflow, "codex").is_some());
        workflow.submissions.push(submission(0, "codex", "done"));
        assert!(eligible_assignment(&workflow, "codex").is_none());
    }

    #[test]
    fn competitive_workers_do_not_receive_peer_outputs() {
        let mut workflow = view(WorkflowMode::Competitive);
        workflow
            .submissions
            .push(submission(1, "claude", "peer idea"));
        let request = provider_request(&workflow, &workflow.plan.assignments[0], 1000);
        assert!(!request.prompt.contains("peer idea"));
    }

    #[test]
    fn sequential_worker_receives_prior_output() {
        let mut workflow = view(WorkflowMode::Sequential);
        workflow
            .plan
            .assignments
            .push(assignment(1, AssignmentRole::Worker, "claude"));
        workflow
            .submissions
            .push(submission(0, "codex", "first idea"));
        let request = provider_request(&workflow, &workflow.plan.assignments[1], 1000);
        assert!(request.prompt.contains("first idea"));
    }

    #[test]
    fn consensus_reviewer_receives_all_worker_outputs() {
        let mut workflow = view(WorkflowMode::Consensus);
        workflow
            .plan
            .assignments
            .push(assignment(1, AssignmentRole::Worker, "claude"));
        workflow
            .plan
            .assignments
            .push(assignment(2, AssignmentRole::Reviewer, "gemini"));
        workflow
            .submissions
            .push(submission(0, "codex", "idea one"));
        workflow
            .submissions
            .push(submission(1, "claude", "idea two"));
        let request = provider_request(&workflow, &workflow.plan.assignments[2], 1000);
        assert!(request.prompt.contains("idea one"));
        assert!(request.prompt.contains("idea two"));
    }
}
