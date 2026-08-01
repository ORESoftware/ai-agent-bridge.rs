//! TCP transport: newline-delimited JSON (JSONL). One request object per line,
//! one response object per line. A `subscribe` turns the socket into a live feed
//! (messages + presence) while the client may keep sending further ops on the
//! same connection — a fluid, bidirectional chat pipe.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::BridgeError;
use crate::metrics;
use crate::state::AppState;
use crate::tcp_security::TcpPrincipal;
use crate::types::{Agent, AgentKind, Event, MemberRole, Role};
use crate::workflow_security::WorkflowSecurity;

type Writer = Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>;

/// Max concurrent `subscribe` forwarders on a single TCP connection.
const MAX_SUBS_PER_CONN: usize = 64;

pub async fn serve(
    state: Arc<AppState>,
    listener: TcpListener,
    security: Arc<WorkflowSecurity>,
) -> anyhow::Result<()> {
    info!(addr = %listener.local_addr()?, "tcp listener up");
    // Bound concurrent connections; excess are dropped (load shed) rather than
    // spawning unbounded tasks.
    let conns = Arc::new(tokio::sync::Semaphore::new(
        state.config.max_tcp_connections,
    ));
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "tcp accept failed");
                continue;
            }
        };
        let permit = match conns.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                metrics::global().tcp_rejected();
                warn!(peer = %peer, "tcp connection cap reached; dropping connection");
                continue;
            }
        };
        let connection_metrics = metrics::global().tcp_connection();
        let state = state.clone();
        let security = security.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            let _connection_metrics = connection_metrics;
            if let Err(e) = handle_conn(state, socket, security).await {
                debug!(peer = %peer, error = %e, "tcp connection closed");
            }
        });
    }
}

/// Read one `\n`-delimited frame, but never buffer more than `max` bytes (a
/// client that never sends a newline must not exhaust memory). Returns `None` at
/// EOF. Trailing `\r` is stripped.
async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let (upto, newline, overflow) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
                };
            }
            match available.iter().position(|&b| b == b'\n') {
                // Check BEFORE copying so `buf` never exceeds `max`, even when a
                // whole oversized line + newline arrives in one fill_buf chunk.
                Some(pos) => {
                    if buf.len() + pos > max {
                        (pos + 1, true, true)
                    } else {
                        buf.extend_from_slice(&available[..pos]);
                        (pos + 1, true, false)
                    }
                }
                None => {
                    if buf.len() + available.len() > max {
                        (available.len(), false, true)
                    } else {
                        let n = available.len();
                        buf.extend_from_slice(available);
                        (n, false, false)
                    }
                }
            }
        };
        reader.consume(upto);
        if overflow {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tcp frame exceeds max size",
            ));
        }
        if newline {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

async fn write_line(writer: &Writer, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"serialize\"}".to_vec());
    line.push(b'\n');
    let mut w = writer.lock().await;
    w.write_all(&line).await?;
    w.flush().await
}

