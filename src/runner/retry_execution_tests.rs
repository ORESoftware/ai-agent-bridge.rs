//! Deterministic end-to-end tests for the DEN-523 retry engine.
//!
//! These cover the acceptance criteria that the pure planner tests in
//! `retry.rs` cannot: no retry attempt starts without a successful policy
//! reservation, cancellation interrupts both sleeping and active attempts,
//! admission loss and workflow submission stop the loop before another
//! attempt, and retried provider request bodies stay byte-identical.
//!
//! The provider and the bridge are scripted loopback HTTP servers so the
//! engine exercises its production transport paths. Paused Tokio time is
//! deliberately not used: with real sockets the paused clock auto-advances
//! to the next armed timer while IO is still in flight, so every client
//! timeout fires spuriously. Determinism instead comes from causal
//! scripting — mock state flips on request arrival, every assertion is an
//! order or count, and the only long delays are interrupt targets that the
//! engine must abandon tens of seconds early, never tuned sleeps.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{Method as HttpMethod, StatusCode as HttpStatusCode};
use axum::response::Response;
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::orchestration::{
    AssignmentRole, WorkflowAssignment, WorkflowMode, WorkflowPlan, WorkflowStage, WorkflowStatus,
    WorkflowSubmission, WorkflowView,
};
use crate::policy::BudgetLimits;
use crate::policy_admission::{
    AdmissionPolicySnapshot, AdmissionRecord, AdmissionStatus, UsageTotals,
};
use crate::providers::{ProviderClient, ProviderConfig, ProviderProtocol, ProviderRequest};

use super::admission::AdmissionControl;
use super::bridge::BridgeClient;
use super::retry::{RetryDelaySource, RetryPolicies, RetryReason};
use super::retry_execution::{execute, RetryAbortReason, RetryRun};
use super::ProviderWorker;

const WORKFLOW_ID: &str = "wf-retry-engine-test";
const AGENT_KEY: &str = "codex";
const TEST_SECRET: &str = "unit-test-provider-secret";

type SharedEvents = Arc<Mutex<Vec<&'static str>>>;

#[derive(Clone)]
struct ScriptedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
}

#[derive(Clone)]
struct ProviderState {
    queued: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    base_status: u16,
    base_body: Value,
    response_delay: Duration,
    bodies: Arc<Mutex<Vec<Value>>>,
    mark_on_request: Option<Arc<AtomicBool>>,
    events: SharedEvents,
}

impl ProviderState {
    fn success_base(events: SharedEvents) -> Self {
        Self {
            queued: Arc::new(Mutex::new(VecDeque::new())),
            base_status: 200,
            base_body: json!({
                "output_text": "engine result",
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
            }),
            response_delay: Duration::ZERO,
            bodies: Arc::new(Mutex::new(Vec::new())),
            mark_on_request: None,
            events,
        }
    }

    fn failing_base(events: SharedEvents) -> Self {
        Self {
            base_status: 503,
            base_body: json!({"error": {"status": "UNAVAILABLE", "message": "provider-controlled"}}),
            ..Self::success_base(events)
        }
    }

    fn with_queued(self, status: u16, headers: &[(&str, &str)], body: Value) -> Self {
        self.queued.lock().unwrap().push_back(ScriptedResponse {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body,
        });
        self
    }

    fn with_response_delay(mut self, delay: Duration) -> Self {
        self.response_delay = delay;
        self
    }

    fn with_mark_on_request(mut self, flag: Arc<AtomicBool>) -> Self {
        self.mark_on_request = Some(flag);
        self
    }
}

