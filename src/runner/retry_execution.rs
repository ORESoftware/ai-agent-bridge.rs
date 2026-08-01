use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::orchestration::{WorkflowAssignment, WorkflowStage, WorkflowView};
use crate::providers::{ProviderError, ProviderRequest, ProviderResponse};

use super::admission::{AdmissionControl, UsageReservation};
use super::bridge::BridgeClient;
use super::retry::{RetryDelaySource, RetryPlan, RetryPolicy, RetryReason};
use super::ProviderWorker;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryAbortReason {
    RunnerShutdown,
    AdmissionNotActive,
    WorkflowNotPending,
    GuardUnavailable,
    RetryReservationRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryAudit {
    pub retry_ordinal: u8,
    pub delay_ms: u64,
    pub delay_source: RetryDelaySource,
    pub reason: RetryReason,
}

pub(crate) struct RetrySuccess {
    pub response: ProviderResponse,
    pub reservation: UsageReservation,
    pub audits: Vec<RetryAudit>,
    pub total_delay: Duration,
}

pub(crate) enum RetryRun {
    Success(RetrySuccess),
    FinalFailure {
        error: ProviderError,
        audits: Vec<RetryAudit>,
        total_delay: Duration,
    },
    Aborted {
        reason: RetryAbortReason,
        audits: Vec<RetryAudit>,
        total_delay: Duration,
    },
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    policy: &RetryPolicy,
    guard_interval: Duration,
    seed: &str,
    admission: &AdmissionControl,
    bridge: &BridgeClient,
    provider: &ProviderWorker,
    workflow: &WorkflowView,
    assignment: &WorkflowAssignment,
    request: &ProviderRequest,
    initial_reservation: UsageReservation,
    shutdown: &mut watch::Receiver<bool>,
) -> RetryRun {
    let mut reservation = initial_reservation;
    let mut audits = Vec::new();
    let mut total_delay = Duration::ZERO;

    loop {
        let attempt = guarded(
            provider.client.execute(request),
            guard_interval,
            shutdown,
            admission,
            bridge,
            workflow,
            assignment,
        )
        .await;
        let result = match attempt {
            Ok(result) => result,
            Err(reason) => {
                return RetryRun::Aborted {
                    reason,
                    audits,
                    total_delay,
                }
            }
        };
        match result {
            Ok(response) => {
                return RetryRun::Success(RetrySuccess {
                    response,
                    reservation,
                    audits,
                    total_delay,
                })
            }
            Err(error) => {
                let retry_ordinal = u8::try_from(audits.len())
                    .unwrap_or(u8::MAX)
                    .saturating_add(1);
                let Some(plan) = policy.plan(&error, retry_ordinal, total_delay, seed) else {
                    return RetryRun::FinalFailure {
                        error,
                        audits,
                        total_delay,
                    };
                };

                if let Err(reason) = guarded(
                    tokio::time::sleep(plan.delay),
                    guard_interval,
                    shutdown,
                    admission,
                    bridge,
                    workflow,
                    assignment,
                )
                .await
                {
                    return RetryRun::Aborted {
                        reason,
                        audits,
                        total_delay,
                    };
                }
                total_delay = total_delay.saturating_add(plan.delay);

                let reserved = guarded(
                    admission.reserve_retry(
                        &workflow.plan.id,
                        &provider.config.name,
                        request,
                        phase_concurrency(workflow, assignment),
                    ),
                    guard_interval,
                    shutdown,
                    admission,
                    bridge,
                    workflow,
                    assignment,
                )
                .await;
                reservation = match reserved {
                    Ok(Ok((_, reservation))) => reservation,
                    Ok(Err(_)) => {
                        return RetryRun::Aborted {
                            reason: RetryAbortReason::RetryReservationRejected,
                            audits,
                            total_delay,
                        }
                    }
                    Err(reason) => {
                        return RetryRun::Aborted {
                            reason,
                            audits,
                            total_delay,
                        }
                    }
                };
                audits.push(audit(plan));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn guarded<F, T>(
    future: F,
    guard_interval: Duration,
    shutdown: &mut watch::Receiver<bool>,
    admission: &AdmissionControl,
    bridge: &BridgeClient,
    workflow: &WorkflowView,
    assignment: &WorkflowAssignment,
) -> Result<T, RetryAbortReason>
where
    F: Future<Output = T>,
{
    check_shutdown(shutdown)?;
    check_guard(admission, bridge, workflow, assignment).await?;

    let future = future;
    tokio::pin!(future);
    let guard_sleep = tokio::time::sleep(guard_interval);
    tokio::pin!(guard_sleep);

    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Err(RetryAbortReason::RunnerShutdown);
                }
            }
            () = &mut guard_sleep => {
                check_guard(admission, bridge, workflow, assignment).await?;
                guard_sleep.as_mut().reset(tokio::time::Instant::now() + guard_interval);
            }
        }
    }
}

fn check_shutdown(shutdown: &watch::Receiver<bool>) -> Result<(), RetryAbortReason> {
    if *shutdown.borrow() {
        Err(RetryAbortReason::RunnerShutdown)
    } else {
        Ok(())
    }
}

async fn check_guard(
    admission: &AdmissionControl,
    bridge: &BridgeClient,
    workflow: &WorkflowView,
    assignment: &WorkflowAssignment,
) -> Result<(), RetryAbortReason> {
    admission
        .ensure_active(&workflow.plan.id)
        .await
        .map_err(|_| RetryAbortReason::AdmissionNotActive)?;
    let current = bridge
        .get_workflow(&workflow.plan.id)
        .await
        .map_err(|_| RetryAbortReason::GuardUnavailable)?;
    if current.status.stage == WorkflowStage::Completed
        || current
            .submissions
            .iter()
            .any(|submission| submission.assignment_ordinal == assignment.ordinal)
        || !current
            .status
            .pending_agents
            .iter()
            .any(|agent| agent == &assignment.agent_key)
    {
        return Err(RetryAbortReason::WorkflowNotPending);
    }
    Ok(())
}

fn phase_concurrency(workflow: &WorkflowView, assignment: &WorkflowAssignment) -> u8 {
    let count = workflow
        .plan
        .assignments
        .iter()
        .filter(|candidate| candidate.phase == assignment.phase)
        .count();
    u8::try_from(count).unwrap_or(u8::MAX).max(1)
}

fn audit(plan: RetryPlan) -> RetryAudit {
    RetryAudit {
        retry_ordinal: plan.retry_ordinal,
        delay_ms: plan.delay.as_millis().try_into().unwrap_or(u64::MAX),
        delay_source: plan.delay_source,
        reason: plan.reason,
    }
}

pub(crate) fn audits_json(audits: &[RetryAudit], total_delay: Duration) -> Value {
    json!({
        "count": audits.len(),
        "total_delay_ms": total_delay.as_millis().try_into().unwrap_or(u64::MAX),
        "attempts": audits.iter().map(|audit| json!({
            "retry_ordinal": audit.retry_ordinal,
            "delay_ms": audit.delay_ms,
            "delay_source": match audit.delay_source {
                RetryDelaySource::RetryAfter => "retry_after",
                RetryDelaySource::ExponentialJitter => "exponential_jitter",
            },
            "reason": retry_reason_json(audit.reason),
        })).collect::<Vec<_>>()
    })
}

pub(crate) fn abort_reason(reason: RetryAbortReason) -> &'static str {
    match reason {
        RetryAbortReason::RunnerShutdown => "runner_shutdown",
        RetryAbortReason::AdmissionNotActive => "admission_not_active",
        RetryAbortReason::WorkflowNotPending => "workflow_not_pending",
        RetryAbortReason::GuardUnavailable => "execution_guard_unavailable",
        RetryAbortReason::RetryReservationRejected => "retry_reservation_rejected",
    }
}

