#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


protocol_path = "sdk/agent-pontifex-protocol/src/lib.rs"
old_protocol = '''pub const PROTOCOL_SCHEMA_VERSION: u16 = 1;
pub const BRIDGE_PROTOCOL_ID: &str = "agent-pontifex.bridge.v1";
pub const COORDINATOR_PROTOCOL_ID: &str = "agent-pontifex.coordinator.v1";
pub type Timestamp = String;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceDescriptor {
    pub schema_version: u16,
    pub protocol: String,
    pub service: String,
    pub implementation: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl ServiceDescriptor {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != PROTOCOL_SCHEMA_VERSION {
            return Err(ValidationError::new("unsupported protocol schema version"));
        }
        validate_identifier(&self.protocol, "protocol")?;
        validate_identifier(&self.service, "service")?;
        validate_identifier(&self.implementation, "implementation")?;

        let mut seen = BTreeSet::new();
        for capability in &self.capabilities {
            validate_identifier(capability, "capability")?;
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

        for extension in self.extensions.keys() {
            validate_identifier(extension, "extension")?;
            if !extension.contains('.') {
                return Err(ValidationError::new(
                    "extension keys must use a vendor namespace",
                ));
            }
        }
        Ok(())
    }
}
'''
new_protocol = '''pub const PROTOCOL_SCHEMA_VERSION: u16 = 1;
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
'''
replace_once(protocol_path, old_protocol, new_protocol)
replace_once(
    protocol_path,
    '''            protocol: BRIDGE_PROTOCOL_ID.to_string(),
            service: "ai-agent-bridge".to_string(),
            implementation: "agent-pontifex".to_string(),''',
    '''            protocol: BRIDGE_PROTOCOL_ID.to_string(),
            protocol_versions: ProtocolVersionRange::current(),
            service: ServiceKind::Bridge.service_id().to_string(),
            implementation: "agent-pontifex.ai-agent-bridge".to_string(),''',
)
replace_once(
    protocol_path,
    '''        let mut unsorted = descriptor.clone();
        unsorted.capabilities.reverse();
        assert!(unsorted.validate().is_err());

        let mut unnamespaced = descriptor;
        unnamespaced.extensions = BTreeMap::from([("file-leases".to_string(), json!({}))]);
        assert!(unnamespaced.validate().is_err());''',
    '''        assert_eq!(
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
            .is_err());''',
)

