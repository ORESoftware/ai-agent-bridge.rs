//! Hardened provider protocol adapters used by external orchestrator workers.
//!
//! This module intentionally does not own workflow scheduling or provider secrets.
//! The bridge selects assignments; an adapter resolves the configured environment
//! variable locally and uses these request/response helpers to call one provider.

use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 90;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONFIGURED_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_TOKENS: u32 = 65_536;

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_connect_timeout_secs() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_SECS
}

fn default_max_response_bytes() -> usize {
    DEFAULT_MAX_RESPONSE_BYTES
}

fn default_max_output_tokens() -> u32 {
    4096
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    OpenAiCompatibleChat,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable that contains the provider credential.
    /// The credential itself must never appear in serialized configuration.
    pub api_key_env: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderRequest {
    pub prompt: String,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderResponse {
    pub provider: String,
    pub model: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub usage: Value,
}

#[derive(Clone)]
struct PreparedRequest {
    method: Method,
    url: Url,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Clone)]
pub struct ProviderClient {
    config: ValidatedProviderConfig,
    api_key: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug)]
struct ValidatedProviderConfig {
    raw: ProviderConfig,
    base_url: Url,
    allowed_hosts: HashSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("invalid provider configuration: {0}")]
    InvalidConfig(String),
    #[error("provider credential environment variable '{0}' is missing or empty")]
    MissingCredential(String),
    #[error("provider prompt exceeds the configured byte limit")]
    PromptTooLarge,
    #[error("provider request failed")]
    Transport,
    #[error("provider returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("provider response exceeded the configured byte limit")]
    ResponseTooLarge,
    #[error("provider response did not contain usable text")]
    InvalidResponse,
}

impl ProviderConfig {
    fn validate(mut self) -> Result<ValidatedProviderConfig, ProviderError> {
        self.name = self.name.trim().to_string();
        self.model = self.model.trim().to_string();
        self.api_key_env = self.api_key_env.trim().to_string();
        self.base_url = self.base_url.trim().to_string();
        let name = self.name.as_str();
        if name.is_empty() || name.len() > 120 {
            return Err(ProviderError::InvalidConfig(
                "name must contain 1-120 characters".into(),
            ));
        }
        if self.model.is_empty() || self.model.len() > 256 {
            return Err(ProviderError::InvalidConfig(
                "model must contain 1-256 characters".into(),
            ));
        }
        if !valid_env_name(&self.api_key_env) {
            return Err(ProviderError::InvalidConfig(
                "api_key_env must be an uppercase environment-variable name".into(),
            ));
        }
        if self.timeout_secs == 0 || self.connect_timeout_secs == 0 {
            return Err(ProviderError::InvalidConfig(
                "timeouts must be greater than zero".into(),
            ));
        }
        if self.max_response_bytes == 0 || self.max_response_bytes > MAX_CONFIGURED_RESPONSE_BYTES {
            return Err(ProviderError::InvalidConfig(format!(
                "max_response_bytes must be between 1 and {MAX_CONFIGURED_RESPONSE_BYTES}"
            )));
        }

        let mut base_url = Url::parse(&self.base_url)
            .map_err(|_| ProviderError::InvalidConfig("base_url is not a valid URL".into()))?;
        if base_url.username() != "" || base_url.password().is_some() {
            return Err(ProviderError::InvalidConfig(
                "base_url must not contain user information".into(),
            ));
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(ProviderError::InvalidConfig(
                "base_url must not contain a query or fragment".into(),
            ));
        }
        let host = base_url
            .host_str()
            .ok_or_else(|| ProviderError::InvalidConfig("base_url requires a host".into()))?
            .to_ascii_lowercase();
        if base_url.scheme() != "https" && !is_loopback_host(&host) {
            return Err(ProviderError::InvalidConfig(
                "provider URLs must use HTTPS except for loopback tests".into(),
            ));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }

        let allowed_hosts: HashSet<String> = if self.allowed_hosts.is_empty() {
            HashSet::from([host.clone()])
        } else {
            self.allowed_hosts
                .iter()
                .map(|value| value.trim().trim_end_matches('.').to_ascii_lowercase())
                .collect()
        };
        if allowed_hosts.iter().any(|value| value.is_empty()) || !allowed_hosts.contains(&host) {
            return Err(ProviderError::InvalidConfig(
                "base_url host must appear exactly in allowed_hosts".into(),
            ));
        }

        self.base_url = base_url.to_string();
        Ok(ValidatedProviderConfig {
            raw: self,
            base_url,
            allowed_hosts,
        })
    }
}

