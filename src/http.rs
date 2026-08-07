//! HTTP transport: REST for request/response, SSE for the live stream. Shapes
//! mirror the TCP JSONL protocol so agents can use either interchangeably.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode},
    middleware::{from_fn, from_fn_with_state, Next},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::error::BridgeError;
use crate::state::AppState;
use crate::types::{AgentKind, MemberRole, Role};

/// Axum wrapper so `BridgeError` can be returned with `?` from handlers.
struct ApiError(BridgeError);

impl From<BridgeError> for ApiError {
    fn from(e: BridgeError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        crate::metrics::global().observe_bridge_error(&self.0);
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.payload())).into_response()
    }
}

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.config.max_http_body_bytes;
    let public = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route(
            "/.well-known/agent-pontifex",
            get(agent_pontifex_descriptor),
        )
        .route("/metrics", get(prometheus_metrics))
        .route("/", get(index))
        // Backward-compatible claude-inbox contract (see crate::compat).
        .route("/health", get(health_compat))
        .route("/claude", post(claude_inbox));

    let api = Router::new()
        .route("/agents/register", post(register_agent))
        .route("/agents", get(list_agents))
        .route("/agents/by-file", get(get_or_list_file_lease_holders))
        .route(
            "/file-leases",
            get(get_or_list_file_lease_holders).post(acquire_compatible_file_lease),
        )
        .route("/file-leases/acquire", post(acquire_file_leases))
        .route("/file-leases/release", post(release_file_lease))
        .route("/file-leases/{lease_id}/renew", post(renew_file_lease))
        .route(
            "/file-leases/{lease_id}/release",
            post(release_compatible_file_lease),
        )
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/search", post(search_channels))
        .route("/channels/resolve", post(resolve_channel))
        .route("/channels/{slug}", get(get_channel))
        .route("/channels/{slug}/join", post(join_channel))
        .route("/channels/{slug}/leave", post(leave_channel))
        .route("/channels/{slug}/members", get(list_members))
        .route(
            "/channels/{slug}/messages",
            get(list_messages).post(post_message),
        )
        .route("/channels/{slug}/stream", get(stream_channel))
        .route(
            "/channels/{slug}/context",
            get(get_context).put(put_context).post(put_context),
        )
        .layer(from_fn_with_state(state.clone(), auth));

    public
        .merge(api)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(from_fn(request_timeout))
        // Fleet convention: per-request trace span, and catch-panic outermost so
        // a panicking handler becomes a 500 instead of a dropped connection.
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(state)
}

/// Bound how long a single request may take to produce a response, so a hung
/// handler can't pin a request forever. SSE is unaffected: its handler returns
/// the stream object immediately, and this times the handler, not the stream.
async fn request_timeout(req: axum::extract::Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "ok": false, "error": "request_timeout" })),
        )
            .into_response(),
    }
}

/// Bearer-token gate. No-op when `API_AUTH_BEARER` is unset. Health/index live
/// outside this layer so probes never need the token.
async fn auth(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.config.api_auth_bearer {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let ok = presented
            .map(|p| crate::config::constant_time_eq(p.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return ApiError(BridgeError::Unauthorized).into_response();
        }
    }
    next.run(req).await
}

fn agent_pontifex_bridge_descriptor() -> crate::agent_pontifex_protocol::ServiceDescriptor {
    use crate::agent_pontifex_protocol::{ServiceDescriptor, ServiceKind};

    let descriptor = ServiceDescriptor::new(
        ServiceKind::Bridge,
        "oresoftware.ai-agent-bridge",
        vec![
            "bridge.agents".to_string(),
            "bridge.channels".to_string(),
            "bridge.context".to_string(),
            "bridge.file-leases".to_string(),
            "bridge.messages".to_string(),
            "bridge.presence".to_string(),
            "bridge.semantic-resolution".to_string(),
            "bridge.transport.http".to_string(),
            "bridge.transport.sse".to_string(),
            "bridge.transport.tcp".to_string(),
        ],
        std::collections::BTreeMap::new(),
    );
    debug_assert!(descriptor.validate().is_ok());
    descriptor
}

async fn agent_pontifex_descriptor() -> impl IntoResponse {
    Json(agent_pontifex_bridge_descriptor())
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ai-agent-bridge" }))
}

#[cfg(test)]
mod agent_pontifex_discovery_tests {
    use super::*;
    use crate::agent_pontifex_protocol::{ProtocolVersionRange, ServiceKind, BRIDGE_PROTOCOL_ID};

    #[test]
    fn bridge_descriptor_is_deterministic_and_v1_compatible() {
        let descriptor = agent_pontifex_bridge_descriptor();
        assert_eq!(descriptor.protocol, BRIDGE_PROTOCOL_ID);
        assert_eq!(descriptor.service, ServiceKind::Bridge.service_id());
        assert_eq!(
            descriptor
                .validate_for(ServiceKind::Bridge, ProtocolVersionRange::current())
                .unwrap(),
            1
        );
        let mut sorted = descriptor.capabilities.clone();
        sorted.sort();
        assert_eq!(descriptor.capabilities, sorted);
        assert!(descriptor.extensions.is_empty());
    }
}

async fn prometheus_metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::global().render(&state),
    )
}

