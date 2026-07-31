#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:100]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/lib.rs",
    "pub mod runner;\n#[expect(",
    "pub mod runner;\npub mod runner_metrics;\n#[expect(",
)

# Health listener: public metrics route and a stable metrics-only snapshot.
replace_once(
    "src/runner_health.rs",
    "use axum::http::StatusCode;",
    "use axum::http::{header, StatusCode};",
)
replace_once(
    "src/runner_health.rs",
    '''#[derive(Clone, Copy, Debug)]
struct HealthSnapshot {
    ready: bool,
    registered: bool,
    poll_fresh: bool,
    shutting_down: bool,
    last_successful_poll_age_ms: Option<u64>,
}
''',
    '''#[derive(Clone, Copy, Debug)]
struct HealthSnapshot {
    ready: bool,
    registered: bool,
    poll_fresh: bool,
    shutting_down: bool,
    last_successful_poll_age_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RunnerHealthMetricsSnapshot {
    pub(crate) ready: bool,
    pub(crate) registered: bool,
    pub(crate) poll_fresh: bool,
    pub(crate) shutting_down: bool,
    pub(crate) last_successful_poll_age_ms: Option<u64>,
    pub(crate) ready_max_staleness_ms: u64,
    pub(crate) required_agents: u64,
}
''',
)
replace_once(
    "src/runner_health.rs",
    '''        Router::new()
            .route("/healthz", get(liveness))
            .route("/readyz", get(readiness))
            .with_state(self.clone())
''',
    '''        Router::new()
            .route("/healthz", get(liveness))
            .route("/readyz", get(readiness))
            .route("/metrics", get(prometheus_metrics))
            .with_state(self.clone())
''',
)
replace_once(
    "src/runner_health.rs",
    '''    fn mark_shutting_down(&self) {
        self.state.shutting_down.store(true, Ordering::Release);
    }

    fn snapshot(&self, now_ms: u64) -> HealthSnapshot {
''',
    '''    fn mark_shutting_down(&self) {
        self.state.shutting_down.store(true, Ordering::Release);
    }

    pub(crate) fn metrics_snapshot(&self) -> RunnerHealthMetricsSnapshot {
        let snapshot = self.snapshot(now_ms());
        RunnerHealthMetricsSnapshot {
            ready: snapshot.ready,
            registered: snapshot.registered,
            poll_fresh: snapshot.poll_fresh,
            shutting_down: snapshot.shutting_down,
            last_successful_poll_age_ms: snapshot.last_successful_poll_age_ms,
            ready_max_staleness_ms: self.config.ready_max_staleness_ms,
            required_agents: self.required_agents.len() as u64,
        }
    }

    fn snapshot(&self, now_ms: u64) -> HealthSnapshot {
''',
)
replace_once(
    "src/runner_health.rs",
    '''async fn readiness(State(health): State<RunnerHealth>) -> Response {
''',
    '''async fn prometheus_metrics(State(health): State<RunnerHealth>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::runner_metrics::global().render(health.metrics_snapshot()),
    )
}

async fn readiness(State(health): State<RunnerHealth>) -> Response {
''',
)

# Admission reservations and accepted usage are the accounting source of truth.
replace_once(
    "src/runner/admission.rs",
    "use crate::providers::ProviderRequest;",
    '''use crate::providers::ProviderRequest;
use crate::runner_metrics::{self, AdmissionResult, ReservationKind};''',
)
replace_once(
    "src/runner/admission.rs",
    '''        let admission = self
            .report_usage(
                workflow_id,
                provider_agent_key,
                UsageDelta {
                    input_tokens,
                    output_tokens,
                    cost_micro_usd,
                    retries: if is_retry { 1 } else { 0 },
                    provider_calls: 1,
                    concurrency,
                    ..UsageDelta::default()
                },
            )
            .await?;
        Ok((
''',
    '''        let admission = self
            .report_usage(
                workflow_id,
                provider_agent_key,
                UsageDelta {
                    input_tokens,
                    output_tokens,
                    cost_micro_usd,
                    retries: if is_retry { 1 } else { 0 },
                    provider_calls: 1,
                    concurrency,
                    ..UsageDelta::default()
                },
            )
            .await?;
        runner_metrics::global().reservation(
            if is_retry {
                ReservationKind::Retry
            } else {
                ReservationKind::Initial
            },
            input_tokens,
            output_tokens,
            cost_micro_usd,
        );
        Ok((
''',
)
replace_once(
    "src/runner/admission.rs",
    '''        self.report_usage(
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
''',
    '''        let admission = self
            .report_usage(
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
            .await?;
        runner_metrics::global().actual_usage(input_tokens, output_tokens, actual_cost);
        Ok(admission)
''',
)
replace_once(
    "src/runner/admission.rs",
    '''        let response: AdmissionResponse = self
            .request(
                Method::POST,
                &format!("workflows/{workflow_id}/admission/{action}"),
                Some(json!({"updated_by":self.actor,"reason":reason})),
            )
            .await?;
        Ok(response.admission)
''',
    '''        let response: Result<AdmissionResponse, AdmissionClientError> = self
            .request(
                Method::POST,
                &format!("workflows/{workflow_id}/admission/{action}"),
                Some(json!({"updated_by":self.actor,"reason":reason})),
            )
            .await;
        match response {
            Ok(response) => {
                runner_metrics::global().admission(match action {
                    "complete" => AdmissionResult::Completed,
                    "cancel" => AdmissionResult::Cancelled,
                    _ => AdmissionResult::Error,
                });
                Ok(response.admission)
            }
            Err(error) => {
                runner_metrics::global().admission(AdmissionResult::Error);
                Err(error)
            }
        }
''',
)