impl ProviderClient {
    pub fn from_config(config: ProviderConfig) -> Result<Self, ProviderError> {
        let env_name = config.api_key_env.clone();
        let api_key = std::env::var(&env_name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or(ProviderError::MissingCredential(env_name))?;
        Self::with_api_key(config, api_key)
    }

    fn with_api_key(
        config: ProviderConfig,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let config = config.validate()?;
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ProviderError::MissingCredential(
                config.raw.api_key_env.clone(),
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(config.raw.timeout_secs))
            .connect_timeout(Duration::from_secs(config.raw.connect_timeout_secs))
            .user_agent("fiducia-ai-agent-orchestrator/0.1")
            .build()
            .map_err(|_| ProviderError::InvalidConfig("failed to build HTTP client".into()))?;
        Ok(Self {
            config,
            api_key,
            http,
        })
    }

    pub fn name(&self) -> &str {
        &self.config.raw.name
    }

    pub fn protocol(&self) -> ProviderProtocol {
        self.config.raw.protocol
    }

    fn prepare(&self, request: &ProviderRequest) -> Result<PreparedRequest, ProviderError> {
        validate_request(request)?;
        prepare_request(&self.config, &self.api_key, request)
    }

    pub async fn execute(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let prepared = self.prepare(request)?;
        let mut builder = self
            .http
            .request(prepared.method, prepared.url)
            .header(CONTENT_TYPE, "application/json");
        for (name, value) in prepared.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ProviderError::InvalidConfig("invalid header name".into()))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|_| ProviderError::InvalidConfig("invalid header value".into()))?;
            builder = builder.header(name, value);
        }
        let response = builder
            .json(&prepared.body)
            .send()
            .await
            .map_err(|_| ProviderError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::HttpStatus(status));
        }
        let request_id = request_id(response.headers());
        let bytes = read_bounded(response, self.config.raw.max_response_bytes).await?;
        let body: Value =
            serde_json::from_slice(&bytes).map_err(|_| ProviderError::InvalidResponse)?;
        let text = parse_response_text(self.config.raw.protocol, &body)?;
        Ok(ProviderResponse {
            provider: self.config.raw.name.clone(),
            model: self.config.raw.model.clone(),
            text,
            request_id,
            usage: normalize_usage(self.config.raw.protocol, &body),
        })
    }
}

pub fn parse_provider_configs(input: &str) -> Result<Vec<ProviderConfig>, ProviderError> {
    let configs: Vec<ProviderConfig> = serde_json::from_str(input)
        .map_err(|_| ProviderError::InvalidConfig("provider config JSON is invalid".into()))?;
    if configs.is_empty() {
        return Err(ProviderError::InvalidConfig(
            "at least one provider is required".into(),
        ));
    }
    let mut names = HashSet::new();
    for config in &configs {
        let name = config.name.trim().to_ascii_lowercase();
        if !names.insert(name) {
            return Err(ProviderError::InvalidConfig(
                "provider names must be unique".into(),
            ));
        }
        config.clone().validate()?;
    }
    Ok(configs)
}

fn validate_request(request: &ProviderRequest) -> Result<(), ProviderError> {
    if request.prompt.trim().is_empty() {
        return Err(ProviderError::InvalidConfig("prompt is required".into()));
    }
    let system_bytes = request
        .system
        .as_ref()
        .map(|value| value.len())
        .unwrap_or(0);
    if request.prompt.len().saturating_add(system_bytes) > MAX_PROMPT_BYTES {
        return Err(ProviderError::PromptTooLarge);
    }
    if request.max_output_tokens == 0 || request.max_output_tokens > MAX_OUTPUT_TOKENS {
        return Err(ProviderError::InvalidConfig(format!(
            "max_output_tokens must be between 1 and {MAX_OUTPUT_TOKENS}"
        )));
    }
    Ok(())
}

