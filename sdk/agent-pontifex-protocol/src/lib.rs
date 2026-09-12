#![forbid(unsafe_code)]

//! Vendor-neutral wire contracts for Agent Pontifex-compatible bridges,
//! coordinators, workers, and clients.
//!
//! This crate intentionally contains no persistence, provider, GitHub, Linear,
//! or Fiducia implementation. Product-specific behavior belongs in namespaced
//! service capabilities and extension objects.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

pub const PROTOCOL_SCHEMA_VERSION: u16 = 1;
pub const CURRENT_PROTOCOL_MAJOR: u16 = 1;
pub const BRIDGE_PROTOCOL_ID: &str = "agent-pontifex.bridge";
pub const COORDINATOR_PROTOCOL_ID: &str = "agent-pontifex.coordinator";
pub const DISCOVERY_PATH_SEGMENTS: [&str; 2] = [".well-known", "agent-pontifex"];
pub type Timestamp = String;

const MAX_CAPABILITIES: usize = 256;
const MAX_EXTENSIONS: usize = 64;
const MAX_EXTENSION_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceKind {
    Bridge,
    Coordinator,
}

impl ServiceKind {
    pub const fn service_id(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Coordinator => "coordinator",
        }
    }

    pub const fn protocol_id(self) -> &'static str {
        match self {
            Self::Bridge => BRIDGE_PROTOCOL_ID,
            Self::Coordinator => COORDINATOR_PROTOCOL_ID,
        }
    }

    fn from_service_id(service: &str) -> Option<Self> {
        match service {
            "bridge" => Some(Self::Bridge),
            "coordinator" => Some(Self::Coordinator),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersionRange {
    pub min_major: u16,
    pub max_major: u16,
}

impl ProtocolVersionRange {
    pub const fn current() -> Self {
        Self {
            min_major: CURRENT_PROTOCOL_MAJOR,
            max_major: CURRENT_PROTOCOL_MAJOR,
        }
    }

    pub fn validate(self) -> Result<(), ValidationError> {
        if self.min_major == 0 || self.min_major > self.max_major {
            return Err(ValidationError::new("invalid protocol major-version range"));
        }
        Ok(())
    }

    pub fn highest_common(self, other: Self) -> Option<u16> {
        let lower = self.min_major.max(other.min_major);
        let upper = self.max_major.min(other.max_major);
        (lower <= upper).then_some(upper)
    }
}

impl Default for ProtocolVersionRange {
    fn default() -> Self {
        Self::current()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceDescriptor {
    pub schema_version: u16,
    pub protocol: String,
    pub protocol_versions: ProtocolVersionRange,
    pub service: String,
    pub implementation: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl ServiceDescriptor {
    pub fn new(
        kind: ServiceKind,
        implementation: impl Into<String>,
        mut capabilities: Vec<String>,
        extensions: BTreeMap<String, Value>,
    ) -> Self {
        capabilities.sort();
        Self {
            schema_version: PROTOCOL_SCHEMA_VERSION,
            protocol: kind.protocol_id().to_string(),
            protocol_versions: ProtocolVersionRange::current(),
            service: kind.service_id().to_string(),
            implementation: implementation.into(),
            capabilities,
            extensions,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != PROTOCOL_SCHEMA_VERSION {
            return Err(ValidationError::new("unsupported protocol schema version"));
        }
        self.protocol_versions.validate()?;
        validate_identifier(&self.protocol, "protocol")?;
        validate_identifier(&self.service, "service")?;
        validate_identifier(&self.implementation, "implementation")?;

        let kind = ServiceKind::from_service_id(&self.service)
            .ok_or_else(|| ValidationError::new("unknown Agent Pontifex service"))?;
        if self.protocol != kind.protocol_id() {
            return Err(ValidationError::new(
                "service and protocol identifiers do not match",
            ));
        }

        if self.capabilities.len() > MAX_CAPABILITIES {
            return Err(ValidationError::new("too many advertised capabilities"));
        }
        let mut seen = BTreeSet::new();
        for capability in &self.capabilities {
            validate_identifier(capability, "capability")?;
            if !capability.contains('.') {
                return Err(ValidationError::new(
                    "capability identifiers must use a namespace",
                ));
            }
            if !seen.insert(capability.as_str()) {
                return Err(ValidationError::new("duplicate capability"));
            }
        }
        let mut sorted = self.capabilities.clone();
        sorted.sort();
        if sorted != self.capabilities {
            return Err(ValidationError::new(
                "capabilities must be sorted for deterministic negotiation",
            ));
        }

        if self.extensions.len() > MAX_EXTENSIONS {
            return Err(ValidationError::new("too many advertised extensions"));
        }
        for (extension, value) in &self.extensions {
            validate_identifier(extension, "extension")?;
            if !extension.contains('.') {
                return Err(ValidationError::new(
                    "extension keys must use a vendor namespace",
                ));
            }
            if serde_json::to_vec(value)
                .map_err(|_| ValidationError::new("extension is not serializable"))?
                .len()
                > MAX_EXTENSION_BYTES
            {
                return Err(ValidationError::new("extension value is too large"));
            }
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        expected: ServiceKind,
        supported: ProtocolVersionRange,
    ) -> Result<u16, ValidationError> {
        self.validate()?;
        supported.validate()?;
        if self.service != expected.service_id() || self.protocol != expected.protocol_id() {
            return Err(ValidationError::new("unexpected Agent Pontifex service"));
        }
        self.protocol_versions
            .highest_common(supported)
            .ok_or_else(|| ValidationError::new("no compatible protocol major version"))
    }
}

fn validate_identifier(value: &str, field: &str) -> Result<(), ValidationError> {
    if value.is_empty() || value.len() > 128 {
        return Err(ValidationError::new(format!(
            "{field} must contain 1 to 128 characters"
        )));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte))
    {
        return Err(ValidationError::new(format!(
            "{field} must use lowercase ASCII identifier characters"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ValidationError {}

pub mod bridge {
    use super::{Timestamp, Value};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum AgentKind {
        Claude,
        Codex,
        Gemini,
        Kimi,
        Qwen,
        Human,
        #[default]
        Other,
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
        Observer,
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum PresenceKind {
        Joined,
        Left,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Agent {
        pub agent_key: String,
        #[serde(default)]
        pub display_name: String,
        #[serde(default)]
        pub kind: AgentKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub host: Option<String>,
        #[serde(default)]
        pub meta: Value,
        #[serde(default)]
        pub registered_at: Timestamp,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct FileLease {
        pub id: String,
        pub repository: String,
        pub path: String,
        pub recursive: bool,
        pub agent_key: String,
        #[serde(default)]
        pub purpose: String,
        #[serde(default)]
        pub meta: Value,
        pub fencing_token: u64,
        pub acquired_at: Timestamp,
        pub expires_at: Timestamp,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct FileLeaseHolder {
        pub lease: FileLease,
        pub agent: Agent,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Member {
        pub agent_key: String,
        #[serde(default)]
        pub role: MemberRole,
        pub joined_at: Timestamp,
        pub last_seen_at: Timestamp,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Message {
        pub id: String,
        pub channel: String,
        pub seq: u64,
        pub from: String,
        #[serde(default)]
        pub role: Role,
        pub content: String,
        #[serde(default)]
        pub meta: Value,
        pub created_at: Timestamp,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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
        pub meta: Value,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct ScoredChannel {
        #[serde(flatten)]
        pub channel: Channel,
        pub score: f32,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct ContextEntry {
        pub key: String,
        pub value: Value,
        pub version: u32,
        pub updated_by: String,
        pub updated_at: Timestamp,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct RegisterAgentRequest {
        pub agent_key: String,
        #[serde(default)]
        pub display_name: String,
        #[serde(default)]
        pub kind: AgentKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub host: Option<String>,
        #[serde(default)]
        pub meta: Value,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct ResolveChannelRequest {
        pub query: String,
        #[serde(default)]
        pub created_by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub threshold: Option<f32>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct PostMessageRequest {
        pub from: String,
        pub content: String,
        #[serde(default)]
        pub role: Role,
        #[serde(default)]
        pub meta: Value,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct AcquireFileLeaseRequest {
        pub repository: String,
        pub paths: Vec<String>,
        pub agent_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ttl_ms: Option<u64>,
        #[serde(default)]
        pub wait: bool,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct RegisterAgentResponse {
        pub ok: bool,
        pub agent: Agent,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct ResolveChannelResponse {
        pub ok: bool,
        pub channel: Channel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub score: Option<f32>,
        pub created: bool,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct PostMessageResponse {
        pub ok: bool,
        pub message: Message,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct MessagesResponse {
        pub ok: bool,
        pub messages: Vec<Message>,
    }
}

pub mod coordinator {
    use super::{Timestamp, Value};
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum JobStatus {
        Queued,
        Running,
        Succeeded,
        Failed,
        Cancelled,
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum CompletionOutcome {
        Succeeded,
        Failed,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Job {
        pub id: String,
        pub org: String,
        pub repo: String,
        pub task_type: String,
        pub payload: Value,
        pub priority: i64,
        pub status: JobStatus,
        pub created_at: Timestamp,
        pub updated_at: Timestamp,
        pub available_at: Timestamp,
        pub claimed_by: Option<String>,
        pub lease_expires_at: Option<Timestamp>,
        pub attempts: i64,
        pub max_attempts: i64,
        pub result: Option<Value>,
        pub last_error: Option<String>,
        pub budget_usd: Option<f64>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct CreateJobRequest {
        pub org: String,
        pub repo: String,
        pub task_type: String,
        #[serde(default)]
        pub payload: Value,
        #[serde(default)]
        pub priority: i64,
        #[serde(default = "default_max_attempts")]
        pub max_attempts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub available_at: Option<Timestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub budget_usd: Option<f64>,
    }

    fn default_max_attempts() -> i64 {
        3
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct ClaimJobRequest {
        pub worker_id: String,
        #[serde(default)]
        pub orgs: Vec<String>,
        #[serde(default)]
        pub repositories: Vec<String>,
        #[serde(default)]
        pub task_types: Vec<String>,
        #[serde(default = "default_lease_seconds")]
        pub lease_seconds: i64,
    }

    fn default_lease_seconds() -> i64 {
        120
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct HeartbeatJobRequest {
        pub worker_id: String,
        #[serde(default = "default_lease_seconds")]
        pub lease_seconds: i64,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct CompleteJobRequest {
        pub worker_id: String,
        pub outcome: CompletionOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
        #[serde(default)]
        pub retryable: bool,
        #[serde(default = "default_retry_delay_seconds")]
        pub retry_delay_seconds: i64,
    }

    fn default_retry_delay_seconds() -> i64 {
        30
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct JobResponse {
        pub job: Job,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ErrorResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn descriptor_requires_sorted_capabilities_and_namespaced_extensions() {
        let descriptor = ServiceDescriptor {
            schema_version: PROTOCOL_SCHEMA_VERSION,
            protocol: BRIDGE_PROTOCOL_ID.to_string(),
            protocol_versions: ProtocolVersionRange::current(),
            service: ServiceKind::Bridge.service_id().to_string(),
            implementation: "agent-pontifex.ai-agent-bridge".to_string(),
            capabilities: vec!["bridge.channels".to_string(), "bridge.messages".to_string()],
            extensions: BTreeMap::from([(
                "fiducia.file-leases".to_string(),
                json!({"fencing": true}),
            )]),
        };
        descriptor.validate().unwrap();

        assert_eq!(
            descriptor
                .validate_for(
                    ServiceKind::Bridge,
                    ProtocolVersionRange {
                        min_major: 1,
                        max_major: 2,
                    },
                )
                .unwrap(),
            1
        );

        let mut unsorted = descriptor.clone();
        unsorted.capabilities.reverse();
        assert!(unsorted.validate().is_err());

        let mut mismatched = descriptor.clone();
        mismatched.protocol = COORDINATOR_PROTOCOL_ID.to_string();
        assert!(mismatched.validate().is_err());

        let mut unnamespaced = descriptor.clone();
        unnamespaced.extensions = BTreeMap::from([("file-leases".to_string(), json!({}))]);
        assert!(unnamespaced.validate().is_err());

        assert!(descriptor
            .validate_for(
                ServiceKind::Bridge,
                ProtocolVersionRange {
                    min_major: 2,
                    max_major: 3,
                },
            )
            .is_err());
    }

    #[test]
    fn bridge_event_and_coordinator_job_shapes_round_trip() {
        let event = bridge::Event::Presence {
            channel: "release-train".to_string(),
            agent_key: "codex".to_string(),
            event: bridge::PresenceKind::Joined,
            member_count: 2,
            at: "2026-08-04T18:00:00.000Z".to_string(),
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], "presence");
        assert_eq!(
            serde_json::from_value::<bridge::Event>(encoded).unwrap(),
            event
        );

        let request = coordinator::CreateJobRequest {
            org: "agent-pontifex".to_string(),
            repo: "agent-coordinator.rs".to_string(),
            task_type: "code_change".to_string(),
            payload: json!({"goal": "add compatibility negotiation"}),
            priority: 25,
            max_attempts: 3,
            available_at: None,
            budget_usd: Some(1.5),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["org"], "agent-pontifex");
        assert_eq!(
            serde_json::from_value::<coordinator::CreateJobRequest>(encoded).unwrap(),
            request
        );
    }
}
