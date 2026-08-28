//! Bounded-cardinality Prometheus metrics for the provider-runner process.
//!
//! The registry never labels metrics with provider/model names, agent keys,
//! workflow IDs, assignment ordinals, repository paths, prompts, outputs,
//! credentials, request IDs, or user-controlled metadata.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::runner_health::RunnerHealthMetricsSnapshot;

const LATENCY_BOUNDS_SECONDS: [f64; 10] = [
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 15.0,
];

pub fn global() -> &'static RunnerMetrics {
    static METRICS: OnceLock<RunnerMetrics> = OnceLock::new();
    METRICS.get_or_init(RunnerMetrics::new)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AssignmentResult {
    Submitted,
    SubmissionFailed,
    Discarded,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AttemptResult {
    Success,
    Failure,
    Aborted,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AdmissionResult {
    Admitted,
    Rejected,
    ReservationRejected,
    Completed,
    Cancelled,
    Error,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RetryReasonMetric {
    Connect,
    Timeout,
    HttpStatus,
    RateLimited,
    Overloaded,
    TemporarilyUnavailable,
    ServerError,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RetryDelayMetric {
    RetryAfter,
    ExponentialJitter,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LeaseKind {
    AssignmentClaim,
    FileLease,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LeaseResult {
    Acquired,
    Unavailable,
    RenewalLost,
    Released,
    ReleaseFailed,
    StaleBeforeSubmission,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SubmissionResult {
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ReservationKind {
    Initial,
    Retry,
}

struct Histogram {
    buckets: [AtomicU64; LATENCY_BOUNDS_SECONDS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
        }
    }

    fn observe(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        for (bound, bucket) in LATENCY_BOUNDS_SECONDS.iter().zip(self.buckets.iter()) {
            if seconds <= *bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(
            duration.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    fn render(&self, output: &mut String, name: &str, help: &str) {
        metric_header(output, name, help, "histogram");
        for (bound, bucket) in LATENCY_BOUNDS_SECONDS.iter().zip(self.buckets.iter()) {
            let _ = writeln!(
                output,
                "{name}_bucket{{le=\"{}\"}} {}",
                format_bound(*bound),
                bucket.load(Ordering::Relaxed)
            );
        }
        let count = self.count.load(Ordering::Relaxed);
        let _ = writeln!(output, "{name}_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(output, "{name}_count {count}");
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let _ = writeln!(output, "{name}_sum {sum}");
    }
}

pub struct RunnerMetrics {
    started_at: Instant,
    started_epoch_seconds: u64,
    providers_configured: AtomicU64,
    max_concurrency: AtomicU64,
    distributed_claims_enabled: AtomicBool,
    polls_success: AtomicU64,
    polls_error: AtomicU64,
    poll_duration: Histogram,
    registrations_success: AtomicU64,
    registrations_error: AtomicU64,
    assignments_active: AtomicU64,
    assignments_started: AtomicU64,
    assignments_submitted: AtomicU64,
    assignments_submission_failed: AtomicU64,
    assignments_discarded: AtomicU64,
    attempts_started: AtomicU64,
    attempts_success: AtomicU64,
    attempts_failure: AtomicU64,
    attempts_aborted: AtomicU64,
    attempt_duration: Histogram,
    retries_connect: AtomicU64,
    retries_timeout: AtomicU64,
    retries_http_status: AtomicU64,
    retries_rate_limited: AtomicU64,
    retries_overloaded: AtomicU64,
    retries_temporarily_unavailable: AtomicU64,
    retries_server_error: AtomicU64,
    retry_after_delay_millis: AtomicU64,
    retry_jitter_delay_millis: AtomicU64,
    admissions_admitted: AtomicU64,
    admissions_rejected: AtomicU64,
    admissions_reservation_rejected: AtomicU64,
    admissions_completed: AtomicU64,
    admissions_cancelled: AtomicU64,
    admissions_error: AtomicU64,
    initial_reserved_input_tokens: AtomicU64,
    initial_reserved_output_tokens: AtomicU64,
    initial_reserved_cost_micro_usd: AtomicU64,
    retry_reserved_input_tokens: AtomicU64,
    retry_reserved_output_tokens: AtomicU64,
    retry_reserved_cost_micro_usd: AtomicU64,
    actual_input_tokens: AtomicU64,
    actual_output_tokens: AtomicU64,
    actual_cost_micro_usd: AtomicU64,
    assignment_claim_acquired: AtomicU64,
    assignment_claim_unavailable: AtomicU64,
    assignment_claim_renewal_lost: AtomicU64,
    assignment_claim_released: AtomicU64,
    assignment_claim_release_failed: AtomicU64,
    assignment_claim_stale_before_submission: AtomicU64,
    file_lease_acquired: AtomicU64,
    file_lease_unavailable: AtomicU64,
    file_lease_renewal_lost: AtomicU64,
    file_lease_released: AtomicU64,
    file_lease_release_failed: AtomicU64,
    file_lease_stale_before_submission: AtomicU64,
    submissions_accepted: AtomicU64,
    submissions_rejected: AtomicU64,
    draining: AtomicBool,
    last_drain_millis: AtomicU64,
}

impl RunnerMetrics {
    fn new() -> Self {
        let started_epoch_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            started_at: Instant::now(),
            started_epoch_seconds,
            providers_configured: AtomicU64::new(0),
            max_concurrency: AtomicU64::new(0),
            distributed_claims_enabled: AtomicBool::new(false),
            polls_success: AtomicU64::new(0),
            polls_error: AtomicU64::new(0),
            poll_duration: Histogram::new(),
            registrations_success: AtomicU64::new(0),
            registrations_error: AtomicU64::new(0),
            assignments_active: AtomicU64::new(0),
            assignments_started: AtomicU64::new(0),
            assignments_submitted: AtomicU64::new(0),
            assignments_submission_failed: AtomicU64::new(0),
            assignments_discarded: AtomicU64::new(0),
            attempts_started: AtomicU64::new(0),
            attempts_success: AtomicU64::new(0),
            attempts_failure: AtomicU64::new(0),
            attempts_aborted: AtomicU64::new(0),
            attempt_duration: Histogram::new(),
            retries_connect: AtomicU64::new(0),
            retries_timeout: AtomicU64::new(0),
            retries_http_status: AtomicU64::new(0),
            retries_rate_limited: AtomicU64::new(0),
            retries_overloaded: AtomicU64::new(0),
            retries_temporarily_unavailable: AtomicU64::new(0),
            retries_server_error: AtomicU64::new(0),
            retry_after_delay_millis: AtomicU64::new(0),
            retry_jitter_delay_millis: AtomicU64::new(0),
            admissions_admitted: AtomicU64::new(0),
            admissions_rejected: AtomicU64::new(0),
            admissions_reservation_rejected: AtomicU64::new(0),
            admissions_completed: AtomicU64::new(0),
            admissions_cancelled: AtomicU64::new(0),
            admissions_error: AtomicU64::new(0),
            initial_reserved_input_tokens: AtomicU64::new(0),
            initial_reserved_output_tokens: AtomicU64::new(0),
            initial_reserved_cost_micro_usd: AtomicU64::new(0),
            retry_reserved_input_tokens: AtomicU64::new(0),
            retry_reserved_output_tokens: AtomicU64::new(0),
            retry_reserved_cost_micro_usd: AtomicU64::new(0),
            actual_input_tokens: AtomicU64::new(0),
            actual_output_tokens: AtomicU64::new(0),
            actual_cost_micro_usd: AtomicU64::new(0),
            assignment_claim_acquired: AtomicU64::new(0),
            assignment_claim_unavailable: AtomicU64::new(0),
            assignment_claim_renewal_lost: AtomicU64::new(0),
            assignment_claim_released: AtomicU64::new(0),
            assignment_claim_release_failed: AtomicU64::new(0),
            assignment_claim_stale_before_submission: AtomicU64::new(0),
            file_lease_acquired: AtomicU64::new(0),
            file_lease_unavailable: AtomicU64::new(0),
            file_lease_renewal_lost: AtomicU64::new(0),
            file_lease_released: AtomicU64::new(0),
            file_lease_release_failed: AtomicU64::new(0),
            file_lease_stale_before_submission: AtomicU64::new(0),
            submissions_accepted: AtomicU64::new(0),
            submissions_rejected: AtomicU64::new(0),
            draining: AtomicBool::new(false),
            last_drain_millis: AtomicU64::new(0),
        }
    }

    pub(crate) fn configure(
        &self,
        providers: usize,
        max_concurrency: usize,
        distributed_claims: bool,
    ) {
        self.providers_configured
            .store(providers as u64, Ordering::Relaxed);
        self.max_concurrency
            .store(max_concurrency as u64, Ordering::Relaxed);
        self.distributed_claims_enabled
            .store(distributed_claims, Ordering::Relaxed);
    }

    pub(crate) fn poll(&self, success: bool, duration: Duration) {
        if success {
            self.polls_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.polls_error.fetch_add(1, Ordering::Relaxed);
        }
        self.poll_duration.observe(duration);
    }

    pub(crate) fn registration(&self, success: bool) {
        if success {
            self.registrations_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.registrations_error.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn assignment(&self) -> AssignmentGuard<'_> {
        self.assignments_started.fetch_add(1, Ordering::Relaxed);
        self.assignments_active.fetch_add(1, Ordering::Relaxed);
        AssignmentGuard {
            metrics: self,
            finished: false,
        }
    }

    pub(crate) fn attempt_started(&self) -> Instant {
        self.attempts_started.fetch_add(1, Ordering::Relaxed);
        Instant::now()
    }

    pub(crate) fn attempt_finished(&self, started: Instant, result: AttemptResult) {
        match result {
            AttemptResult::Success => &self.attempts_success,
            AttemptResult::Failure => &self.attempts_failure,
            AttemptResult::Aborted => &self.attempts_aborted,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.attempt_duration.observe(started.elapsed());
    }

    pub(crate) fn retry(
        &self,
        reason: RetryReasonMetric,
        source: RetryDelayMetric,
        delay: Duration,
    ) {
        match reason {
            RetryReasonMetric::Connect => &self.retries_connect,
            RetryReasonMetric::Timeout => &self.retries_timeout,
            RetryReasonMetric::HttpStatus => &self.retries_http_status,
            RetryReasonMetric::RateLimited => &self.retries_rate_limited,
            RetryReasonMetric::Overloaded => &self.retries_overloaded,
            RetryReasonMetric::TemporarilyUnavailable => &self.retries_temporarily_unavailable,
            RetryReasonMetric::ServerError => &self.retries_server_error,
        }
        .fetch_add(1, Ordering::Relaxed);
        let millis = delay.as_millis().min(u128::from(u64::MAX)) as u64;
        match source {
            RetryDelayMetric::RetryAfter => &self.retry_after_delay_millis,
            RetryDelayMetric::ExponentialJitter => &self.retry_jitter_delay_millis,
        }
        .fetch_add(millis, Ordering::Relaxed);
    }

    pub(crate) fn admission(&self, result: AdmissionResult) {
        match result {
            AdmissionResult::Admitted => &self.admissions_admitted,
            AdmissionResult::Rejected => &self.admissions_rejected,
            AdmissionResult::ReservationRejected => &self.admissions_reservation_rejected,
            AdmissionResult::Completed => &self.admissions_completed,
            AdmissionResult::Cancelled => &self.admissions_cancelled,
            AdmissionResult::Error => &self.admissions_error,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reservation(
        &self,
        kind: ReservationKind,
        input_tokens: u64,
        output_tokens: u64,
        cost_micro_usd: u64,
    ) {
        let (input, output, cost) = match kind {
            ReservationKind::Initial => (
                &self.initial_reserved_input_tokens,
                &self.initial_reserved_output_tokens,
                &self.initial_reserved_cost_micro_usd,
            ),
            ReservationKind::Retry => (
                &self.retry_reserved_input_tokens,
                &self.retry_reserved_output_tokens,
                &self.retry_reserved_cost_micro_usd,
            ),
        };
        input.fetch_add(input_tokens, Ordering::Relaxed);
        output.fetch_add(output_tokens, Ordering::Relaxed);
        cost.fetch_add(cost_micro_usd, Ordering::Relaxed);
    }

    pub(crate) fn actual_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cost_micro_usd: u64,
    ) {
        self.actual_input_tokens
            .fetch_add(input_tokens, Ordering::Relaxed);
        self.actual_output_tokens
            .fetch_add(output_tokens, Ordering::Relaxed);
        self.actual_cost_micro_usd
            .fetch_add(cost_micro_usd, Ordering::Relaxed);
    }

    pub(crate) fn lease(&self, kind: LeaseKind, result: LeaseResult) {
        let counter = match (kind, result) {
            (LeaseKind::AssignmentClaim, LeaseResult::Acquired) => &self.assignment_claim_acquired,
            (LeaseKind::AssignmentClaim, LeaseResult::Unavailable) => {
                &self.assignment_claim_unavailable
            }
            (LeaseKind::AssignmentClaim, LeaseResult::RenewalLost) => {
                &self.assignment_claim_renewal_lost
            }
            (LeaseKind::AssignmentClaim, LeaseResult::Released) => &self.assignment_claim_released,
            (LeaseKind::AssignmentClaim, LeaseResult::ReleaseFailed) => {
                &self.assignment_claim_release_failed
            }
            (LeaseKind::AssignmentClaim, LeaseResult::StaleBeforeSubmission) => {
                &self.assignment_claim_stale_before_submission
            }
            (LeaseKind::FileLease, LeaseResult::Acquired) => &self.file_lease_acquired,
            (LeaseKind::FileLease, LeaseResult::Unavailable) => &self.file_lease_unavailable,
            (LeaseKind::FileLease, LeaseResult::RenewalLost) => &self.file_lease_renewal_lost,
            (LeaseKind::FileLease, LeaseResult::Released) => &self.file_lease_released,
            (LeaseKind::FileLease, LeaseResult::ReleaseFailed) => &self.file_lease_release_failed,
            (LeaseKind::FileLease, LeaseResult::StaleBeforeSubmission) => {
                &self.file_lease_stale_before_submission
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn submission(&self, result: SubmissionResult) {
        match result {
            SubmissionResult::Accepted => &self.submissions_accepted,
            SubmissionResult::Rejected => &self.submissions_rejected,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn drain(&self) -> DrainGuard<'_> {
        self.draining.store(true, Ordering::Relaxed);
        DrainGuard {
            metrics: self,
            started: Instant::now(),
        }
    }

    pub(crate) fn render(&self, health: RunnerHealthMetricsSnapshot) -> String {
        let mut output = String::with_capacity(10 * 1024);
        metric_header(
            &mut output,
            "ai_agent_runner_build_info",
            "Static provider-runner build information.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "ai_agent_runner_build_info{{version=\"{}\"}} 1",
            escape_label(env!("CARGO_PKG_VERSION"))
        );
        gauge(
            &mut output,
            "ai_agent_runner_process_start_time_seconds",
            "Unix timestamp when this runner metrics registry started.",
            self.started_epoch_seconds,
        );
        gauge_f64(
            &mut output,
            "ai_agent_runner_process_uptime_seconds",
            "Provider-runner process uptime in seconds.",
            self.started_at.elapsed().as_secs_f64(),
        );
        gauge(
            &mut output,
            "ai_agent_runner_registered",
            "Whether all required runner identities are registered with the bridge.",
            u64::from(health.registered),
        );
        gauge(
            &mut output,
            "ai_agent_runner_poll_fresh",
            "Whether the latest successful bridge poll is within the readiness window.",
            u64::from(health.poll_fresh),
        );
        gauge(
            &mut output,
            "ai_agent_runner_ready",
            "Whether registration, polling freshness, and shutdown state permit work.",
            u64::from(health.ready),
        );
        gauge(
            &mut output,
            "ai_agent_runner_shutting_down",
            "Whether the runner is draining or shutting down.",
            u64::from(health.shutting_down),
        );
        gauge_f64(
            &mut output,
            "ai_agent_runner_last_successful_poll_age_seconds",
            "Age in seconds of the latest successful bridge poll, or -1 before success.",
            health
                .last_successful_poll_age_ms
                .map(|age| age as f64 / 1_000.0)
                .unwrap_or(-1.0),
        );
        gauge_f64(
            &mut output,
            "ai_agent_runner_ready_max_staleness_seconds",
            "Configured maximum successful-poll age for readiness.",
            health.ready_max_staleness_ms as f64 / 1_000.0,
        );
        gauge(
            &mut output,
            "ai_agent_runner_required_identities",
            "Number of provider and claim-owner identities required for readiness.",
            health.required_agents,
        );
        gauge(
            &mut output,
            "ai_agent_runner_providers_configured",
            "Number of configured provider adapters without provider identity labels.",
            self.providers_configured.load(Ordering::Relaxed),
        );
        gauge(
            &mut output,
            "ai_agent_runner_distributed_claims_enabled",
            "Whether distributed assignment claims are enabled.",
            u64::from(self.distributed_claims_enabled.load(Ordering::Relaxed)),
        );
        metric_header(
            &mut output,
            "ai_agent_runner_concurrency",
            "Current active assignments and configured concurrency limit.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "ai_agent_runner_concurrency{{kind=\"current\"}} {}",
            self.assignments_active.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            output,
            "ai_agent_runner_concurrency{{kind=\"limit\"}} {}",
            self.max_concurrency.load(Ordering::Relaxed)
        );
        counter_family(
            &mut output,
            "ai_agent_runner_polls_total",
            "Workflow-list polls grouped by bounded result.",
            "result",
            &[
                ("success", self.polls_success.load(Ordering::Relaxed)),
                ("error", self.polls_error.load(Ordering::Relaxed)),
            ],
        );
        self.poll_duration.render(
            &mut output,
            "ai_agent_runner_poll_duration_seconds",
            "Workflow-list poll duration without URL or identity labels.",
        );
        counter_family(
            &mut output,
            "ai_agent_runner_registrations_total",
            "Runner/provider registration rounds grouped by bounded result.",
            "result",
            &[
                (
                    "success",
                    self.registrations_success.load(Ordering::Relaxed),
                ),
                ("error", self.registrations_error.load(Ordering::Relaxed)),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_runner_assignments_total",
            "Assignment lifecycle outcomes without workflow or provider labels.",
            "result",
            &[
                ("started", self.assignments_started.load(Ordering::Relaxed)),
                (
                    "submitted",
                    self.assignments_submitted.load(Ordering::Relaxed),
                ),
                (
                    "submission_failed",
                    self.assignments_submission_failed.load(Ordering::Relaxed),
                ),
                (
                    "discarded",
                    self.assignments_discarded.load(Ordering::Relaxed),
                ),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_runner_provider_attempts_total",
            "External provider attempts grouped by bounded result.",
            "result",
            &[
                ("started", self.attempts_started.load(Ordering::Relaxed)),
                ("success", self.attempts_success.load(Ordering::Relaxed)),
                ("failure", self.attempts_failure.load(Ordering::Relaxed)),
                ("aborted", self.attempts_aborted.load(Ordering::Relaxed)),
            ],
        );
        self.attempt_duration.render(
            &mut output,
            "ai_agent_runner_provider_attempt_duration_seconds",
            "Provider attempt duration without provider, model, or workflow labels.",
        );
        counter_family(
            &mut output,
            "ai_agent_runner_retries_total",
            "Scheduled retries grouped by bounded retry reason.",
            "reason",
            &[
                ("connect", self.retries_connect.load(Ordering::Relaxed)),
                ("timeout", self.retries_timeout.load(Ordering::Relaxed)),
                (
                    "http_status",
                    self.retries_http_status.load(Ordering::Relaxed),
                ),
                (
                    "rate_limited",
                    self.retries_rate_limited.load(Ordering::Relaxed),
                ),
                ("overloaded", self.retries_overloaded.load(Ordering::Relaxed)),
                (
                    "temporarily_unavailable",
                    self.retries_temporarily_unavailable.load(Ordering::Relaxed),
                ),
                (
                    "server_error",
                    self.retries_server_error.load(Ordering::Relaxed),
                ),
            ],
        );
        counter_family_f64(
            &mut output,
            "ai_agent_runner_retry_delay_seconds_total",
            "Cumulative scheduled retry delay grouped by bounded source.",
            "source",
            &[
                (
                    "retry_after",
                    self.retry_after_delay_millis.load(Ordering::Relaxed) as f64 / 1_000.0,
                ),
                (
                    "exponential_jitter",
                    self.retry_jitter_delay_millis.load(Ordering::Relaxed) as f64 / 1_000.0,
                ),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_runner_admission_events_total",
            "Policy admission lifecycle events grouped by bounded result.",
            "result",
            &[
                (
                    "admitted",
                    self.admissions_admitted.load(Ordering::Relaxed),
                ),
                (
                    "rejected",
                    self.admissions_rejected.load(Ordering::Relaxed),
                ),
                (
                    "reservation_rejected",
                    self.admissions_reservation_rejected.load(Ordering::Relaxed),
                ),
                (
                    "completed",
                    self.admissions_completed.load(Ordering::Relaxed),
                ),
                (
                    "cancelled",
                    self.admissions_cancelled.load(Ordering::Relaxed),
                ),
                ("error", self.admissions_error.load(Ordering::Relaxed)),
            ],
        );
        metric_header(
            &mut output,
            "ai_agent_runner_reserved_tokens_total",
            "Conservatively reserved tokens grouped by initial/retry attempt and input/output kind.",
            "counter",
        );
        for (attempt, input, output) in [
            (
                "initial",
                self.initial_reserved_input_tokens.load(Ordering::Relaxed),
                self.initial_reserved_output_tokens.load(Ordering::Relaxed),
            ),
            (
                "retry",
                self.retry_reserved_input_tokens.load(Ordering::Relaxed),
                self.retry_reserved_output_tokens.load(Ordering::Relaxed),
            ),
        ] {
            let _ = writeln!(
                output,
                "ai_agent_runner_reserved_tokens_total{{attempt=\"{attempt}\",kind=\"input\"}} {input}"
            );
            let _ = writeln!(
                output,
                "ai_agent_runner_reserved_tokens_total{{attempt=\"{attempt}\",kind=\"output\"}} {output}"
            );
        }
        counter_family(
            &mut output,
            "ai_agent_runner_reserved_cost_micro_usd_total",
            "Conservatively reserved provider cost in micro-USD.",
            "attempt",
            &[
                (
                    "initial",
                    self.initial_reserved_cost_micro_usd.load(Ordering::Relaxed),
                ),
                (
                    "retry",
                    self.retry_reserved_cost_micro_usd.load(Ordering::Relaxed),
                ),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_runner_actual_tokens_total",
            "Provider-reported token usage accepted by admission accounting.",
            "kind",
            &[
                (
                    "input",
                    self.actual_input_tokens.load(Ordering::Relaxed),
                ),
                (
                    "output",
                    self.actual_output_tokens.load(Ordering::Relaxed),
                ),
            ],
        );
        counter(
            &mut output,
            "ai_agent_runner_actual_cost_micro_usd_total",
            "Provider cost accepted by admission accounting in micro-USD.",
            self.actual_cost_micro_usd.load(Ordering::Relaxed),
        );
        metric_header(
            &mut output,
            "ai_agent_runner_lease_events_total",
            "Assignment-claim and file-lease events grouped by bounded kind and result.",
            "counter",
        );
        for (kind, values) in [
            (
                "assignment_claim",
                [
                    ("acquired", self.assignment_claim_acquired.load(Ordering::Relaxed)),
                    (
                        "unavailable",
                        self.assignment_claim_unavailable.load(Ordering::Relaxed),
                    ),
                    (
                        "renewal_lost",
                        self.assignment_claim_renewal_lost.load(Ordering::Relaxed),
                    ),
                    ("released", self.assignment_claim_released.load(Ordering::Relaxed)),
                    (
                        "release_failed",
                        self.assignment_claim_release_failed.load(Ordering::Relaxed),
                    ),
                    (
                        "stale_before_submission",
                        self.assignment_claim_stale_before_submission
                            .load(Ordering::Relaxed),
                    ),
                ],
            ),
            (
                "file_lease",
                [
                    ("acquired", self.file_lease_acquired.load(Ordering::Relaxed)),
                    ("unavailable", self.file_lease_unavailable.load(Ordering::Relaxed)),
                    (
                        "renewal_lost",
                        self.file_lease_renewal_lost.load(Ordering::Relaxed),
                    ),
                    ("released", self.file_lease_released.load(Ordering::Relaxed)),
                    (
                        "release_failed",
                        self.file_lease_release_failed.load(Ordering::Relaxed),
                    ),
                    (
                        "stale_before_submission",
                        self.file_lease_stale_before_submission.load(Ordering::Relaxed),
                    ),
                ],
            ),
        ] {
            for (result, count) in values {
                let _ = writeln!(
                    output,
                    "ai_agent_runner_lease_events_total{{kind=\"{kind}\",result=\"{result}\"}} {count}"
                );
            }
        }
        counter_family(
            &mut output,
            "ai_agent_runner_submissions_total",
            "Workflow submissions grouped by bounded result.",
            "result",
            &[
                (
                    "accepted",
                    self.submissions_accepted.load(Ordering::Relaxed),
                ),
                (
                    "rejected",
                    self.submissions_rejected.load(Ordering::Relaxed),
                ),
            ],
        );
        gauge(
            &mut output,
            "ai_agent_runner_draining",
            "Whether the runner is waiting for active work to drain.",
            u64::from(self.draining.load(Ordering::Relaxed)),
        );
        gauge_f64(
            &mut output,
            "ai_agent_runner_last_drain_duration_seconds",
            "Duration of the latest completed shutdown drain.",
            self.last_drain_millis.load(Ordering::Relaxed) as f64 / 1_000.0,
        );
        output
    }
}

pub(crate) struct AssignmentGuard<'a> {
    metrics: &'a RunnerMetrics,
    finished: bool,
}

impl AssignmentGuard<'_> {
    pub(crate) fn finish(mut self, result: AssignmentResult) {
        match result {
            AssignmentResult::Submitted => &self.metrics.assignments_submitted,
            AssignmentResult::SubmissionFailed => &self.metrics.assignments_submission_failed,
            AssignmentResult::Discarded => &self.metrics.assignments_discarded,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.finished = true;
    }
}

impl Drop for AssignmentGuard<'_> {
    fn drop(&mut self) {
        self.metrics
            .assignments_active
            .fetch_sub(1, Ordering::Relaxed);
        if !self.finished {
            self.metrics
                .assignments_discarded
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) struct DrainGuard<'a> {
    metrics: &'a RunnerMetrics,
    started: Instant,
}

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        self.metrics.draining.store(false, Ordering::Relaxed);
        self.metrics.last_drain_millis.store(
            self.started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }
}

fn metric_header(output: &mut String, name: &str, help: &str, metric_type: &str) {
    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} {metric_type}");
}

fn gauge(output: &mut String, name: &str, help: &str, value: u64) {
    metric_header(output, name, help, "gauge");
    let _ = writeln!(output, "{name} {value}");
}

fn gauge_f64(output: &mut String, name: &str, help: &str, value: f64) {
    metric_header(output, name, help, "gauge");
    let _ = writeln!(output, "{name} {value}");
}

fn counter(output: &mut String, name: &str, help: &str, value: u64) {
    metric_header(output, name, help, "counter");
    let _ = writeln!(output, "{name} {value}");
}

fn counter_family(
    output: &mut String,
    name: &str,
    help: &str,
    label: &str,
    values: &[(&str, u64)],
) {
    metric_header(output, name, help, "counter");
    for (value, count) in values {
        let _ = writeln!(output, "{name}{{{label}=\"{value}\"}} {count}");
    }
}

fn counter_family_f64(
    output: &mut String,
    name: &str,
    help: &str,
    label: &str,
    values: &[(&str, f64)],
) {
    metric_header(output, name, help, "counter");
    for (value, count) in values {
        let _ = writeln!(output, "{name}{{{label}=\"{value}\"}} {count}");
    }
}

fn format_bound(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}