fn prepare_request(
    config: &ValidatedProviderConfig,
    api_key: &str,
    request: &ProviderRequest,
) -> Result<PreparedRequest, ProviderError> {
    let (relative_path, headers, body) = match config.raw.protocol {
        ProviderProtocol::OpenAiResponses => {
            let mut body = json!({
                "model": config.raw.model,
                "input": request.prompt,
                "max_output_tokens": request.max_output_tokens,
                "store": false,
            });
            if let Some(system) = request.system.as_ref() {
                body["instructions"] = Value::String(system.clone());
            }
            ("responses".to_string(), bearer_headers(api_key), body)
        }
        ProviderProtocol::AnthropicMessages => {
            let mut headers = BTreeMap::new();
            headers.insert("x-api-key".into(), api_key.into());
            headers.insert("anthropic-version".into(), "2023-06-01".into());
            let mut body = json!({
                "model": config.raw.model,
                "max_tokens": request.max_output_tokens,
                "messages": [{"role":"user","content":request.prompt}],
            });
            if let Some(system) = request.system.as_ref() {
                body["system"] = Value::String(system.clone());
            }
            ("messages".to_string(), headers, body)
        }
        ProviderProtocol::GeminiGenerateContent => {
            let mut headers = BTreeMap::new();
            headers.insert("x-goog-api-key".into(), api_key.into());
            let mut body = json!({
                "contents": [{"role":"user","parts":[{"text":request.prompt}]}],
                "generationConfig": {"maxOutputTokens":request.max_output_tokens},
            });
            if let Some(system) = request.system.as_ref() {
                body["systemInstruction"] = json!({"parts":[{"text":system}]});
            }
            if !valid_gemini_model(&config.raw.model) {
                return Err(ProviderError::InvalidConfig(
                    "Gemini model names may contain only letters, digits, dot, dash, and underscore"
                        .into(),
                ));
            }
            (
                format!("models/{}:generateContent", config.raw.model),
                headers,
                body,
            )
        }
        ProviderProtocol::OpenAiCompatibleChat => {
            let mut messages = Vec::new();
            if let Some(system) = request.system.as_ref() {
                messages.push(json!({"role":"system","content":system}));
            }
            messages.push(json!({"role":"user","content":request.prompt}));
            (
                "chat/completions".to_string(),
                bearer_headers(api_key),
                json!({
                    "model": config.raw.model,
                    "messages": messages,
                    "max_tokens": request.max_output_tokens,
                }),
            )
        }
    };
    let url = config
        .base_url
        .join(&relative_path)
        .map_err(|_| ProviderError::InvalidConfig("provider endpoint is invalid".into()))?;
    enforce_destination(config, &url)?;
    Ok(PreparedRequest {
        method: Method::POST,
        url,
        headers,
        body,
    })
}

fn bearer_headers(api_key: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(
        AUTHORIZATION.as_str().to_string(),
        format!("Bearer {api_key}"),
    )])
}

fn enforce_destination(config: &ValidatedProviderConfig, url: &Url) -> Result<(), ProviderError> {
    let host = url
        .host_str()
        .ok_or_else(|| ProviderError::InvalidConfig("provider endpoint requires a host".into()))?
        .to_ascii_lowercase();
    if !config.allowed_hosts.contains(&host) {
        return Err(ProviderError::InvalidConfig(
            "provider endpoint host is not allowlisted".into(),
        ));
    }
    if url.scheme() != "https" && !is_loopback_host(&host) {
        return Err(ProviderError::InvalidConfig(
            "provider endpoint must use HTTPS".into(),
        ));
    }
    Ok(())
}