async fn index() -> impl IntoResponse {
    Json(json!({
        "service": "ai-agent-bridge",
        "docs": "https://github.com/ORESoftware/ai-agent-bridge.rs",
        "transports": { "http": "REST + SSE", "tcp": "newline-delimited JSON" },
        "max_members_per_channel": crate::config::MAX_MEMBERS,
    }))
}

// ---- claude-inbox backward compatibility (see crate::compat) -----------------

/// Legacy `GET /health` — same payload shape as the retired ai-agent-bridge-rs.
async fn health_compat(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": "ai-agent-bridge",
        "port": s.config.http_port,
        "inbox_messages": s.inbox_message_count(),
        "auth": "Bearer token required for POST /claude",
    }))
}

/// Legacy `POST /claude` — Bearer-protected inbox append. Appends the exact
/// `{id, ts, from, topic, prompt}` line to `inbox.jsonl`, and as a superset bonus
/// also mirrors the message onto the chat bus (best-effort).
async fn claude_inbox(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Authorized if the presented bearer matches EITHER the inbox token or the
    // global API bearer. If neither is configured, /claude is open (legacy
    // default). This closes the bypass where API_AUTH_BEARER locked the rest of
    // the API but left /claude reachable.
    let accepted: Vec<&String> = [
        s.config.inbox_token.as_ref(),
        s.config.api_auth_bearer.as_ref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !accepted.is_empty() {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        // Non-short-circuiting fold (`|`, not `any`): every candidate is compared
        // so the check doesn't leak, via timing, which token matched.
        let ok = presented
            .map(|p| {
                accepted.iter().fold(false, |acc, tok| {
                    acc | crate::config::constant_time_eq(
                        p.as_bytes(),
                        format!("Bearer {tok}").as_bytes(),
                    )
                })
            })
            .unwrap_or(false);
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized" })),
            )
                .into_response();
        }
    }

    let data: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            // Don't echo parser offsets back to the client.
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "invalid json body" })),
                )
                    .into_response()
            }
        }
    };

    let id = crate::compat::now_millis();
    let from = crate::compat::field(&data, "from", "codex", 64);
    let topic = crate::compat::field(&data, "topic", "", 128);
    let prompt = data
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Typed struct -> exact key order {id, ts, from, topic, prompt} in inbox.jsonl.
    let entry = crate::compat::InboxLine {
        id,
        ts: crate::compat::iso8601_secs(),
        from: from.clone(),
        topic: topic.clone(),
        prompt: prompt.clone(),
    };

    if let Err(e) = s.append_inbox(&entry) {
        tracing::warn!(error = %e, "inbox write failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "inbox write failed" })),
        )
            .into_response();
    }

    // Superset bonus: surface it on the chat bus too, so subscribed agents see it
    // live. Never fails the inbox contract.
    if !prompt.is_empty() {
        let slug = crate::state::slugify(&topic);
        let slug = if slug.is_empty() {
            "claude-inbox".to_string()
        } else {
            slug
        };
        let topic_text = if topic.trim().is_empty() {
            "claude inbox"
        } else {
            &topic
        };
        if s.create_or_get_channel(&slug, topic_text, &from)
            .await
            .is_ok()
        {
            if let Err(error) = s.post_message(
                &slug,
                &from,
                Role::User,
                &prompt,
                json!({ "via": "claude-inbox", "id": id }),
            ) {
                // Near-impossible after create_or_get_channel succeeded, but an
                // ingested inbox message silently vanishing must leave a trace.
                tracing::warn!(%slug, %error, "claude-inbox message failed to post to its channel");
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "queued": true,
            "id": id,
            "note": "Claude will read this on its next watcher wake and reply via the peer bridge.",
        })),
    )
        .into_response()
}

