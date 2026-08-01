//! Durable policy admission and cumulative usage accounting for managed workflows.
//!
//! Admission is stored inside the workflow's persisted shared context. The policy
//! engine is evaluated server-side; callers cannot submit a pre-approved decision.
//! Every usage increment is checked atomically before it is accepted. The first
//! over-budget attempt terminally exhausts the admission and must cancel provider
//! work plus any Fiducia-backed write authority.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{BridgeError, BridgeResult};
use crate::orchestration::{WorkflowMode, WorkflowPlan};
use crate::policy::{self, BudgetLimits, ExecutionTarget, PolicyRequest};
use crate::state::AppState;
use crate::types::now_ts;

include!("policy_admission/types.rs");
include!("policy_admission/storage.rs");
include!("policy_admission/http.rs");

#[cfg(test)]
include!("policy_admission/tests.rs");