async fn read_bounded(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ProviderError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ProviderError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn parse_response_text(
    protocol: ProviderProtocol,
    body: &Value,
) -> Result<String, ProviderError> {
    let parts: Vec<&str> = match protocol {
        ProviderProtocol::OpenAiResponses => {
            if let Some(text) = body.get("output_text").and_then(Value::as_str) {
                vec![text]
            } else {
                body.get("output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|item| {
                        item.get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            }
        }
        ProviderProtocol::AnthropicMessages => body
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        ProviderProtocol::GeminiGenerateContent => body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        ProviderProtocol::OpenAiCompatibleChat => {
            let content = body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"));
            match content {
                Some(Value::String(text)) => vec![text],
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect(),
                _ => Vec::new(),
            }
        }
    };
    let text = parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        Err(ProviderError::InvalidResponse)
    } else {
        Ok(text)
    }
}

fn normalize_usage(protocol: ProviderProtocol, body: &Value) -> Value {
    let raw = match protocol {
        ProviderProtocol::GeminiGenerateContent => body.get("usageMetadata"),
        _ => body.get("usage"),
    };
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    let (input_keys, output_keys, total_keys): (&[&str], &[&str], &[&str]) = match protocol {
        ProviderProtocol::OpenAiResponses => (
            &["input_tokens", "prompt_tokens"],
            &["output_tokens", "completion_tokens"],
            &["total_tokens"],
        ),
        ProviderProtocol::AnthropicMessages => (
            &["input_tokens"],
            &["output_tokens"],
            &["total_tokens"],
        ),
        ProviderProtocol::GeminiGenerateContent => (
            &["promptTokenCount", "prompt_token_count"],
            &["candidatesTokenCount", "candidates_token_count"],
            &["totalTokenCount", "total_token_count"],
        ),
        ProviderProtocol::OpenAiCompatibleChat => (
            &["prompt_tokens", "input_tokens"],
            &["completion_tokens", "output_tokens"],
            &["total_tokens"],
        ),
    };
    let input_tokens = usage_u64(raw, input_keys).unwrap_or(0);
    let output_tokens = usage_u64(raw, output_keys).unwrap_or(0);
    let total_tokens = usage_u64(raw, total_keys)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "raw": raw,
    })
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| find_usage_u64(value, key))
}

