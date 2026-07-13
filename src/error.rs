//! Domain errors, with the HTTP status + machine-readable code each maps to.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("channel '{0}' not found")]
    ChannelNotFound(String),

    #[error("channel '{slug}' is full ({current}/{limit}) — the {next} participant was bounced")]
    ChannelFull {
        slug: String,
        current: usize,
        limit: usize,
        /// Ordinal of the bounced participant, e.g. 33 for a 32-cap room.
        next: usize,
    },

    #[error("agent '{agent}' is not a member of channel '{slug}'")]
    NotAMember { agent: String, slug: String },

    #[error("agent '{0}' is not registered")]
    AgentNotFound(String),

    #[error("path '{path}' in repository '{repository}' is leased by agent '{holder}'")]
    FileLeaseConflict {
        repository: String,
        path: String,
        holder: String,
    },

    #[error("file lease '{0}' not found or expired")]
    FileLeaseNotFound(String),

    #[error("agent '{agent}' does not own file lease '{lease_id}'")]
    FileLeaseOwnerMismatch { lease_id: String, agent: String },

    #[error("stale fencing token for file lease '{0}'")]
    StaleFencingToken(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("{what} exceeds the {limit}-byte limit")]
    PayloadTooLarge { what: &'static str, limit: usize },

    #[error("capacity for {what} reached ({limit})")]
    CapacityExceeded { what: &'static str, limit: usize },

    #[error("unauthorized")]
    Unauthorized,
}

impl BridgeError {
    /// Stable, machine-readable code clients can switch on.
    pub fn code(&self) -> &'static str {
        match self {
            BridgeError::ChannelNotFound(_) => "channel_not_found",
            BridgeError::ChannelFull { .. } => "channel_full",
            BridgeError::NotAMember { .. } => "not_a_member",
            BridgeError::AgentNotFound(_) => "agent_not_found",
            BridgeError::FileLeaseConflict { .. } => "file_lease_conflict",
            BridgeError::FileLeaseNotFound(_) => "file_lease_not_found",
            BridgeError::FileLeaseOwnerMismatch { .. } => "file_lease_owner_mismatch",
            BridgeError::StaleFencingToken(_) => "stale_fencing_token",
            BridgeError::BadRequest(_) => "bad_request",
            BridgeError::PayloadTooLarge { .. } => "payload_too_large",
            BridgeError::CapacityExceeded { .. } => "capacity_exceeded",
            BridgeError::Unauthorized => "unauthorized",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            BridgeError::ChannelNotFound(_) => 404,
            BridgeError::ChannelFull { .. } => 409,
            BridgeError::NotAMember { .. } => 403,
            BridgeError::AgentNotFound(_) => 404,
            BridgeError::FileLeaseConflict { .. } => 409,
            BridgeError::FileLeaseNotFound(_) => 404,
            BridgeError::FileLeaseOwnerMismatch { .. } => 403,
            BridgeError::StaleFencingToken(_) => 409,
            BridgeError::BadRequest(_) => 400,
            BridgeError::PayloadTooLarge { .. } => 413,
            BridgeError::CapacityExceeded { .. } => 429,
            BridgeError::Unauthorized => 401,
        }
    }

    /// Structured payload returned to clients on both transports.
    pub fn payload(&self) -> ErrorPayload {
        let (limit, current) = match self {
            BridgeError::ChannelFull { limit, current, .. } => (Some(*limit), Some(*current)),
            BridgeError::PayloadTooLarge { limit, .. } => (Some(*limit), None),
            BridgeError::CapacityExceeded { limit, .. } => (Some(*limit), None),
            _ => (None, None),
        };
        ErrorPayload {
            ok: false,
            error: self.code(),
            message: self.to_string(),
            limit,
            current,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorPayload {
    pub ok: bool,
    pub error: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<usize>,
}

pub type BridgeResult<T> = Result<T, BridgeError>;