// ---- agents -----------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterReq {
    agent_key: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    kind: AgentKind,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    meta: serde_json::Value,
}

async fn register_agent(State(s): State<Arc<AppState>>, Json(req): Json<RegisterReq>) -> ApiResult {
    let agent = s.register_agent(crate::types::Agent {
        agent_key: req.agent_key,
        display_name: req.display_name,
        kind: req.kind,
        host: req.host,
        meta: req.meta,
        registered_at: crate::types::now_ts(),
    })?;
    Ok(Json(json!({ "ok": true, "agent": agent })))
}

async fn list_agents(State(s): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "agents": s.list_agents() })))
}

// ---- repository path leases -------------------------------------------------

fn default_file_lease_ttl_ms() -> u64 {
    30_000
}

#[derive(Deserialize)]
struct AcquireFileLeasesReq {
    repository: String,
    paths: Vec<String>,
    agent_key: String,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    wait: bool,
}

#[derive(Deserialize)]
struct AcquireCompatibleFileLeaseReq {
    repository: String,
    path: String,
    agent_key: String,
    #[serde(default = "default_file_lease_ttl_ms")]
    ttl_ms: u64,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Deserialize)]
struct ReleaseFileLeaseReq {
    agent_key: String,
    fencing_token: u64,
}

#[derive(Deserialize)]
struct MutateFileLeaseReq {
    agent_key: String,
    fencing_token: u64,
    #[serde(default = "default_file_lease_ttl_ms")]
    ttl_ms: u64,
}

#[derive(Deserialize, Default)]
struct FileLeaseQuery {
    repository: Option<String>,
    path: Option<String>,
    agent_key: Option<String>,
    #[serde(default)]
    include_descendants: bool,
}