fn find_usage_u64(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_u64)
            .or_else(|| map.values().find_map(|value| find_usage_u64(value, key))),
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_usage_u64(value, key)),
        _ => None,
    }
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "x-goog-request-id"]
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_uppercase())
        && chars.all(|ch| ch == '_' || ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn valid_gemini_model(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|ch| ch == '[' || ch == ']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
#[path = "providers/http_tests.rs"]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn config(protocol: ProviderProtocol) -> ProviderConfig {
        ProviderConfig {
            name: "provider".into(),
            protocol,
            base_url: match protocol {
                ProviderProtocol::OpenAiResponses | ProviderProtocol::AnthropicMessages => {
                    "https://api.example.com/v1/"
                }
                ProviderProtocol::GeminiGenerateContent => "https://api.example.com/v1beta/",
                ProviderProtocol::OpenAiCompatibleChat => {
                    "https://api.example.com/compatible-mode/v1/"
                }
            }
            .into(),
            model: "model-1".into(),
            api_key_env: "PROVIDER_API_KEY".into(),
            allowed_hosts: vec!["api.example.com".into()],
            timeout_secs: 30,
            connect_timeout_secs: 5,
            max_response_bytes: 1024,
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            prompt: "Solve the issue".into(),
            max_output_tokens: 512,
            system: Some("Be precise".into()),
        }
    }

    #[test]
    fn rejects_non_https_remote_hosts() {
        let mut value = config(ProviderProtocol::OpenAiResponses);
        value.base_url = "http://api.example.com".into();
        assert!(matches!(
            value.validate(),
            Err(ProviderError::InvalidConfig(_))
        ));
    }

    #[test]
    fn permits_loopback_http_for_mock_servers() {
        let mut value = config(ProviderProtocol::OpenAiResponses);
        value.base_url = "http://127.0.0.1:9999".into();
        value.allowed_hosts = vec!["127.0.0.1".into()];
        assert!(value.validate().is_ok());
    }

    #[test]
    fn requires_exact_host_allowlist_match() {
        let mut value = config(ProviderProtocol::OpenAiResponses);
        value.allowed_hosts = vec!["example.com".into()];
        assert!(value.validate().is_err());
    }

    #[test]
    fn openai_responses_request_uses_responses_endpoint() {
        let client =
            ProviderClient::with_api_key(config(ProviderProtocol::OpenAiResponses), "secret")
                .unwrap();
        let prepared = client.prepare(&request()).unwrap();
        assert_eq!(
            prepared.url.as_str(),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(prepared.body["model"], "model-1");
        assert_eq!(prepared.body["instructions"], "Be precise");
        assert_eq!(prepared.body["input"], "Solve the issue");
        assert_eq!(prepared.body["store"], false);
        assert_eq!(prepared.headers["authorization"], "Bearer secret");
    }

    #[test]
    fn anthropic_request_uses_messages_contract() {
        let client =
            ProviderClient::with_api_key(config(ProviderProtocol::AnthropicMessages), "secret")
                .unwrap();
        let prepared = client.prepare(&request()).unwrap();
        assert_eq!(prepared.url.as_str(), "https://api.example.com/v1/messages");
        assert_eq!(prepared.headers["x-api-key"], "secret");
        assert_eq!(prepared.body["system"], "Be precise");
    }

    #[test]
    fn gemini_request_keeps_secret_out_of_url() {
        let client =
            ProviderClient::with_api_key(config(ProviderProtocol::GeminiGenerateContent), "secret")
                .unwrap();
        let prepared = client.prepare(&request()).unwrap();
        assert_eq!(
            prepared.url.as_str(),
            "https://api.example.com/v1beta/models/model-1:generateContent"
        );
        assert!(prepared.url.query().is_none());
        assert_eq!(prepared.headers["x-goog-api-key"], "secret");
    }

    #[test]
    fn openai_compatible_request_supports_kimi_and_qwen_shape() {
        let client =
            ProviderClient::with_api_key(config(ProviderProtocol::OpenAiCompatibleChat), "secret")
                .unwrap();
        let prepared = client.prepare(&request()).unwrap();
        assert_eq!(
            prepared.url.as_str(),
            "https://api.example.com/compatible-mode/v1/chat/completions"
        );
        assert_eq!(prepared.body["messages"][0]["role"], "system");
        assert_eq!(prepared.body["messages"][1]["role"], "user");
    }

    #[test]
    fn parses_provider_response_shapes() {
        assert_eq!(
            parse_response_text(
                ProviderProtocol::OpenAiResponses,
                &json!({"output":[{"content":[{"type":"output_text","text":"openai"}]}]})
            )
            .unwrap(),
            "openai"
        );
        assert_eq!(
            parse_response_text(
                ProviderProtocol::AnthropicMessages,
                &json!({"content":[{"type":"text","text":"claude"}]})
            )
            .unwrap(),
            "claude"
        );
        assert_eq!(
            parse_response_text(
                ProviderProtocol::GeminiGenerateContent,
                &json!({"candidates":[{"content":{"parts":[{"text":"gemini"}]}}]})
            )
            .unwrap(),
            "gemini"
        );
        assert_eq!(
            parse_response_text(
                ProviderProtocol::OpenAiCompatibleChat,
                &json!({"choices":[{"message":{"content":"compatible"}}]})
            )
            .unwrap(),
            "compatible"
        );
    }

    #[test]
    fn normalizes_all_provider_usage_shapes() {
        assert_eq!(
            normalize_usage(
                ProviderProtocol::OpenAiResponses,
                &json!({"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}})
            )["total_tokens"],
            15
        );
        assert_eq!(
            normalize_usage(
                ProviderProtocol::AnthropicMessages,
                &json!({"usage":{"input_tokens":11,"output_tokens":6}})
            )["total_tokens"],
            17
        );
        let gemini = normalize_usage(
            ProviderProtocol::GeminiGenerateContent,
            &json!({"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":7,"totalTokenCount":19}}),
        );
        assert_eq!(gemini["input_tokens"], 12);
        assert_eq!(gemini["output_tokens"], 7);
        assert_eq!(gemini["total_tokens"], 19);
        assert_eq!(
            normalize_usage(
                ProviderProtocol::OpenAiCompatibleChat,
                &json!({"usage":{"prompt_tokens":13,"completion_tokens":8,"total_tokens":21}})
            )["total_tokens"],
            21
        );
    }

    #[test]
    fn config_json_rejects_duplicate_names() {
        let input = r#"[
          {"name":"kimi","protocol":"open_ai_compatible_chat","base_url":"https://api.example.com","model":"m1","api_key_env":"KIMI_KEY"},
          {"name":"KIMI","protocol":"open_ai_compatible_chat","base_url":"https://api.example.com","model":"m2","api_key_env":"KIMI_KEY_2"}
        ]"#;
        assert!(parse_provider_configs(input).is_err());
    }
}
