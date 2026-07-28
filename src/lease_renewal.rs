//! External Fiducia file-lease renewal boundary.
//!
//! This middleware intercepts both the canonical `/file-leases/renew` route and
//! the legacy `/file-leases/{lease_id}/renew` shape when an external control plane
//! is configured. Renewal always carries the complete repository/path union and
//! current fencing token; it never falls back to local lease state.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::BridgeError;
use crate::state::AppState;

const DEFAULT_TTL_MS: u64 = 30_000;
const MAX_TTL_MS: u64 = 86_400_000;

#[derive(Debug, Deserialize)]
struct RenewFileLeaseReq {
    repository: String,
    paths: Vec<String>,
    agent_key: String,
    fencing_token: u64,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: u64,
}

const fn default_ttl_ms() -> u64 {
    DEFAULT_TTL_MS
}

pub async fn intercept(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || !is_renewal_path(request.uri().path()) {
        return next.run(request).await;
    }

    let canonical = request.uri().path() == "/file-leases/renew";
    let Some(control_plane) = state.control_plane.as_ref() else {
        if canonical {
            return error_response(BridgeError::ControlPlaneNotConfigured);
        }
        return next.run(request).await;
    };

    let (_parts, body) = request.into_parts();
    let bytes = match to_bytes(body, state.config.max_http_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(BridgeError::PayloadTooLarge {
                what: "file lease renewal request",
                limit: state.config.max_http_body_bytes,
            })
        }
    };
    let input = match serde_json::from_slice::<RenewFileLeaseReq>(&bytes) {
        Ok(input) => input,
        Err(_) => {
            return error_response(BridgeError::BadRequest(
                "invalid file lease renewal JSON".into(),
            ))
        }
    };

    let repository = input.repository.trim();
    let agent_key = input.agent_key.trim();
    if repository.is_empty() {
        return error_response(BridgeError::BadRequest("repository is required".into()));
    }
    if input.paths.is_empty() || input.paths.iter().any(|path| path.trim().is_empty()) {
        return error_response(BridgeError::BadRequest(
            "paths must contain at least one non-empty repository-relative path".into(),
        ));
    }
    if state.get_agent(agent_key).is_none() {
        return error_response(BridgeError::AgentNotFound(agent_key.to_string()));
    }
    if input.fencing_token == 0 {
        return error_response(BridgeError::BadRequest(
            "fencing_token must be non-zero".into(),
        ));
    }
    if input.ttl_ms == 0 || input.ttl_ms > MAX_TTL_MS {
        return error_response(BridgeError::BadRequest(format!(
            "ttl_ms must be between 1 and {MAX_TTL_MS}"
        )));
    }

    let body = json!({
        "repository": repository,
        "paths": input.paths,
        "agent_key": agent_key,
        "fencing_token": input.fencing_token,
        "ttl_ms": input.ttl_ms,
    });
    match control_plane.renew(&body).await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, Json(response.body)).into_response()
        }
        Err(error) => error_response(error),
    }
}

fn is_renewal_path(path: &str) -> bool {
    if path == "/file-leases/renew" {
        return true;
    }
    let mut parts = path.trim_matches('/').split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("file-leases"), Some(lease_id), Some("renew"), None) if !lease_id.is_empty()
    )
}

fn error_response(error: BridgeError) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error.payload())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_canonical_and_one_id_compatibility_routes() {
        assert!(is_renewal_path("/file-leases/renew"));
        assert!(is_renewal_path("/file-leases/lease-1/renew"));
        assert!(!is_renewal_path("/file-leases//renew"));
        assert!(!is_renewal_path("/file-leases/a/b/renew"));
        assert!(!is_renewal_path("/file-leases/release"));
    }
}
