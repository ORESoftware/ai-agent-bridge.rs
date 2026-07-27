use std::net::IpAddr;
use std::time::Duration;

use futures::StreamExt;
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::orchestration::{WorkflowPlan, WorkflowStatus, WorkflowSubmission, WorkflowView};
use crate::types::AgentKind;

const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct BridgeClient {
    base_url: Url,
    bearer: Option<String>,
    http: reqwest::Client,
    max_response_bytes: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct LeaseHandle {
    pub fencing_token: u64,
    pub release_path: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BridgeClientError {
    #[error("invalid bridge configuration: {0}")]
    InvalidConfig(String),
    #[error("bridge request failed")]
    Transport,
    #[error("bridge returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("bridge response exceeded the configured byte limit")]
    ResponseTooLarge,
    #[error("bridge response was invalid")]
    InvalidResponse,
    #[error("Fiducia lease was not acquired")]
    LeaseNotAcquired,
}

#[derive(Deserialize)]
struct WorkflowViewWire {
    plan: WorkflowPlan,
    status: WorkflowStatus,
    submissions: Vec<WorkflowSubmission>,
}

impl From<WorkflowViewWire> for WorkflowView {
    fn from(value: WorkflowViewWire) -> Self {
        Self {
            plan: value.plan,
            status: value.status,
            submissions: value.submissions,
        }
    }
}

#[derive(Deserialize)]
struct WorkflowListEnvelope {
    workflows: Vec<WorkflowViewWire>,
}

#[derive(Deserialize)]
struct WorkflowGetEnvelope {
    workflow: WorkflowViewWire,
}

impl BridgeClient {
    pub(crate) fn from_env() -> Result<Self, BridgeClientError> {
        let raw_url = std::env::var("AI_AGENT_RUNNER_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8142/".to_string());
        let mut base_url = Url::parse(raw_url.trim())
            .map_err(|_| BridgeClientError::InvalidConfig("bridge URL is invalid".into()))?;
        if base_url.username() != "" || base_url.password().is_some() {
            return Err(BridgeClientError::InvalidConfig(
                "bridge URL must not contain user information".into(),
            ));
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(BridgeClientError::InvalidConfig(
                "bridge URL must not contain a query or fragment".into(),
            ));
        }
        let host = base_url
            .host_str()
            .ok_or_else(|| BridgeClientError::InvalidConfig("bridge URL requires a host".into()))?
            .to_ascii_lowercase();
        let bearer =
            env_opt("AI_AGENT_RUNNER_BRIDGE_BEARER").or_else(|| env_opt("API_AUTH_BEARER"));
        if base_url.scheme() != "https" && !is_loopback_host(&host) {
            return Err(BridgeClientError::InvalidConfig(
                "remote bridge URLs must use HTTPS".into(),
            ));
        }
        if !is_loopback_host(&host) && bearer.is_none() {
            return Err(BridgeClientError::InvalidConfig(
                "remote bridge URLs require AI_AGENT_RUNNER_BRIDGE_BEARER".into(),
            ));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let timeout_secs = env_u64("AI_AGENT_RUNNER_BRIDGE_TIMEOUT_SECS", 30).max(1);
        let max_response_bytes = env_usize(
            "AI_AGENT_RUNNER_MAX_BRIDGE_RESPONSE_BYTES",
            DEFAULT_MAX_RESPONSE_BYTES,
        )
        .clamp(1, MAX_RESPONSE_BYTES);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("fiducia-ai-agent-runner/0.1")
            .build()
            .map_err(|_| {
                BridgeClientError::InvalidConfig("failed to build bridge HTTP client".into())
            })?;
        Ok(Self {
            base_url,
            bearer,
            http,
            max_response_bytes,
        })
    }

    pub(crate) async fn register_agent(
        &self,
        agent_key: &str,
        display_name: &str,
        kind: AgentKind,
        capabilities: &[String],
        provider: &str,
        model: &str,
    ) -> Result<(), BridgeClientError> {
        let body = json!({
            "agent_key": agent_key,
            "display_name": display_name,
            "kind": kind,
            "meta": {
                "capabilities": capabilities,
                "managed_by": "fiducia-ai-agent-runner",
                "provider": provider,
                "model": model,
            }
        });
        let _: Value = self
            .request(Method::POST, "agents/register", Some(body))
            .await?;
        Ok(())
    }

    pub(crate) async fn list_workflows(&self) -> Result<Vec<WorkflowView>, BridgeClientError> {
        let response: WorkflowListEnvelope = self.request(Method::GET, "workflows", None).await?;
        Ok(response
            .workflows
            .into_iter()
            .map(WorkflowView::from)
            .collect())
    }

    #[allow(dead_code)]
    pub(crate) async fn get_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<WorkflowView, BridgeClientError> {
        let response: WorkflowGetEnvelope = self
            .request(Method::GET, &format!("workflows/{workflow_id}"), None)
            .await?;
        Ok(response.workflow.into())
    }

    pub(crate) async fn submit(
        &self,
        workflow_id: &str,
        agent_key: &str,
        content: &str,
        meta: Value,
    ) -> Result<WorkflowSubmission, BridgeClientError> {
        let body = json!({
            "agent_key": agent_key,
            "content": content,
            "meta": meta,
        });
        let response: Value = self
            .request(
                Method::POST,
                &format!("workflows/{workflow_id}/submissions"),
                Some(body),
            )
            .await?;
        serde_json::from_value(response["submission"].clone())
            .map_err(|_| BridgeClientError::InvalidResponse)
    }

    pub(crate) async fn acquire_lease(
        &self,
        acquire_path: &str,
        release_path: &str,
        repository: &str,
        paths: &[String],
        agent_key: &str,
        ttl_ms: u64,
    ) -> Result<LeaseHandle, BridgeClientError> {
        let body = json!({
            "repository": repository,
            "paths": paths,
            "agent_key": agent_key,
            "ttl_ms": ttl_ms,
            "wait": false,
        });
        let response: Value = self
            .request(Method::POST, trim_path(acquire_path), Some(body))
            .await?;
        let acquired = find_bool(&response, "acquired").unwrap_or(true);
        let fencing_token = find_u64(&response, "fencing_token")
            .filter(|token| *token > 0)
            .ok_or(BridgeClientError::LeaseNotAcquired)?;
        if !acquired {
            return Err(BridgeClientError::LeaseNotAcquired);
        }
        Ok(LeaseHandle {
            fencing_token,
            release_path: trim_path(release_path).to_string(),
        })
    }

    pub(crate) async fn release_lease(
        &self,
        handle: &LeaseHandle,
        agent_key: &str,
    ) -> Result<(), BridgeClientError> {
        let body = json!({
            "agent_key": agent_key,
            "fencing_token": handle.fencing_token,
        });
        let _: Value = self
            .request(Method::POST, &handle.release_path, Some(body))
            .await?;
        Ok(())
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, BridgeClientError> {
        let path = trim_path(path);
        let url = self
            .base_url
            .join(path)
            .map_err(|_| BridgeClientError::InvalidConfig("bridge route is invalid".into()))?;
        if url.origin() != self.base_url.origin() {
            return Err(BridgeClientError::InvalidConfig(
                "bridge route changed the configured origin".into(),
            ));
        }
        let mut request = self.http.request(method, url);
        if let Some(bearer) = &self.bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| BridgeClientError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(BridgeClientError::HttpStatus(status));
        }
        let bytes = read_bounded(response, self.max_response_bytes).await?;
        serde_json::from_slice(&bytes).map_err(|_| BridgeClientError::InvalidResponse)
    }
}

async fn read_bounded(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, BridgeClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(BridgeClientError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BridgeClientError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(BridgeClientError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn find_u64(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_u64)
            .or_else(|| map.values().find_map(|value| find_u64(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_u64(value, key)),
        _ => None,
    }
}

fn find_bool(value: &Value, key: &str) -> Option<bool> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_bool)
            .or_else(|| map.values().find_map(|value| find_bool(value, key))),
        Value::Array(values) => values.iter().find_map(|value| find_bool(value, key)),
        _ => None,
    }
}

fn trim_path(value: &str) -> &str {
    value.trim().trim_start_matches('/')
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_opt(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env_opt(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|character| character == '[' || character == ']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_fencing_token() {
        let body = json!({"result":{"output":{"fencing_token":42,"acquired":true}}});
        assert_eq!(find_u64(&body, "fencing_token"), Some(42));
        assert_eq!(find_bool(&body, "acquired"), Some(true));
    }

    #[test]
    fn route_trimming_keeps_requests_relative() {
        assert_eq!(trim_path("/file-leases/acquire"), "file-leases/acquire");
    }
}
