//! ai-agent-bridge: a topic-routed, multi-participant conversation bus for AI
//! agents. See `README.md` and `docs/agents-guide.md`.
//!
//! The in-memory core ([`state`], [`embed`]) has no external dependencies and is
//! what the test suite exercises. Postgres persistence ([`db`]) is optional and
//! compiled in only with `--features postgres`.

pub mod compat;
pub mod config;
pub mod control_plane;
pub mod embed;
pub mod error;
pub mod http;
pub mod preflight;
pub mod state;
pub mod tcp;
pub mod types;

#[cfg(feature = "postgres")]
pub mod db;

pub use config::Config;
pub use embed::Embedder;
pub use state::AppState;