fn retry_reason_json(reason: RetryReason) -> Value {
    match reason {
        RetryReason::Connect => json!({"kind":"connect"}),
        RetryReason::Timeout => json!({"kind":"timeout"}),
        RetryReason::HttpStatus(status) => json!({"kind":"http_status","status":status}),
        RetryReason::RateLimited(status) => {
            json!({"kind":"rate_limited","status":status})
        }
        RetryReason::Overloaded(status) => json!({"kind":"overloaded","status":status}),
        RetryReason::TemporarilyUnavailable(status) => {
            json!({"kind":"temporarily_unavailable","status":status})
        }
        RetryReason::ServerError(status) => json!({"kind":"server_error","status":status}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_audit_contains_only_stable_bounded_metadata() {
        let value = audits_json(
            &[RetryAudit {
                retry_ordinal: 1,
                delay_ms: 250,
                delay_source: RetryDelaySource::ExponentialJitter,
                reason: RetryReason::RateLimited(429),
            }],
            Duration::from_millis(250),
        );
        assert_eq!(value["count"], 1);
        assert_eq!(value["attempts"][0]["reason"]["status"], 429);
        let rendered = value.to_string();
        assert!(!rendered.contains("http://"));
        assert!(!rendered.contains("prompt"));
        assert!(!rendered.contains("token"));
    }
}
