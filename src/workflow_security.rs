//! HTTP authorization boundary for managed workflow adapters.
//!
//! Scoped adapter credentials are optional for compatibility. When configured,
//! adapters receive only explicit HTTP capabilities and requests that carry an
//! identity field must match the authenticated adapter. Generic HTTP context writes
//! can never target reserved internal namespaces such as `workflow.*`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

include!("workflow_security/types.rs");
include!("workflow_security/config.rs");
include!("workflow_security/middleware.rs");

#[cfg(test)]
include!("workflow_security/tests.rs");
