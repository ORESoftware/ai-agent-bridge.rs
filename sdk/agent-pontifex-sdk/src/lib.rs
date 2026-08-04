#![forbid(unsafe_code)]

//! Typed HTTP client for Agent Pontifex-compatible bridge and coordinator
//! services. Credentials are stored in sensitive header values, redirects are
//! disabled, response bodies are bounded, and dynamic path values are encoded as
//! individual URL segments.

pub use agent_pontifex_protocol as protocol;

use protocol::{bridge, coordinator, ErrorResponse};
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: Url,
    authorization: Option<HeaderValue>,
}

impl Client {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, SdkError> {
        let base_url = normalize_base_url(base_url.as_ref())?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base_url,
            authorization: None,
        })
    }

    pub fn with_bearer(mut self, token: impl AsRef<str>) -> Result<Self, SdkError> {
        let token = token.as_ref();
        if token.is_empty()
            || token.trim() != token
            || token.len() > 16 * 1024
            || token.chars().any(char::is_control)
        {
            return Err(SdkError::InvalidBearer);
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| SdkError::InvalidBearer)?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    pub fn bridge(&self) -> BridgeClient {
        BridgeClient {
            client: self.clone(),
        }
    }

    pub fn coordinator(&self) -> CoordinatorClient {
        CoordinatorClient {
            client: self.clone(),
        }
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, SdkError> {
        let mut url = self.base_url.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                SdkError::InvalidBaseUrl("base URL cannot carry path segments".into())
            })?;
            path.pop_if_empty();
            for segment in segments {
                validate_path_segment(segment)?;
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, url)
            .header(ACCEPT, "application/json");
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        request
    }

    async fn execute(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, Vec<u8>), SdkError> {
        let response = request.send().await?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(SdkError::ResponseTooLarge);
        }
        let body = response.bytes().await?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(SdkError::ResponseTooLarge);
        }
        Ok((status, body.to_vec()))
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, SdkError> {
        let (status, body) = self.execute(request).await?;
        ensure_success(status, &body)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

#[derive(Clone)]
pub struct BridgeClient {
    client: Client,
}

impl BridgeClient {
    pub async fn register_agent(
        &self,
        request: &bridge::RegisterAgentRequest,
    ) -> Result<bridge::RegisterAgentResponse, SdkError> {
        self.post_json(&["agents", "register"], request).await
    }

    pub async fn resolve_channel(
        &self,
        request: &bridge::ResolveChannelRequest,
    ) -> Result<bridge::ResolveChannelResponse, SdkError> {
        self.post_json(&["channels", "resolve"], request).await
    }

    pub async fn post_message(
        &self,
        channel: &str,
        request: &bridge::PostMessageRequest,
    ) -> Result<bridge::PostMessageResponse, SdkError> {
        self.post_json(&["channels", channel, "messages"], request)
            .await
    }

    pub async fn list_messages(
        &self,
        channel: &str,
        since: Option<u64>,
    ) -> Result<bridge::MessagesResponse, SdkError> {
        let mut url = self.client.endpoint(&["channels", channel, "messages"])?;
        if let Some(since) = since {
            url.query_pairs_mut()
                .append_pair("since", &since.to_string());
        }
        let request = self.client.request(Method::GET, url);
        self.client.decode(request).await
    }

    pub async fn acquire_file_lease(
        &self,
        request: &bridge::AcquireFileLeaseRequest,
    ) -> Result<serde_json::Value, SdkError> {
        self.post_json(&["file-leases", "acquire"], request).await
    }

    async fn post_json<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &[&str],
        body: &T,
    ) -> Result<R, SdkError> {
        let url = self.client.endpoint(path)?;
        let request = self.client.request(Method::POST, url).json(body);
        self.client.decode(request).await
    }
}

#[derive(Clone)]
pub struct CoordinatorClient {
    client: Client,
}

impl CoordinatorClient {
    pub async fn create_job(
        &self,
        request: &coordinator::CreateJobRequest,
        idempotency_key: Option<&str>,
    ) -> Result<coordinator::Job, SdkError> {
        let url = self.client.endpoint(&["v1", "jobs"])?;
        let mut builder = self.client.request(Method::POST, url).json(request);
        if let Some(key) = idempotency_key {
            let value = idempotency_header(key)?;
            builder = builder.header("idempotency-key", value);
        }
        let response: coordinator::JobResponse = self.client.decode(builder).await?;
        Ok(response.job)
    }

    pub async fn claim_job(
        &self,
        request: &coordinator::ClaimJobRequest,
    ) -> Result<Option<coordinator::Job>, SdkError> {
        let url = self.client.endpoint(&["v1", "jobs", "claim"])?;
        let builder = self.client.request(Method::POST, url).json(request);
        let (status, body) = self.client.execute(builder).await?;
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        ensure_success(status, &body)?;
        let response: coordinator::JobResponse = serde_json::from_slice(&body)?;
        Ok(Some(response.job))
    }

