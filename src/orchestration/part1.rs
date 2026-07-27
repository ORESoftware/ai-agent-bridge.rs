use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    middleware::{from_fn, from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use parking_lot::Mutex;
use serde_json::json;

use crate::error::{BridgeError, BridgeResult};
use crate::state::AppState;
use crate::types::{now_ts, Agent, AgentKind, ContextEntry, MemberRole, Role};

const PLAN_CONTEXT_KEY: &str = "workflow.plan.v1";
const SUBMISSION_CONTEXT_PREFIX: &str = "workflow.submission.v1.";
const MAX_WORKFLOW_TITLE_BYTES: usize = 1_024;
const MAX_ASSIGNMENTS: usize = 30;
const MAX_REPOSITORY_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 4_096;
const DEFAULT_LEASE_TTL_MS: u64 = 30_000;
const MIN_LEASE_TTL_MS: u64 = 100;
const MAX_LEASE_TTL_MS: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    Single,
    Sequential,
    #[serde(alias = "compete")]
    Competitive,
    Consensus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentRole {
    Worker,
    Reviewer,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowAssignment {
    pub ordinal: usize,
    pub agent_key: String,
    pub role: AssignmentRole,
    /// Assignments sharing a phase may run independently in parallel. A higher
    /// phase cannot submit until the preceding phase's gate has been satisfied.
    pub phase: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileLeaseRequirement {
    pub repository: String,
    pub paths: Vec<String>,
    pub required: bool,
    pub ttl_ms: u64,
    pub acquire_path: String,
    pub release_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowPlan {
    pub version: u32,
    pub id: String,
    pub channel: String,
    pub title: String,
    pub prompt: String,
    pub mode: WorkflowMode,
    pub created_by: String,
    pub created_at: String,
    pub assignments: Vec<WorkflowAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_lease: Option<FileLeaseRequirement>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowSubmission {
    pub workflow_id: String,
    pub assignment_ordinal: usize,
    pub agent_key: String,
    pub role: AssignmentRole,
    pub content: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub submitted_at: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStage {
    Ready,
    Running,
    AwaitingReview,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowStatus {
    pub stage: WorkflowStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_agent_key: Option<String>,
    pub submitted_agents: Vec<String>,
    pub pending_agents: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkflowView {
    pub plan: WorkflowPlan,
    pub status: WorkflowStatus,
    pub submissions: Vec<WorkflowSubmission>,
}

#[derive(Debug, Deserialize)]
struct CreateWorkflowReq {
    title: String,
    prompt: String,
    created_by: String,
    mode: WorkflowMode,
    #[serde(default)]
    agent_keys: Vec<String>,
    #[serde(default)]
    agent_kinds: Vec<AgentKind>,
    #[serde(default)]
    required_capabilities: Vec<String>,
    #[serde(default)]
    worker_count: Option<usize>,
    #[serde(default)]
    reviewer_agent_key: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    require_file_leases: bool,
    #[serde(default)]
    lease_ttl_ms: Option<u64>,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct SubmitWorkflowReq {
    agent_key: String,
    content: String,
    #[serde(default)]
    meta: serde_json::Value,
}

struct ApiError(BridgeError);

impl From<BridgeError> for ApiError {
    fn from(error: BridgeError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.payload())).into_response()
    }
}

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

/// Add workflow routes to the process without changing the established chat API.
pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.max_http_body_bytes;
    Router::new()
        .route("/workflows", get(list_workflows).post(create_workflow))
        .route("/workflows/{workflow_id}", get(get_workflow))
        .route(
            "/workflows/{workflow_id}/submissions",
            post(submit_workflow),
        )
        .layer(from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(from_fn(request_timeout))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(state)
}

async fn request_timeout(req: axum::extract::Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), next.run(req)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "ok": false, "error": "request_timeout" })),
        )
            .into_response(),
    }
}

async fn auth(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.config.api_auth_bearer {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let authorized = presented
            .map(|value| {
                crate::config::constant_time_eq(value.as_bytes(), expected.as_bytes())
            })
            .unwrap_or(false);
        if !authorized {
            return ApiError(BridgeError::Unauthorized).into_response();
        }
    }
    next.run(req).await
}

async fn create_workflow(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateWorkflowReq>,
) -> ApiResult {
    let title = normalize_required_text(&req.title, "title", MAX_WORKFLOW_TITLE_BYTES)?;
    let prompt = normalize_required_text(&req.prompt, "prompt", state.config.max_content_bytes)?;
    let created_by = normalize_agent_key(&req.created_by)?;
    validate_json_size(&req.meta, state.config.max_content_bytes, "workflow meta")?;

    let required_capabilities = normalize_capabilities(&req.required_capabilities)?;
    let all_agents = state.list_agents();
    let mut candidates = select_candidates(
        &all_agents,
        &req.agent_keys,
        &req.agent_kinds,
        &required_capabilities,
    )?;

    let explicit_reviewer = req
        .reviewer_agent_key
        .as_deref()
        .map(normalize_agent_key)
        .transpose()?;
    if let Some(reviewer) = &explicit_reviewer {
        if !all_agents.iter().any(|agent| agent.agent_key == *reviewer) {
            return Err(BridgeError::AgentNotFound(reviewer.clone()).into());
        }
        candidates.retain(|agent| agent.agent_key != *reviewer);
    }

    let assignments = build_assignments(
        req.mode,
        &candidates,
        req.worker_count,
        explicit_reviewer.as_deref(),
    )?;
    if assignments.len() > MAX_ASSIGNMENTS {
        return Err(BridgeError::CapacityExceeded {
            what: "workflow assignments",
            limit: MAX_ASSIGNMENTS,
        }
        .into());
    }

    let file_lease = build_file_lease_requirement(
        req.repository.as_deref(),
        &req.paths,
        req.require_file_leases,
        req.lease_ttl_ms,
    )?;

    let id = crate::types::new_id();
    let channel = format!("workflow-{id}");
    let plan = WorkflowPlan {
        version: 1,
        id: id.clone(),
        channel: channel.clone(),
        title,
        prompt,
        mode: req.mode,
        created_by: created_by.clone(),
        created_at: now_ts(),
        assignments,
        file_lease,
        required_capabilities,
        meta: req.meta,
    };

    state
        .create_or_get_channel(&channel, &plan.title, &created_by)
        .await?;
    for assignment in &plan.assignments {
        state.join(&channel, &assignment.agent_key, MemberRole::Member)?;
    }
    insert_workflow_context(
        &state,
        &channel,
        PLAN_CONTEXT_KEY,
        serde_json::to_value(&plan)
            .map_err(|_| BridgeError::BadRequest("workflow plan is not serializable".into()))?,
        &created_by,
    )?;
    state.post_message(
        &channel,
        &created_by,
        Role::System,
        &format!(
            "Workflow {} created in {:?} mode with {} assignment(s).",
            plan.id,
            plan.mode,
            plan.assignments.len()
        ),
        json!({
            "kind": "workflow_created",
            "workflow_id": plan.id,
            "mode": plan.mode,
        }),
    )?;

    let view = workflow_view(&state, &plan)?;
    Ok(Json(json!({
        "ok": true,
        "workflow": view,
        "stream": format!("/channels/{}/stream", plan.channel),
        "messages": format!("/channels/{}/messages", plan.channel),
    })))
}

async fn list_workflows(State(state): State<Arc<AppState>>) -> ApiResult {
    let mut workflows = Vec::new();
    for channel in state
        .list_channels()
        .into_iter()
        .filter(|channel| channel.slug.starts_with("workflow-"))
    {
        if let Ok(plan) = load_plan_by_channel(&state, &channel.slug) {
            if let Ok(view) = workflow_view(&state, &plan) {
                workflows.push(view);
            }
        }
    }
    workflows.sort_by(|left, right| left.plan.created_at.cmp(&right.plan.created_at));
    Ok(Json(json!({ "ok": true, "workflows": workflows })))
}

async fn get_workflow(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
) -> ApiResult {
    let plan = load_plan(&state, &workflow_id)?;
    let view = workflow_view(&state, &plan)?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}

async fn submit_workflow(
    State(state): State<Arc<AppState>>,
    Path(workflow_id): Path<String>,
    Json(req): Json<SubmitWorkflowReq>,
) -> ApiResult {
    let agent_key = normalize_agent_key(&req.agent_key)?;
    let content = normalize_required_text(
        &req.content,
        "submission content",
        state.config.max_content_bytes,
    )?;
    validate_json_size(
        &req.meta,
        state.config.max_content_bytes,
        "submission meta",
    )?;

    let plan = load_plan(&state, &workflow_id)?;
    let assignment = plan
        .assignments
        .iter()
        .find(|assignment| assignment.agent_key == agent_key)
        .cloned()
        .ok_or_else(|| {
            BridgeError::BadRequest(format!(
                "agent '{agent_key}' is not assigned to workflow '{}'",
                plan.id
            ))
        })?;
    let submissions = load_submissions(&state, &plan.channel)?;
    validate_submission_turn(&plan, &assignment, &submissions)?;

    let submission = WorkflowSubmission {
        workflow_id: plan.id.clone(),
        assignment_ordinal: assignment.ordinal,
        agent_key: agent_key.clone(),
        role: assignment.role,
        content: content.clone(),
        meta: req.meta,
        submitted_at: now_ts(),
    };
    let context_key = format!("{SUBMISSION_CONTEXT_PREFIX}{}", assignment.ordinal);
    insert_workflow_context(
        &state,
        &plan.channel,
        &context_key,
        serde_json::to_value(&submission)
            .map_err(|_| BridgeError::BadRequest("submission is not serializable".into()))?,
        &agent_key,
    )?;
    state.post_message(
        &plan.channel,
        &agent_key,
        Role::Assistant,
        &content,
        json!({
            "kind": "workflow_submission",
            "workflow_id": plan.id,
            "assignment_ordinal": assignment.ordinal,
            "assignment_role": assignment.role,
            "phase": assignment.phase,
        }),
    )?;

    let view = workflow_view(&state, &plan)?;
    Ok(Json(json!({ "ok": true, "workflow": view })))
}
