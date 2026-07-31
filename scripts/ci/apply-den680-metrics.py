#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:100]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# Public module.
replace_once(
    "src/lib.rs",
    "pub mod lease_renewal;\npub mod orchestration;",
    "pub mod lease_renewal;\npub mod metrics;\npub mod orchestration;",
)

# State snapshot and message outcomes.
replace_once(
    "src/state.rs",
    "use crate::error::{BridgeError, BridgeResult};\nuse crate::types::*;",
    "use crate::error::{BridgeError, BridgeResult};\nuse crate::metrics::StateMetricsSnapshot;\nuse crate::types::*;",
)

snapshot = '''    pub fn metrics_snapshot(&self) -> StateMetricsSnapshot {
        let now = Instant::now();
        let agents = self.agents.read().len() as u64;
        let (
            channels,
            members,
            retained_messages,
            total_messages,
            context_keys,
            broadcast_queue_depth,
        ) = {
            let channels = self.channels.read();
            (
                channels.len() as u64,
                channels
                    .values()
                    .map(|channel| channel.members.len() as u64)
                    .sum(),
                channels
                    .values()
                    .map(|channel| channel.messages.len() as u64)
                    .sum(),
                channels.values().map(|channel| channel.message_count).sum(),
                channels
                    .values()
                    .map(|channel| channel.context.len() as u64)
                    .sum(),
                channels
                    .values()
                    .map(|channel| channel.tx.len() as u64)
                    .sum(),
            )
        };
        let active_file_leases = self
            .file_leases
            .read()
            .values()
            .filter(|lease| lease.expires_at > now)
            .count() as u64;
        let active_sse_connections = self
            .config
            .max_sse_connections
            .saturating_sub(self.sse_connections.available_permits())
            as u64;

        #[cfg(feature = "postgres")]
        let (persistence_mode, persistence_ready, persistence_queue_depth, persistence_shed_writes) =
            if self.db.is_some() {
                (
                    "postgres",
                    true,
                    PERSIST_CONCURRENCY
                        .saturating_sub(self.persist_sem.available_permits())
                        as u64,
                    self.shed_persist_writes.load(Ordering::Relaxed),
                )
            } else {
                ("memory", true, 0, 0)
            };
        #[cfg(not(feature = "postgres"))]
        let (persistence_mode, persistence_ready, persistence_queue_depth, persistence_shed_writes) =
            ("memory", true, 0, 0);

        StateMetricsSnapshot {
            agents,
            channels,
            members,
            retained_messages,
            total_messages,
            context_keys,
            broadcast_queue_depth,
            active_file_leases,
            inbox_messages: self.inbox_message_count(),
            active_sse_connections,
            max_agents: self.config.max_agents as u64,
            max_channels: self.config.max_channels as u64,
            max_file_leases: self.config.max_file_leases as u64,
            max_sse_connections: self.config.max_sse_connections as u64,
            max_tcp_connections: self.config.max_tcp_connections as u64,
            persistence_mode,
            persistence_ready,
            persistence_queue_depth,
            persistence_shed_writes,
            control_plane_configured: self.control_plane.is_some(),
        }
    }

'''
replace_once(
    "src/state.rs",
    '''    pub fn inbox_message_count(&self) -> u64 {
        self.inbox_count.load(Ordering::Relaxed)
    }

    #[cfg(feature = "postgres")]
''',
    '''    pub fn inbox_message_count(&self) -> u64 {
        self.inbox_count.load(Ordering::Relaxed)
    }

''' + snapshot + '''    #[cfg(feature = "postgres")]
''',
)

state = Path("src/state.rs")
text = state.read_text(encoding="utf-8")
post_start = text.index("    pub fn post_message(")
from_marker = "        let from = from.trim();"
from_index = text.index(from_marker, post_start)
text = text[:from_index] + "        let started = Instant::now();\n" + text[from_index:]
block_start = text.index("        let message = {", post_start)
block_end_marker = "        self.persist_message(&message);\n        Ok(message)\n"
block_end = text.index(block_end_marker, block_start) + len(block_end_marker)
block = text[block_start:block_end]
block = block.replace("        let message = {", "        let (message, evicted, receivers) = {", 1)
block = block.replace(
    "            // Evict oldest by count AND by total retained bytes (always keep >= 1),",
    "            let mut evicted = 0_u64;\n            // Evict oldest by count AND by total retained bytes (always keep >= 1),",
    1,
)
block = block.replace(
    '''                if let Some(old) = ch.messages.pop_front() {
                    ch.history_bytes = ch
                        .history_bytes
                        .saturating_sub(message_retained_bytes(&old));
                }
''',
    '''                if let Some(old) = ch.messages.pop_front() {
                    ch.history_bytes = ch
                        .history_bytes
                        .saturating_sub(message_retained_bytes(&old));
                    evicted = evicted.saturating_add(1);
                }
''',
    1,
)
block = block.replace(
    '''            let _ = ch.tx.send(Event::Message(message.clone()));
            message
        };
        self.persist_message(&message);
        Ok(message)
''',
    '''            let receivers = ch.tx.send(Event::Message(message.clone())).unwrap_or(0);
            (message, evicted, receivers)
        };
        self.persist_message(&message);
        crate::metrics::global().observe_message(started.elapsed(), receivers, evicted);
        Ok(message)
''',
    1,
)
if "observe_message(started.elapsed()" not in block:
    raise SystemExit("state message metrics replacement failed")
