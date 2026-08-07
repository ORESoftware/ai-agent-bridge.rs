//! ai-agent-bridge: a topic-routed, multi-participant conversation bus for AI
//! agents. See `README.md` and `docs/agents-guide.md`.
//!
//! The in-memory core ([`state`], [`embed`]) has no external dependencies and is
//! what the test suite exercises. Postgres persistence (the `db` module) is
//! optional and compiled in only with `--features postgres`.

pub mod assignment_claims;
pub mod blind_competition;
mod bridge_origin_policy;
pub mod compat;
pub mod config;
mod context_access;
pub mod control_plane;
pub mod embed;
pub mod error;
pub mod http;
pub mod lease_descriptors;
pub mod lease_renewal;
pub mod metrics;
pub mod orchestration;
pub mod policy;
pub mod policy_admission;
pub mod preflight;
pub mod providers;
pub mod runner;
#[expect(
    clippy::collapsible_if,
    reason = "keep Slack delivery and durable journal persistence as explicit ordered side effects"
)]
pub mod slack_bridge;
pub mod slack_commands;
pub mod slack_project_bindings;
pub mod state;
pub mod tcp;
mod tcp_security;
pub mod types;
pub mod workflow_security;

#[cfg(feature = "postgres")]
pub mod db;

pub use config::Config;
pub use embed::Embedder;
pub use state::AppState;