async fn handle_conn(
    state: Arc<AppState>,
    socket: TcpStream,
    security: Arc<WorkflowSecurity>,
) -> anyhow::Result<()> {
    let _ = socket.set_nodelay(true);
    let (read_half, write_half) = socket.into_split();
    let writer: Writer = Arc::new(Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);
    let max_line = state.config.max_tcp_line_bytes;

    // Auth handshake: when a bearer is configured, nothing but `auth`/`ping`
    // works until the client presents it.
    let mut principal = TcpPrincipal::initial(&security);
    let mut sub_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut subscribed: std::collections::HashSet<String> = std::collections::HashSet::new();

    write_line(
        &writer,
        &json!({
            "ok": true,
            "hello": "ai-agent-bridge",
            "needs_auth": !principal.authenticated(),
            "max_members": crate::config::MAX_MEMBERS,
            "auth": principal.hello_json(),
        }),
    )
    .await?;

    loop {
        // Read deadline: an unauthenticated connection must authenticate fast; an
        // authed-but-idle connection that never subscribes can't squat a slot
        // forever; a subscribed connection may idle (it receives events, not sends).
        let deadline_secs = if !principal.authenticated() {
            Some(state.config.tcp_auth_deadline_secs)
        } else if subscribed.is_empty() {
            Some(state.config.tcp_idle_deadline_secs)
        } else {
            None
        };
        let read = read_capped_line(&mut reader, max_line);
        let line = match deadline_secs {
            Some(secs) => {
                match tokio::time::timeout(std::time::Duration::from_secs(secs), read).await {
                    Ok(r) => match r? {
                        Some(l) => l,
                        None => break,
                    },
                    Err(_) => {
                        let err = if principal.authenticated() {
                            "idle_timeout"
                        } else {
                            "auth_timeout"
                        };
                        let _ = write_line(&writer, &json!({ "ok": false, "error": err })).await;
                        break;
                    }
                }
            }
            None => match read.await? {
                Some(l) => l,
                None => break,
            },
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Req = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                metrics::global().tcp_frame(false);
                write_line(
                    &writer,
                    &json!({ "ok": false, "error": "bad_request", "message": e.to_string() }),
                )
                .await?;
                continue;
            }
        };

        if let Req::Auth { token } = &req {
            match principal.authenticate(&security, token) {
                Ok(next) => {
                    metrics::global().tcp_frame(true);
                    principal = next;
                    write_line(
                        &writer,
                        &json!({ "ok": true, "op": "auth", "auth": principal.hello_json() }),
                    )
                    .await?;
                }
                Err(error) => {
                    metrics::global().tcp_frame(false);
                    write_line(&writer, &error.payload()).await?;
                }
            }
            continue;
        }
        if matches!(req, Req::Ping) {
            metrics::global().tcp_frame(true);
            write_line(&writer, &json!({ "ok": true, "op": "ping", "pong": true })).await?;
            continue;
        }
        if !principal.authenticated() {
            metrics::global().tcp_frame(false);
            write_line(&writer, &BridgeError::Unauthorized.payload_json()).await?;
            continue;
        }

        if let Err(error) = principal.authorize(&req) {
            metrics::global().tcp_frame(false);
            write_line(&writer, &error.payload()).await?;
            continue;
        }

        let response = dispatch(&state, &writer, req, &mut sub_tasks, &mut subscribed).await;
        if let Some(resp) = response {
            metrics::global().tcp_frame(resp.get("ok").and_then(Value::as_bool).unwrap_or(false));
            write_line(&writer, &resp).await?;
        }
    }

    for t in sub_tasks {
        t.abort();
    }
    Ok(())
}