text = text[:block_start] + block + text[block_end:]
state.write_text(text, encoding="utf-8")

# Public /metrics route and lease-error accounting.
replace_once(
    "src/http.rs",
    "    http::StatusCode,",
    "    http::{header, StatusCode},",
)
replace_once(
    "src/http.rs",
    '''    fn into_response(self) -> Response {
        let status =
''',
    '''    fn into_response(self) -> Response {
        crate::metrics::global().observe_bridge_error(&self.0);
        let status =
''',
)
replace_once(
    "src/http.rs",
    '''        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/", get(index))
''',
    '''        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/metrics", get(prometheus_metrics))
        .route("/", get(index))
''',
)
replace_once(
    "src/http.rs",
    '''async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ai-agent-bridge" }))
}

async fn index() -> impl IntoResponse {
''',
    '''async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ai-agent-bridge" }))
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
''',
)

# HTTP request timing and overload accounting.
replace_once(
    "src/main.rs",
    '''    assignment_claims, blind_competition, http, lease_descriptors, lease_renewal, orchestration,
    policy, policy_admission, tcp, workflow_security,
''',
    '''    assignment_claims, blind_competition, http, lease_descriptors, lease_renewal, metrics,
    orchestration, policy, policy_admission, tcp, workflow_security,
''',
)
replace_once(
    "src/main.rs",
    '''        .layer(axum::middleware::from_fn_with_state(
            http_admission,
            enforce_http_admission,
        ));
''',
    '''        .layer(axum::middleware::from_fn_with_state(
            http_admission,
            enforce_http_admission,
        ))
        .layer(axum::middleware::from_fn(observe_http_metrics));
''',
)
replace_once(
    "src/main.rs",
    '''/// Shed excess ordinary HTTP work instead of allowing an authenticated client to
/// queue an unbounded number of request futures. Probe and index routes bypass the
''',
    '''async fn observe_http_metrics(req: axum::extract::Request, next: Next) -> Response {
    let started = metrics::global().http_started();
    let response = next.run(req).await;
    metrics::global().http_finished(started, response.status().as_u16());
    response
}

/// Shed excess ordinary HTTP work instead of allowing an authenticated client to
/// queue an unbounded number of request futures. Probe and index routes bypass the
''',
)
replace_once(
    "src/main.rs",
    '''    if matches!(req.uri().path(), "/" | "/health" | "/healthz" | "/readyz") {
''',
    '''    if matches!(
        req.uri().path(),
        "/" | "/health" | "/healthz" | "/readyz" | "/metrics"
    ) {
''',
)
replace_once(
    "src/main.rs",
    '''    let Ok(permit) = admission.try_acquire_owned() else {
        return (
''',
    '''    let Ok(permit) = admission.try_acquire_owned() else {
        metrics::global().http_capacity_rejected();
        return (
''',
)

# TCP admission, active connections, and bounded frame outcomes.
replace_once(
    "src/tcp.rs",
    "use crate::error::BridgeError;\nuse crate::state::AppState;",
    "use crate::error::BridgeError;\nuse crate::metrics;\nuse crate::state::AppState;",
)
replace_once(
    "src/tcp.rs",
    '''            Err(_) => {
                warn!(peer = %peer, "tcp connection cap reached; dropping connection");
                continue;
            }
        };
        let state = state.clone();
''',
    '''            Err(_) => {
                metrics::global().tcp_rejected();
                warn!(peer = %peer, "tcp connection cap reached; dropping connection");
                continue;
            }
        };
        let connection_metrics = metrics::global().tcp_connection();
        let state = state.clone();
''',
)
replace_once(
    "src/tcp.rs",
    '''        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            if let Err(e) = handle_conn(state, socket, security).await {
''',
    '''        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            let _connection_metrics = connection_metrics;
            if let Err(e) = handle_conn(state, socket, security).await {
''',
)
replace_once(
    "src/tcp.rs",
    '''            Err(e) => {
                write_line(
                    &writer,
                    &json!({ "ok": false, "error": "bad_request", "message": e.to_string() }),
                )
                .await?;
                continue;
            }
''',
    '''            Err(e) => {
                metrics::global().tcp_frame(false);
                write_line(
                    &writer,
                    &json!({ "ok": false, "error": "bad_request", "message": e.to_string() }),
                )
                .await?;
                continue;
            }
''',
)
replace_once(
    "src/tcp.rs",
    '''                Ok(next) => {
                    principal = next;
                    write_line(
''',
    '''                Ok(next) => {
                    metrics::global().tcp_frame(true);
                    principal = next;
                    write_line(
''',
)
replace_once(
    "src/tcp.rs",
    '''                Err(error) => write_line(&writer, &error.payload()).await?,
''',
    '''                Err(error) => {
                    metrics::global().tcp_frame(false);
                    write_line(&writer, &error.payload()).await?;
                }
''',
)
replace_once(
    "src/tcp.rs",
    '''        if matches!(req, Req::Ping) {
            write_line(&writer, &json!({ "ok": true, "op": "ping", "pong": true })).await?;
''',
    '''        if matches!(req, Req::Ping) {
            metrics::global().tcp_frame(true);
            write_line(&writer, &json!({ "ok": true, "op": "ping", "pong": true })).await?;
''',
)
replace_once(
    "src/tcp.rs",
    '''        if !principal.authenticated() {
            write_line(&writer, &BridgeError::Unauthorized.payload_json()).await?;
''',
    '''        if !principal.authenticated() {
            metrics::global().tcp_frame(false);
            write_line(&writer, &BridgeError::Unauthorized.payload_json()).await?;
''',
)
replace_once(
    "src/tcp.rs",
    '''        if let Err(error) = principal.authorize(&req) {
            write_line(&writer, &error.payload()).await?;
''',
    '''        if let Err(error) = principal.authorize(&req) {
            metrics::global().tcp_frame(false);
            write_line(&writer, &error.payload()).await?;
''',
)
replace_once(
    "src/tcp.rs",
    '''        if let Some(resp) = response {
            write_line(&writer, &resp).await?;
        }
''',
    '''        if let Some(resp) = response {
            metrics::global().tcp_frame(
                resp.get("ok").and_then(Value::as_bool).unwrap_or(false),
            );
            write_line(&writer, &resp).await?;
        }
''',
)