# Retry attempt boundaries, accepted plans, and aborts.
replace_once(
    "src/runner/retry_execution.rs",
    "use crate::providers::{ProviderError, ProviderRequest, ProviderResponse};",
    '''use crate::providers::{ProviderError, ProviderRequest, ProviderResponse};
use crate::runner_metrics::{
    self, AttemptResult, RetryDelayMetric, RetryReasonMetric,
};''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''    loop {
        let attempt = guarded(
''',
    '''    loop {
        let attempt_started = runner_metrics::global().attempt_started();
        let attempt = guarded(
''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''            Err(reason) => {
                return RetryRun::Aborted {
''',
    '''            Err(reason) => {
                runner_metrics::global().attempt_finished(attempt_started, AttemptResult::Aborted);
                return RetryRun::Aborted {
''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''            Ok(response) => {
                return RetryRun::Success(RetrySuccess {
''',
    '''            Ok(response) => {
                runner_metrics::global().attempt_finished(attempt_started, AttemptResult::Success);
                return RetryRun::Success(RetrySuccess {
''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''            Err(error) => {
                let retry_ordinal = u8::try_from(audits.len())
''',
    '''            Err(error) => {
                runner_metrics::global().attempt_finished(attempt_started, AttemptResult::Failure);
                let retry_ordinal = u8::try_from(audits.len())
''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''                if let Err(reason) = guarded(
                    tokio::time::sleep(plan.delay),
''',
    '''                runner_metrics::global().retry(
                    retry_reason_metric(plan.reason),
                    match plan.delay_source {
                        RetryDelaySource::RetryAfter => RetryDelayMetric::RetryAfter,
                        RetryDelaySource::ExponentialJitter => {
                            RetryDelayMetric::ExponentialJitter
                        }
                    },
                    plan.delay,
                );
                if let Err(reason) = guarded(
                    tokio::time::sleep(plan.delay),
''',
)
replace_once(
    "src/runner/retry_execution.rs",
    '''fn retry_reason_json(reason: RetryReason) -> Value {
''',
    '''fn retry_reason_metric(reason: RetryReason) -> RetryReasonMetric {
    match reason {
        RetryReason::Connect => RetryReasonMetric::Connect,
        RetryReason::Timeout => RetryReasonMetric::Timeout,
        RetryReason::HttpStatus(_) => RetryReasonMetric::HttpStatus,
        RetryReason::RateLimited(_) => RetryReasonMetric::RateLimited,
        RetryReason::Overloaded(_) => RetryReasonMetric::Overloaded,
        RetryReason::TemporarilyUnavailable(_) => RetryReasonMetric::TemporarilyUnavailable,
        RetryReason::ServerError(_) => RetryReasonMetric::ServerError,
    }
}

fn retry_reason_json(reason: RetryReason) -> Value {
''',
)

