mod bridge;
mod work;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::orchestration::{WorkflowAssignment, WorkflowView};
use crate::providers::{parse_provider_configs, ProviderClient, ProviderConfig};

use bridge::{BridgeClient, LeaseHandle};
use work::{
    configured_capabilities, eligible_assignment, failure_meta, infer_agent_kind, lease_is_safe,
    protocol_label, provider_request, success_meta,
};

const DEFAULT_POLL_INTERVAL_MS: u64 = 5_000;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY: usize = 64;
const DEFAULT_LEASE_SAFETY_MARGIN_MS: u64 = 15_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_OUTPUT_TOKENS: u32 = 65_536;

#[derive(Clone)]
struct ProviderWorker {
    config: ProviderConfig,
    client: ProviderClient,
    capabilities: Vec<String>,
}

pub struct Runner {
    bridge: BridgeClient,
    providers: Arc<Vec<ProviderWorker>>,
    poll_interval: Duration,
    max_concurrency: usize,
    concurrency: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<(String, usize)>>>,
    lease_safety_margin_ms: u64,
    max_output_tokens: u32,
}

impl Runner {
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("AI_PROVIDER_CONFIG_JSON")
            .map_err(|_| anyhow::anyhow!("AI_PROVIDER_CONFIG_JSON is required"))?;
        let configs = parse_provider_configs(&raw)?;
        let capability_map = env_json("AI_PROVIDER_CAPABILITIES_JSON").unwrap_or_else(|| json!({}));
        if !capability_map.is_object() {
            anyhow::bail!("AI_PROVIDER_CAPABILITIES_JSON must be a JSON object");
        }
        let providers = configs
            .into_iter()
            .map(|config| {
                let capabilities = configured_capabilities(
                    &config.name,
                    &capability_map,
                    config.protocol,
                );
                let client = ProviderClient::from_config(config.clone())?;
                Ok(ProviderWorker {
                    config,
                    client,
                    capabilities,
                })
            })
            .collect::<Result<Vec<_>, crate::providers::ProviderError>>()?;
        let max_concurrency = env_usize(
            "AI_AGENT_RUNNER_MAX_CONCURRENCY",
            DEFAULT_MAX_CONCURRENCY,
        )
        .clamp(1, MAX_CONCURRENCY);
        let poll_interval_ms = env_u64(
            "AI_AGENT_RUNNER_POLL_INTERVAL_MS",
            DEFAULT_POLL_INTERVAL_MS,
        )
        .max(250);
        let lease_safety_margin_ms = env_u64(
            "AI_AGENT_RUNNER_LEASE_SAFETY_MARGIN_MS",
            DEFAULT_LEASE_SAFETY_MARGIN_MS,
        );
        let max_output_tokens = env_u32(
            "AI_AGENT_RUNNER_MAX_OUTPUT_TOKENS",
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .clamp(1, MAX_OUTPUT_TOKENS);
        Ok(Self {
            bridge: BridgeClient::from_env()?,
            providers: Arc::new(providers),
            poll_interval: Duration::from_millis(poll_interval_ms),
            max_concurrency,
            concurrency: Arc::new(Semaphore::new(max_concurrency)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            lease_safety_margin_ms,
            max_output_tokens,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.register_providers().await?;
        info!(
            providers = self.providers.len(),
            max_concurrency = self.max_concurrency,
            poll_interval_ms = self.poll_interval.as_millis(),
            "provider runner started"
        );

        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown_signal() => {
                    info!("provider runner shutdown requested");
                    break;
                }
                _ = interval.tick() => self.poll_once().await,
            }
        }

        match self
            .concurrency
            .clone()
            .acquire_many_owned(self.max_concurrency as u32)
            .await
        {
            Ok(_all_permits) => info!("provider runner work drained"),
            Err(error) => warn!(%error, "provider runner semaphore closed during drain"),
        }
        Ok(())
    }

    async fn register_providers(&self) -> anyhow::Result<()> {
        for provider in self.providers.iter() {
            let kind = infer_agent_kind(&provider.config);
            self.bridge
                .register_agent(
                    &provider.config.name,
                    &format!("{} ({})", provider.config.name, provider.config.model),
                    kind,
                    &provider.capabilities,
                    &provider.config.name,
                    &provider.config.model,
                )
                .await?;
            info!(
                agent_key = %provider.config.name,
                kind = ?kind,
                protocol = protocol_label(provider.config.protocol),
                "registered provider adapter"
            );
        }
        Ok(())
    }

    async fn poll_once(&self) {
        let workflows = match self.bridge.list_workflows().await {
            Ok(workflows) => workflows,
            Err(error) => {
                warn!(%error, "failed to list workflows");
                return;
            }
        };
        for workflow in workflows {
            for provider in self.providers.iter() {
                let Some(assignment) = eligible_assignment(&workflow, &provider.config.name).cloned()
                else {
                    continue;
                };
                let key = (workflow.plan.id.clone(), assignment.ordinal);
                let permit = match self.concurrency.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => return,
                };
                if !self.in_flight.lock().insert(key.clone()) {
                    drop(permit);
                    continue;
                }
                let bridge = self.bridge.clone();
                let provider = provider.clone();
                let in_flight = self.in_flight.clone();
                let lease_safety_margin_ms = self.lease_safety_margin_ms;
                let max_output_tokens = self.max_output_tokens;
                tokio::spawn(async move {
                    let _permit = permit;
                    execute_assignment(
                        bridge,
                        provider,
                        workflow,
                        assignment,
                        lease_safety_margin_ms,
                        max_output_tokens,
                    )
                    .await;
                    in_flight.lock().remove(&key);
                });
            }
        }
    }
}