# Control-plane health without URL/repository labels or response bodies.
replace_once(
    "src/control_plane.rs",
    "use std::time::Duration;",
    "use std::time::Duration;",
)
replace_once(
    "src/control_plane.rs",
    '''    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        query: &[(&str, &str)],
    ) -> BridgeResult<ControlPlaneResponse> {
        let mut request = self
''',
    '''    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        query: &[(&str, &str)],
    ) -> BridgeResult<ControlPlaneResponse> {
        let started = crate::metrics::global().control_plane_started();
        let mut request = self
''',
)
replace_once(
    "src/control_plane.rs",
    '''        let response = request.send().await.map_err(|error| {
            let detail = if error.is_timeout() {
                "request timed out"
            } else if error.is_connect() {
                "connection failed"
            } else {
                "request failed"
            };
            BridgeError::ControlPlane(detail.to_string())
        })?;
''',
    '''        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                crate::metrics::global().control_plane_finished(
                    started,
                    crate::metrics::ControlPlaneResult::TransportError,
                );
                let detail = if error.is_timeout() {
                    "request timed out"
                } else if error.is_connect() {
                    "connection failed"
                } else {
                    "request failed"
                };
                return Err(BridgeError::ControlPlane(detail.to_string()));
            }
        };
''',
)
replace_once(
    "src/control_plane.rs",
    '''        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(BridgeError::ControlPlane(
                "response exceeded 1 MiB".to_string(),
            ));
        }
''',
    '''        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            crate::metrics::global().control_plane_finished(
                started,
                crate::metrics::ControlPlaneResult::TransportError,
            );
            return Err(BridgeError::ControlPlane(
                "response exceeded 1 MiB".to_string(),
            ));
        }
''',
)
replace_once(
    "src/control_plane.rs",
    '''            let chunk =
                chunk.map_err(|_| BridgeError::ControlPlane("response read failed".to_string()))?;
''',
    '''            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    crate::metrics::global().control_plane_finished(
                        started,
                        crate::metrics::ControlPlaneResult::TransportError,
                    );
                    return Err(BridgeError::ControlPlane("response read failed".to_string()));
                }
            };
''',
)
replace_once(
    "src/control_plane.rs",
    '''            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(BridgeError::ControlPlane(
                    "response exceeded 1 MiB".to_string(),
                ));
            }
''',
    '''            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                crate::metrics::global().control_plane_finished(
                    started,
                    crate::metrics::ControlPlaneResult::TransportError,
                );
                return Err(BridgeError::ControlPlane(
                    "response exceeded 1 MiB".to_string(),
                ));
            }
''',
)
replace_once(
    "src/control_plane.rs",
    '''        Ok(ControlPlaneResponse { status, body })
''',
    '''        let result = match status {
            200..=399 => crate::metrics::ControlPlaneResult::Success,
            400..=499 => crate::metrics::ControlPlaneResult::ClientError,
            _ => crate::metrics::ControlPlaneResult::ServerError,
        };
        crate::metrics::global().control_plane_finished(started, result);
        Ok(ControlPlaneResponse { status, body })
''',
)

for path in [
    "src/lib.rs",
    "src/state.rs",
    "src/http.rs",
    "src/main.rs",
    "src/tcp.rs",
    "src/control_plane.rs",
]:
    if "\r" in Path(path).read_text(encoding="utf-8"):
        raise SystemExit(f"{path}: unexpected CR characters")
