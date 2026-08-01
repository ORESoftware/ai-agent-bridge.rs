//! Deterministic, versioned execution-policy decisions for multi-model workflows.
//!
//! The policy engine is deliberately provider-neutral. It chooses a workflow mode,
//! provider roles, and hard resource ceilings before any provider execution starts.
//! Callers can dry-run a decision through `/workflow-policy/explain` without
//! creating a workflow or spending provider tokens.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::orchestration::WorkflowMode;
use crate::types::AgentKind;

include!("policy/types.rs");
include!("policy/http.rs");
include!("policy/engine.rs");

#[cfg(test)]
include!("policy/tests.rs");
