//! Runtime configuration, sourced entirely from the environment so the same
//! binary runs unchanged on a laptop and in-cluster.

use std::net::{IpAddr, Ipv4Addr};

/// Default embedding width. Chosen small so the deterministic local embedder is
/// cheap; when a remote embedder returns a different width we adopt whatever it
/// gives us (see [`crate::embed`]). Cosine similarity is dimension-agnostic as
/// long as both vectors in a comparison share a width.
pub const DEFAULT_EMBED_DIM: usize = 256;

/// Hard ceiling on chatroom participants. The 33rd join is bounced.
pub const MAX_MEMBERS: usize = 32;

/// How many recent messages each channel keeps hot in memory. Full history lives
/// in Postgres when persistence is enabled.
pub const DEFAULT_HISTORY_LIMIT: usize = 1000;

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
    /// Optional Postgres URL. Only consulted when built with `--features postgres`.
    pub database_url: Option<String>,
    pub history_limit: usize,
    /// Similarity below which `resolve` mints a brand-new topic instead of
    /// joining an existing one (the "fluid topic formation" knob).
    pub resolve_threshold: f32,
    /// Bearer token for the legacy `POST /claude` claude-inbox route. When set,
    /// that route requires `Authorization: Bearer <token>`.
    pub inbox_token: Option<String>,
    /// Directory holding `inbox.jsonl` for the legacy claude-inbox contract.
    pub inbox_dir: std::path::PathBuf,
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

impl Config {
    /// Build config from the environment. Accepts both the service-specific env
    /// names and a few common aliases so it slots into existing deployments.
    pub fn from_env() -> anyhow::Result<Self> {
        let host = env_or("HOST", "0.0.0.0")
            .parse::<IpAddr>()
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

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
            .unwrap_or(DEFAULT_EMBED_DIM);

        let history_limit = env_opt("HISTORY_LIMIT")
            .and_then(|v| v.parse().ok())
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_HISTORY_LIMIT);

        let resolve_threshold = env_opt("RESOLVE_THRESHOLD")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.72);

        Ok(Self {
            host,
            http_port,
            tcp_port,
            api_auth_bearer: env_opt("API_AUTH_BEARER"),
            embeddings_url: env_opt("EMBEDDINGS_URL"),
            embeddings_model: env_or("EMBEDDINGS_MODEL", "local-hash-v1"),
            embeddings_bearer: env_opt("EMBEDDINGS_API_AUTH_BEARER").or_else(|| env_opt("EMBEDDINGS_BEARER")),
            embed_dim,
            database_url: env_opt("DATABASE_URL").or_else(|| env_opt("RDS_DATABASE_URL")),
            history_limit,
            resolve_threshold,
            inbox_token: env_opt("AI_AGENT_BRIDGE_TOKEN").or_else(|| env_opt("CLAUDE_INBOX_TOKEN")),
            inbox_dir: std::path::PathBuf::from(
                env_opt("AI_AGENT_BRIDGE_DIR")
                    .or_else(|| env_opt("CLAUDE_INBOX_DIR"))
                    .unwrap_or_else(|| "/tmp/claude_bridge".to_string()),
            ),
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
            database_url: None,
            history_limit: DEFAULT_HISTORY_LIMIT,
            resolve_threshold: 0.72,
            inbox_token: None,
            inbox_dir: std::env::temp_dir().join("claude_bridge_test"),
        }
    }
}