/// Returns `Some(response)` to write, or `None` when the op already handled its
/// own output (subscribe).
async fn dispatch(
    state: &Arc<AppState>,
    writer: &Writer,
    req: Req,
    sub_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    subscribed: &mut std::collections::HashSet<String>,
) -> Option<Value> {
    match req {
        Req::Auth { .. } | Req::Ping => None, // handled upstream
        Req::Register {
            agent_key,
            display_name,
            kind,
            host,
            meta,
        } => Some(reply(
            state
                .register_agent(Agent {
                    agent_key,
                    display_name,
                    kind,
                    host,
                    meta,
                    registered_at: crate::types::now_ts(),
                })
                .map(|a| json!({ "agent": a })),
        )),
        Req::ListChannels => Some(json!({ "ok": true, "channels": state.list_channels() })),
        Req::CreateChannel {
            slug,
            topic,
            created_by,
        } => Some(reply(
            state
                .create_or_get_channel(&slug, &topic, &created_by)
                .await
                .map(|c| json!({ "channel": c })),
        )),
        Req::Resolve {
            query,
            created_by,
            threshold,
        } => Some(reply(
            state
                .resolve_channel(&query, &created_by, threshold)
                .await
                .map(|o| json!({ "channel": o.channel, "score": o.score, "created": o.created })),
        )),
        Req::Search { query, limit } => {
            let results = state.search_channels(&query, limit).await;
            Some(json!({ "ok": true, "results": results }))
        }
        Req::Join {
            channel,
            agent_key,
            role,
        } => Some(reply(state.join(&channel, &agent_key, role).map(
            |o| json!({ "member": o.member, "channel": o.channel, "newly_joined": o.newly_joined }),
        ))),
        Req::Leave { channel, agent_key } => Some(reply(
            state
                .leave(&channel, &agent_key)
                .map(|removed| json!({ "removed": removed })),
        )),
        Req::Members { channel } => Some(reply(
            state.members(&channel).map(|m| json!({ "members": m })),
        )),
        Req::Post {
            channel,
            from,
            content,
            role,
            meta,
        } => Some(reply(
            state
                .post_message(&channel, &from, role, &content, meta)
                .map(|m| json!({ "message": m })),
        )),
        Req::History { channel, since } => Some(reply(
            state
                .history(&channel, since)
                .map(|m| json!({ "messages": m })),
        )),
        Req::GetContext { channel, key } => {
            let result = match key {
                Some(k) => state
                    .get_context_key(&channel, &k)
                    .map(|e| json!({ "entry": e })),
                None => state.get_context(&channel).map(|c| json!({ "context": c })),
            };
            Some(reply(result))
        }
        Req::SetContext {
            channel,
            key,
            value,
            updated_by,
        } => Some(reply(
            state
                .set_context(&channel, &key, value, &updated_by)
                .map(|e| json!({ "entry": e })),
        )),
        Req::Subscribe {
            channel,
            agent_key,
            since,
        } => {
            // Drop finished forwarders, then cap live subscriptions per connection
            // so a client cannot spawn unbounded tasks with repeated `subscribe`s.
            sub_tasks.retain(|t| !t.is_finished());
            // One live stream per channel per connection: a second subscribe to the
            // same channel would deliver every event twice (or 64x at the cap).
            if subscribed.contains(&channel) {
                return Some(
                    json!({ "ok": false, "error": "already_subscribed", "channel": channel }),
                );
            }
            if sub_tasks.len() >= MAX_SUBS_PER_CONN {
                return Some(json!({
                    "ok": false,
                    "error": "too_many_subscriptions",
                    "limit": MAX_SUBS_PER_CONN
                }));
            }
            match state.subscribe(&channel, agent_key.as_deref()) {
                Ok((mut rx, high_water)) => {
                    // Replay history only up to the subscribe high-water; the live
                    // receiver yields strictly newer messages, so no message is
                    // delivered twice (replay + live).
                    if let Ok(history) = state.history(&channel, since) {
                        for m in history {
                            if m.seq <= high_water {
                                let _ = write_line(writer, &event_json(&Event::Message(m))).await;
                            }
                        }
                    }
                    subscribed.insert(channel.clone());
                    let ch_name = channel.clone();
                    let _ = write_line(writer, &json!({ "ok": true, "subscribed": channel })).await;
                    let writer = writer.clone();
                    let task = tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(event) => {
                                    if write_line(&writer, &event_json(&event)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    // Fell behind the broadcast ring: `n` messages were
                                    // dropped. Signal it (don't silently gap) so the client
                                    // can reconcile via `history` with `since`.
                                    let notice = json!({ "type": "lagged", "channel": ch_name, "dropped": n });
                                    if write_line(&writer, &notice).await.is_err() {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                    sub_tasks.push(task);
                    None
                }
                Err(e) => Some(e.payload_json()),
            }
        }
    }
}

fn reply(result: Result<Value, BridgeError>) -> Value {
    match result {
        Ok(mut v) => {
            if let Value::Object(map) = &mut v {
                map.insert("ok".to_string(), Value::Bool(true));
            }
            v
        }
        Err(e) => e.payload_json(),
    }
}

fn event_json(event: &Event) -> Value {
    serde_json::to_value(event).unwrap_or_else(|_| json!({ "type": "error" }))
}

impl BridgeError {
    /// Same structured shape as HTTP, plus a `warning` alias for the full-room
    /// case so streaming clients can surface a human-readable bounce notice.
    pub fn payload_json(&self) -> Value {
        let mut v = serde_json::to_value(self.payload()).unwrap_or_else(|_| json!({ "ok": false }));
        if let BridgeError::ChannelFull { limit, .. } = self {
            if let Value::Object(map) = &mut v {
                map.insert("warning".to_string(), json!("channel_full"));
                map.insert("limit".to_string(), json!(limit));
            }
        }
        v
    }
}

fn default_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Req {
    Auth {
        token: String,
    },
    Ping,
    Register {
        agent_key: String,
        #[serde(default)]
        display_name: String,
        #[serde(default)]
        kind: AgentKind,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        meta: Value,
    },
    ListChannels,
    CreateChannel {
        slug: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        created_by: String,
    },
    Resolve {
        query: String,
        #[serde(default)]
        created_by: String,
        #[serde(default)]
        threshold: Option<f32>,
    },
    Search {
        query: String,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Join {
        channel: String,
        agent_key: String,
        #[serde(default)]
        role: MemberRole,
    },
    Leave {
        channel: String,
        agent_key: String,
    },
    Members {
        channel: String,
    },
    Post {
        channel: String,
        from: String,
        content: String,
        #[serde(default)]
        role: Role,
        #[serde(default)]
        meta: Value,
    },
    History {
        channel: String,
        #[serde(default)]
        since: Option<u64>,
    },
    Subscribe {
        channel: String,
        #[serde(default)]
        agent_key: Option<String>,
        #[serde(default)]
        since: Option<u64>,
    },
    GetContext {
        channel: String,
        #[serde(default)]
        key: Option<String>,
    },
    SetContext {
        channel: String,
        key: String,
        value: Value,
        #[serde(default)]
        updated_by: String,
    },
}
