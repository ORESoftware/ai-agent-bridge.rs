//! Stable ORES identity and discovery constants for the AI-agent bridge.
//!
//! `LOGICAL_SERVICE_ID` is a reverse-DNS identifier for clients and registries.
//! It is deliberately distinct from the Rust package name, Kubernetes Service
//! name, and wire-level compatibility name so those implementation details can
//! evolve without making every consumer guess which endpoint it reached.

/// Stable logical identifier used by ORES clients and service registries.
pub const LOGICAL_SERVICE_ID: &str = "com.ores.ai-agent-bridge";

/// Existing service name advertised by the HTTP identity and health payloads.
pub const WIRE_SERVICE_NAME: &str = "ai-agent-bridge";

/// HTTP transport description advertised by the root identity endpoint.
pub const HTTP_TRANSPORT: &str = "REST + SSE";

/// TCP transport description advertised by the root identity endpoint.
pub const TCP_TRANSPORT: &str = "newline-delimited JSON";

/// Default bridge URL for a bridge running on the same machine.
pub const DEFAULT_LOCAL_HTTP_BASE_URL: &str = "http://127.0.0.1:8142";

/// Cluster-local URL of the reviewed `dd-ai-agent-bridge` Kubernetes Service.
pub const KUBERNETES_HTTP_BASE_URL: &str =
    "http://dd-ai-agent-bridge.default.svc.cluster.local:8142";

pub const DEFAULT_HTTP_PORT: u16 = 8142;
pub const DEFAULT_TCP_PORT: u16 = 8143;

pub const BASE_URL_ENV: &str = "ORES_AI_AGENT_BRIDGE_BASE_URL";
pub const TCP_PORT_ENV: &str = "ORES_AI_AGENT_BRIDGE_TCP_PORT";
pub const BEARER_ENV: &str = "ORES_AI_AGENT_BRIDGE_BEARER";
pub const AGENT_KEY_ENV: &str = "ORES_AI_AGENT_BRIDGE_AGENT_KEY";
pub const TOPIC_ENV: &str = "ORES_AI_AGENT_BRIDGE_TOPIC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_id_is_a_bounded_reverse_dns_identifier() {
        let labels = LOGICAL_SERVICE_ID.split('.').collect::<Vec<_>>();
        assert_eq!(labels, ["com", "ores", "ai-agent-bridge"]);
        assert!(labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        }));
    }
}
