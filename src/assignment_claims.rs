//! Distributed assignment-claim validation for horizontally scaled runners.
//!
//! A runner acquires one Fiducia lease for `(workflow_id, assignment_ordinal)`.
//! Immediately before accepting its submission, this middleware renews the exact
//! claim union with the presented fencing token. A stale replica therefore cannot
//! publish after another replica has acquired a newer grant.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::BridgeError;
use crate::orchestration::WorkflowPlan;
use crate::state::AppState;

const PLAN_CONTEXT_KEY: &str = "workflow.plan.v1";
const DEFAULT_CLAIM_REPOSITORY: &str = "fiducia-cloud/ai-agent-assignment-claims";
const DEFAULT_MAX_CLAIM_TTL_MS: u64 = 60_000;
const ABSOLUTE_MAX_TTL_MS: u64 = 86_400_000;

#[derive(Clone)]
pub struct AssignmentClaimPolicy {
    state: Arc<AppState>,
    required: bool,
    repository: String,
    max_ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
struct SubmissionEnvelope {
    agent_key: String,
    #[serde(default)]
    meta: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct AssignmentClaim {
    repository: String,
    paths: Vec<String>,
    owner: String,
    fencing_token: u64,
    ttl_ms: u64,
    assignment_ordinal: usize,
    instance_id: String,
}

impl AssignmentClaimPolicy {
    pub fn from_env(state: Arc<AppState>) -> anyhow::Result<Arc<Self>> {
        let required = env_bool("AI_AGENT_BRIDGE_REQUIRE_ASSIGNMENT_CLAIMS", false)?;
        let repository = std::env::var("AI_AGENT_BRIDGE_ASSIGNMENT_CLAIM_REPOSITORY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLAIM_REPOSITORY.to_string());
        let max_ttl_ms = std::env::var("AI_AGENT_BRIDGE_ASSIGNMENT_CLAIM_MAX_TTL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_MAX_CLAIM_TTL_MS)
            .clamp(1, ABSOLUTE_MAX_TTL_MS);
        Self::new(state, required, repository, max_ttl_ms)
    }

    pub fn new(
        state: Arc<AppState>,
        required: bool,
        repository: String,
        max_ttl_ms: u64,
    ) -> anyhow::Result<Arc<Self>> {
        validate_repository(&repository)?;
        if required && state.control_plane.is_none() {
            anyhow::bail!(
                "AI_AGENT_BRIDGE_REQUIRE_ASSIGNMENT_CLAIMS requires the Fiducia control plane"
            );
        }
        Ok(Arc::new(Self {
            state,
            required,
            repository: repository.trim().to_ascii_lowercase(),
            max_ttl_ms: max_ttl_ms.clamp(1, ABSOLUTE_MAX_TTL_MS),
        }))
    }
}

pub async fn enforce(
    State(policy): State<Arc<AssignmentClaimPolicy>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(workflow_id) = submission_workflow_id(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };

    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, policy.state.config.max_http_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(BridgeError::PayloadTooLarge {
                what: "workflow submission",
                limit: policy.state.config.max_http_body_bytes,
            })
        }
    };
    let submission = match serde_json::from_slice::<SubmissionEnvelope>(&bytes) {
        Ok(submission) => submission,
        Err(_) => {
            return error_response(BridgeError::BadRequest(
                "invalid workflow submission JSON".into(),
            ))
        }
    };
    let claim_value = submission.meta.get("assignment_claim").cloned();
    let Some(claim_value) = claim_value else {
        if policy.required {
            return claim_error(
                StatusCode::CONFLICT,
                "assignment_claim_required",
                "a current Fiducia assignment claim is required",
            );
        }
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    let claim = match serde_json::from_value::<AssignmentClaim>(claim_value) {
        Ok(claim) => claim,
        Err(_) => {
            return claim_error(
                StatusCode::BAD_REQUEST,
                "invalid_assignment_claim",
                "assignment_claim has an invalid shape",
            )
        }
    };

    let Some(control_plane) = policy.state.control_plane.as_ref() else {
        return error_response(BridgeError::ControlPlaneNotConfigured);
    };
    let plan = match load_plan(&policy.state, &workflow_id) {
        Ok(plan) => plan,
        Err(error) => return error_response(error),
    };
    let agent_key = submission.agent_key.trim();
    let Some(assignment) = plan
        .assignments
        .iter()
        .find(|assignment| assignment.agent_key == agent_key)
    else {
        return claim_error(
            StatusCode::BAD_REQUEST,
            "assignment_claim_agent_mismatch",
            "the submitting agent is not assigned to this workflow",
        );
    };
    let expected_path = claim_path(&workflow_id, assignment.ordinal);
    if claim.assignment_ordinal != assignment.ordinal
        || claim.repository.trim().to_ascii_lowercase() != policy.repository
        || claim.paths != [expected_path.clone()]
        || claim.owner.trim().is_empty()
        || claim.owner.trim() != format!("runner/{}", claim.instance_id.trim())
        || claim.instance_id.trim().is_empty()
        || claim.instance_id.chars().any(char::is_control)
        || claim.fencing_token == 0
        || claim.ttl_ms == 0
        || claim.ttl_ms > policy.max_ttl_ms
    {
        return claim_error(
            StatusCode::BAD_REQUEST,
            "assignment_claim_mismatch",
            "assignment claim does not match the workflow assignment or policy",
        );
    }

    let renewal = json!({
        "repository": policy.repository,
        "paths": [expected_path],
        "agent_key": claim.owner.trim(),
        "fencing_token": claim.fencing_token,
        "ttl_ms": claim.ttl_ms,
    });
    let response = match control_plane.renew(&renewal).await {
        Ok(response) => response,
        Err(error) => return error_response(error),
    };
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        return (status, Json(response.body)).into_response();
    }
    let renewed = find_bool(&response.body, "renewed").unwrap_or(false);
    let token = find_u64(&response.body, "fencing_token").unwrap_or(0);
    if !renewed || token != claim.fencing_token {
        return claim_error(
            StatusCode::CONFLICT,
            "stale_assignment_claim",
            "the assignment claim is stale or no longer owned by this runner",
        );
    }

    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

fn submission_workflow_id(method: &Method, path: &str) -> Option<String> {
    if method != Method::POST {
        return None;
    }
    let mut parts = path.trim_matches('/').split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("workflows"), Some(id), Some("submissions"), None) if valid_workflow_id(id) => {
            Some(id.to_string())
        }
        _ => None,
    }
}

fn valid_workflow_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

pub(crate) fn claim_path(workflow_id: &str, assignment_ordinal: usize) -> String {
    format!("workflows/{workflow_id}/assignments/{assignment_ordinal}")
}

fn load_plan(state: &AppState, workflow_id: &str) -> Result<WorkflowPlan, BridgeError> {
    let channel = format!("workflow-{workflow_id}");
    let entry = state
        .get_context_key(&channel, PLAN_CONTEXT_KEY)?
        .ok_or_else(|| BridgeError::BadRequest("workflow plan is missing".into()))?;
    serde_json::from_value(entry.value)
        .map_err(|_| BridgeError::BadRequest("workflow plan is invalid".into()))
}

fn find_bool(value: &Value, key: &str) -> Option<bool> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_bool)
            .or_else(|| map.values().find_map(|value| find_bool(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_bool(value, key)),
        _ => None,
    }
}

fn find_u64(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_u64)
            .or_else(|| map.values().find_map(|value| find_u64(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_u64(value, key)),
        _ => None,
    }
}

fn validate_repository(value: &str) -> anyhow::Result<()> {
    let parts = value.trim().split('/').collect::<Vec<_>>();
    let valid_part = |part: &&str| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    };
    if parts.len() != 2 || !parts.iter().all(valid_part) {
        anyhow::bail!("assignment claim repository must be canonical owner/repo");
    }
    Ok(())
}

fn env_bool(key: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(key)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(_) => anyhow::bail!("{key} must be a boolean"),
    }
}

fn error_response(error: BridgeError) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error.payload())).into_response()
}

fn claim_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(json!({
            "ok": false,
            "error": code,
            "message": message,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_route_and_claim_path_are_canonical() {
        assert_eq!(
            submission_workflow_id(&Method::POST, "/workflows/abc-123/submissions"),
            Some("abc-123".into())
        );
        assert_eq!(claim_path("abc-123", 2), "workflows/abc-123/assignments/2");
        assert_eq!(
            submission_workflow_id(&Method::GET, "/workflows/abc-123/submissions"),
            None
        );
    }
}