async fn provider_handler(State(state): State<ProviderState>, request: AxumRequest) -> Response {
    let (_, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    state.bodies.lock().unwrap().push(body);
    state.events.lock().unwrap().push("provider_request");
    if let Some(flag) = &state.mark_on_request {
        flag.store(true, Ordering::SeqCst);
    }

    let scripted = state.queued.lock().unwrap().pop_front();
    let (status, headers, body) = match scripted {
        Some(response) => (response.status, response.headers, response.body),
        None => (state.base_status, Vec::new(), state.base_body.clone()),
    };
    if !state.response_delay.is_zero() {
        tokio::time::sleep(state.response_delay).await;
    }
    json_response(status, &headers, &body)
}

#[derive(Clone)]
struct BridgeState {
    admission_gets: Arc<AtomicUsize>,
    /// 1-based GET ordinals up to this value return an Active admission;
    /// later GETs return Exhausted. `usize::MAX` keeps admission Active.
    admission_active_gets: usize,
    reject_retry_reservations: bool,
    submitted: Arc<AtomicBool>,
    usage_deltas: Arc<Mutex<Vec<Value>>>,
    events: SharedEvents,
}

impl BridgeState {
    fn active(events: SharedEvents) -> Self {
        Self {
            admission_gets: Arc::new(AtomicUsize::new(0)),
            admission_active_gets: usize::MAX,
            reject_retry_reservations: false,
            submitted: Arc::new(AtomicBool::new(false)),
            usage_deltas: Arc::new(Mutex::new(Vec::new())),
            events,
        }
    }

    fn with_admission_active_gets(mut self, limit: usize) -> Self {
        self.admission_active_gets = limit;
        self
    }

    fn with_rejected_retry_reservations(mut self) -> Self {
        self.reject_retry_reservations = true;
        self
    }

    fn workflow_view(&self) -> WorkflowView {
        let submitted = self.submitted.load(Ordering::SeqCst);
        WorkflowView {
            plan: WorkflowPlan {
                version: 1,
                id: WORKFLOW_ID.into(),
                channel: "retry-engine-tests".into(),
                title: "retry engine".into(),
                prompt: "Solve the issue".into(),
                mode: WorkflowMode::Single,
                created_by: "retry-engine-tests".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                assignments: vec![assignment()],
                file_lease: None,
                required_capabilities: Vec::new(),
                meta: json!({}),
            },
            status: WorkflowStatus {
                stage: if submitted {
                    WorkflowStage::Completed
                } else {
                    WorkflowStage::Running
                },
                current_agent_key: None,
                submitted_agents: if submitted {
                    vec![AGENT_KEY.into()]
                } else {
                    Vec::new()
                },
                pending_agents: if submitted {
                    Vec::new()
                } else {
                    vec![AGENT_KEY.into()]
                },
            },
            submissions: if submitted {
                vec![WorkflowSubmission {
                    workflow_id: WORKFLOW_ID.into(),
                    assignment_ordinal: 0,
                    agent_key: AGENT_KEY.into(),
                    role: AssignmentRole::Worker,
                    content: "done".into(),
                    meta: json!({}),
                    submitted_at: "2026-01-01T00:00:01Z".into(),
                }]
            } else {
                Vec::new()
            },
        }
    }
}

fn admission_record(status: AdmissionStatus) -> AdmissionRecord {
    AdmissionRecord {
        version: 1,
        workflow_id: WORKFLOW_ID.into(),
        requested_by: "retry-engine-tests".into(),
        approved_by: None,
        override_reason: None,
        policy: AdmissionPolicySnapshot {
            policy_version: "retry-engine-tests-1".into(),
            mode: WorkflowMode::Single,
            selected_agent_keys: vec![AGENT_KEY.into()],
            budget: BudgetLimits {
                max_providers: 1,
                max_rounds: 1,
                max_wall_clock_ms: 3_600_000,
                max_input_tokens: 1_000_000,
                max_output_tokens: 1_000_000,
                max_retries: 5,
                max_concurrency: 1,
                max_cost_micro_usd: 10_000_000,
            },
            require_human_approval: false,
            require_fiducia_lease: false,
            reasons: Vec::new(),
        },
        status,
        usage: UsageTotals::default(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        terminal_reason: None,
        last_rejected_delta: None,
    }
}

async fn bridge_handler(State(state): State<BridgeState>, request: AxumRequest) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let path = parts.uri.path().to_string();

    let admission_path = format!("/workflows/{WORKFLOW_ID}/admission");
    let usage_path = format!("/workflows/{WORKFLOW_ID}/admission/usage");
    let cancel_path = format!("/workflows/{WORKFLOW_ID}/admission/cancel");
    let workflow_path = format!("/workflows/{WORKFLOW_ID}");

    if parts.method == HttpMethod::GET && path == admission_path {
        let ordinal = state.admission_gets.fetch_add(1, Ordering::SeqCst) + 1;
        let status = if ordinal <= state.admission_active_gets {
            AdmissionStatus::Active
        } else {
            AdmissionStatus::Exhausted
        };
        return json_response(
            200,
            &[],
            &json!({"admission": serde_json::to_value(admission_record(status)).unwrap()}),
        );
    }
    if parts.method == HttpMethod::POST && path == usage_path {
        let retries = body
            .pointer("/delta/retries")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        state.usage_deltas.lock().unwrap().push(body);
        let status = if retries >= 1 {
            if state.reject_retry_reservations {
                state
                    .events
                    .lock()
                    .unwrap()
                    .push("retry_reservation_rejected");
                AdmissionStatus::Exhausted
            } else {
                state.events.lock().unwrap().push("retry_reserved");
                AdmissionStatus::Active
            }
        } else {
            AdmissionStatus::Active
        };
        return json_response(
            200,
            &[],
            &json!({"admission": serde_json::to_value(admission_record(status)).unwrap()}),
        );
    }
    if parts.method == HttpMethod::POST && path == cancel_path {
        state.events.lock().unwrap().push("admission_cancelled");
        return json_response(
            200,
            &[],
            &json!({"admission": serde_json::to_value(admission_record(AdmissionStatus::Cancelled)).unwrap()}),
        );
    }
    if parts.method == HttpMethod::GET && path == workflow_path {
        return json_response(
            200,
            &[],
            &json!({"workflow": serde_json::to_value(state.workflow_view()).unwrap()}),
        );
    }
    json_response(404, &[], &json!({"error": "unexpected route"}))
}

fn json_response(status: u16, headers: &[(String, String)], body: &Value) -> Response {
    let mut builder = Response::builder()
        .status(HttpStatusCode::from_u16(status).unwrap())
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

async fn spawn_router(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}/")
}

fn assignment() -> WorkflowAssignment {
    WorkflowAssignment {
        ordinal: 0,
        agent_key: AGENT_KEY.into(),
        role: AssignmentRole::Worker,
        phase: 0,
    }
}

fn provider_request() -> ProviderRequest {
    ProviderRequest {
        prompt: "Solve the issue".into(),
        max_output_tokens: 128,
        system: Some("Be precise".into()),
    }
}

struct Harness {
    admission: AdmissionControl,
    bridge: BridgeClient,
    worker: ProviderWorker,
    workflow: WorkflowView,
    assignment: WorkflowAssignment,
    request: ProviderRequest,
    policies: RetryPolicies,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl Harness {
    async fn start(
        provider_state: ProviderState,
        bridge_state: BridgeState,
        retry_policy_json: &str,
    ) -> Self {
        let provider_url = spawn_router(
            Router::new()
                .fallback(provider_handler)
                .with_state(provider_state),
        )
        .await;
        let bridge_url = spawn_router(
            Router::new()
                .fallback(bridge_handler)
                .with_state(bridge_state.clone()),
        )
        .await;

        let config = ProviderConfig {
            name: AGENT_KEY.into(),
            protocol: ProviderProtocol::OpenAiResponses,
            base_url: format!("{provider_url}v1/"),
            model: "gpt-5-codex".into(),
            api_key_env: "UNIT_TEST_PROVIDER_KEY".into(),
            allowed_hosts: vec!["127.0.0.1".into()],
            timeout_secs: 3_600,
            connect_timeout_secs: 3_600,
            max_response_bytes: 1024 * 1024,
        };
        let worker = ProviderWorker {
            client: ProviderClient::for_test_with_api_key(config.clone(), TEST_SECRET).unwrap(),
            config,
            capabilities: Vec::new(),
        };
        let policies =
            RetryPolicies::from_json_for_test(std::slice::from_ref(&worker), retry_policy_json)
                .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            admission: AdmissionControl::for_test(&bridge_url, AGENT_KEY),
            bridge: BridgeClient::for_test(&bridge_url),
            worker,
            workflow: bridge_state.workflow_view(),
            assignment: assignment(),
            request: provider_request(),
            policies,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Reserve the initial provider call (as `execute_assignment` does) and
    /// then run the retry engine to completion.
    async fn run(&mut self) -> RetryRun {
        let (_, reservation) = self
            .admission
            .reserve_call(WORKFLOW_ID, AGENT_KEY, &self.request, 1)
            .await
            .expect("initial provider-call reservation is admitted");
        let policy = self.policies.policy(AGENT_KEY);
        execute(
            &policy,
            self.policies.guard_interval(),
            &format!("{WORKFLOW_ID}/0/{AGENT_KEY}/test-instance"),
            &self.admission,
            &self.bridge,
            &self.worker,
            &self.workflow,
            &self.assignment,
            &self.request,
            reservation,
            &mut self.shutdown_rx,
        )
        .await
    }
}

fn delta_retries(delta: &Value) -> u64 {
    delta
        .pointer("/delta/retries")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX)
}

const DEFAULT_POLICY_JSON: &str = r#"{
    "guard_interval_ms": 250,
    "providers": {
        "codex": {
            "max_retries": 2,
            "base_delay_ms": 100,
            "max_delay_ms": 5000,
            "max_total_delay_ms": 10000
        }
    }
}"#;

/// A retry may only start after a successful policy reservation: the
/// `retries: 1` usage delta must be accepted by the bridge strictly between
/// the failing first attempt and the succeeding second attempt, and the
/// retried request body must be byte-identical.
#[tokio::test]
async fn success_after_transient_failure_reserves_before_second_attempt() {
    let events: SharedEvents = Arc::default();
    let provider = ProviderState::success_base(events.clone()).with_queued(
        429,
        &[("retry-after", "1")],
        json!({"error": {"code": "rate_limit_exceeded", "message": "provider-controlled"}}),
    );
    let provider_bodies = provider.bodies.clone();
    let bridge = BridgeState::active(events.clone());
    let usage_deltas = bridge.usage_deltas.clone();
    let mut harness = Harness::start(provider, bridge, DEFAULT_POLICY_JSON).await;

    let run = harness.run().await;
    let RetryRun::Success(success) = run else {
        panic!("expected retry success");
    };
    assert_eq!(success.response.text, "engine result");
    assert_eq!(success.audits.len(), 1);
    assert_eq!(success.audits[0].retry_ordinal, 1);
    assert_eq!(success.audits[0].delay_source, RetryDelaySource::RetryAfter);
    assert_eq!(success.audits[0].delay_ms, 1_000);
    assert_eq!(success.audits[0].reason, RetryReason::RateLimited(429));
    assert_eq!(success.total_delay, Duration::from_secs(1));

    let events = events.lock().unwrap();
    let ordered: Vec<&str> = events
        .iter()
        .filter(|event| ["provider_request", "retry_reserved"].contains(*event))
        .copied()
        .collect();
    assert_eq!(
        ordered,
        vec!["provider_request", "retry_reserved", "provider_request"],
        "the retry reservation must land between the two provider attempts"
    );

    let deltas = usage_deltas.lock().unwrap();
    assert_eq!(deltas.len(), 2, "initial reservation plus one retry");
    assert_eq!(delta_retries(&deltas[0]), 0);
    assert_eq!(delta_retries(&deltas[1]), 1);
    assert_eq!(
        deltas[1]
            .pointer("/delta/provider_calls")
            .and_then(Value::as_u64),
        Some(1),
        "every retry reserves a provider call"
    );

    let bodies = provider_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1], "retried request body must not drift");
}

