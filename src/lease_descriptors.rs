//! Durable compatibility descriptors for externally authoritative Fiducia leases.
//!
//! The legacy bridge API identifies a lease by a local `lease_id`, while Fiducia
//! renewal requires the exact canonical repository/path union. Successful external
//! compatibility acquisition therefore stores that immutable descriptor in the
//! bridge's persisted shared-context mirror. Renewal and release resolve the local
//! ID back to the authoritative union instead of guessing or reacquiring.

use std::sync::{Arc, OnceLock};

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::control_plane::ControlPlaneResponse;
use crate::error::BridgeError;
use crate::state::AppState;
use crate::types::{new_id, now_ts};

const REGISTRY_CHANNEL: &str = "internal-file-lease-descriptors";
const CONTEXT_PREFIX: &str = "internal.file-lease.v1.";
const DEFAULT_TTL_MS: u64 = 30_000;
const MIN_TTL_MS: u64 = 100;
const MAX_TTL_MS: u64 = 3_600_000;
const MAX_REPOSITORY_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 4_096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DescriptorStatus {
    Active,
    Released,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthoritativeLeaseDescriptor {
    pub version: u32,
    pub lease_id: String,
    pub repository: String,
    pub paths: Vec<String>,
    pub agent_key: String,
    pub fencing_token: u64,
    pub ttl_ms: u64,
    pub acquired_at: String,
    pub expires_at: String,
    status: DescriptorStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    released_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AcquireCompatibleReq {
    repository: String,
    path: String,
    agent_key: String,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: u64,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    meta: Value,
}

#[derive(Debug, Deserialize)]
struct MutateCompatibleReq {
    agent_key: String,
    fencing_token: u64,
    #[serde(default = "default_ttl_ms")]
    ttl_ms: u64,
}

const fn default_ttl_ms() -> u64 {
    DEFAULT_TTL_MS
}

fn mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub async fn intercept(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || state.control_plane.is_none() {
        return next.run(request).await;
    }
    let path = request.uri().path().to_string();
    let route = compatibility_route(&path);
    let Some(route) = route else {
        return next.run(request).await;
    };

    let (_parts, body) = request.into_parts();
    let bytes = match to_bytes(body, state.config.max_http_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(BridgeError::PayloadTooLarge {
                what: "file lease compatibility request",
                limit: state.config.max_http_body_bytes,
            })
        }
    };
    match route {
        CompatibilityRoute::Acquire => acquire(state, &bytes).await,
        CompatibilityRoute::Renew(lease_id) => renew(state, &lease_id, &bytes).await,
        CompatibilityRoute::Release(lease_id) => release(state, &lease_id, &bytes).await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CompatibilityRoute {
    Acquire,
    Renew(String),
    Release(String),
}

fn compatibility_route(path: &str) -> Option<CompatibilityRoute> {
    if path == "/file-leases" {
        return Some(CompatibilityRoute::Acquire);
    }
    let mut parts = path.trim_matches('/').split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("file-leases"), Some(id), Some("renew"), None) if !id.is_empty() => {
            Some(CompatibilityRoute::Renew(id.to_string()))
        }
        (Some("file-leases"), Some(id), Some("release"), None) if !id.is_empty() => {
            Some(CompatibilityRoute::Release(id.to_string()))
        }
        _ => None,
    }
}

async fn acquire(state: Arc<AppState>, bytes: &[u8]) -> Response {
    let req = match serde_json::from_slice::<AcquireCompatibleReq>(bytes) {
        Ok(req) => req,
        Err(_) => {
            return error_response(BridgeError::BadRequest(
                "invalid compatible file lease acquisition JSON".into(),
            ))
        }
    };
    if req.recursive {
        return error_response(BridgeError::BadRequest(
            "recursive leases are unavailable with the external control plane; send exact paths"
                .into(),
        ));
    }
    if state.get_agent(req.agent_key.trim()).is_none() {
        return error_response(BridgeError::AgentNotFound(
            req.agent_key.trim().to_string(),
        ));
    }
    let repository = match normalize_repository(&req.repository) {
        Ok(value) => value,
        Err(error) => return error_response(error),
    };
    let path = match normalize_path(&req.path) {
        Ok(value) => value,
        Err(error) => return error_response(error),
    };
    let ttl_ms = match normalize_ttl(req.ttl_ms) {
        Ok(value) => value,
        Err(error) => return error_response(error),
    };
    let body = json!({
        "repository": repository,
        "paths": [path],
        "agent_key": req.agent_key.trim(),
        "ttl_ms": ttl_ms,
        "wait": false,
    });
    let control_plane = state
        .control_plane
        .as_ref()
        .expect("external descriptor middleware requires control plane");
    let response = match control_plane.acquire(&body).await {
        Ok(response) => response,
        Err(error) => return error_response(error),
    };
    if response.status >= 400 {
        return control_plane_response(response);
    }

    let fencing_token = match find_u64(&response.body, "fencing_token").filter(|token| *token > 0) {
        Some(token) => token,
        None => {
            return error_response(BridgeError::ControlPlane(
                "acquire response omitted fencing_token".into(),
            ))
        }
    };
    if find_bool(&response.body, "acquired") != Some(true) {
        return error_response(BridgeError::ControlPlane(
            "control plane did not explicitly acquire the lease".into(),
        ));
    }

    let descriptor = AuthoritativeLeaseDescriptor {
        version: 1,
        lease_id: new_id(),
        repository: body["repository"].as_str().unwrap_or_default().to_string(),
        paths: vec![body["paths"][0].as_str().unwrap_or_default().to_string()],
        agent_key: req.agent_key.trim().to_string(),
        fencing_token,
        ttl_ms,
        acquired_at: now_ts(),
        expires_at: expiry_from_response(&response.body, ttl_ms),
        status: DescriptorStatus::Active,
        released_at: None,
    };
    if let Err(error) = store_descriptor(&state, &descriptor).await {
        let _ = control_plane
            .release(&json!({
                "agent_key": descriptor.agent_key,
                "fencing_token": descriptor.fencing_token,
            }))
            .await;
        return error_response(error);
    }

    let mut body = response.body;
    body["compatibility_lease"] = json!({
        "id": descriptor.lease_id,
        "repository": descriptor.repository,
        "path": descriptor.paths[0],
        "recursive": false,
        "agent_key": descriptor.agent_key,
        "purpose": req.purpose,
        "meta": req.meta,
        "fencing_token": descriptor.fencing_token,
        "acquired_at": descriptor.acquired_at,
        "expires_at": descriptor.expires_at,
    });
    control_plane_response(ControlPlaneResponse {
        status: response.status,
        body,
    })
}

async fn renew(state: Arc<AppState>, lease_id: &str, bytes: &[u8]) -> Response {
    let req = match serde_json::from_slice::<MutateCompatibleReq>(bytes) {
        Ok(req) => req,
        Err(_) => {
            return error_response(BridgeError::BadRequest(
                "invalid compatible file lease renewal JSON".into(),
            ))
        }
    };
    let ttl_ms = match normalize_ttl(req.ttl_ms) {
        Ok(value) => value,
        Err(error) => return error_response(error),
    };
    let _guard = mutation_lock().lock().await;
    let mut descriptor = match load_active_descriptor(&state, lease_id) {
        Ok(descriptor) => descriptor,
        Err(error) => return error_response(error),
    };
    if let Err(error) = validate_owner_and_fence(&descriptor, &req.agent_key, req.fencing_token) {
        return error_response(error);
    }
    let body = json!({
        "repository": descriptor.repository,
        "paths": descriptor.paths,
        "agent_key": descriptor.agent_key,
        "fencing_token": descriptor.fencing_token,
        "ttl_ms": ttl_ms,
    });
    let response = match state
        .control_plane
        .as_ref()
        .expect("external descriptor middleware requires control plane")
        .renew(&body)
        .await
    {
        Ok(response) => response,
        Err(error) => return error_response(error),
    };
    if response.status >= 400 {
        return control_plane_response(response);
    }
    let renewed = find_bool(&response.body, "renewed").unwrap_or(false);
    let returned_token = find_u64(&response.body, "fencing_token").unwrap_or(0);
    if !renewed || returned_token != descriptor.fencing_token {
        return error_response(BridgeError::StaleFencingToken(lease_id.to_string()));
    }
    descriptor.ttl_ms = ttl_ms;
    descriptor.expires_at = expiry_from_response(&response.body, ttl_ms);
    if let Err(error) = persist_descriptor(&state, &descriptor) {
        return error_response(error);
    }
    let mut response_body = response.body;
    response_body["compatibility_lease"] = descriptor_json(&descriptor);
    control_plane_response(ControlPlaneResponse {
        status: response.status,
        body: response_body,
    })
}

async fn release(state: Arc<AppState>, lease_id: &str, bytes: &[u8]) -> Response {
    let req = match serde_json::from_slice::<MutateCompatibleReq>(bytes) {
        Ok(req) => req,
        Err(_) => {
            return error_response(BridgeError::BadRequest(
                "invalid compatible file lease release JSON".into(),
            ))
        }
    };
    let _guard = mutation_lock().lock().await;
    let mut descriptor = match load_active_descriptor(&state, lease_id) {
        Ok(descriptor) => descriptor,
        Err(error) => return error_response(error),
    };
    if let Err(error) = validate_owner_and_fence(&descriptor, &req.agent_key, req.fencing_token) {
        return error_response(error);
    }
    let response = match state
        .control_plane
        .as_ref()
        .expect("external descriptor middleware requires control plane")
        .release(&json!({
            "agent_key": descriptor.agent_key,
            "fencing_token": descriptor.fencing_token,
        }))
        .await
    {
        Ok(response) => response,
        Err(error) => return error_response(error),
    };
    if response.status < 400 {
        descriptor.status = DescriptorStatus::Released;
        descriptor.released_at = Some(now_ts());
        descriptor.expires_at = now_ts();
        if let Err(error) = persist_descriptor(&state, &descriptor) {
            return error_response(error);
        }
    }
    let mut response_body = response.body;
    response_body["compatibility_lease"] = descriptor_json(&descriptor);
    control_plane_response(ControlPlaneResponse {
        status: response.status,
        body: response_body,
    })
}

async fn store_descriptor(
    state: &Arc<AppState>,
    descriptor: &AuthoritativeLeaseDescriptor,
) -> Result<(), BridgeError> {
    state
        .create_or_get_channel(
            REGISTRY_CHANNEL,
            "internal authoritative file lease descriptors",
            &descriptor.agent_key,
        )
        .await?;
    let _guard = mutation_lock().lock().await;
    let key = descriptor_key(&descriptor.lease_id);
    if state.get_context_key(REGISTRY_CHANNEL, &key)?.is_some() {
        return Err(BridgeError::BadRequest(
            "compatibility lease ID collision".into(),
        ));
    }
    persist_descriptor(state, descriptor)
}

fn persist_descriptor(
    state: &AppState,
    descriptor: &AuthoritativeLeaseDescriptor,
) -> Result<(), BridgeError> {
    state.set_context(
        REGISTRY_CHANNEL,
        &descriptor_key(&descriptor.lease_id),
        serde_json::to_value(descriptor).map_err(|_| {
            BridgeError::BadRequest("lease descriptor is not serializable".into())
        })?,
        &descriptor.agent_key,
    )?;
    Ok(())
}

fn load_active_descriptor(
    state: &AppState,
    lease_id: &str,
) -> Result<AuthoritativeLeaseDescriptor, BridgeError> {
    let entry = state
        .get_context_key(REGISTRY_CHANNEL, &descriptor_key(lease_id))
        .map_err(|_| BridgeError::FileLeaseNotFound(lease_id.to_string()))?
        .ok_or_else(|| BridgeError::FileLeaseNotFound(lease_id.to_string()))?;
    let descriptor = serde_json::from_value::<AuthoritativeLeaseDescriptor>(entry.value)
        .map_err(|_| BridgeError::FileLeaseNotFound(lease_id.to_string()))?;
    if descriptor.status != DescriptorStatus::Active || descriptor_expired(&descriptor) {
        return Err(BridgeError::FileLeaseNotFound(lease_id.to_string()));
    }
    Ok(descriptor)
}

fn validate_owner_and_fence(
    descriptor: &AuthoritativeLeaseDescriptor,
    agent_key: &str,
    fencing_token: u64,
) -> Result<(), BridgeError> {
    if descriptor.agent_key != agent_key.trim() {
        return Err(BridgeError::FileLeaseOwnerMismatch {
            lease_id: descriptor.lease_id.clone(),
            agent: agent_key.trim().to_string(),
        });
    }
    if descriptor.fencing_token != fencing_token {
        return Err(BridgeError::StaleFencingToken(descriptor.lease_id.clone()));
    }
    Ok(())
}

fn descriptor_expired(descriptor: &AuthoritativeLeaseDescriptor) -> bool {
    chrono::DateTime::parse_from_rfc3339(&descriptor.expires_at)
        .map(|expires| expires.with_timezone(&chrono::Utc) <= chrono::Utc::now())
        .unwrap_or(true)
}

fn descriptor_key(lease_id: &str) -> String {
    format!("{CONTEXT_PREFIX}{lease_id}")
}

fn descriptor_json(descriptor: &AuthoritativeLeaseDescriptor) -> Value {
    json!({
        "id": descriptor.lease_id,
        "repository": descriptor.repository,
        "paths": descriptor.paths,
        "agent_key": descriptor.agent_key,
        "fencing_token": descriptor.fencing_token,
        "ttl_ms": descriptor.ttl_ms,
        "acquired_at": descriptor.acquired_at,
        "expires_at": descriptor.expires_at,
        "status": descriptor.status,
        "released_at": descriptor.released_at,
    })
}

fn expiry_from_response(body: &Value, ttl_ms: u64) -> String {
    if let Some(value) = find_string(body, "expires_at") {
        if chrono::DateTime::parse_from_rfc3339(&value).is_ok() {
            return value;
        }
    }
    if let Some(value) = find_u64(body, "expires_at_ms") {
        if let Ok(value) = i64::try_from(value) {
            if let Some(timestamp) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value) {
                return timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            }
        }
    }
    (chrono::Utc::now() + chrono::Duration::milliseconds(ttl_ms as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn normalize_repository(value: &str) -> Result<String, BridgeError> {
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

fn normalize_path(value: &str) -> Result<String, BridgeError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BridgeError::BadRequest("path is required".into()));
    }
    if value.len() > MAX_PATH_BYTES {
        return Err(BridgeError::PayloadTooLarge {
            what: "file path",
            limit: MAX_PATH_BYTES,
        });
    }
    if value.starts_with('/') || value.contains('\\') || value.chars().any(char::is_control) {
        return Err(BridgeError::BadRequest(
            "path must be a repository-relative POSIX path".into(),
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(BridgeError::BadRequest(
                    "path must not traverse outside the repository".into(),
                ))
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(BridgeError::BadRequest("path is required".into()));
    }
    Ok(parts.join("/"))
}

fn normalize_ttl(ttl_ms: u64) -> Result<u64, BridgeError> {
    if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms) {
        return Err(BridgeError::BadRequest(format!(
            "ttl_ms must be between {MIN_TTL_MS} and {MAX_TTL_MS}"
        )));
    }
    Ok(ttl_ms)
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

fn find_string(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| map.values().find_map(|value| find_string(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_string(value, key)),
        _ => None,
    }
}

fn control_plane_response(response: ControlPlaneResponse) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(response.body)).into_response()
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
    fn matches_only_legacy_compatibility_routes() {
        assert_eq!(
            compatibility_route("/file-leases"),
            Some(CompatibilityRoute::Acquire)
        );
        assert_eq!(
            compatibility_route("/file-leases/abc/renew"),
            Some(CompatibilityRoute::Renew("abc".into()))
        );
        assert_eq!(
            compatibility_route("/file-leases/abc/release"),
            Some(CompatibilityRoute::Release("abc".into()))
        );
        assert_eq!(compatibility_route("/file-leases/renew"), None);
    }

    #[test]
    fn normalizes_paths_and_rejects_traversal() {
        assert_eq!(normalize_path("src/./lib.rs").unwrap(), "src/lib.rs");
        assert!(normalize_path("../secrets").is_err());
        assert!(normalize_path("/etc/passwd").is_err());
        assert!(normalize_path("src\\lib.rs").is_err());
    }
}
