//! Independent liveness/readiness surface for the provider-runner process.
//!
//! Readiness is based on a bounded, authenticated bridge probe: every configured
//! provider adapter (and the per-instance claim owner when distributed claims are
//! enabled) must be registered, and the workflows endpoint must have succeeded
//! recently. Responses contain no provider configuration, prompts, URLs, secrets,
//! assignments, or workflow records.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;

use crate::bridge_origin_policy::{validate_bridge_origin, INTERNAL_HTTP_HOSTS_ENV};
use crate::providers::parse_provider_configs;

const DEFAULT_HEALTH_PORT: u16 = 8_144;
const DEFAULT_READY_MAX_STALENESS_MS: u64 = 30_000;
const DEFAULT_PROBE_INTERVAL_MS: u64 = 5_000;
const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 5;
const MIN_READY_MAX_STALENESS_MS: u64 = 1_000;
const MAX_READY_MAX_STALENESS_MS: u64 = 600_000;
const MIN_PROBE_INTERVAL_MS: u64 = 250;
const MAX_PROBE_INTERVAL_MS: u64 = 60_000;
const MAX_PROBE_RESPONSE_BYTES: usize = 1_048_576;

#[derive(Clone)]
pub struct RunnerHealth {
    config: HealthConfig,
    state: Arc<HealthState>,
    bridge: BridgeProbe,
    required_agents: Arc<BTreeSet<String>>,
}

#[derive(Clone, Copy)]
struct HealthConfig {
    host: IpAddr,
    port: u16,
    ready_max_staleness_ms: u64,
    probe_interval_ms: u64,
}

struct HealthState {
    registered: AtomicBool,
    last_successful_poll_ms: AtomicU64,
    shutting_down: AtomicBool,
}

#[derive(Clone)]
struct BridgeProbe {
    base_url: Url,
    bearer: Option<String>,
    http: reqwest::Client,
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
    pub fn from_env() -> anyhow::Result<Self> {
        let host = std::env::var("AI_AGENT_RUNNER_HEALTH_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| Ipv4Addr::UNSPECIFIED.to_string())
            .parse::<IpAddr>()
            .map_err(|error| anyhow::anyhow!("AI_AGENT_RUNNER_HEALTH_HOST is invalid: {error}"))?;
        let port = env_u16("AI_AGENT_RUNNER_HEALTH_PORT", DEFAULT_HEALTH_PORT)?;
        let ready_max_staleness_ms = env_u64(
            "AI_AGENT_RUNNER_READY_MAX_STALENESS_MS",
            DEFAULT_READY_MAX_STALENESS_MS,
        )
        .clamp(MIN_READY_MAX_STALENESS_MS, MAX_READY_MAX_STALENESS_MS);
        let probe_interval_ms = env_u64(
            "AI_AGENT_RUNNER_HEALTH_PROBE_INTERVAL_MS",
            DEFAULT_PROBE_INTERVAL_MS,
        )
        .clamp(MIN_PROBE_INTERVAL_MS, MAX_PROBE_INTERVAL_MS)
        .min(ready_max_staleness_ms);
        let probe_timeout_secs = env_u64(
            "AI_AGENT_RUNNER_HEALTH_PROBE_TIMEOUT_SECS",
            DEFAULT_PROBE_TIMEOUT_SECS,
        )
        .max(1);

        let raw_provider_config = std::env::var("AI_PROVIDER_CONFIG_JSON")
            .map_err(|_| anyhow::anyhow!("AI_PROVIDER_CONFIG_JSON is required"))?;
        let mut required_agents = parse_provider_configs(&raw_provider_config)?
            .into_iter()
            .map(|config| config.name.trim().to_string())
            .collect::<BTreeSet<_>>();
        if env_bool("AI_AGENT_RUNNER_DISTRIBUTED_CLAIMS", false)? {
            let instance_id = std::env::var("AI_AGENT_RUNNER_INSTANCE_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    std::env::var("HOSTNAME")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "distributed runner health requires AI_AGENT_RUNNER_INSTANCE_ID or HOSTNAME"
                    )
                })?;
            required_agents.insert(format!("runner/{}", instance_id.trim()));
        }

