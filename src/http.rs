//! HTTP transport: REST for request/response, SSE for the live stream. Shapes
//! mirror the TCP JSONL protocol so agents can use either interchangeably.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    middleware::{from_fn_with_state, Next},
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
        let status = StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.payload())).into_response()
    }
}

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

pub fn router(state: Arc<AppState>) -> Router {
    let public = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/", get(index))
        // Backward-compatible claude-inbox contract (see crate::compat).
        .route("/health", get(health_compat))
        .route("/claude", post(claude_inbox));

    let api = Router::new()
        .route("/agents/register", post(register_agent))
        .route("/agents", get(list_agents))
        .route("/channels", get(list_channels).post(create_channel))
        .route("/channels/search", post(search_channels))
        .route("/channels/resolve", post(resolve_channel))
        .route("/channels/{slug}", get(get_channel))
        .route("/channels/{slug}/join", post(join_channel))
        .route("/channels/{slug}/leave", post(leave_channel))
        .route("/channels/{slug}/members", get(list_members))
        .route("/channels/{slug}/messages", get(list_messages).post(post_message))
        .route("/channels/{slug}/stream", get(stream_channel))
        .route("/channels/{slug}/context", get(get_context).put(put_context).post(put_context))
        .layer(from_fn_with_state(state.clone(), auth));

    public.merge(api).with_state(state)
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

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ai-agent-bridge" }))
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
        "inbox_messages": crate::compat::inbox_count(&s.config.inbox_dir),
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
    if let Some(tok) = &s.config.inbox_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let expected = format!("Bearer {tok}");
        let ok = presented
            .map(|p| crate::config::constant_time_eq(p.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
        }
    }

    let data: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("bad json: {e}") }))).into_response()
            }
        }
    };

    let id = crate::compat::now_millis();
    let from = crate::compat::field(&data, "from", "codex", 64);
    let topic = crate::compat::field(&data, "topic", "", 128);
    let prompt = data.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let msg = json!({ "id": id, "ts": crate::compat::iso8601_secs(), "from": from, "topic": topic, "prompt": prompt });

    if let Err(e) = crate::compat::append_inbox(&s.config.inbox_dir, &msg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("inbox write failed: {e}") }))).into_response();
    }

    // Superset bonus: surface it on the chat bus too, so subscribed agents see it
    // live. Never fails the inbox contract.
    if !prompt.is_empty() {
        let slug = crate::state::slugify(&topic);
        let slug = if slug.is_empty() { "claude-inbox".to_string() } else { slug };
        let topic_text = if topic.trim().is_empty() { "claude inbox" } else { &topic };
        if s.create_or_get_channel(&slug, topic_text, &from).await.is_ok() {
            let _ = s.post_message(&slug, &from, Role::User, &prompt, json!({ "via": "claude-inbox", "id": id }));
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

// ---- channels ---------------------------------------------------------------

#[derive(Deserialize)]
struct CreateChannelReq {
    slug: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    created_by: String,
}

async fn create_channel(State(s): State<Arc<AppState>>, Json(req): Json<CreateChannelReq>) -> ApiResult {
    let ch = s.create_or_get_channel(&req.slug, &req.topic, &req.created_by).await?;
    Ok(Json(json!({ "ok": true, "channel": ch })))
}

async fn list_channels(State(s): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "channels": s.list_channels() })))
}

async fn get_channel(State(s): State<Arc<AppState>>, Path(slug): Path<String>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "channel": s.get_channel(&slug)? })))
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
    let out = s.resolve_channel(&req.query, &req.created_by, req.threshold).await?;
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
    Ok(Json(json!({ "ok": true, "messages": s.history(&slug, q.since)? })))
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
    let rx = s.subscribe(&slug, q.agent_key.as_deref())?;
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok(SseEvent::default().json_data(&event).unwrap_or_default())),
            // A lagged subscriber just skips the dropped events.
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---- shared context ---------------------------------------------------------

async fn get_context(State(s): State<Arc<AppState>>, Path(slug): Path<String>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "context": s.get_context(&slug)? })))
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
