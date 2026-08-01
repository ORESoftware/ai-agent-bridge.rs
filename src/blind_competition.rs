//! Blind multi-model competition with append-only submissions and reviewer reveal.
//!
//! Candidate outputs remain hidden from peers, the reviewer, operators, and the
//! shared channel until every worker has submitted and the designated reviewer
//! explicitly reveals the immutable candidate set.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Extension, Path, State},
    http::StatusCode,
    middleware::{from_fn, from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{BridgeError, BridgeResult};
use crate::state::AppState;
use crate::types::{now_ts, Agent, MemberRole, Role};
use crate::workflow_security::AuthenticatedAdapter;

include!("blind_competition/types.rs");
include!("blind_competition/storage.rs");
include!("blind_competition/http.rs");

#[cfg(test)]
include!("blind_competition/tests.rs");
