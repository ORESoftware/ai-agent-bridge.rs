use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;

const DEFAULT_HEALTH_PORT: u16 = 8_144;
const DEFAULT_READY_MAX_STALENESS_MS: u64 = 30_000;
const MIN_READY_MAX_STALENESS_MS: u64 = 1_000;
const MAX_READY_MAX_STALENESS_MS: u64 = 600_000;

#[derive(Clone)]
pub(crate) struct RunnerHealth {
    config: HealthConfig,
    state: Arc<HealthState>,
}

#[derive(Clone, Copy)]
struct HealthConfig {
    host: IpAddr,
    port: u16,
    ready_max_staleness_ms: u64,
}

struct HealthState {
    registered: AtomicBool,
    last_successful_poll_ms: AtomicU64,
    shutting_down: AtomicBool,
}

#[derive(Debug, Serialize)]
struct HealthPayload {
    ok: bool,
    status: &'static str,
    registered: bool,
    poll_fresh: bool,
    shutting_down: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_successful_poll_age_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct HealthSnapshot {
    ready: bool,
    registered: bool,
    poll_fresh: bool,
    shutting_down: bool,
    last_successful_poll_age_ms: Option<u64>,
}

impl RunnerHealth {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let host = std::env::var("AI_AGENT_RUNNER_HEALTH_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| Ipv4Addr::UNSPECIFIED.to_string())
            .parse::<IpAddr>()
            .map_err(|error| anyhow::anyhow!("AI_AGENT_RUNNER_HEALTH_HOST is invalid: {error}"))?;
        let port = env_u16("AI_AGENT_RUNNER_HEALTH_PORT", DEFAULT_HEALTH_PORT)?;
        let ready_max_staleness_ms = std::env::var("AI_AGENT_RUNNER_READY_MAX_STALENESS_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_READY_MAX_STALENESS_MS)
            .clamp(MIN_READY_MAX_STALENESS_MS, MAX_READY_MAX_STALENESS_MS);
        Ok(Self::new(host, port, ready_max_staleness_ms))
    }

    fn new(host: IpAddr, port: u16, ready_max_staleness_ms: u64) -> Self {
        Self {
            config: HealthConfig {
                host,
                port,
                ready_max_staleness_ms: ready_max_staleness_ms
                    .clamp(MIN_READY_MAX_STALENESS_MS, MAX_READY_MAX_STALENESS_MS),
            },
            state: Arc::new(HealthState {
                registered: AtomicBool::new(false),
                last_successful_poll_ms: AtomicU64::new(0),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) async fn bind(&self) -> anyhow::Result<TcpListener> {
        let address = SocketAddr::new(self.config.host, self.config.port);
        TcpListener::bind(address)
            .await
            .map_err(|error| anyhow::anyhow!("failed to bind runner health listener {address}: {error}"))
    }

    pub(crate) async fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        axum::serve(listener, self.router()).await
    }

    pub(crate) fn address(&self) -> SocketAddr {
        SocketAddr::new(self.config.host, self.config.port)
    }

    pub(crate) fn mark_registered(&self) {
        self.state.registered.store(true, Ordering::Release);
    }

    pub(crate) fn mark_poll_success(&self) {
        self.state
            .last_successful_poll_ms
            .store(now_ms(), Ordering::Release);
    }

    pub(crate) fn mark_shutting_down(&self) {
        self.state.shutting_down.store(true, Ordering::Release);
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(liveness))
            .route("/readyz", get(readiness))
            .with_state(self.clone())
    }

    fn snapshot(&self, now_ms: u64) -> HealthSnapshot {
        let registered = self.state.registered.load(Ordering::Acquire);
        let last_poll = self
            .state
            .last_successful_poll_ms
            .load(Ordering::Acquire);
        let shutting_down = self.state.shutting_down.load(Ordering::Acquire);
        let age = (last_poll > 0).then(|| now_ms.saturating_sub(last_poll));
        let poll_fresh = age.is_some_and(|age| age <= self.config.ready_max_staleness_ms);
        HealthSnapshot {
            ready: registered && poll_fresh && !shutting_down,
            registered,
            poll_fresh,
            shutting_down,
            last_successful_poll_age_ms: age,
        }
    }
}

async fn liveness() -> Json<HealthPayload> {
    Json(HealthPayload {
        ok: true,
        status: "alive",
        registered: false,
        poll_fresh: false,
        shutting_down: false,
        last_successful_poll_age_ms: None,
    })
}

async fn readiness(State(health): State<RunnerHealth>) -> Response {
    let snapshot = health.snapshot(now_ms());
    let status = if snapshot.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(HealthPayload {
            ok: snapshot.ready,
            status: if snapshot.ready { "ready" } else { "not_ready" },
            registered: snapshot.registered,
            poll_fresh: snapshot.poll_fresh,
            shutting_down: snapshot.shutting_down,
            last_successful_poll_age_ms: snapshot.last_successful_poll_age_ms,
        }),
    )
        .into_response()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn env_u16(key: &str, default: u16) -> anyhow::Result<u16> {
    match std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(value) => value
            .parse::<u16>()
            .map_err(|error| anyhow::anyhow!("{key} is invalid: {error}")),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_registration_and_recent_poll() {
        let health = RunnerHealth::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 5_000);
        let now = 100_000;
        assert!(!health.snapshot(now).ready);

        health.mark_registered();
        assert!(!health.snapshot(now).ready);

        health
            .state
            .last_successful_poll_ms
            .store(now - 1_000, Ordering::Release);
        assert!(health.snapshot(now).ready);

        health
            .state
            .last_successful_poll_ms
            .store(now - 6_000, Ordering::Release);
        assert!(!health.snapshot(now).ready);
    }

    #[test]
    fn draining_runner_is_not_ready() {
        let health = RunnerHealth::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 5_000);
        health.mark_registered();
        health
            .state
            .last_successful_poll_ms
            .store(99_000, Ordering::Release);
        assert!(health.snapshot(100_000).ready);
        health.mark_shutting_down();
        assert!(!health.snapshot(100_000).ready);
    }

    #[tokio::test]
    async fn http_contract_transitions_from_not_ready_to_ready() {
        let health = RunnerHealth::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 5_000);
        let listener = health.bind().await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_health = health.clone();
        let task = tokio::spawn(async move {
            let _ = server_health.serve(listener).await;
        });

        let client = reqwest::Client::new();
        let live = client
            .get(format!("http://{address}/healthz"))
            .send()
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);

        let not_ready = client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .unwrap();
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        health.mark_registered();
        health.mark_poll_success();
        let ready = client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);

        health.mark_shutting_down();
        let draining = client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .unwrap();
        assert_eq!(draining.status(), StatusCode::SERVICE_UNAVAILABLE);
        task.abort();
    }
}
