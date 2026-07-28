mod admission;
mod bridge;
mod claims;
mod heartbeat;
mod work;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::orchestration::{WorkflowAssignment, WorkflowStage, WorkflowView};
use crate::providers::{parse_provider_configs, ProviderClient, ProviderConfig};
use crate::types::AgentKind;

use admission::AdmissionControl;
use bridge::{BridgeClient, LeaseHandle};
use claims::ClaimConfig;
use heartbeat::{run_with_heartbeat, HeartbeatOutcome};
use work::{
    configured_capabilities, eligible_assignment, failure_meta, infer_agent_kind, protocol_label,
    provider_request, success_meta,
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
    admission: AdmissionControl,
    providers: Arc<Vec<ProviderWorker>>,
    claims: ClaimConfig,
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
        let capability_map = capability_map_from_env()?;
        let providers = configs
            .into_iter()
            .map(|config| {
                let capabilities =
                    configured_capabilities(&config.name, &capability_map, config.protocol);
                let client = ProviderClient::from_config(config.clone())?;
                Ok(ProviderWorker {
                    config,
                    client,
                    capabilities,
                })
            })
            .collect::<Result<Vec<_>, crate::providers::ProviderError>>()?;
        let max_concurrency = env_usize("AI_AGENT_RUNNER_MAX_CONCURRENCY", DEFAULT_MAX_CONCURRENCY)
            .clamp(1, MAX_CONCURRENCY);
        let poll_interval_ms =
            env_u64("AI_AGENT_RUNNER_POLL_INTERVAL_MS", DEFAULT_POLL_INTERVAL_MS).max(250);
        let lease_safety_margin_ms = env_u64(
            "AI_AGENT_RUNNER_LEASE_SAFETY_MARGIN_MS",
            DEFAULT_LEASE_SAFETY_MARGIN_MS,
        );
        let max_output_tokens = env_u32(
            "AI_AGENT_RUNNER_MAX_OUTPUT_TOKENS",
            DEFAULT_MAX_OUTPUT_TOKENS,
        )
        .clamp(1, MAX_OUTPUT_TOKENS);
        let claims = ClaimConfig::from_env()?;
        if claims.enabled
            && heartbeat::renewal_delay(claims.ttl_ms, lease_safety_margin_ms).is_none()
        {
            anyhow::bail!(
                "assignment claim TTL must exceed the runner lease safety margin by at least 250ms"
            );
        }
        let admission = AdmissionControl::from_env(&providers, &claims)?;
        Ok(Self {
            bridge: BridgeClient::from_env()?,
            admission,
            providers: Arc::new(providers),
            claims,
            poll_interval: Duration::from_millis(poll_interval_ms),
            max_concurrency,
            concurrency: Arc::new(Semaphore::new(max_concurrency)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            lease_safety_margin_ms,
            max_output_tokens,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        self.register_claim_owner().await?;
        self.register_providers().await?;
        info!(
            providers = self.providers.len(),
            max_concurrency = self.max_concurrency,
            poll_interval_ms = self.poll_interval.as_millis(),
            distributed_claims = self.claims.enabled,
            declared_replicas = self.claims.replica_count,
            runner_instance = %self.claims.instance_id,
            admission_actor = %self.admission.actor(),
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

    async fn register_claim_owner(&self) -> anyhow::Result<()> {
        if !self.claims.enabled {
            return Ok(());
        }
        self.bridge
            .register_agent(
                &self.claims.owner,
                &format!("AI agent runner {}", self.claims.instance_id),
                AgentKind::Other,
                &["assignment-claims".into(), "fiducia-leases".into()],
                "fiducia",
                "assignment-claim",
            )
            .await?;
        info!(
            claim_owner = %self.claims.owner,
            claim_repository = %self.claims.repository,
            claim_ttl_ms = self.claims.ttl_ms,
            "registered distributed assignment-claim owner"
        );
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
            if workflow.status.pending_agents.is_empty() {
                continue;
            }
            let admission = match self.admission.ensure(&workflow, &self.providers).await {
                Ok(admission) => admission,
                Err(error) => {
                    warn!(
                        workflow_id = %workflow.plan.id,
                        %error,
                        "workflow is not admitted; no provider calls will run"
                    );
                    continue;
                }
            };
            for provider in self.providers.iter() {
                if !admission
                    .policy
                    .selected_agent_keys
                    .iter()
                    .any(|agent| agent == &provider.config.name)
                {
                    continue;
                }
                let Some(assignment) =
                    eligible_assignment(&workflow, &provider.config.name).cloned()
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
                let admission = self.admission.clone();
                let provider = provider.clone();
                let workflow = workflow.clone();
                let claims = self.claims.clone();
                let in_flight = self.in_flight.clone();
                let lease_safety_margin_ms = self.lease_safety_margin_ms;
                let max_output_tokens = self.max_output_tokens;
                tokio::spawn(async move {
                    let _permit = permit;
                    execute_assignment(
                        bridge,
                        admission,
                        provider,
                        workflow,
                        assignment,
                        claims,
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

#[allow(clippy::too_many_arguments)]
async fn execute_assignment(
    bridge: BridgeClient,
    admission: AdmissionControl,
    provider: ProviderWorker,
    workflow: WorkflowView,
    assignment: WorkflowAssignment,
    claims: ClaimConfig,
    lease_safety_margin_ms: u64,
    max_output_tokens: u32,
) {
    let claim = if claims.enabled {
        let path = claims.path(&workflow.plan.id, assignment.ordinal);
        match bridge
            .acquire_lease(
                "/file-leases/acquire",
                "/file-leases/release",
                &claims.repository,
                &[path],
                &claims.owner,
                claims.ttl_ms,
            )
            .await
        {
            Ok(handle) => Some(handle),
            Err(error) => {
                info!(
                    workflow_id = %workflow.plan.id,
                    assignment_ordinal = assignment.ordinal,
                    agent_key = %provider.config.name,
                    runner_instance = %claims.instance_id,
                    %error,
                    "assignment claim not acquired; another runner may own the work"
                );
                return;
            }
        }
    } else {
        None
    };

    let mut file_lease: Option<LeaseHandle> = None;
    if let Some(requirement) = workflow
        .plan
        .file_lease
        .as_ref()
        .filter(|requirement| requirement.required)
    {
        if heartbeat::renewal_delay(requirement.ttl_ms, lease_safety_margin_ms).is_none() {
            warn!(
                workflow_id = %workflow.plan.id,
                agent_key = %provider.config.name,
                ttl_ms = requirement.ttl_ms,
                safety_margin_ms = lease_safety_margin_ms,
                "skipping assignment because the lease TTL cannot preserve its heartbeat margin"
            );
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
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
            Ok(handle) => file_lease = Some(handle),
            Err(error) => {
                warn!(
                    workflow_id = %workflow.plan.id,
                    agent_key = %provider.config.name,
                    %error,
                    "Fiducia file lease acquisition failed; assignment remains pending"
                );
                release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
                return;
            }
        }
    }

    let phase_concurrency = workflow
        .plan
        .assignments
        .iter()
        .filter(|candidate| candidate.phase == assignment.phase)
        .count();
    let phase_concurrency = u8::try_from(phase_concurrency).unwrap_or(u8::MAX);
    let reserved = match admission
        .reserve_call(
            &workflow.plan.id,
            &provider.config.name,
            phase_concurrency.max(1),
        )
        .await
    {
        Ok(admission) => admission,
        Err(error) => {
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                %error,
                "policy budget did not admit another provider call"
            );
            release_file_lease(
                &bridge,
                file_lease.as_ref(),
                &provider.config.name,
                &workflow,
            )
            .await;
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
            return;
        }
    };
    let remaining_output_tokens = reserved
        .policy
        .budget
        .max_output_tokens
        .saturating_sub(reserved.usage.output_tokens);
    if remaining_output_tokens == 0 {
        let _ = admission
            .cancel(
                &workflow.plan.id,
                "output token budget exhausted before provider call",
            )
            .await;
        release_file_lease(
            &bridge,
            file_lease.as_ref(),
            &provider.config.name,
            &workflow,
        )
        .await;
        release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
        return;
    }
    let effective_max_output_tokens = u64::from(max_output_tokens)
        .min(remaining_output_tokens)
        .try_into()
        .unwrap_or(max_output_tokens);

    let file_fencing_token = file_lease.as_ref().map(|handle| handle.fencing_token);
    let request = provider_request(&workflow, &assignment, effective_max_output_tokens);
    let heartbeat_ttl_ms = claim
        .iter()
        .chain(file_lease.iter())
        .map(|handle| handle.ttl_ms)
        .min();
    let started = Instant::now();
    let execution = if let Some(ttl_ms) = heartbeat_ttl_ms {
        let renewal_bridge = bridge.clone();
        let renewal_claim = claim.clone();
        let renewal_file_lease = file_lease.clone();
        let claim_owner = claims.owner.clone();
        let provider_agent_key = provider.config.name.clone();
        run_with_heartbeat(
            provider.client.execute(&request),
            ttl_ms,
            lease_safety_margin_ms,
            move || {
                let bridge = renewal_bridge.clone();
                let claim = renewal_claim.clone();
                let file_lease = renewal_file_lease.clone();
                let claim_owner = claim_owner.clone();
                let provider_agent_key = provider_agent_key.clone();
                async move {
                    if let Some(handle) = claim.as_ref() {
                        bridge.renew_lease(handle, &claim_owner).await?;
                    }
                    if let Some(handle) = file_lease.as_ref() {
                        bridge.renew_lease(handle, &provider_agent_key).await?;
                    }
                    Ok::<_, bridge::BridgeClientError>(())
                }
            },
        )
        .await
    } else {
        HeartbeatOutcome::Completed {
            output: provider.client.execute(&request).await,
            renewals: 0,
        }
    };
    let elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    let (content, mut meta, renewals) = match execution {
        HeartbeatOutcome::Completed {
            output: Ok(response),
            renewals,
        } => {
            if let Err(error) = admission
                .report_response(
                    &workflow.plan.id,
                    &provider.config.name,
                    &response.usage,
                    elapsed_ms,
                )
                .await
            {
                warn!(
                    workflow_id = %workflow.plan.id,
                    assignment_ordinal = assignment.ordinal,
                    agent_key = %provider.config.name,
                    %error,
                    "provider output exceeded or could not prove the admitted budget; discarded"
                );
                release_file_lease(
                    &bridge,
                    file_lease.as_ref(),
                    &provider.config.name,
                    &workflow,
                )
                .await;
                release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
                return;
            }
            (
                response.text,
                success_meta(
                    &response.provider,
                    &response.model,
                    provider.config.protocol,
                    response.request_id.as_deref(),
                    response.usage,
                    file_fencing_token,
                ),
                renewals,
            )
        }
        HeartbeatOutcome::Completed {
            output: Err(error),
            renewals,
        } => {
            let error = error.to_string();
            let synthetic_usage = json!({
                "input_tokens": 0,
                "output_tokens": 0
            });
            if let Err(accounting_error) = admission
                .report_response(
                    &workflow.plan.id,
                    &provider.config.name,
                    &synthetic_usage,
                    elapsed_ms,
                )
                .await
            {
                warn!(
                    workflow_id = %workflow.plan.id,
                    assignment_ordinal = assignment.ordinal,
                    agent_key = %provider.config.name,
                    %accounting_error,
                    "failed provider call could not be accounted; workflow output discarded"
                );
                release_file_lease(
                    &bridge,
                    file_lease.as_ref(),
                    &provider.config.name,
                    &workflow,
                )
                .await;
                release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
                return;
            }
            (
                format!("Provider execution failed: {error}"),
                failure_meta(
                    &provider.config.name,
                    &provider.config.model,
                    provider.config.protocol,
                    &error,
                    file_fencing_token,
                ),
                renewals,
            )
        }
        HeartbeatOutcome::LeaseLost { error, renewals } => {
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                file_fencing_token = ?file_fencing_token,
                claim_fencing_token = ?claim.as_ref().map(|handle| handle.fencing_token),
                successful_renewals = renewals,
                %error,
                "assignment or file lease heartbeat failed; provider output discarded"
            );
            let _ = admission
                .cancel(&workflow.plan.id, "Fiducia heartbeat failed")
                .await;
            release_file_lease(
                &bridge,
                file_lease.as_ref(),
                &provider.config.name,
                &workflow,
            )
            .await;
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
            return;
        }
    };

    annotate_heartbeat(&mut meta, claim.is_some(), file_lease.is_some(), renewals);
    if let Some(object) = meta.as_object_mut() {
        object.insert("policy_admitted".into(), json!(true));
        object.insert(
            "policy_version".into(),
            json!(reserved.policy.policy_version),
        );
        object.insert("provider_elapsed_ms".into(), json!(elapsed_ms));
    }
    if let Some(handle) = claim.as_ref() {
        if let Err(error) = bridge.renew_lease(handle, &claims.owner).await {
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                runner_instance = %claims.instance_id,
                claim_fencing_token = handle.fencing_token,
                %error,
                "assignment claim became stale before submission; provider output discarded"
            );
            let _ = admission
                .cancel(
                    &workflow.plan.id,
                    "assignment claim became stale before submission",
                )
                .await;
            release_file_lease(
                &bridge,
                file_lease.as_ref(),
                &provider.config.name,
                &workflow,
            )
            .await;
            release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
            return;
        }
        if let Some(object) = meta.as_object_mut() {
            object.insert(
                "assignment_claim".into(),
                claims.metadata(&workflow.plan.id, assignment.ordinal, handle.fencing_token),
            );
        }
    }

    match bridge
        .submit(&workflow.plan.id, &provider.config.name, &content, meta)
        .await
    {
        Ok(updated_workflow) => {
            info!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                runner_instance = %claims.instance_id,
                lease_renewals = renewals,
                "provider workflow submission accepted"
            );
            if updated_workflow.status.stage == WorkflowStage::Completed {
                if let Err(error) = admission.complete(&workflow.plan.id).await {
                    warn!(
                        workflow_id = %workflow.plan.id,
                        %error,
                        "workflow completed but policy admission could not be finalized"
                    );
                }
            }
        }
        Err(error) => {
            error!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                runner_instance = %claims.instance_id,
                %error,
                "provider result could not be submitted"
            );
            let _ = admission
                .cancel(
                    &workflow.plan.id,
                    "workflow submission failed after provider execution",
                )
                .await;
        }
    }

    release_file_lease(
        &bridge,
        file_lease.as_ref(),
        &provider.config.name,
        &workflow,
    )
    .await;
    release_claim(&bridge, claim.as_ref(), &claims, &workflow, &assignment).await;
}

async fn release_file_lease(
    bridge: &BridgeClient,
    lease: Option<&LeaseHandle>,
    agent_key: &str,
    workflow: &WorkflowView,
) {
    let Some(handle) = lease else {
        return;
    };
    if let Err(error) = bridge.release_lease(handle, agent_key).await {
        warn!(
            workflow_id = %workflow.plan.id,
            agent_key,
            fencing_token = handle.fencing_token,
            %error,
            "Fiducia file lease release failed; waiting for TTL expiry"
        );
    }
}

async fn release_claim(
    bridge: &BridgeClient,
    claim: Option<&LeaseHandle>,
    claims: &ClaimConfig,
    workflow: &WorkflowView,
    assignment: &WorkflowAssignment,
) {
    let Some(handle) = claim else {
        return;
    };
    if let Err(error) = bridge.release_lease(handle, &claims.owner).await {
        warn!(
            workflow_id = %workflow.plan.id,
            assignment_ordinal = assignment.ordinal,
            runner_instance = %claims.instance_id,
            fencing_token = handle.fencing_token,
            %error,
            "assignment claim release failed; waiting for TTL expiry"
        );
    }
}

fn annotate_heartbeat(meta: &mut Value, claimed: bool, file_leased: bool, renewals: u64) {
    if let Some(object) = meta.as_object_mut() {
        object.insert("assignment_claimed".into(), json!(claimed));
        object.insert("file_leased".into(), json!(file_leased));
        object.insert("lease_renewals".into(), json!(renewals));
        object.insert("lease_heartbeat_failed".into(), json!(false));
    }
}

fn capability_map_from_env() -> anyhow::Result<Value> {
    let Some(raw) = std::env::var("AI_PROVIDER_CAPABILITIES_JSON")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(json!({}));
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("AI_PROVIDER_CAPABILITIES_JSON is invalid JSON"))?;
    if !value.is_object() {
        anyhow::bail!("AI_PROVIDER_CAPABILITIES_JSON must be a JSON object");
    }
    Ok(value)
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
