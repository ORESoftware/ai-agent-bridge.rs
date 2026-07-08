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
use crate::state::AppState;
use crate::types::{Agent, AgentKind, Event, MemberRole, Role};

type Writer = Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>;

pub async fn serve(state: Arc<AppState>, listener: TcpListener) -> anyhow::Result<()> {
    info!(addr = %listener.local_addr()?, "tcp listener up");
    // Bound concurrent connections; excess are dropped (load shed) rather than
    // spawning unbounded tasks.
    let conns = Arc::new(tokio::sync::Semaphore::new(state.config.max_tcp_connections));
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
                warn!(peer = %peer, "tcp connection cap reached; dropping connection");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the connection ends
            if let Err(e) = handle_conn(state, socket).await {
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
        let (upto, newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
                };
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    buf.extend_from_slice(&available[..pos]);
                    (pos + 1, true)
                }
                None => {
                    buf.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        reader.consume(upto);
        if newline {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tcp frame exceeds max size",
            ));
        }
    }
}

async fn write_line(writer: &Writer, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"serialize\"}".to_vec());
    line.push(b'\n');
    let mut w = writer.lock().await;
    w.write_all(&line).await?;
    w.flush().await
}

async fn handle_conn(state: Arc<AppState>, socket: TcpStream) -> anyhow::Result<()> {
    let _ = socket.set_nodelay(true);
    let (read_half, write_half) = socket.into_split();
    let writer: Writer = Arc::new(Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);
    let max_line = state.config.max_tcp_line_bytes;

    // Auth handshake: when a bearer is configured, nothing but `auth`/`ping`
    // works until the client presents it.
    let mut authed = state.config.api_auth_bearer.is_none();
    let mut sub_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    write_line(
        &writer,
        &json!({ "ok": true, "hello": "ai-agent-bridge", "needs_auth": !authed, "max_members": crate::config::MAX_MEMBERS }),
    )
    .await?;

    while let Some(line) = read_capped_line(&mut reader, max_line).await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Req = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                write_line(&writer, &json!({ "ok": false, "error": "bad_request", "message": e.to_string() })).await?;
                continue;
            }
        };

        if let Req::Auth { token } = &req {
            authed = match state.config.api_auth_bearer.as_deref() {
                Some(expected) => crate::config::constant_time_eq(expected.as_bytes(), token.as_bytes()),
                None => true,
            };
            write_line(&writer, &json!({ "ok": authed, "op": "auth" })).await?;
            if !authed {
                write_line(&writer, &json!({ "ok": false, "error": "unauthorized" })).await?;
            }
            continue;
        }
        if matches!(req, Req::Ping) {
            write_line(&writer, &json!({ "ok": true, "op": "ping", "pong": true })).await?;
            continue;
        }
        if !authed {
            write_line(&writer, &BridgeError::Unauthorized.payload_json()).await?;
            continue;
        }

        let response = dispatch(&state, &writer, req, &mut sub_tasks).await;
        if let Some(resp) = response {
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
) -> Option<Value> {
    match req {
        Req::Auth { .. } | Req::Ping => None, // handled upstream
        Req::Register { agent_key, display_name, kind, host, meta } => {
            Some(reply(state.register_agent(Agent {
                agent_key,
                display_name,
                kind,
                host,
                meta,
                registered_at: crate::types::now_ts(),
            }).map(|a| json!({ "agent": a }))))
        }
        Req::ListChannels => Some(json!({ "ok": true, "channels": state.list_channels() })),
        Req::CreateChannel { slug, topic, created_by } => Some(reply(
            state.create_or_get_channel(&slug, &topic, &created_by).await.map(|c| json!({ "channel": c })),
        )),
        Req::Resolve { query, created_by, threshold } => Some(reply(
            state
                .resolve_channel(&query, &created_by, threshold)
                .await
                .map(|o| json!({ "channel": o.channel, "score": o.score, "created": o.created })),
        )),
        Req::Search { query, limit } => {
            let results = state.search_channels(&query, limit).await;
            Some(json!({ "ok": true, "results": results }))
        }
        Req::Join { channel, agent_key, role } => Some(reply(
            state
                .join(&channel, &agent_key, role)
                .map(|o| json!({ "member": o.member, "channel": o.channel, "newly_joined": o.newly_joined })),
        )),
        Req::Leave { channel, agent_key } => {
            Some(reply(state.leave(&channel, &agent_key).map(|removed| json!({ "removed": removed }))))
        }
        Req::Members { channel } => Some(reply(state.members(&channel).map(|m| json!({ "members": m })))),
        Req::Post { channel, from, content, role, meta } => Some(reply(
            state.post_message(&channel, &from, role, &content, meta).map(|m| json!({ "message": m })),
        )),
        Req::History { channel, since } => {
            Some(reply(state.history(&channel, since).map(|m| json!({ "messages": m }))))
        }
        Req::GetContext { channel, key } => {
            let result = match key {
                Some(k) => state.get_context_key(&channel, &k).map(|e| json!({ "entry": e })),
                None => state.get_context(&channel).map(|c| json!({ "context": c })),
            };
            Some(reply(result))
        }
        Req::SetContext { channel, key, value, updated_by } => {
            Some(reply(state.set_context(&channel, &key, value, &updated_by).map(|e| json!({ "entry": e }))))
        }
        Req::Subscribe { channel, agent_key, since } => {
            match state.subscribe(&channel, agent_key.as_deref()) {
                Ok(mut rx) => {
                    // Replay recent history first so a late joiner has context.
                    if let Ok(history) = state.history(&channel, since) {
                        for m in history {
                            let _ = write_line(writer, &event_json(&Event::Message(m))).await;
                        }
                    }
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
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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
enum Req {
    Auth { token: String },
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