        let raw_url = std::env::var("AI_AGENT_RUNNER_BRIDGE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8142/".to_string());
        let mut base_url = Url::parse(raw_url.trim())
            .map_err(|_| anyhow::anyhow!("AI_AGENT_RUNNER_BRIDGE_URL is invalid"))?;
        if base_url.username() != "" || base_url.password().is_some() {
            anyhow::bail!("AI_AGENT_RUNNER_BRIDGE_URL must not contain user information");
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            anyhow::bail!("AI_AGENT_RUNNER_BRIDGE_URL must not contain a query or fragment");
        }
        let bearer =
            env_opt("AI_AGENT_RUNNER_BRIDGE_BEARER").or_else(|| env_opt("API_AUTH_BEARER"));
        let internal_http_hosts = env_opt(INTERNAL_HTTP_HOSTS_ENV);
        validate_bridge_origin(
            &base_url,
            bearer.as_deref(),
            internal_http_hosts.as_deref(),
        )
        .map_err(|error| anyhow::anyhow!("invalid runner bridge configuration: {error}"))?;
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let timeout = Duration::from_secs(probe_timeout_secs);
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .user_agent("fiducia-ai-agent-runner-health/0.1")
            .build()
            .map_err(|_| anyhow::anyhow!("failed to build runner health probe client"))?;

        Ok(Self {
            config: HealthConfig {
                host,
                port,
                ready_max_staleness_ms,
                probe_interval_ms,
            },
            state: Arc::new(HealthState {
                registered: AtomicBool::new(false),
                last_successful_poll_ms: AtomicU64::new(0),
                shutting_down: AtomicBool::new(false),
            }),
            bridge: BridgeProbe {
                base_url,
                bearer,
                http,
            },
            required_agents: Arc::new(required_agents),
        })
    }

    pub async fn run<F>(&self, runner: F) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>>,
    {
        let listener = self.bind().await?;
        let server_health = self.clone();
        let mut server = tokio::spawn(async move { server_health.serve(listener).await });
        let monitor_health = self.clone();
        let mut monitor = tokio::spawn(async move { monitor_health.monitor().await });
        tokio::pin!(runner);

        let result = tokio::select! {
            result = &mut runner => result,
            result = &mut server => match result {
                Ok(Ok(())) => Err(anyhow::anyhow!("runner health server exited unexpectedly")),
                Ok(Err(error)) => Err(anyhow::anyhow!("runner health server failed: {error}")),
                Err(error) => Err(anyhow::anyhow!("runner health task failed: {error}")),
            },
            result = &mut monitor => match result {
                Ok(()) => Err(anyhow::anyhow!("runner health monitor exited unexpectedly")),
                Err(error) => Err(anyhow::anyhow!("runner health monitor task failed: {error}")),
            },
        };

        self.mark_shutting_down();
        server.abort();
        monitor.abort();
        result
    }

    async fn bind(&self) -> anyhow::Result<TcpListener> {
        let address = SocketAddr::new(self.config.host, self.config.port);
        TcpListener::bind(address)
            .await
            .map_err(|error| anyhow::anyhow!("failed to bind runner health listener {address}: {error}"))
    }

    async fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        axum::serve(listener, self.router()).await
    }

    async fn monitor(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(
            self.config.probe_interval_ms,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.probe_once().await;
        }
    }

    async fn probe_once(&self) {
        let agents = self.bridge.get_json("agents").await;
        let registered = agents
            .as_ref()
            .ok()
            .map(registered_agent_keys)
            .is_some_and(|agents| self.required_agents.iter().all(|key| agents.contains(key)));
        self.state.registered.store(registered, Ordering::Release);

        if self.bridge.get_json("workflows").await.is_ok() {
            self.state
                .last_successful_poll_ms
                .store(now_ms(), Ordering::Release);
        }
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(liveness))
            .route("/readyz", get(readiness))
            .with_state(self.clone())
    }

    fn mark_shutting_down(&self) {
        self.state.shutting_down.store(true, Ordering::Release);
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

impl BridgeProbe {
    async fn get_json(&self, path: &str) -> Result<Value, ()> {
        let url = self.base_url.join(path).map_err(|_| ())?;
        if url.origin() != self.base_url.origin() {
            return Err(());
        }
        let mut request = self.http.get(url);
        if let Some(bearer) = &self.bearer {
            request = request.bearer_auth(bearer);
        }
        let response = request.send().await.map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROBE_RESPONSE_BYTES as u64)
        {
            return Err(());
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ())?;
            if bytes.len().saturating_add(chunk.len()) > MAX_PROBE_RESPONSE_BYTES {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ())
    }
}

