//! Wire types shared by the HTTP and TCP transports. Everything an agent sends
//! or receives is one of these serde shapes, so the two transports stay in lockstep.

use serde::{Deserialize, Serialize};

/// ISO-8601 / RFC-3339 UTC timestamp, e.g. `2026-07-08T12:34:56.789Z`.
pub type Timestamp = String;

pub fn now_ts() -> Timestamp {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Human,
    #[default]
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub agent_key: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub kind: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
    #[serde(default = "now_ts")]
    pub registered_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    #[default]
    Member,
    /// May read the stream but is not counted as an active chatter in presence
    /// (still counts against the 32 cap — a seat is a seat).
    Observer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub agent_key: String,
    #[serde(default)]
    pub role: MemberRole,
    pub joined_at: Timestamp,
    pub last_seen_at: Timestamp,
}

/// A single chat message in a channel. `seq` is a per-channel monotonic counter
/// (1-based) so clients can page and dedupe deterministically.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel: String,
    pub seq: u64,
    /// `agent_key` of the sender.
    pub from: String,
    #[serde(default)]
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub meta: serde_json::Value,
    pub created_at: Timestamp,
}

/// Public view of a channel (embedding vector intentionally omitted).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub slug: String,
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_summary: Option<String>,
    pub created_by: String,
    pub created_at: Timestamp,
    pub member_count: usize,
    pub message_count: u64,
    pub embedding_model: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// A channel returned by semantic search, with its cosine score against the query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoredChannel {
    #[serde(flatten)]
    pub channel: Channel,
    pub score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextEntry {
    pub key: String,
    pub value: serde_json::Value,
    pub version: u32,
    pub updated_by: String,
    pub updated_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresenceKind {
    Joined,
    Left,
}

/// Everything that fans out to subscribers (SSE + TCP). Tagged so a client can
/// switch on `type`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Message(Message),
    Presence {
        channel: String,
        agent_key: String,
        event: PresenceKind,
        member_count: usize,
        at: Timestamp,
    },
}