/// A rejected retry reservation terminates the loop without a second
/// provider attempt: retry accounting is still incremented at reservation
/// time even though the retry never ran.
#[tokio::test]
async fn rejected_reservation_aborts_without_second_attempt() {
    let events: SharedEvents = Arc::default();
    let provider = ProviderState::failing_base(events.clone());
    let provider_bodies = provider.bodies.clone();
    let bridge = BridgeState::active(events.clone()).with_rejected_retry_reservations();
    let usage_deltas = bridge.usage_deltas.clone();
    let mut harness = Harness::start(provider, bridge, DEFAULT_POLICY_JSON).await;

    let run = harness.run().await;
    let RetryRun::Aborted { reason, audits, .. } = run else {
        panic!("expected an aborted retry run");
    };
    assert_eq!(reason, RetryAbortReason::RetryReservationRejected);
    assert!(audits.is_empty(), "no reserved retry ever started");
    assert_eq!(provider_bodies.lock().unwrap().len(), 1);

    let deltas = usage_deltas.lock().unwrap();
    assert_eq!(deltas.len(), 2);
    assert_eq!(
        delta_retries(&deltas[1]),
        1,
        "the failed retry attempt is still accounted at reservation time"
    );
}

/// Shutdown must interrupt a sleeping backoff immediately instead of waiting
/// out the planned Retry-After delay: the 5s Retry-After sleep is only an
/// interrupt target, and the run must end as soon as the shutdown signal
/// lands roughly 100ms in.
#[tokio::test]
async fn shutdown_interrupts_backoff_sleep() {
    let events: SharedEvents = Arc::default();
    let provider = ProviderState::success_base(events.clone()).with_queued(
        503,
        &[("retry-after", "5")],
        json!({"error": {"status": "UNAVAILABLE", "message": "provider-controlled"}}),
    );
    let provider_bodies = provider.bodies.clone();
    let bridge = BridgeState::active(events.clone());
    let usage_deltas = bridge.usage_deltas.clone();
    let mut harness = Harness::start(provider, bridge, DEFAULT_POLICY_JSON).await;

    let shutdown = harness.shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = shutdown.send(true);
    });

    let started = tokio::time::Instant::now();
    let run = harness.run().await;
    let elapsed = started.elapsed();
    let RetryRun::Aborted { reason, audits, .. } = run else {
        panic!("expected an aborted retry run");
    };
    assert_eq!(reason, RetryAbortReason::RunnerShutdown);
    assert!(audits.is_empty());
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown must interrupt the 5s Retry-After sleep; elapsed {elapsed:?}"
    );
    assert_eq!(provider_bodies.lock().unwrap().len(), 1);
    assert_eq!(
        usage_deltas.lock().unwrap().len(),
        1,
        "no retry reservation may be attempted after shutdown"
    );
}

