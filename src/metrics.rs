//! Bounded-cardinality Prometheus metrics for the bridge process.
//!
//! Metrics deliberately avoid prompts, message bodies, channel names, agent keys,
//! repository paths, provider model names, tokens, peer addresses, and user data.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::BridgeError;
use crate::state::AppState;

const LATENCY_BOUNDS_SECONDS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

pub fn global() -> &'static BridgeMetrics {
    static METRICS: OnceLock<BridgeMetrics> = OnceLock::new();
    METRICS.get_or_init(BridgeMetrics::new)
}

#[derive(Clone, Copy, Debug)]
pub struct StateMetricsSnapshot {
    pub agents: u64,
    pub channels: u64,
    pub members: u64,
    pub retained_messages: u64,
    pub total_messages: u64,
    pub context_keys: u64,
    pub broadcast_queue_depth: u64,
    pub active_file_leases: u64,
    pub inbox_messages: u64,
    pub active_sse_connections: u64,
    pub max_agents: u64,
    pub max_channels: u64,
    pub max_file_leases: u64,
    pub max_sse_connections: u64,
    pub max_tcp_connections: u64,
    pub persistence_mode: &'static str,
    pub persistence_ready: bool,
    pub persistence_queue_depth: u64,
    pub persistence_shed_writes: u64,
    pub control_plane_configured: bool,
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
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
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

pub struct BridgeMetrics {
    started_at: Instant,
    started_epoch_seconds: u64,
    http_in_flight: AtomicU64,
    http_2xx: AtomicU64,
    http_4xx: AtomicU64,
    http_5xx: AtomicU64,
    http_other: AtomicU64,
    http_capacity_rejected: AtomicU64,
    http_duration: Histogram,
    tcp_active: AtomicU64,
    tcp_accepted: AtomicU64,
    tcp_rejected: AtomicU64,
    tcp_frames_ok: AtomicU64,
    tcp_frames_error: AtomicU64,
    messages_accepted: AtomicU64,
    messages_no_subscribers: AtomicU64,
    messages_evicted: AtomicU64,
    message_send_duration: Histogram,
    lease_conflict: AtomicU64,
    lease_not_found: AtomicU64,
    lease_owner_mismatch: AtomicU64,
    lease_stale_fencing: AtomicU64,
    control_plane_in_flight: AtomicU64,
    control_plane_success: AtomicU64,
    control_plane_client_error: AtomicU64,
    control_plane_server_error: AtomicU64,
    control_plane_transport_error: AtomicU64,
    control_plane_last_success_epoch_seconds: AtomicU64,
    control_plane_duration: Histogram,
}

impl BridgeMetrics {
    fn new() -> Self {
        let started_epoch_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            started_at: Instant::now(),
            started_epoch_seconds,
            http_in_flight: AtomicU64::new(0),
            http_2xx: AtomicU64::new(0),
            http_4xx: AtomicU64::new(0),
            http_5xx: AtomicU64::new(0),
            http_other: AtomicU64::new(0),
            http_capacity_rejected: AtomicU64::new(0),
            http_duration: Histogram::new(),
            tcp_active: AtomicU64::new(0),
            tcp_accepted: AtomicU64::new(0),
            tcp_rejected: AtomicU64::new(0),
            tcp_frames_ok: AtomicU64::new(0),
            tcp_frames_error: AtomicU64::new(0),
            messages_accepted: AtomicU64::new(0),
            messages_no_subscribers: AtomicU64::new(0),
            messages_evicted: AtomicU64::new(0),
            message_send_duration: Histogram::new(),
            lease_conflict: AtomicU64::new(0),
            lease_not_found: AtomicU64::new(0),
            lease_owner_mismatch: AtomicU64::new(0),
            lease_stale_fencing: AtomicU64::new(0),
            control_plane_in_flight: AtomicU64::new(0),
            control_plane_success: AtomicU64::new(0),
            control_plane_client_error: AtomicU64::new(0),
            control_plane_server_error: AtomicU64::new(0),
            control_plane_transport_error: AtomicU64::new(0),
            control_plane_last_success_epoch_seconds: AtomicU64::new(0),
            control_plane_duration: Histogram::new(),
        }
    }