pub async fn run_from_env() -> anyhow::Result<()> {
    Runner::from_env()?.run().await
}

async fn execute_assignment(
    bridge: BridgeClient,
    provider: ProviderWorker,
    workflow: WorkflowView,
    assignment: WorkflowAssignment,
    lease_safety_margin_ms: u64,
    max_output_tokens: u32,
) {
    let mut lease: Option<LeaseHandle> = None;
    if let Some(requirement) = workflow
        .plan
        .file_lease
        .as_ref()
        .filter(|requirement| requirement.required)
    {
        if !lease_is_safe(
            requirement,
            provider.config.timeout_secs,
            lease_safety_margin_ms,
        ) {
            warn!(
                workflow_id = %workflow.plan.id,
                agent_key = %provider.config.name,
                ttl_ms = requirement.ttl_ms,
                provider_timeout_secs = provider.config.timeout_secs,
                safety_margin_ms = lease_safety_margin_ms,
                "skipping assignment because the unrenewed lease TTL is unsafe"
            );
            return;
        }
        match bridge
            .acquire_lease(
                &requirement.acquire_path,
                &requirement.release_path,
                &requirement.repository,
                &requirement.paths,
                &provider.config.name,
                requirement.ttl_ms,
            )
            .await
        {
            Ok(handle) => lease = Some(handle),
            Err(error) => {
                warn!(
                    workflow_id = %workflow.plan.id,
                    agent_key = %provider.config.name,
                    %error,
                    "Fiducia lease acquisition failed; assignment remains pending"
                );
                return;
            }
        }
    }

    let fencing_token = lease.as_ref().map(|handle| handle.fencing_token);
    let request = provider_request(&workflow, &assignment, max_output_tokens);
    let (content, meta) = match provider.client.execute(&request).await {
        Ok(response) => (
            response.text,
            success_meta(
                &response.provider,
                &response.model,
                provider.config.protocol,
                response.request_id.as_deref(),
                response.usage,
                fencing_token,
            ),
        ),
        Err(error) => {
            let error = error.to_string();
            (
                format!("Provider execution failed: {error}"),
                failure_meta(
                    &provider.config.name,
                    &provider.config.model,
                    provider.config.protocol,
                    &error,
                    fencing_token,
                ),
            )
        }
    };

    match bridge
        .submit(
            &workflow.plan.id,
            &provider.config.name,
            &content,
            meta,
        )
        .await
    {
        Ok(submission) => info!(
            workflow_id = %workflow.plan.id,
            assignment_ordinal = submission.assignment_ordinal,
            agent_key = %provider.config.name,
            "provider workflow submission accepted"
        ),
        Err(error) => error!(
            workflow_id = %workflow.plan.id,
            assignment_ordinal = assignment.ordinal,
            agent_key = %provider.config.name,
            %error,
            "provider result could not be submitted"
        ),
    }

    if let Some(handle) = lease {
        if let Err(error) = bridge
            .release_lease(&handle, &provider.config.name)
            .await
        {
            warn!(
                workflow_id = %workflow.plan.id,
                agent_key = %provider.config.name,
                fencing_token = handle.fencing_token,
                %error,
                "Fiducia lease release failed; waiting for TTL expiry"
            );
        }
    }
}

fn env_json(key: &str) -> Option<Value> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| serde_json::from_str(&value).ok())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