sdk_path = "sdk/agent-pontifex-sdk/src/lib.rs"
replace_once(
    sdk_path,
    '''use protocol::{bridge, coordinator, ErrorResponse};''',
    '''use protocol::{
    bridge, coordinator, ErrorResponse, ProtocolVersionRange, ServiceDescriptor, ServiceKind,
    DISCOVERY_PATH_SEGMENTS,
};''',
)
replace_once(
    sdk_path,
    '''use std::time::Duration;''',
    '''use std::net::IpAddr;
use std::time::Duration;''',
)
replace_once(
    sdk_path,
    '''const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
''',
    '''const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredService {
    pub descriptor: ServiceDescriptor,
    pub negotiated_protocol_major: u16,
}
''',
)
replace_once(
    sdk_path,
    '''    pub fn coordinator(&self) -> CoordinatorClient {
        CoordinatorClient {
            client: self.clone(),
        }
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, SdkError> {''',
    '''    pub fn coordinator(&self) -> CoordinatorClient {
        CoordinatorClient {
            client: self.clone(),
        }
    }

    async fn discover(&self, expected: ServiceKind) -> Result<DiscoveredService, SdkError> {
        let url = self.endpoint(&DISCOVERY_PATH_SEGMENTS)?;
        let descriptor: ServiceDescriptor =
            self.decode(self.request(Method::GET, url)).await?;
        let negotiated_protocol_major = descriptor
            .validate_for(expected, ProtocolVersionRange::current())
            .map_err(|error| {
                SdkError::IncompatibleService(sanitize_public_message(&error.to_string()))
            })?;
        Ok(DiscoveredService {
            descriptor,
            negotiated_protocol_major,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, SdkError> {''',
)
replace_once(
    sdk_path,
    '''        let response = request.send().await?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(SdkError::ResponseTooLarge);
        }
        let body = response.bytes().await?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(SdkError::ResponseTooLarge);
        }
        Ok((status, body.to_vec()))''',
    '''        let mut response = request.send().await?;
        let status = response.status();
        let content_length = response.content_length();
        if content_length.is_some_and(|length| length > MAX_RESPONSE_BYTES as u64) {
            return Err(SdkError::ResponseTooLarge);
        }

        let mut body = Vec::with_capacity(
            content_length
                .unwrap_or(0)
                .min(MAX_RESPONSE_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await? {
            let next_len = body
                .len()
                .checked_add(chunk.len())
                .ok_or(SdkError::ResponseTooLarge)?;
            if next_len > MAX_RESPONSE_BYTES {
                return Err(SdkError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))''',
)
replace_once(
    sdk_path,
    '''impl BridgeClient {
    pub async fn register_agent(''',
    '''impl BridgeClient {
    pub async fn discover(&self) -> Result<DiscoveredService, SdkError> {
        self.client.discover(ServiceKind::Bridge).await
    }

    pub async fn register_agent(''',
)
replace_once(
    sdk_path,
    '''impl CoordinatorClient {
    pub async fn create_job(''',
    '''impl CoordinatorClient {
    pub async fn discover(&self) -> Result<DiscoveredService, SdkError> {
        self.client.discover(ServiceKind::Coordinator).await
    }

    pub async fn create_job(''',
)
old_normalize = '''fn normalize_base_url(input: &str) -> Result<Url, SdkError> {
    let mut url = Url::parse(input).map_err(|error| SdkError::InvalidBaseUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SdkError::InvalidBaseUrl(
            "only http and https are supported".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "credentials are not allowed in the base URL".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "query strings and fragments are not allowed in the base URL".into(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}
'''
new_normalize = '''fn normalize_base_url(input: &str) -> Result<Url, SdkError> {
    let mut url = Url::parse(input).map_err(|error| SdkError::InvalidBaseUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SdkError::InvalidBaseUrl(
            "only http and https are supported".into(),
        ));
    }
    if url.host_str().is_none() {
        return Err(SdkError::InvalidBaseUrl("base URL must include a host".into()));
    }
    if url.scheme() == "http" && !is_loopback_host(&url) {
        return Err(SdkError::InvalidBaseUrl(
            "plaintext HTTP is allowed only for loopback development".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "credentials are not allowed in the base URL".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "query strings and fragments are not allowed in the base URL".into(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}
'''
replace_once(sdk_path, old_normalize, new_normalize)
replace_once(
    sdk_path,
    '''    #[error("response exceeds the SDK size limit")]
    ResponseTooLarge,''',
    '''    #[error("service discovery is incompatible: {0}")]
    IncompatibleService(String),
    #[error("response exceeds the SDK size limit")]
    ResponseTooLarge,''',
)
replace_once(
    sdk_path,
    '''        assert!(Client::new("ftp://example.com").is_err());
        assert!(Client::new("https://user:pass@example.com").is_err());
        assert!(Client::new("https://example.com?tenant=one").is_err());
        assert!(Client::new("https://example.com/#fragment").is_err());
        assert!(Client::new("https://example.com/api").is_ok());''',
    '''        assert!(Client::new("ftp://example.com").is_err());
        assert!(Client::new("http://example.com").is_err());
        assert!(Client::new("http://127.0.0.1:8142").is_ok());
        assert!(Client::new("http://127.0.0.42:8142").is_ok());
        assert!(Client::new("http://[::1]:8142").is_ok());
        assert!(Client::new("http://localhost:8142").is_ok());
        assert!(Client::new("https://user:pass@example.com").is_err());
        assert!(Client::new("https://example.com?tenant=one").is_err());
        assert!(Client::new("https://example.com/#fragment").is_err());
        assert!(Client::new("https://example.com/api").is_ok());''',
)
replace_once(
    sdk_path,
    '''    #[test]
    fn dynamic_identifiers_are_encoded_as_single_path_segments() {
        let client = Client::new("https://example.com/root").unwrap();
        let url = client
            .endpoint(&["v1", "jobs", "owner/repo", "heartbeat"])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/root/v1/jobs/owner%2Frepo/heartbeat"
        );
    }
}''',
    '''    #[test]
    fn dynamic_identifiers_are_encoded_as_single_path_segments() {
        let client = Client::new("https://example.com/root").unwrap();
        let url = client
            .endpoint(&["v1", "jobs", "owner/repo", "heartbeat"])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/root/v1/jobs/owner%2Frepo/heartbeat"
        );
    }

    #[test]
    fn discovery_uses_the_well_known_path() {
        let client = Client::new("https://example.com/root").unwrap();
        let url = client.endpoint(&DISCOVERY_PATH_SEGMENTS).unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/root/.well-known/agent-pontifex"
        );
    }
}''',
)

http_path = "src/http.rs"
replace_once(
    http_path,
    '''        .route("/readyz", get(healthz))
        .route("/metrics", get(prometheus_metrics))''',
    '''        .route("/readyz", get(healthz))
        .route(
            "/.well-known/agent-pontifex",
            get(agent_pontifex_descriptor),
        )
        .route("/metrics", get(prometheus_metrics))''',
)
replace_once(
    http_path,
    '''async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ai-agent-bridge" }))
}
''',
    '''fn agent_pontifex_bridge_descriptor() -> crate::agent_pontifex_protocol::ServiceDescriptor {
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
    use crate::agent_pontifex_protocol::{
        ProtocolVersionRange, ServiceKind, BRIDGE_PROTOCOL_ID,
    };

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
''',
)

replace_once(
    ".github/workflows/agent-pontifex-sdk.yml",
    "    runs-on: ubuntu-latest",
    "    runs-on: ubuntu-24.04",
)

readme_path = "sdk/README.md"
replace_once(
    readme_path,
    '''## Development
''',
    '''## Discovery and negotiation

Compatible servers expose `GET /.well-known/agent-pontifex` without requiring
application credentials. The document binds a canonical bridge or coordinator
service ID to its matching protocol ID, advertises an explicit supported major-
version range, and keeps capabilities sorted for deterministic comparison.
Clients negotiate the highest shared major version and fail closed when the
service role, protocol, or version range does not match the client they opened.

Remote SDK connections require HTTPS. Plaintext HTTP is accepted only for
loopback development addresses. Response bodies are consumed incrementally and
aborted once the four-megabyte SDK ceiling would be exceeded, including chunked
responses with no `Content-Length` header.

## Development
''',
)
replace_once(
    readme_path,
    '''2. Stabilize capability names and add server discovery endpoints.''',
    '''2. Stabilize capability names and validate the well-known discovery endpoint across both servers.''',
)