    pub fn http_started(&self) -> Instant {
        self.http_in_flight.fetch_add(1, Ordering::Relaxed);
        Instant::now()
    }

    pub fn http_finished(&self, started: Instant, status: u16) {
        self.http_in_flight.fetch_sub(1, Ordering::Relaxed);
        match status {
            200..=299 => &self.http_2xx,
            400..=499 => &self.http_4xx,
            500..=599 => &self.http_5xx,
            _ => &self.http_other,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.http_duration.observe(started.elapsed());
    }

    pub fn http_capacity_rejected(&self) {
        self.http_capacity_rejected
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn tcp_connection(&self) -> TcpConnectionGuard<'_> {
        self.tcp_accepted.fetch_add(1, Ordering::Relaxed);
        self.tcp_active.fetch_add(1, Ordering::Relaxed);
        TcpConnectionGuard { metrics: self }
    }

    pub fn tcp_rejected(&self) {
        self.tcp_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn tcp_frame(&self, ok: bool) {
        if ok {
            self.tcp_frames_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.tcp_frames_error.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn observe_message(&self, duration: Duration, receivers: usize, evicted: u64) {
        self.messages_accepted.fetch_add(1, Ordering::Relaxed);
        if receivers == 0 {
            self.messages_no_subscribers
                .fetch_add(1, Ordering::Relaxed);
        }
        self.messages_evicted.fetch_add(evicted, Ordering::Relaxed);
        self.message_send_duration.observe(duration);
    }

    pub fn observe_bridge_error(&self, error: &BridgeError) {
        match error {
            BridgeError::FileLeaseConflict { .. } => {
                self.lease_conflict.fetch_add(1, Ordering::Relaxed);
            }
            BridgeError::FileLeaseNotFound(_) => {
                self.lease_not_found.fetch_add(1, Ordering::Relaxed);
            }
            BridgeError::FileLeaseOwnerMismatch { .. } => {
                self.lease_owner_mismatch.fetch_add(1, Ordering::Relaxed);
            }
            BridgeError::StaleFencingToken(_) => {
                self.lease_stale_fencing.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn control_plane_started(&self) -> Instant {
        self.control_plane_in_flight
            .fetch_add(1, Ordering::Relaxed);
        Instant::now()
    }

    pub fn control_plane_finished(&self, started: Instant, result: ControlPlaneResult) {
        self.control_plane_in_flight
            .fetch_sub(1, Ordering::Relaxed);
        match result {
            ControlPlaneResult::Success => {
                self.control_plane_success.fetch_add(1, Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                self.control_plane_last_success_epoch_seconds
                    .store(now, Ordering::Relaxed);
            }
            ControlPlaneResult::ClientError => {
                self.control_plane_client_error
                    .fetch_add(1, Ordering::Relaxed);
            }
            ControlPlaneResult::ServerError => {
                self.control_plane_server_error
                    .fetch_add(1, Ordering::Relaxed);
            }
            ControlPlaneResult::TransportError => {
                self.control_plane_transport_error
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.control_plane_duration.observe(started.elapsed());
    }

    pub fn render(&self, state: &AppState) -> String {
        let snapshot = state.metrics_snapshot();
        let mut output = String::with_capacity(12 * 1024);
        let version = escape_label(env!("CARGO_PKG_VERSION"));

        metric_header(
            &mut output,
            "ai_agent_bridge_build_info",
            "Static bridge build information.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "ai_agent_bridge_build_info{{version=\"{version}\"}} 1"
        );
        gauge(
            &mut output,
            "ai_agent_bridge_process_start_time_seconds",
            "Unix timestamp when this process metrics registry started.",
            self.started_epoch_seconds,
        );
        gauge_f64(
            &mut output,
            "ai_agent_bridge_process_uptime_seconds",
            "Bridge process uptime in seconds.",
            self.started_at.elapsed().as_secs_f64(),
        );
        if let Some(bytes) = resident_memory_bytes() {
            gauge(
                &mut output,
                "ai_agent_bridge_process_resident_memory_bytes",
                "Resident process memory in bytes when supported by the host.",
                bytes,
            );
        }

        gauge(
            &mut output,
            "ai_agent_bridge_http_requests_in_flight",
            "Ordinary HTTP requests currently executing.",
            self.http_in_flight.load(Ordering::Relaxed),
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_http_requests_total",
            "HTTP responses grouped only by bounded status class.",
            "status_class",
            &[
                ("2xx", self.http_2xx.load(Ordering::Relaxed)),
                ("4xx", self.http_4xx.load(Ordering::Relaxed)),
                ("5xx", self.http_5xx.load(Ordering::Relaxed)),
                ("other", self.http_other.load(Ordering::Relaxed)),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_http_rejected_total",
            "HTTP work rejected before execution.",
            "reason",
            &[(
                "capacity",
                self.http_capacity_rejected.load(Ordering::Relaxed),
            )],
        );
        self.http_duration.render(
            &mut output,
            "ai_agent_bridge_http_request_duration_seconds",
            "HTTP request duration without path, identity, or payload labels.",
        );

        gauge(
            &mut output,
            "ai_agent_bridge_tcp_connections_active",
            "Accepted TCP connections currently active.",
            self.tcp_active.load(Ordering::Relaxed),
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_tcp_connections_total",
            "TCP connection admissions grouped by bounded result.",
            "result",
            &[
                ("accepted", self.tcp_accepted.load(Ordering::Relaxed)),
                ("rejected", self.tcp_rejected.load(Ordering::Relaxed)),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_tcp_frames_total",
            "TCP request frames grouped by bounded outcome.",
            "result",
            &[
                ("ok", self.tcp_frames_ok.load(Ordering::Relaxed)),
                ("error", self.tcp_frames_error.load(Ordering::Relaxed)),
            ],
        );

        counter_family(
            &mut output,
            "ai_agent_bridge_messages_total",
            "Accepted messages grouped by bounded delivery outcome.",
            "result",
            &[
                ("accepted", self.messages_accepted.load(Ordering::Relaxed)),
                (
                    "no_subscribers",
                    self.messages_no_subscribers.load(Ordering::Relaxed),
                ),
            ],
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_messages_evicted_total",
            "Messages evicted from bounded retained history.",
            "reason",
            &[(
                "history_limit",
                self.messages_evicted.load(Ordering::Relaxed),
            )],
        );
        self.message_send_duration.render(
            &mut output,
            "ai_agent_bridge_message_send_duration_seconds",
            "Time to validate, retain, and publish one accepted message.",
        );

        gauge(
            &mut output,
            "ai_agent_bridge_agents",
            "Currently registered agents.",
            snapshot.agents,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_channels",
            "Currently retained channels.",
            snapshot.channels,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_channel_members",
            "Current channel memberships summed across channels.",
            snapshot.members,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_messages_retained",
            "Messages currently retained in bounded channel history.",
            snapshot.retained_messages,
        );
        counter(
            &mut output,
            "ai_agent_bridge_messages_created_total",
            "Cumulative messages created across all retained channels.",
            snapshot.total_messages,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_context_keys",
            "Current shared-context key count including internal records.",
            snapshot.context_keys,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_broadcast_queue_depth",
            "Current bounded broadcast backlog summed across channels.",
            snapshot.broadcast_queue_depth,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_file_leases_active",
            "Unexpired local file leases currently active.",
            snapshot.active_file_leases,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_inbox_messages",
            "Current compatibility inbox line count.",
            snapshot.inbox_messages,
        );
        gauge(
            &mut output,
            "ai_agent_bridge_sse_connections_active",
            "Active SSE connections.",
            snapshot.active_sse_connections,
        );

        counter_family(
            &mut output,
            "ai_agent_bridge_file_lease_errors_total",
            "Lease and fencing failures grouped by bounded reason.",
            "reason",
            &[
                ("conflict", self.lease_conflict.load(Ordering::Relaxed)),
                ("not_found", self.lease_not_found.load(Ordering::Relaxed)),
                (
                    "owner_mismatch",
                    self.lease_owner_mismatch.load(Ordering::Relaxed),
                ),
                (
                    "stale_fencing_token",
                    self.lease_stale_fencing.load(Ordering::Relaxed),
                ),
            ],
        );

        let mode = escape_label(snapshot.persistence_mode);
        metric_header(
            &mut output,
            "ai_agent_bridge_persistence_info",
            "Selected persistence mode.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "ai_agent_bridge_persistence_info{{mode=\"{mode}\"}} 1"
        );
        gauge(
            &mut output,
            "ai_agent_bridge_persistence_ready",
            "Whether the selected persistence mode is ready.",
            u64::from(snapshot.persistence_ready),
        );
        gauge(
            &mut output,
            "ai_agent_bridge_persistence_queue_depth",
            "Accepted best-effort persistence writes currently in flight.",
            snapshot.persistence_queue_depth,
        );
        counter(
            &mut output,
            "ai_agent_bridge_persistence_shed_writes_total",
            "Best-effort persistence writes shed under overload.",
            snapshot.persistence_shed_writes,
        );

        metric_header(
            &mut output,
            "ai_agent_bridge_dependency_configured",
            "Whether a bounded external dependency is configured.",
            "gauge",
        );
        let _ = writeln!(
            output,
            "ai_agent_bridge_dependency_configured{{dependency=\"control_plane\"}} {}",
            u64::from(snapshot.control_plane_configured)
        );
        gauge(
            &mut output,
            "ai_agent_bridge_control_plane_requests_in_flight",
            "Control-plane requests currently in flight.",
            self.control_plane_in_flight.load(Ordering::Relaxed),
        );
        counter_family(
            &mut output,
            "ai_agent_bridge_control_plane_requests_total",
            "Control-plane requests grouped by bounded result.",
            "result",
            &[
                ("success", self.control_plane_success.load(Ordering::Relaxed)),
                (
                    "client_error",
                    self.control_plane_client_error.load(Ordering::Relaxed),
                ),
                (
                    "server_error",
                    self.control_plane_server_error.load(Ordering::Relaxed),
                ),
                (
                    "transport_error",
                    self.control_plane_transport_error.load(Ordering::Relaxed),
                ),
            ],
        );
        gauge(
            &mut output,
            "ai_agent_bridge_control_plane_last_success_timestamp_seconds",
            "Unix timestamp of the last successful control-plane request, or zero.",
            self.control_plane_last_success_epoch_seconds
                .load(Ordering::Relaxed),
        );
        self.control_plane_duration.render(
            &mut output,
            "ai_agent_bridge_control_plane_request_duration_seconds",
            "Control-plane request duration without URL or repository labels.",
        );

        capacity_gauge(
            &mut output,
            "agents",
            snapshot.agents,
            snapshot.max_agents,
        );
        capacity_gauge(
            &mut output,
            "channels",
            snapshot.channels,
            snapshot.max_channels,
        );
        capacity_gauge(
            &mut output,
            "file_leases",
            snapshot.active_file_leases,
            snapshot.max_file_leases,
        );
        capacity_gauge(
            &mut output,
            "sse_connections",
            snapshot.active_sse_connections,
            snapshot.max_sse_connections,
        );
        capacity_gauge(
            &mut output,
            "tcp_connections",
            self.tcp_active.load(Ordering::Relaxed),
            snapshot.max_tcp_connections,
        );

        output
    }
}

pub struct TcpConnectionGuard<'a> {
    metrics: &'a BridgeMetrics,
}

impl Drop for TcpConnectionGuard<'_> {
    fn drop(&mut self) {
        self.metrics.tcp_active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ControlPlaneResult {
    Success,
    ClientError,
    ServerError,
    TransportError,
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

fn capacity_gauge(output: &mut String, resource: &str, current: u64, limit: u64) {
    metric_header(
        output,
        "ai_agent_bridge_capacity",
        "Current usage and configured limit for bounded bridge resources.",
        "gauge",
    );
    let _ = writeln!(
        output,
        "ai_agent_bridge_capacity{{resource=\"{resource}\",kind=\"current\"}} {current}"
    );
    let _ = writeln!(
        output,
        "ai_agent_bridge_capacity{{resource=\"{resource}\",kind=\"limit\"}} {limit}"
    );
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

fn resident_memory_bytes() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = raw.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page_size > 0).then(|| resident_pages.saturating_mul(page_size as u64))
}