/// Losing admission while a provider attempt is in flight must abort the
/// attempt at the next guard check instead of waiting for the response.
#[tokio::test]
async fn admission_loss_cancels_active_attempt() {
    let events: SharedEvents = Arc::default();
    let provider =
        ProviderState::success_base(events.clone()).with_response_delay(Duration::from_secs(10));
    let provider_bodies = provider.bodies.clone();
    let bridge = BridgeState::active(events.clone()).with_admission_active_gets(1);
    let mut harness = Harness::start(provider, bridge, DEFAULT_POLICY_JSON).await;

    let started = tokio::time::Instant::now();
    let run = harness.run().await;
    let elapsed = started.elapsed();
    let RetryRun::Aborted { reason, .. } = run else {
        panic!("expected an aborted retry run");
    };
    assert_eq!(reason, RetryAbortReason::AdmissionNotActive);
    assert!(
        elapsed < Duration::from_secs(10),
        "the in-flight attempt must be abandoned at a guard check; elapsed {elapsed:?}"
    );
    assert_eq!(
        provider_bodies.lock().unwrap().len(),
        1,
        "the attempt reached the provider and was abandoned in flight"
    );
}

/// Once this assignment has a submission, the guard must refuse further
/// retries: the response arrived after submission began and must be
/// discarded rather than retried.
#[tokio::test]
async fn submission_guard_prevents_retry_after_submission() {
    let events: SharedEvents = Arc::default();
    let bridge = BridgeState::active(events.clone());
    let provider =
        ProviderState::failing_base(events.clone()).with_mark_on_request(bridge.submitted.clone());
    let provider_bodies = provider.bodies.clone();
    let usage_deltas = bridge.usage_deltas.clone();
    let mut harness = Harness::start(provider, bridge, DEFAULT_POLICY_JSON).await;

    let run = harness.run().await;
    let RetryRun::Aborted { reason, audits, .. } = run else {
        panic!("expected an aborted retry run");
    };
    assert_eq!(reason, RetryAbortReason::WorkflowNotPending);
    assert!(audits.is_empty());
    assert_eq!(provider_bodies.lock().unwrap().len(), 1);
    assert_eq!(
        usage_deltas.lock().unwrap().len(),
        1,
        "no retry reservation may follow a workflow submission"
    );
}