async fn acquire_file_leases(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AcquireFileLeasesReq>,
) -> Response {
    if let Err(error) = require_registered_agent(&s, &req.agent_key) {
        return error.into_response();
    }
    let Some(control_plane) = &s.control_plane else {
        return ApiError(BridgeError::ControlPlaneNotConfigured).into_response();
    };
    let body = json!({
        "repository": req.repository,
        "paths": req.paths,
        "agent_key": req.agent_key.trim(),
        "ttl_ms": req.ttl_ms.unwrap_or(30_000),
        "wait": req.wait,
    });
    match control_plane.acquire(&body).await {
        Ok(response) => control_plane_response(response),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn acquire_compatible_file_lease(
    State(s): State<Arc<AppState>>,
    Json(req): Json<AcquireCompatibleFileLeaseReq>,
) -> Response {
    if let Err(error) = require_registered_agent(&s, &req.agent_key) {
        return error.into_response();
    }
    if let Some(control_plane) = &s.control_plane {
        if req.recursive {
            return ApiError(BridgeError::BadRequest(
                "recursive leases are unavailable with the external control plane; send exact paths"
                    .into(),
            ))
            .into_response();
        }
        let body = json!({
            "repository": req.repository,
            "paths": [req.path],
            "agent_key": req.agent_key.trim(),
            "ttl_ms": req.ttl_ms,
            "wait": false,
        });
        return match control_plane.acquire(&body).await {
            Ok(response) => control_plane_response(response),
            Err(error) => ApiError(error).into_response(),
        };
    }
    match s.acquire_file_lease(
        &req.repository,
        &req.path,
        &req.agent_key,
        req.ttl_ms,
        req.recursive,
        &req.purpose,
        req.meta,
    ) {
        Ok(lease) => Json(json!({ "ok": true, "lease": lease })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn renew_file_lease(
    State(s): State<Arc<AppState>>,
    Path(lease_id): Path<String>,
    Json(req): Json<MutateFileLeaseReq>,
) -> Response {
    if s.control_plane.is_some() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "ok": false,
                "error": "external_lease_renewal_not_supported",
                "message": "acquire a fresh fenced lease after the current TTL expires"
            })),
        )
            .into_response();
    }
    match s.renew_file_lease(&lease_id, &req.agent_key, req.fencing_token, req.ttl_ms) {
        Ok(lease) => Json(json!({ "ok": true, "lease": lease })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn release_file_lease(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ReleaseFileLeaseReq>,
) -> Response {
    let Some(control_plane) = &s.control_plane else {
        return ApiError(BridgeError::ControlPlaneNotConfigured).into_response();
    };
    release_through_control_plane(control_plane, &req.agent_key, req.fencing_token).await
}

async fn release_compatible_file_lease(
    State(s): State<Arc<AppState>>,
    Path(lease_id): Path<String>,
    Json(req): Json<MutateFileLeaseReq>,
) -> Response {
    if let Some(control_plane) = &s.control_plane {
        return release_through_control_plane(control_plane, &req.agent_key, req.fencing_token)
            .await;
    }
    match s.release_file_lease(&lease_id, &req.agent_key, req.fencing_token) {
        Ok(lease) => Json(json!({ "ok": true, "released": true, "lease": lease })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

async fn get_or_list_file_lease_holders(
    State(s): State<Arc<AppState>>,
    Query(query): Query<FileLeaseQuery>,
) -> Response {
    if let Some(control_plane) = &s.control_plane {
        let (Some(repository), Some(path)) = (query.repository.as_deref(), query.path.as_deref())
        else {
            return ApiError(BridgeError::BadRequest(
                "repository and path are required for control-plane lookup".into(),
            ))
            .into_response();
        };
        return match control_plane.lookup(repository, path).await {
            Ok(mut response) => {
                if response.status < 400 {
                    let holder = response.body["lock"]["holder"].as_str();
                    let agent = holder.and_then(|key| s.get_agent(key));
                    response.body["agent"] = serde_json::to_value(agent).unwrap_or_default();
                }
                control_plane_response(response)
            }
            Err(error) => ApiError(error).into_response(),
        };
    }
    match s.file_lease_holders(
        query.repository.as_deref(),
        query.path.as_deref(),
        query.agent_key.as_deref(),
        query.include_descendants,
    ) {
        Ok(holders) => Json(json!({ "ok": true, "assignments": holders })).into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

fn require_registered_agent(state: &AppState, agent_key: &str) -> Result<(), ApiError> {
    if state.get_agent(agent_key.trim()).is_some() {
        Ok(())
    } else {
        Err(ApiError(BridgeError::AgentNotFound(
            agent_key.trim().to_string(),
        )))
    }
}

async fn release_through_control_plane(
    control_plane: &crate::control_plane::ControlPlaneClient,
    agent_key: &str,
    fencing_token: u64,
) -> Response {
    let body = json!({
        "agent_key": agent_key.trim(),
        "fencing_token": fencing_token,
    });
    match control_plane.release(&body).await {
        Ok(response) => control_plane_response(response),
        Err(error) => ApiError(error).into_response(),
    }
}

fn control_plane_response(response: crate::control_plane::ControlPlaneResponse) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(response.body)).into_response()
}

// ---- channels ---------------------------------------------------------------

#[derive(Deserialize)]
struct CreateChannelReq {
    slug: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    created_by: String,
}

async fn create_channel(
    State(s): State<Arc<AppState>>,
    Json(req): Json<CreateChannelReq>,
) -> ApiResult {
    let ch = s
        .create_or_get_channel(&req.slug, &req.topic, &req.created_by)
        .await?;
    Ok(Json(json!({ "ok": true, "channel": ch })))
}

async fn list_channels(State(s): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "channels": s.list_channels() })))
}

async fn get_channel(State(s): State<Arc<AppState>>, Path(slug): Path<String>) -> ApiResult {
    Ok(Json(
        json!({ "ok": true, "channel": s.get_channel(&slug)? }),
    ))
}

#[derive(Deserialize)]
struct SearchReq {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    10
}

async fn search_channels(State(s): State<Arc<AppState>>, Json(req): Json<SearchReq>) -> ApiResult {
    let results = s.search_channels(&req.query, req.limit).await;
    Ok(Json(json!({ "ok": true, "results": results })))
}

#[derive(Deserialize)]
struct ResolveReq {
    query: String,
    #[serde(default)]
    created_by: String,
    #[serde(default)]
    threshold: Option<f32>,
}

async fn resolve_channel(State(s): State<Arc<AppState>>, Json(req): Json<ResolveReq>) -> ApiResult {
    let out = s
        .resolve_channel(&req.query, &req.created_by, req.threshold)
        .await?;
    Ok(Json(json!({
        "ok": true,
        "channel": out.channel,
        "score": out.score,
        "created": out.created,
    })))
}

// ---- membership -------------------------------------------------------------

#[derive(Deserialize)]
struct JoinReq {
    agent_key: String,
    #[serde(default)]
    role: MemberRole,
}

async fn join_channel(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Json(req): Json<JoinReq>,
) -> ApiResult {
    let out = s.join(&slug, &req.agent_key, req.role)?;
    Ok(Json(json!({
        "ok": true,
        "member": out.member,
        "channel": out.channel,
        "newly_joined": out.newly_joined,
    })))
}

#[derive(Deserialize)]
struct LeaveReq {
    agent_key: String,
}

async fn leave_channel(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Json(req): Json<LeaveReq>,
) -> ApiResult {
    let removed = s.leave(&slug, &req.agent_key)?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

async fn list_members(State(s): State<Arc<AppState>>, Path(slug): Path<String>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "members": s.members(&slug)? })))
}