    pub async fn get_job(&self, job_id: &str) -> Result<coordinator::Job, SdkError> {
        let url = self.client.endpoint(&["v1", "jobs", job_id])?;
        let response: coordinator::JobResponse = self
            .client
            .decode(self.client.request(Method::GET, url))
            .await?;
        Ok(response.job)
    }

    pub async fn heartbeat_job(
        &self,
        job_id: &str,
        request: &coordinator::HeartbeatJobRequest,
    ) -> Result<coordinator::Job, SdkError> {
        self.job_mutation(job_id, "heartbeat", request).await
    }

    pub async fn complete_job(
        &self,
        job_id: &str,
        request: &coordinator::CompleteJobRequest,
    ) -> Result<coordinator::Job, SdkError> {
        self.job_mutation(job_id, "complete", request).await
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<coordinator::Job, SdkError> {
        let url = self.client.endpoint(&["v1", "jobs", job_id, "cancel"])?;
        let response: coordinator::JobResponse = self
            .client
            .decode(self.client.request(Method::POST, url))
            .await?;
        Ok(response.job)
    }

    async fn job_mutation<T: Serialize + ?Sized>(
        &self,
        job_id: &str,
        operation: &str,
        body: &T,
    ) -> Result<coordinator::Job, SdkError> {
        let url = self.client.endpoint(&["v1", "jobs", job_id, operation])?;
        let builder = self.client.request(Method::POST, url).json(body);
        let response: coordinator::JobResponse = self.client.decode(builder).await?;
        Ok(response.job)
    }
}

fn normalize_base_url(input: &str) -> Result<Url, SdkError> {
    let mut url = Url::parse(input).map_err(|error| SdkError::InvalidBaseUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SdkError::InvalidBaseUrl(
            "only http and https are supported".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "credentials are not allowed in the base URL".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SdkError::InvalidBaseUrl(
            "query strings and fragments are not allowed in the base URL".into(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn validate_path_segment(segment: &str) -> Result<(), SdkError> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.len() > 1024
        || segment.chars().any(char::is_control)
    {
        return Err(SdkError::InvalidPathSegment);
    }
    Ok(())
}

fn idempotency_header(key: &str) -> Result<HeaderValue, SdkError> {
    if key.is_empty()
        || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || key.trim() != key
        || key.chars().any(char::is_control)
    {
        return Err(SdkError::InvalidIdempotencyKey);
    }
    HeaderValue::from_str(key).map_err(|_| SdkError::InvalidIdempotencyKey)
}

fn ensure_success(status: StatusCode, body: &[u8]) -> Result<(), SdkError> {
    if status.is_success() {
        return Ok(());
    }
    let message = serde_json::from_slice::<ErrorResponse>(body)
        .ok()
        .map(|response| response.error)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("request failed")
                .to_string()
        });
    Err(SdkError::Http {
        status: status.as_u16(),
        message: sanitize_public_message(&message),
    })
}

fn sanitize_public_message(message: &str) -> String {
    let sanitized: String = message
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect();
    if sanitized.is_empty() {
        "request failed".to_string()
    } else {
        sanitized
    }
}

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("invalid base URL: {0}")]
    InvalidBaseUrl(String),
    #[error("invalid bearer token")]
    InvalidBearer,
    #[error("invalid URL path segment")]
    InvalidPathSegment,
    #[error("invalid idempotency key")]
    InvalidIdempotencyKey,
    #[error("response exceeds the SDK size limit")]
    ResponseTooLarge,
    #[error("HTTP request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("response JSON did not match the protocol: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("service returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_credential_bearing_or_ambiguous_base_urls() {
        assert!(Client::new("ftp://example.com").is_err());
        assert!(Client::new("https://user:pass@example.com").is_err());
        assert!(Client::new("https://example.com?tenant=one").is_err());
        assert!(Client::new("https://example.com/#fragment").is_err());
        assert!(Client::new("https://example.com/api").is_ok());
    }

    #[test]
    fn bearer_and_idempotency_values_are_strictly_bounded() {
        assert!(Client::new("https://example.com")
            .unwrap()
            .with_bearer("secret\nheader")
            .is_err());
        assert!(idempotency_header(" run-1").is_err());
        assert!(idempotency_header("run-1").is_ok());
    }

    #[test]
    fn dynamic_identifiers_are_encoded_as_single_path_segments() {
        let client = Client::new("https://example.com/root").unwrap();
        let url = client
            .endpoint(&["v1", "jobs", "owner/repo", "heartbeat"])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/root/v1/jobs/owner%2Frepo/heartbeat"
        );
    }
}
