use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::time::Duration;

use futures::StreamExt;
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::orchestration::{WorkflowMode, WorkflowView};
use crate::policy::{DataSensitivity, PolicyRequest, ProviderCandidate, RequestedBudget, TaskRisk};
use crate::policy_admission::{AdmissionRecord, AdmissionStatus, UsageDelta};
use crate::providers::ProviderRequest;

use super::{infer_agent_kind, ClaimConfig, ProviderWorker};

const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const INPUT_TOKEN_OVERHEAD: u64 = 256;

#[derive(Clone)]
pub(crate) struct AdmissionControl {
    base_url: Url,
    bearer: Option<String>,
    http: reqwest::Client,
    actor: String,
    approved_by: Option<String>,
    pricing: HashMap<String, ProviderPricing>,
    max_response_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderPricing {
    input_micro_usd_per_million: u64,
    output_micro_usd_per_million: u64,
    #[serde(default, alias = "estimated_call_cost_micro_usd")]
    fixed_call_reserve_micro_usd: u64,
    #[serde(default)]
    max_context_tokens: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UsageReservation {
    input_tokens: u64,
    output_tokens: u64,
    cost_micro_usd: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionClientError {
    #[error("invalid admission configuration: {0}")]
    InvalidConfig(String),
    #[error("admission request failed")]
    Transport,
    #[error("admission returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("admission response exceeded the configured byte limit")]
    ResponseTooLarge,
    #[error("admission response was invalid")]
    InvalidResponse,
    #[error("workflow admission is not active")]
    NotActive,
    #[error("provider usage was missing token or pricing data")]
    UnpricedUsage,
    #[error("provider input exceeds the configured context limit")]
    InputTooLarge,
}

#[derive(Deserialize)]
struct AdmissionResponse {
    admission: AdmissionRecord,
}

impl AdmissionControl {
    pub(crate) fn from_env(
        providers: &[ProviderWorker],
        claims: &ClaimConfig,
    ) -> Result<Self, AdmissionClientError> {
        let raw_url = std::env::var("AI_AGENT_RUNNER_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8142/".to_string());
        let mut base_url = Url::parse(raw_url.trim())
            .map_err(|_| AdmissionClientError::InvalidConfig("bridge URL is invalid".into()))?;
        if base_url.username() != "" || base_url.password().is_some() {
            return Err(AdmissionClientError::InvalidConfig(
                "bridge URL must not contain user information".into(),
            ));
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(AdmissionClientError::InvalidConfig(
                "bridge URL must not contain a query or fragment".into(),
            ));
        }
        let host = base_url
            .host_str()
            .ok_or_else(|| {
                AdmissionClientError::InvalidConfig("bridge URL requires a host".into())
            })?
            .to_ascii_lowercase();
        let bearer =
            env_opt("AI_AGENT_RUNNER_BRIDGE_BEARER").or_else(|| env_opt("API_AUTH_BEARER"));
        if base_url.scheme() != "https" && !is_loopback_host(&host) {
            return Err(AdmissionClientError::InvalidConfig(
                "remote bridge URLs must use HTTPS".into(),
            ));
        }
        if !is_loopback_host(&host) && bearer.is_none() {
            return Err(AdmissionClientError::InvalidConfig(
                "remote policy admission requires a bridge bearer".into(),
            ));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }

        let raw_pricing = std::env::var("AI_PROVIDER_PRICING_JSON").map_err(|_| {
            AdmissionClientError::InvalidConfig(
                "AI_PROVIDER_PRICING_JSON is required for hard cost accounting".into(),
            )
        })?;
        let pricing: HashMap<String, ProviderPricing> = serde_json::from_str(&raw_pricing)
            .map_err(|_| {
                AdmissionClientError::InvalidConfig(
                    "AI_PROVIDER_PRICING_JSON is invalid JSON".into(),
                )
            })?;
        for provider in providers {
            if !pricing.contains_key(&provider.config.name) {
                return Err(AdmissionClientError::InvalidConfig(format!(
                    "pricing is missing for provider '{}'",
                    provider.config.name
                )));
            }
        }

        let actor =
            env_opt("AI_AGENT_RUNNER_ADMISSION_ACTOR").unwrap_or_else(|| claims.owner.clone());
        validate_actor(&actor)?;
        let approved_by = env_opt("AI_AGENT_RUNNER_POLICY_APPROVED_BY");
        if let Some(value) = &approved_by {
            validate_actor(value)?;
        }
        let timeout = Duration::from_secs(
            env_u64(
                "AI_AGENT_RUNNER_ADMISSION_TIMEOUT_SECS",
                DEFAULT_TIMEOUT_SECS,
            )
            .max(1),
        );
        let max_response_bytes = env_usize(
            "AI_AGENT_RUNNER_MAX_ADMISSION_RESPONSE_BYTES",
            DEFAULT_MAX_RESPONSE_BYTES,
        )
        .clamp(1, MAX_RESPONSE_BYTES);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .user_agent("fiducia-ai-agent-runner-admission/0.1")
            .build()
            .map_err(|_| {
                AdmissionClientError::InvalidConfig("failed to build admission HTTP client".into())
            })?;
        Ok(Self {
            base_url,
            bearer,
            http,
            actor,
            approved_by,
            pricing,
            max_response_bytes,
        })
    }

    pub(crate) fn actor(&self) -> &str {
        &self.actor
    }

    pub(crate) async fn ensure(
        &self,
        workflow: &WorkflowView,
        providers: &[ProviderWorker],
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        match self.get(&workflow.plan.id).await {
            Ok(admission) => return validate_active(admission),
            Err(AdmissionClientError::HttpStatus(StatusCode::NOT_FOUND)) => {}
            Err(error) => return Err(error),
        }
        let request = self.policy_request(workflow, providers)?;
        let response: AdmissionResponse = self
            .request(
                Method::POST,
                &format!("workflows/{}/admission", workflow.plan.id),
                Some(json!({
                    "requested_by": self.actor,
                    "approved_by": self.approved_by,
                    "policy_request": request,
                })),
            )
            .await?;
        validate_active(response.admission)
    }

    pub(crate) async fn reserve_call(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        request: &ProviderRequest,
        concurrency: u8,
    ) -> Result<(AdmissionRecord, UsageReservation), AdmissionClientError> {
        let pricing = self
            .pricing
            .get(provider_agent_key)
            .ok_or(AdmissionClientError::UnpricedUsage)?;
        let input_tokens = conservative_input_tokens(request);
        let output_tokens = u64::from(request.max_output_tokens);
        if pricing.max_context_tokens > 0 && input_tokens > pricing.max_context_tokens {
            return Err(AdmissionClientError::InputTooLarge);
        }
        let token_cost = price_usage(pricing, input_tokens, output_tokens);
        let cost_micro_usd = token_cost.saturating_add(pricing.fixed_call_reserve_micro_usd);
        let admission = self
            .report_usage(
                workflow_id,
                provider_agent_key,
                UsageDelta {
                    input_tokens,
                    output_tokens,
                    cost_micro_usd,
                    provider_calls: 1,
                    concurrency,
                    ..UsageDelta::default()
                },
            )
            .await?;
        Ok((
            admission,
            UsageReservation {
                input_tokens,
                output_tokens,
                cost_micro_usd,
            },
        ))
    }

    pub(crate) async fn report_response(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        usage: &Value,
        elapsed_ms: u64,
        reservation: UsageReservation,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        let pricing = self
            .pricing
            .get(provider_agent_key)
            .ok_or(AdmissionClientError::UnpricedUsage)?;
        let input_tokens = find_first_u64(
            usage,
            &[
                "input_tokens",
                "prompt_tokens",
                "prompt_token_count",
                "promptTokenCount",
            ],
        )
        .ok_or(AdmissionClientError::UnpricedUsage)?;
        let output_tokens = find_first_u64(
            usage,
            &[
                "output_tokens",
                "completion_tokens",
                "candidates_token_count",
                "candidatesTokenCount",
            ],
        )
        .ok_or(AdmissionClientError::UnpricedUsage)?;
        let actual_cost = find_first_u64(usage, &["cost_micro_usd", "costMicroUsd"])
            .unwrap_or_else(|| {
                price_usage(pricing, input_tokens, output_tokens)
                    .saturating_add(pricing.fixed_call_reserve_micro_usd)
            });
        self.report_usage(
            workflow_id,
            provider_agent_key,
            UsageDelta {
                input_tokens: input_tokens.saturating_sub(reservation.input_tokens),
                output_tokens: output_tokens.saturating_sub(reservation.output_tokens),
                cost_micro_usd: actual_cost.saturating_sub(reservation.cost_micro_usd),
                elapsed_ms,
                ..UsageDelta::default()
            },
        )
        .await
    }

    pub(crate) async fn report_failure(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        elapsed_ms: u64,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        self.report_usage(
            workflow_id,
            provider_agent_key,
            UsageDelta {
                elapsed_ms,
                ..UsageDelta::default()
            },
        )
        .await
    }

    pub(crate) async fn complete(
        &self,
        workflow_id: &str,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        self.terminal(workflow_id, "complete", "workflow completed")
            .await
    }

    pub(crate) async fn cancel(
        &self,
        workflow_id: &str,
        reason: &str,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        self.terminal(workflow_id, "cancel", reason).await
    }

    async fn terminal(
        &self,
        workflow_id: &str,
        action: &str,
        reason: &str,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        let response: AdmissionResponse = self
            .request(
                Method::POST,
                &format!("workflows/{workflow_id}/admission/{action}"),
                Some(json!({"updated_by":self.actor,"reason":reason})),
            )
            .await?;
        Ok(response.admission)
    }

    async fn report_usage(
        &self,
        workflow_id: &str,
        provider_agent_key: &str,
        delta: UsageDelta,
    ) -> Result<AdmissionRecord, AdmissionClientError> {
        let response: AdmissionResponse = self
            .request(
                Method::POST,
                &format!("workflows/{workflow_id}/admission/usage"),
                Some(json!({
                    "updated_by": self.actor,
                    "provider_agent_key": provider_agent_key,
                    "delta": delta,
                })),
            )
            .await?;
        validate_active(response.admission)
    }

    async fn get(&self, workflow_id: &str) -> Result<AdmissionRecord, AdmissionClientError> {
        let response: AdmissionResponse = self
            .request(
                Method::GET,
                &format!("workflows/{workflow_id}/admission"),
                None,
            )
            .await?;
        Ok(response.admission)
    }

    fn policy_request(
        &self,
        workflow: &WorkflowView,
        providers: &[ProviderWorker],
    ) -> Result<PolicyRequest, AdmissionClientError> {
        let planned = workflow
            .plan
            .assignments
            .iter()
            .map(|assignment| assignment.agent_key.as_str())
            .collect::<BTreeSet<_>>();
        let mut candidates = Vec::new();
        for provider in providers {
            if !planned.contains(provider.config.name.as_str()) {
                continue;
            }
            let pricing = self
                .pricing
                .get(&provider.config.name)
                .ok_or(AdmissionClientError::UnpricedUsage)?;
            candidates.push(ProviderCandidate {
                agent_key: provider.config.name.clone(),
                kind: infer_agent_kind(&provider.config),
                model: provider.config.model.clone(),
                available: true,
                capabilities: provider.capabilities.clone(),
                trusted_for_restricted: provider
                    .capabilities
                    .iter()
                    .any(|value| value == "restricted-data"),
                health_score_bps: 10_000,
                p95_latency_ms: 0,
                estimated_cost_micro_usd: pricing.fixed_call_reserve_micro_usd,
                max_context_tokens: pricing.max_context_tokens,
            });
        }
        if candidates.len() != planned.len() {
            return Err(AdmissionClientError::InvalidConfig(
                "workflow assignments do not all have configured providers".into(),
            ));
        }
        let requested_budget = workflow
            .plan
            .meta
            .get("requested_budget")
            .cloned()
            .map(serde_json::from_value::<RequestedBudget>)
            .transpose()
            .map_err(|_| {
                AdmissionClientError::InvalidConfig(
                    "workflow requested_budget metadata is invalid".into(),
                )
            })?
            .unwrap_or_default();
        let data_sensitivity = workflow
            .plan
            .meta
            .get("data_sensitivity")
            .cloned()
            .map(serde_json::from_value::<DataSensitivity>)
            .transpose()
            .map_err(|_| {
                AdmissionClientError::InvalidConfig(
                    "workflow data_sensitivity metadata is invalid".into(),
                )
            })?
            .unwrap_or_default();
        let expected_duration_ms = workflow
            .plan
            .meta
            .get("expected_duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Ok(PolicyRequest {
            task_risk: risk_for_mode(workflow.plan.mode),
            data_sensitivity,
            requested_mode: Some(workflow.plan.mode),
            required_capabilities: workflow.plan.required_capabilities.clone(),
            requires_repository_write: workflow
                .plan
                .file_lease
                .as_ref()
                .is_some_and(|lease| lease.required),
            expected_duration_ms,
            requested_budget,
            providers: candidates,
        })
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, AdmissionClientError> {
        let url = self.base_url.join(path).map_err(|_| {
            AdmissionClientError::InvalidConfig("admission route is invalid".into())
        })?;
        if url.origin() != self.base_url.origin() {
            return Err(AdmissionClientError::InvalidConfig(
                "admission route changed the configured origin".into(),
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
            .map_err(|_| AdmissionClientError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(AdmissionClientError::HttpStatus(status));
        }
        let bytes = read_bounded(response, self.max_response_bytes).await?;
        serde_json::from_slice(&bytes).map_err(|_| AdmissionClientError::InvalidResponse)
    }
}

fn validate_active(admission: AdmissionRecord) -> Result<AdmissionRecord, AdmissionClientError> {
    if admission.status == AdmissionStatus::Active {
        Ok(admission)
    } else {
        Err(AdmissionClientError::NotActive)
    }
}

fn risk_for_mode(mode: WorkflowMode) -> TaskRisk {
    match mode {
        WorkflowMode::Single => TaskRisk::Low,
        WorkflowMode::Sequential => TaskRisk::Medium,
        WorkflowMode::Competitive | WorkflowMode::Consensus => TaskRisk::High,
    }
}

fn conservative_input_tokens(request: &ProviderRequest) -> u64 {
    let prompt = u64::try_from(request.prompt.len()).unwrap_or(u64::MAX);
    let system = request
        .system
        .as_ref()
        .map(|value| u64::try_from(value.len()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    prompt
        .saturating_add(system)
        .saturating_add(INPUT_TOKEN_OVERHEAD)
}

fn price_usage(pricing: &ProviderPricing, input_tokens: u64, output_tokens: u64) -> u64 {
    let input =
        u128::from(input_tokens).saturating_mul(u128::from(pricing.input_micro_usd_per_million));
    let output =
        u128::from(output_tokens).saturating_mul(u128::from(pricing.output_micro_usd_per_million));
    let total = input.saturating_add(output).saturating_add(999_999) / 1_000_000;
    u64::try_from(total).unwrap_or(u64::MAX)
}

fn find_first_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = find_u64(value, key) {
            return Some(value);
        }
    }
    None
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

async fn read_bounded(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, AdmissionClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(AdmissionClientError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| AdmissionClientError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(AdmissionClientError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_actor(value: &str) -> Result<(), AdmissionClientError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 120 || value.chars().any(char::is_control) {
        return Err(AdmissionClientError::InvalidConfig(
            "admission actor must be 1-120 printable bytes".into(),
        ));
    }
    Ok(())
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

    fn pricing() -> ProviderPricing {
        ProviderPricing {
            input_micro_usd_per_million: 10,
            output_micro_usd_per_million: 20,
            fixed_call_reserve_micro_usd: 3,
            max_context_tokens: 1000,
        }
    }

    #[test]
    fn pricing_rounds_up_to_one_micro_dollar() {
        assert_eq!(price_usage(&pricing(), 1, 1), 1);
    }

    #[test]
    fn conservative_reservation_includes_system_and_overhead() {
        let request = ProviderRequest {
            prompt: "abc".into(),
            max_output_tokens: 10,
            system: Some("xy".into()),
        };
        assert_eq!(conservative_input_tokens(&request), 261);
        let token_cost = price_usage(&pricing(), 261, 10);
        assert_eq!(
            token_cost.saturating_add(pricing().fixed_call_reserve_micro_usd),
            4
        );
    }

    #[test]
    fn finds_common_provider_usage_keys() {
        let usage = json!({"usage":{"prompt_tokens":10,"completion_tokens":4}});
        assert_eq!(
            find_first_u64(&usage, &["input_tokens", "prompt_tokens"]),
            Some(10)
        );
        assert_eq!(
            find_first_u64(&usage, &["output_tokens", "completion_tokens"]),
            Some(4)
        );
    }
}