async fn liveness(State(health): State<RunnerHealth>) -> Json<HealthPayload> {
    let snapshot = health.snapshot(now_ms());
    Json(HealthPayload {
        ok: true,
        status: "alive",
        registered: snapshot.registered,
        poll_fresh: snapshot.poll_fresh,
        shutting_down: snapshot.shutting_down,
        last_successful_poll_age_ms: snapshot.last_successful_poll_age_ms,
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

fn registered_agent_keys(value: &Value) -> BTreeSet<String> {
    value
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| agent.get("agent_key").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

fn env_u16(key: &str, default: u16) -> anyhow::Result<u16> {
    match env_opt(key) {
        Some(value) => value
            .parse::<u16>()
            .map_err(|error| anyhow::anyhow!("{key} is invalid: {error}")),
        None => Ok(default),
    }
}

fn env_bool(key: &str, default: bool) -> anyhow::Result<bool> {
    match env_opt(key)
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(_) => anyhow::bail!("{key} must be a boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_health() -> RunnerHealth {
        RunnerHealth {
            config: HealthConfig {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                ready_max_staleness_ms: 5_000,
                probe_interval_ms: 1_000,
            },
            state: Arc::new(HealthState {
                registered: AtomicBool::new(false),
                last_successful_poll_ms: AtomicU64::new(0),
                shutting_down: AtomicBool::new(false),
            }),
            bridge: BridgeProbe {
                base_url: Url::parse("http://127.0.0.1:1/").unwrap(),
                bearer: None,
                http: reqwest::Client::new(),
            },
            required_agents: Arc::new(BTreeSet::new()),
        }
    }

    #[test]
    fn readiness_requires_registration_and_recent_poll() {
        let health = test_health();
        let now = 100_000;
        assert!(!health.snapshot(now).ready);

        health.state.registered.store(true, Ordering::Release);
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
        let health = test_health();
        health.state.registered.store(true, Ordering::Release);
        health
            .state
            .last_successful_poll_ms
            .store(99_000, Ordering::Release);
        assert!(health.snapshot(100_000).ready);
        health.mark_shutting_down();
        assert!(!health.snapshot(100_000).ready);
    }

    #[test]
    fn agent_response_is_reduced_to_keys_only() {
        let keys = registered_agent_keys(&serde_json::json!({
            "agents": [
                {"agent_key":"codex","meta":{"secret":"must-not-escape"}},
                {"agent_key":"runner/pod-0","host":"internal.example"}
            ]
        }));
        assert_eq!(
            keys,
            BTreeSet::from(["codex".to_string(), "runner/pod-0".to_string()])
        );
    }

    #[tokio::test]
    async fn http_contract_transitions_from_not_ready_to_ready() {
        let health = test_health();
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

        health.state.registered.store(true, Ordering::Release);
        health
            .state
            .last_successful_poll_ms
            .store(now_ms(), Ordering::Release);
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
