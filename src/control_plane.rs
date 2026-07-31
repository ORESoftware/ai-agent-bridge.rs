//! Narrow HTTP client for the agent control plane's file-lease API.

use std::time::Duration;

use futures::StreamExt;
use reqwest::Method;
use serde_json::Value;

use crate::config::Config;
use crate::error::{BridgeError, BridgeResult};

const MAX_RESPONSE_BYTES: usize = 1_048_576;

#[derive(Clone)]
pub struct ControlPlaneClient {
    base_url: String,
    secret: Option<String>,
    http: reqwest::Client,
}

pub struct ControlPlaneResponse {
    pub status: u16,
    pub body: Value,
}

impl ControlPlaneClient {
    pub fn from_config(config: &Config) -> Option<Self> {
        let base_url = config.control_plane_url.as_deref()?;
        let base_url = base_url.trim().trim_end_matches('/');
        let timeout = Duration::from_secs(config.control_plane_timeout_secs.max(1));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .user_agent("fiducia-ai-agent-bridge/0.1")
            .build()
            .expect("static control-plane HTTP client configuration must be valid");
        Some(Self {
            base_url: base_url.to_string(),
            secret: config.control_plane_secret.clone(),
            http,
        })
    }

    pub async fn acquire(&self, body: &Value) -> BridgeResult<ControlPlaneResponse> {
        self.request(Method::POST, "/v1/file-leases/acquire", Some(body), &[])
            .await
    }

    pub async fn renew(&self, body: &Value) -> BridgeResult<ControlPlaneResponse> {
        self.request(Method::POST, "/v1/file-leases/renew", Some(body), &[])
            .await
    }

    pub async fn release(&self, body: &Value) -> BridgeResult<ControlPlaneResponse> {
        self.request(Method::POST, "/v1/file-leases/release", Some(body), &[])
            .await
    }

    pub async fn lookup(&self, repository: &str, path: &str) -> BridgeResult<ControlPlaneResponse> {
        self.request(
            Method::GET,
            "/v1/file-leases",
            None,
            &[("repository", repository), ("path", path)],
        )
        .await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        query: &[(&str, &str)],
    ) -> BridgeResult<ControlPlaneResponse> {
        let started = crate::metrics::global().control_plane_started();
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .query(query);
        if let Some(secret) = &self.secret {
            request = request.header("x-internal-auth", secret);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                crate::metrics::global().control_plane_finished(
                    started,
                    crate::metrics::ControlPlaneResult::TransportError,
                );
                let detail = if error.is_timeout() {
                    "request timed out"
                } else if error.is_connect() {
                    "connection failed"
                } else {
                    "request failed"
                };
                return Err(BridgeError::ControlPlane(detail.to_string()));
            }
        };
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            crate::metrics::global().control_plane_finished(
                started,
                crate::metrics::ControlPlaneResult::TransportError,
            );
            return Err(BridgeError::ControlPlane(
                "response exceeded 1 MiB".to_string(),
            ));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    crate::metrics::global().control_plane_finished(
                        started,
                        crate::metrics::ControlPlaneResult::TransportError,
                    );
                    return Err(BridgeError::ControlPlane(
                        "response read failed".to_string(),
                    ));
                }
            };
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                crate::metrics::global().control_plane_finished(
                    started,
                    crate::metrics::ControlPlaneResult::TransportError,
                );
                return Err(BridgeError::ControlPlane(
                    "response exceeded 1 MiB".to_string(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = if bytes.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::json!({
                    "error": "control_plane_error",
                    "detail": "control plane returned a non-JSON response"
                })
            })
        };
        let result = match status {
            200..=299 => crate::metrics::ControlPlaneResult::Success,
            400..=499 => crate::metrics::ControlPlaneResult::ClientError,
            _ => crate::metrics::ControlPlaneResult::ServerError,
        };
        crate::metrics::global().control_plane_finished(started, result);
        Ok(ControlPlaneResponse { status, body })
    }
}