# Runner lifecycle and assignment authority.
replace_once(
    "src/runner/mod.rs",
    "use crate::providers::{parse_provider_configs, ProviderClient, ProviderConfig};",
    '''use crate::providers::{parse_provider_configs, ProviderClient, ProviderConfig};
use crate::runner_metrics::{
    self, AdmissionResult as AdmissionMetricResult, AssignmentResult, LeaseKind, LeaseResult,
    SubmissionResult,
};''',
)
replace_once(
    "src/runner/mod.rs",
    '''        let admission = AdmissionControl::from_env(&providers, &claims)?;
        let retry_policies = RetryPolicies::from_env(&providers)?;
''',
    '''        let admission = AdmissionControl::from_env(&providers, &claims)?;
        let retry_policies = RetryPolicies::from_env(&providers)?;
        runner_metrics::global().configure(providers.len(), max_concurrency, claims.enabled);
''',
)
replace_once(
    "src/runner/mod.rs",
    '''    pub async fn run(self) -> anyhow::Result<()> {
        self.register_claim_owner().await?;
        self.register_providers().await?;
''',
    '''    pub async fn run(self) -> anyhow::Result<()> {
        let registration = async {
            self.register_claim_owner().await?;
            self.register_providers().await
        }
        .await;
        runner_metrics::global().registration(registration.is_ok());
        registration?;
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        match self
            .concurrency
''',
    '''        let _drain_metrics = runner_metrics::global().drain();
        match self
            .concurrency
''',
)
replace_once(
    "src/runner/mod.rs",
    '''    async fn poll_once(&self) {
        let workflows = match self.bridge.list_workflows().await {
            Ok(workflows) => workflows,
            Err(error) => {
''',
    '''    async fn poll_once(&self) {
        let poll_started = Instant::now();
        let workflows = match self.bridge.list_workflows().await {
            Ok(workflows) => {
                runner_metrics::global().poll(true, poll_started.elapsed());
                workflows
            }
            Err(error) => {
                runner_metrics::global().poll(false, poll_started.elapsed());
''',
)
replace_once(
    "src/runner/mod.rs",
    '''            let admission = match self.admission.ensure(&workflow, &self.providers).await {
                Ok(admission) => admission,
                Err(error) => {
''',
    '''            let admission = match self.admission.ensure(&workflow, &self.providers).await {
                Ok(admission) => {
                    runner_metrics::global().admission(AdmissionMetricResult::Admitted);
                    admission
                }
                Err(error) => {
                    runner_metrics::global().admission(AdmissionMetricResult::Rejected);
''',
)
replace_once(
    "src/runner/mod.rs",
    '''async fn execute_assignment(
    bridge: BridgeClient,
''',
    '''async fn execute_assignment(
    bridge: BridgeClient,
''',
)
replace_once(
    "src/runner/mod.rs",
    ''') {
    let claim = if claims.enabled {
''',
    ''') {
    let mut assignment_metrics = Some(runner_metrics::global().assignment());
    let claim = if claims.enabled {
''',
)
replace_once(
    "src/runner/mod.rs",
    '''            Ok(handle) => Some(handle),
            Err(error) => {
''',
    '''            Ok(handle) => {
                runner_metrics::global().lease(LeaseKind::AssignmentClaim, LeaseResult::Acquired);
                Some(handle)
            }
            Err(error) => {
                runner_metrics::global().lease(LeaseKind::AssignmentClaim, LeaseResult::Unavailable);
''',
)
replace_once(
    "src/runner/mod.rs",
    '''            Ok(handle) => file_lease = Some(handle),
            Err(error) => {
''',
    '''            Ok(handle) => {
                runner_metrics::global().lease(LeaseKind::FileLease, LeaseResult::Acquired);
                file_lease = Some(handle);
            }
            Err(error) => {
                runner_metrics::global().lease(LeaseKind::FileLease, LeaseResult::Unavailable);
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        Err(error) => {
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                %error,
                "policy budget did not admit the provider call"
            );
''',
    '''        Err(error) => {
            runner_metrics::global().admission(AdmissionMetricResult::ReservationRejected);
            warn!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                %error,
                "policy budget did not admit the provider call"
            );
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        HeartbeatOutcome::LeaseLost { error, renewals } => {
            warn!(
''',
    '''        HeartbeatOutcome::LeaseLost { error, renewals } => {
            if claim.is_some() {
                runner_metrics::global().lease(
                    LeaseKind::AssignmentClaim,
                    LeaseResult::RenewalLost,
                );
            }
            if file_lease.is_some() {
                runner_metrics::global().lease(LeaseKind::FileLease, LeaseResult::RenewalLost);
            }
            warn!(
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        if let Err(error) = bridge.renew_lease(handle, &claims.owner).await {
            warn!(
''',
    '''        if let Err(error) = bridge.renew_lease(handle, &claims.owner).await {
            runner_metrics::global().lease(
                LeaseKind::AssignmentClaim,
                LeaseResult::StaleBeforeSubmission,
            );
            warn!(
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        Ok(updated_workflow) => {
            info!(
''',
    '''        Ok(updated_workflow) => {
            runner_metrics::global().submission(SubmissionResult::Accepted);
            if let Some(metrics) = assignment_metrics.take() {
                metrics.finish(AssignmentResult::Submitted);
            }
            info!(
''',
)
replace_once(
    "src/runner/mod.rs",
    '''        Err(error) => {
            error!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                runner_instance = %claims.instance_id,
                %error,
                "provider result could not be submitted"
            );
''',
    '''        Err(error) => {
            runner_metrics::global().submission(SubmissionResult::Rejected);
            if let Some(metrics) = assignment_metrics.take() {
                metrics.finish(AssignmentResult::SubmissionFailed);
            }
            error!(
                workflow_id = %workflow.plan.id,
                assignment_ordinal = assignment.ordinal,
                agent_key = %provider.config.name,
                runner_instance = %claims.instance_id,
                %error,
                "provider result could not be submitted"
            );
''',
)
replace_once(
    "src/runner/mod.rs",
    '''    if let Err(error) = bridge.release_lease(handle, agent_key).await {
        warn!(
''',
    '''    if let Err(error) = bridge.release_lease(handle, agent_key).await {
        runner_metrics::global().lease(LeaseKind::FileLease, LeaseResult::ReleaseFailed);
        warn!(
''',
)
replace_once(
    "src/runner/mod.rs",
    '''            "Fiducia file lease release failed; waiting for TTL expiry"
        );
    }
}
''',
    '''            "Fiducia file lease release failed; waiting for TTL expiry"
        );
    } else {
        runner_metrics::global().lease(LeaseKind::FileLease, LeaseResult::Released);
    }
}
''',
)
replace_once(
    "src/runner/mod.rs",
    '''    if let Err(error) = bridge.release_lease(handle, &claims.owner).await {
        warn!(
''',
    '''    if let Err(error) = bridge.release_lease(handle, &claims.owner).await {
        runner_metrics::global().lease(
            LeaseKind::AssignmentClaim,
            LeaseResult::ReleaseFailed,
        );
        warn!(
''',
)
replace_once(
    "src/runner/mod.rs",
    '''            "assignment claim release failed; waiting for TTL expiry"
        );
    }
}
''',
    '''            "assignment claim release failed; waiting for TTL expiry"
        );
    } else {
        runner_metrics::global().lease(LeaseKind::AssignmentClaim, LeaseResult::Released);
    }
}
''',
)

