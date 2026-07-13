//! Runtime configuration, sourced entirely from the environment so the same
//! binary runs unchanged on a laptop and in-cluster.

use std::net::{IpAddr, Ipv4Addr};

/// Default embedding width. Chosen small so the deterministic local embedder is
/// cheap. Remote embeddings must return this exact width so one channel cannot
/// introduce a mixed-dimensional embedding space.
pub const DEFAULT_EMBED_DIM: usize = 256;
/// Upper bound on the local embedding width, so a bogus `EMBED_DIM` env can't
/// force a giant per-channel allocation.
pub const MAX_EMBED_DIM: usize = 8192;
/// Default and absolute ceilings for a remote embedding HTTP response. The
/// response is streamed into a bounded buffer before JSON deserialization.
pub const DEFAULT_MAX_EMBEDDING_RESPONSE_BYTES: usize = 1_048_576;
pub const MAX_EMBEDDING_RESPONSE_BYTES: usize = 16_777_216;

/// Hard ceiling on chatroom participants. The 33rd join is bounced.
pub const MAX_MEMBERS: usize = 32;

/// How many recent messages each channel keeps hot in memory. Full history lives
/// in Postgres when persistence is enabled.
pub const DEFAULT_HISTORY_LIMIT: usize = 1000;

/// Resource ceilings (all overridable via env) that bound memory against a
/// hostile or buggy client. Channels/agents grow unbounded otherwise; messages
/// and TCP frames are read from the network.
pub const DEFAULT_MAX_CHANNELS: usize = 10_000;
pub const DEFAULT_MAX_AGENTS: usize = 50_000;
/// Max simultaneously active repository-path leases held in memory.
pub const DEFAULT_MAX_FILE_LEASES: usize = 100_000;
/// Max message/context byte length — matches the DB `content` CHECK (1 MiB).
pub const DEFAULT_MAX_CONTENT_BYTES: usize = 1_048_576;
/// Max bytes in one TCP JSONL frame before the connection is dropped (a frame
/// with no newline must not buffer without bound).
pub const DEFAULT_MAX_TCP_LINE_BYTES: usize = 2_097_152;
/// Max concurrent TCP connections (each is a task); excess are dropped.
pub const DEFAULT_MAX_TCP_CONNECTIONS: usize = 4_096;
/// Max concurrent SSE streams over HTTP (each holds a broadcast receiver + task);
/// excess are shed with `429 capacity_exceeded`, mirroring the TCP connection cap
/// so an authenticated client cannot open unbounded live streams.
pub const DEFAULT_MAX_SSE_CONNECTIONS: usize = 4_096;
/// Seconds an unauthenticated TCP connection has to present valid auth.
pub const DEFAULT_TCP_AUTH_DEADLINE_SECS: u64 = 15;
/// Seconds an authed-but-not-subscribed TCP connection may idle before it's
/// dropped (a subscribed connection may idle indefinitely — it's receiving).
pub const DEFAULT_TCP_IDLE_DEADLINE_SECS: u64 = 300;
/// Max HTTP request body bytes (also guards `POST /claude`).
pub const DEFAULT_MAX_HTTP_BODY_BYTES: usize = 2_097_152;
/// Max retained message-content bytes per channel's in-memory history ring, so a
/// hot channel is bounded by size, not just `history_limit` * message size.
pub const DEFAULT_MAX_CHANNEL_HISTORY_BYTES: usize = 8_388_608;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: IpAddr,
    pub http_port: u16,
    pub tcp_port: u16,
    /// Optional bearer token. When set, every non-health HTTP route and every
    /// TCP connection must present it.
    pub api_auth_bearer: Option<String>,
    /// Optional remote embedder endpoint (e.g. the in-cluster dd-embeddings-rs).
    /// When unset or unreachable we fall back to the deterministic local embedder.
    pub embeddings_url: Option<String>,
    pub embeddings_model: String,
    pub embeddings_bearer: Option<String>,
    pub embed_dim: usize,
    pub max_embedding_response_bytes: usize,
    /// Optional Postgres URL. Only consulted when built with `--features postgres`.
    pub database_url: Option<String>,
    /// Control-plane base URL used for file leases and active-agent lookup.
    pub control_plane_url: Option<String>,
    /// Shared secret sent only in `x-internal-auth` to the control plane.
    pub control_plane_secret: Option<String>,
    pub control_plane_timeout_secs: u64,
    pub history_limit: usize,
    /// Similarity below which `resolve` mints a brand-new topic instead of
    /// joining an existing one (the "fluid topic formation" knob).
    pub resolve_threshold: f32,
    /// Bearer token for the legacy `POST /claude` claude-inbox route. When set,
    /// that route requires `Authorization: Bearer <token>`.
    pub inbox_token: Option<String>,
    /// Directory holding `inbox.jsonl` for the legacy claude-inbox contract.
    pub inbox_dir: std::path::PathBuf,
    pub max_channels: usize,
    pub max_agents: usize,
    pub max_file_leases: usize,
    pub max_content_bytes: usize,
    pub max_tcp_line_bytes: usize,
    pub max_tcp_connections: usize,
    pub max_sse_connections: usize,
    pub max_http_body_bytes: usize,
    pub max_channel_history_bytes: usize,
    pub tcp_auth_deadline_secs: u64,
    pub tcp_idle_deadline_secs: u64,
}

