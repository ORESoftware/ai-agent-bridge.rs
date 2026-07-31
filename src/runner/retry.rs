use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::providers::{ProviderError, ProviderFailureKind, ProviderTransportKind};

use super::ProviderWorker;

const CONFIG_ENV: &str = "AI_PROVIDER_RETRY_POLICY_JSON";
const ABS_MAX_RETRIES: u8 = 5;
const MIN_DELAY_MS: u64 = 10;
const ABS_MAX_DELAY_MS: u64 = 300_000;
const ABS_MAX_TOTAL_DELAY_MS: u64 = 900_000;
const DEFAULT_BASE_DELAY_MS: u64 = 250;
const DEFAULT_MAX_DELAY_MS: u64 = 5_000;
const DEFAULT_MAX_TOTAL_DELAY_MS: u64 = 15_000;
const DEFAULT_GUARD_INTERVAL_MS: u64 = 1_000;
const MIN_GUARD_INTERVAL_MS: u64 = 250;
const MAX_GUARD_INTERVAL_MS: u64 = 30_000;
const DEFAULT_RETRYABLE_STATUSES: &[u16] = &[408, 425, 429, 500, 502, 503, 504, 529];

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicies {
    policies: BTreeMap<String, RetryPolicy>,
    guard_interval: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
    max_retries: u8,
    base_delay_ms: u64,
    max_delay_ms: u64,
    max_total_delay_ms: u64,
    retryable_statuses: BTreeSet<u16>,
    retry_connect_errors: bool,
    retry_timeout_errors: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryDelaySource {
    RetryAfter,
    ExponentialJitter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryReason {
    Connect,
    Timeout,
    HttpStatus(u16),
    RateLimited(u16),
    Overloaded(u16),
    TemporarilyUnavailable(u16),
    ServerError(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryPlan {
    pub retry_ordinal: u8,
    pub delay: Duration,
    pub delay_source: RetryDelaySource,
    pub reason: RetryReason,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RetryConfigError {
    #[error("invalid retry configuration: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryDocument {
    #[serde(default)]
    guard_interval_ms: Option<u64>,
    #[serde(default)]
    providers: BTreeMap<String, RetryPolicyInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryPolicyInput {
    #[serde(default)]
    max_retries: u8,
    #[serde(default = "default_base_delay_ms")]
    base_delay_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    max_delay_ms: u64,
    #[serde(default = "default_max_total_delay_ms")]
    max_total_delay_ms: u64,
    #[serde(default = "default_retryable_statuses")]
    retryable_statuses: Vec<u16>,
    #[serde(default = "default_true")]
    retry_connect_errors: bool,
    #[serde(default)]
    retry_timeout_errors: bool,
}

impl RetryPolicies {
    pub(crate) fn from_env(providers: &[ProviderWorker]) -> Result<Self, RetryConfigError> {
        let raw = std::env::var(CONFIG_ENV).ok();
        Self::from_optional_json(providers, raw.as_deref())
    }

    fn from_optional_json(
        providers: &[ProviderWorker],
        raw: Option<&str>,
    ) -> Result<Self, RetryConfigError> {
        let document = match raw.map(str::trim).filter(|value| !value.is_empty()) {
            Some(raw) => serde_json::from_str::<RetryDocument>(raw)
                .map_err(|_| RetryConfigError::Invalid(format!("{CONFIG_ENV} is invalid JSON")))?,
            None => RetryDocument::default(),
        };
        let configured_names = providers
            .iter()
            .map(|provider| provider.config.name.as_str())
            .collect::<BTreeSet<_>>();
        for name in document.providers.keys() {
            if !configured_names.contains(name.as_str()) {
                return Err(RetryConfigError::Invalid(format!(
                    "retry policy references unknown provider '{name}'"
                )));
            }
        }

        let guard_interval_ms = document
            .guard_interval_ms
            .unwrap_or(DEFAULT_GUARD_INTERVAL_MS);
        if !(MIN_GUARD_INTERVAL_MS..=MAX_GUARD_INTERVAL_MS).contains(&guard_interval_ms) {
            return Err(RetryConfigError::Invalid(format!(
                "guard_interval_ms must be between {MIN_GUARD_INTERVAL_MS} and {MAX_GUARD_INTERVAL_MS}"
            )));
        }

        let mut policies = BTreeMap::new();
        for provider in providers {
            let policy = document
                .providers
                .get(&provider.config.name)
                .cloned()
                .unwrap_or_else(RetryPolicyInput::disabled)
                .validate(&provider.config.name)?;
            policies.insert(provider.config.name.clone(), policy);
        }
        Ok(Self {
            policies,
            guard_interval: Duration::from_millis(guard_interval_ms),
        })
    }

    pub(crate) fn policy(&self, provider: &str) -> RetryPolicy {
        self.policies
            .get(provider)
            .cloned()
            .unwrap_or_else(RetryPolicy::disabled)
    }

    pub(crate) fn guard_interval(&self) -> Duration {
        self.guard_interval
    }
}

impl RetryPolicyInput {
    fn disabled() -> Self {
        Self {
            max_retries: 0,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            max_total_delay_ms: DEFAULT_MAX_TOTAL_DELAY_MS,
            retryable_statuses: default_retryable_statuses(),
            retry_connect_errors: true,
            retry_timeout_errors: false,
        }
    }

    fn validate(self, provider: &str) -> Result<RetryPolicy, RetryConfigError> {
        if self.max_retries > ABS_MAX_RETRIES {
            return Err(RetryConfigError::Invalid(format!(
                "provider '{provider}' max_retries must be <= {ABS_MAX_RETRIES}"
            )));
        }
        if !(MIN_DELAY_MS..=ABS_MAX_DELAY_MS).contains(&self.base_delay_ms) {
            return Err(RetryConfigError::Invalid(format!(
                "provider '{provider}' base_delay_ms must be between {MIN_DELAY_MS} and {ABS_MAX_DELAY_MS}"
            )));
        }
        if self.max_delay_ms < self.base_delay_ms || self.max_delay_ms > ABS_MAX_DELAY_MS {
            return Err(RetryConfigError::Invalid(format!(
                "provider '{provider}' max_delay_ms must be >= base_delay_ms and <= {ABS_MAX_DELAY_MS}"
            )));
        }
        if self.max_total_delay_ms < self.base_delay_ms
            || self.max_total_delay_ms > ABS_MAX_TOTAL_DELAY_MS
        {
            return Err(RetryConfigError::Invalid(format!(
                "provider '{provider}' max_total_delay_ms must be >= base_delay_ms and <= {ABS_MAX_TOTAL_DELAY_MS}"
            )));
        }
        if self.retryable_statuses.len() > 32 {
            return Err(RetryConfigError::Invalid(format!(
                "provider '{provider}' retryable_statuses exceeds 32 entries"
            )));
        }
        let mut statuses = BTreeSet::new();
        for status in self.retryable_statuses {
            if !status_may_be_retried(status) {
                return Err(RetryConfigError::Invalid(format!(
                    "provider '{provider}' status {status} is not an allowed retry status"
                )));
            }
            statuses.insert(status);
        }
        Ok(RetryPolicy {
            max_retries: self.max_retries,
            base_delay_ms: self.base_delay_ms,
            max_delay_ms: self.max_delay_ms,
            max_total_delay_ms: self.max_total_delay_ms,
            retryable_statuses: statuses,
            retry_connect_errors: self.retry_connect_errors,
            retry_timeout_errors: self.retry_timeout_errors,
        })
    }
}

impl RetryPolicy {
    fn disabled() -> Self {
        RetryPolicyInput::disabled()
            .validate("disabled")
            .expect("built-in retry defaults are valid")
    }

    pub(crate) fn plan(
        &self,
        error: &ProviderError,
        retry_ordinal: u8,
        spent_delay: Duration,
        seed: &str,
    ) -> Option<RetryPlan> {
        if retry_ordinal == 0 || retry_ordinal > self.max_retries {
            return None;
        }
        let reason = self.retry_reason(error)?;
        let (delay, delay_source) = error
            .retry_after()
            .filter(|delay| !delay.is_zero())
            .map(|delay| {
                (
                    delay.min(Duration::from_millis(self.max_delay_ms)),
                    RetryDelaySource::RetryAfter,
                )
            })
            .unwrap_or_else(|| {
                (
                    deterministic_backoff(
                        self.base_delay_ms,
                        self.max_delay_ms,
                        retry_ordinal,
                        seed,
                    ),
                    RetryDelaySource::ExponentialJitter,
                )
            });
        if spent_delay.saturating_add(delay) > Duration::from_millis(self.max_total_delay_ms) {
            return None;
        }
        Some(RetryPlan {
            retry_ordinal,
            delay,
            delay_source,
            reason,
        })
    }

    fn retry_reason(&self, error: &ProviderError) -> Option<RetryReason> {
        if let Some(kind) = error.transport_kind() {
            return match kind {
                ProviderTransportKind::Connect if self.retry_connect_errors => {
                    Some(RetryReason::Connect)
                }
                ProviderTransportKind::Timeout if self.retry_timeout_errors => {
                    Some(RetryReason::Timeout)
                }
                ProviderTransportKind::Connect
                | ProviderTransportKind::Timeout
                | ProviderTransportKind::Request
                | ProviderTransportKind::ResponseBody => None,
            };
        }
        let status = error.http_status()?.as_u16();
        if !self.retryable_statuses.contains(&status) {
            return None;
        }
        match error.failure_kind() {
            Some(ProviderFailureKind::RateLimited) => Some(RetryReason::RateLimited(status)),
            Some(ProviderFailureKind::Overloaded) => Some(RetryReason::Overloaded(status)),
            Some(ProviderFailureKind::TemporarilyUnavailable) => {
                Some(RetryReason::TemporarilyUnavailable(status))
            }
            Some(ProviderFailureKind::ServerError) => Some(RetryReason::ServerError(status)),
            None => Some(RetryReason::HttpStatus(status)),
        }
    }
}

fn deterministic_backoff(
    base_delay_ms: u64,
    max_delay_ms: u64,
    retry_ordinal: u8,
    seed: &str,
) -> Duration {
    let exponent = u32::from(retry_ordinal.saturating_sub(1)).min(20);
    let exponential = base_delay_ms
        .saturating_mul(1u64 << exponent)
        .min(max_delay_ms);
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update([retry_ordinal]);
    let digest = hasher.finalize();
    let sample = u16::from_be_bytes([digest[0], digest[1]]);
    let jitter_bps = 5_000u64.saturating_add(u64::from(sample) % 5_001);
    let jittered = exponential.saturating_mul(jitter_bps).saturating_add(9_999) / 10_000;
    Duration::from_millis(jittered.max(1).min(max_delay_ms))
}

fn status_may_be_retried(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429) || (500..=599).contains(&status)
}

const fn default_base_delay_ms() -> u64 {
    DEFAULT_BASE_DELAY_MS
}

const fn default_max_delay_ms() -> u64 {
    DEFAULT_MAX_DELAY_MS
}

const fn default_max_total_delay_ms() -> u64 {
    DEFAULT_MAX_TOTAL_DELAY_MS
}

fn default_retryable_statuses() -> Vec<u16> {
    DEFAULT_RETRYABLE_STATUSES.to_vec()
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::*;

    fn input(max_retries: u8) -> RetryPolicyInput {
        RetryPolicyInput {
            max_retries,
            base_delay_ms: 100,
            max_delay_ms: 2_000,
            max_total_delay_ms: 5_000,
            retryable_statuses: default_retryable_statuses(),
            retry_connect_errors: true,
            retry_timeout_errors: false,
        }
    }

    #[test]
    fn deterministic_jitter_is_stable_and_bounded() {
        let first = deterministic_backoff(100, 2_000, 3, "workflow/assignment/provider");
        let second = deterministic_backoff(100, 2_000, 3, "workflow/assignment/provider");
        assert_eq!(first, second);
        assert!((Duration::from_millis(200)..=Duration::from_millis(400)).contains(&first));
    }

    #[test]
    fn retry_after_is_honored_within_configured_bounds() {
        let policy = input(2).validate("codex").unwrap();
        let error = ProviderError::http_status_for_test(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(10)),
            Some(ProviderFailureKind::RateLimited),
        );
        let plan = policy.plan(&error, 1, Duration::ZERO, "seed").unwrap();
        assert_eq!(plan.delay, Duration::from_millis(2_000));
        assert_eq!(plan.delay_source, RetryDelaySource::RetryAfter);
        assert_eq!(plan.reason, RetryReason::RateLimited(429));
    }

    #[test]
    fn ambiguous_timeout_is_non_retryable_by_default() {
        let policy = input(2).validate("codex").unwrap();
        let error = ProviderError::transport_for_test(ProviderTransportKind::Timeout);
        assert!(policy.plan(&error, 1, Duration::ZERO, "seed").is_none());
    }

    #[test]
    fn connect_failure_is_retryable_but_response_body_failure_is_not() {
        let policy = input(2).validate("codex").unwrap();
        assert!(policy
            .plan(
                &ProviderError::transport_for_test(ProviderTransportKind::Connect),
                1,
                Duration::ZERO,
                "seed",
            )
            .is_some());
        assert!(policy
            .plan(
                &ProviderError::transport_for_test(ProviderTransportKind::ResponseBody),
                1,
                Duration::ZERO,
                "seed",
            )
            .is_none());
    }

    #[test]
    fn status_409_requires_explicit_configuration() {
        let default = input(2).validate("codex").unwrap();
        let error = ProviderError::http_status_for_test(StatusCode::CONFLICT, None, None);
        assert!(default.plan(&error, 1, Duration::ZERO, "seed").is_none());

        let mut configured = input(2);
        configured.retryable_statuses.push(409);
        let configured = configured.validate("codex").unwrap();
        assert!(configured.plan(&error, 1, Duration::ZERO, "seed").is_some());
    }

    #[test]
    fn total_delay_ceiling_refuses_another_attempt() {
        let policy = input(3).validate("codex").unwrap();
        let error = ProviderError::http_status_for_test(
            StatusCode::SERVICE_UNAVAILABLE,
            Some(Duration::from_secs(2)),
            None,
        );
        assert!(policy
            .plan(&error, 1, Duration::from_millis(4_000), "seed")
            .is_none());
    }

    #[test]
    fn retry_configuration_rejects_auth_and_validation_statuses() {
        let mut authentication = input(1);
        authentication.retryable_statuses = vec![401];
        assert!(authentication.validate("codex").is_err());

        let mut validation = input(1);
        validation.retryable_statuses = vec![422];
        assert!(validation.validate("codex").is_err());
    }
}