// ---- messages ---------------------------------------------------------------

#[derive(Deserialize)]
struct PostReq {
    from: String,
    content: String,
    #[serde(default)]
    role: Role,
    #[serde(default)]
    meta: serde_json::Value,
}

async fn post_message(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Json(req): Json<PostReq>,
) -> ApiResult {
    let msg = s.post_message(&slug, &req.from, req.role, &req.content, req.meta)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

#[derive(Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    since: Option<u64>,
}

async fn list_messages(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> ApiResult {
    Ok(Json(
        json!({ "ok": true, "messages": s.history(&slug, q.since)? }),
    ))
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    agent_key: Option<String>,
}

async fn stream_channel(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    // Cap concurrent SSE streams the way the TCP listener caps connections, so a
    // client cannot open unbounded live streams (each holds a broadcast receiver
    // and a forwarder task). Excess streams are shed with 429 capacity_exceeded.
    // The permit is held by the stream and released when the client disconnects.
    let permit = s.sse_connections.clone().try_acquire_owned().map_err(|_| {
        BridgeError::CapacityExceeded {
            what: "sse connections",
            limit: s.config.max_sse_connections,
        }
    })?;
    // SSE streams live only (no history replay), so the high-water mark is unused.
    let (rx, _high_water) = s.subscribe(&slug, q.agent_key.as_deref())?;
    let stream = BroadcastStream::new(rx).filter_map(move |item| {
        // Keep the connection permit owned by the stream for its whole lifetime;
        // dropped (slot released) when the client disconnects and the stream ends.
        let _permit = &permit;
        async move {
            match item {
                Ok(event) => Some(Ok(SseEvent::default()
                    .json_data(&event)
                    .unwrap_or_default())),
                // Signal a lag (don't silently drop) so the client knows it missed
                // messages and can reconcile via `GET /messages?since=`.
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(SseEvent::default()
                        .json_data(json!({ "type": "lagged", "dropped": n }))
                        .unwrap_or_default()))
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---- shared context ---------------------------------------------------------

async fn get_context(State(s): State<Arc<AppState>>, Path(slug): Path<String>) -> ApiResult {
    Ok(Json(
        json!({ "ok": true, "context": s.get_context(&slug)? }),
    ))
}

#[derive(Deserialize)]
struct SetContextReq {
    key: String,
    value: serde_json::Value,
    #[serde(default)]
    updated_by: String,
}

async fn put_context(
    State(s): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Json(req): Json<SetContextReq>,
) -> ApiResult {
    let entry = s.set_context(&slug, &req.key, req.value, &req.updated_by)?;
    Ok(Json(json!({ "ok": true, "entry": entry })))
}