# Registry self-tests prove valid metadata and bounded labels.
runner_metrics = Path("src/runner_metrics.rs")
text = runner_metrics.read_text(encoding="utf-8")
if "registry_emits_unique_metadata_and_bounded_labels" in text:
    raise SystemExit("runner metrics tests already exist")
text += '''

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> RunnerHealthMetricsSnapshot {
        RunnerHealthMetricsSnapshot {
            ready: true,
            registered: true,
            poll_fresh: true,
            shutting_down: false,
            last_successful_poll_age_ms: Some(500),
            ready_max_staleness_ms: 30_000,
            required_agents: 3,
        }
    }

    #[test]
    fn registry_emits_unique_metadata_and_bounded_labels() {
        let metrics = RunnerMetrics::new();
        metrics.configure(3, 4, true);
        metrics.poll(true, Duration::from_millis(2));
        metrics.registration(true);
        metrics.admission(AdmissionResult::Admitted);
        metrics.reservation(ReservationKind::Initial, 10, 20, 30);
        metrics.actual_usage(8, 7, 12);
        metrics.retry(
            RetryReasonMetric::RateLimited,
            RetryDelayMetric::RetryAfter,
            Duration::from_secs(1),
        );
        metrics.lease(LeaseKind::AssignmentClaim, LeaseResult::Acquired);
        metrics.submission(SubmissionResult::Accepted);
        let attempt = metrics.attempt_started();
        metrics.attempt_finished(attempt, AttemptResult::Success);
        metrics.assignment().finish(AssignmentResult::Submitted);
        {
            let _drain = metrics.drain();
        }

        let text = metrics.render(health());
        let mut help = std::collections::BTreeSet::new();
        let mut metric_types = std::collections::BTreeSet::new();
        for line in text.lines() {
            if let Some(name) = line
                .strip_prefix("# HELP ")
                .and_then(|line| line.split_whitespace().next())
            {
                assert!(help.insert(name), "duplicate HELP metadata for {name}");
            }
            if let Some(name) = line
                .strip_prefix("# TYPE ")
                .and_then(|line| line.split_whitespace().next())
            {
                assert!(metric_types.insert(name), "duplicate TYPE metadata for {name}");
            }
        }
        for expected in [
            "ai_agent_runner_ready 1",
            "ai_agent_runner_polls_total{result=\"success\"} 1",
            "ai_agent_runner_provider_attempts_total{result=\"success\"} 1",
            "ai_agent_runner_retries_total{reason=\"rate_limited\"} 1",
            "ai_agent_runner_reserved_tokens_total{attempt=\"initial\",kind=\"input\"} 10",
            "ai_agent_runner_actual_cost_micro_usd_total 12",
            "ai_agent_runner_submissions_total{result=\"accepted\"} 1",
        ] {
            assert!(text.contains(expected), "missing metric sample: {expected}");
        }
        for forbidden in [
            "provider-name",
            "model-name",
            "agent-key",
            "workflow-id",
            "repository/path",
            "prompt-body",
            "secret-token",
        ] {
            assert!(!text.contains(forbidden));
        }
    }
}
'''
runner_metrics.write_text(text, encoding="utf-8")