/// Constant-time byte comparison for bearer tokens (avoids leaking a match
/// prefix via response timing). Length is allowed to leak, as is conventional.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn env_opt(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

fn env_usize(key: &str, default: usize) -> usize {
    env_opt(key)
        .and_then(|v| v.parse().ok())
        .filter(|d| *d > 0)
        .unwrap_or(default)
}

impl Config {
    /// Build config from the environment. Accepts both the service-specific env
    /// names and a few common aliases so it slots into existing deployments.
    pub fn from_env() -> anyhow::Result<Self> {
        let host = env_or("HOST", "0.0.0.0")
            .parse::<IpAddr>()
            .map_err(|error| anyhow::anyhow!("HOST is not a valid IP address: {error}"))?;

        let http_port = env_opt("HTTP_PORT")
            .or_else(|| env_opt("PORT"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(8142);

        let tcp_port = env_opt("TCP_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8143);

        let embed_dim = env_opt("EMBED_DIM")
            .and_then(|v| v.parse().ok())
            .filter(|d| *d > 0)
            .map(|d: usize| d.min(MAX_EMBED_DIM))
            .unwrap_or(DEFAULT_EMBED_DIM);

        let history_limit = env_opt("HISTORY_LIMIT")
            .and_then(|v| v.parse().ok())
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_HISTORY_LIMIT);

        let resolve_threshold = env_opt("RESOLVE_THRESHOLD")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|t| t.is_finite())
            .map(|t| t.clamp(0.0, 1.0))
            .unwrap_or(0.72);

        let api_auth_bearer = env_opt("API_AUTH_BEARER");
        if !host.is_loopback() && api_auth_bearer.is_none() {
            anyhow::bail!("API_AUTH_BEARER must be configured when HOST is not loopback");
        }
        let control_plane_url = env_opt("FIDUCIA_CONTROL_PLANE_URL");
        let control_plane_secret =
            env_opt("FIDUCIA_CONTROL_PLANE_SECRET").or_else(|| env_opt("FIDUCIA_INTERNAL_SECRET"));
        if control_plane_url.is_some() != control_plane_secret.is_some() {
            anyhow::bail!(
                "FIDUCIA_CONTROL_PLANE_URL and FIDUCIA_CONTROL_PLANE_SECRET must be configured together"
            );
        }

        let inbox_dir = env_opt("AI_AGENT_BRIDGE_DIR")
            .or_else(|| env_opt("CLAUDE_INBOX_DIR"))
            .map(std::path::PathBuf::from)
            .map(Ok)
            .unwrap_or_else(default_inbox_dir)?;
        if !inbox_dir.is_absolute() {
            anyhow::bail!("AI_AGENT_BRIDGE_DIR/CLAUDE_INBOX_DIR must be an absolute path");
        }

        Ok(Self {
            host,
            http_port,
            tcp_port,
            api_auth_bearer,
            embeddings_url: env_opt("EMBEDDINGS_URL"),
            embeddings_model: env_or("EMBEDDINGS_MODEL", "local-hash-v1"),
            embeddings_bearer: env_opt("EMBEDDINGS_API_AUTH_BEARER")
                .or_else(|| env_opt("EMBEDDINGS_BEARER")),
            embed_dim,
            max_embedding_response_bytes: env_usize(
                "MAX_EMBEDDING_RESPONSE_BYTES",
                DEFAULT_MAX_EMBEDDING_RESPONSE_BYTES,
            )
            .min(MAX_EMBEDDING_RESPONSE_BYTES),
            database_url: env_opt("DATABASE_URL").or_else(|| env_opt("RDS_DATABASE_URL")),
            control_plane_url,
            control_plane_secret,
            control_plane_timeout_secs: env_usize("CONTROL_PLANE_TIMEOUT_SECS", 10) as u64,
            history_limit,
            resolve_threshold,
            inbox_token: env_opt("AI_AGENT_BRIDGE_TOKEN").or_else(|| env_opt("CLAUDE_INBOX_TOKEN")),
            inbox_dir,
            max_channels: env_usize("MAX_CHANNELS", DEFAULT_MAX_CHANNELS),
            max_agents: env_usize("MAX_AGENTS", DEFAULT_MAX_AGENTS),
            max_file_leases: env_usize("MAX_FILE_LEASES", DEFAULT_MAX_FILE_LEASES),
            max_content_bytes: env_usize("MAX_CONTENT_BYTES", DEFAULT_MAX_CONTENT_BYTES),
            max_tcp_line_bytes: env_usize("MAX_TCP_LINE_BYTES", DEFAULT_MAX_TCP_LINE_BYTES),
            max_tcp_connections: env_usize("MAX_TCP_CONNECTIONS", DEFAULT_MAX_TCP_CONNECTIONS),
            max_sse_connections: env_usize("MAX_SSE_CONNECTIONS", DEFAULT_MAX_SSE_CONNECTIONS),
            max_http_body_bytes: env_usize("MAX_HTTP_BODY_BYTES", DEFAULT_MAX_HTTP_BODY_BYTES),
            max_channel_history_bytes: env_usize(
                "MAX_CHANNEL_HISTORY_BYTES",
                DEFAULT_MAX_CHANNEL_HISTORY_BYTES,
            ),
            tcp_auth_deadline_secs: env_usize(
                "TCP_AUTH_DEADLINE_SECS",
                DEFAULT_TCP_AUTH_DEADLINE_SECS as usize,
            ) as u64,
            tcp_idle_deadline_secs: env_usize(
                "TCP_IDLE_DEADLINE_SECS",
                DEFAULT_TCP_IDLE_DEADLINE_SECS as usize,
            ) as u64,
        })
    }

    /// A dependency-free config — no DB, no remote embedder, no auth, OS-assigned
    /// ports. Used by tests and one-off local runs.
    pub fn in_memory() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 0,
            tcp_port: 0,
            api_auth_bearer: None,
            embeddings_url: None,
            embeddings_model: "local-hash-v1".to_string(),
            embeddings_bearer: None,
            embed_dim: DEFAULT_EMBED_DIM,
            max_embedding_response_bytes: DEFAULT_MAX_EMBEDDING_RESPONSE_BYTES,
            database_url: None,
            control_plane_url: None,
            control_plane_secret: None,
            control_plane_timeout_secs: 10,
            history_limit: DEFAULT_HISTORY_LIMIT,
            resolve_threshold: 0.72,
            inbox_token: None,
            inbox_dir: transient_inbox_dir(),
            max_channels: DEFAULT_MAX_CHANNELS,
            max_agents: DEFAULT_MAX_AGENTS,
            max_file_leases: DEFAULT_MAX_FILE_LEASES,
            max_content_bytes: DEFAULT_MAX_CONTENT_BYTES,
            max_tcp_line_bytes: DEFAULT_MAX_TCP_LINE_BYTES,
            max_tcp_connections: DEFAULT_MAX_TCP_CONNECTIONS,
            max_sse_connections: DEFAULT_MAX_SSE_CONNECTIONS,
            max_http_body_bytes: DEFAULT_MAX_HTTP_BODY_BYTES,
            max_channel_history_bytes: DEFAULT_MAX_CHANNEL_HISTORY_BYTES,
            tcp_auth_deadline_secs: DEFAULT_TCP_AUTH_DEADLINE_SECS,
            tcp_idle_deadline_secs: DEFAULT_TCP_IDLE_DEADLINE_SECS,
        }
    }
}

fn default_inbox_dir() -> anyhow::Result<std::path::PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "set AI_AGENT_BRIDGE_DIR (or XDG_STATE_HOME/HOME) for the private inbox state directory"
            )
        })?;
    if !state_home.is_absolute() {
        anyhow::bail!("XDG_STATE_HOME/HOME must resolve to an absolute path");
    }
    Ok(state_home
        .join("fiducia-ai-agent-bridge")
        .join("claude-inbox"))
}

fn transient_inbox_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fiducia-ai-agent-bridge-test-{}-{count}",
        std::process::id()
    ))
}
