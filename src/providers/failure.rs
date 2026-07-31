use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderTransportKind {
    Connect,
    Timeout,
    Request,
    ResponseBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFailureKind {
    RateLimited,
    Overloaded,
    TemporarilyUnavailable,
    ServerError,
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
    Transport { kind: ProviderTransportKind },
    #[error("provider returned HTTP {status}")]
    HttpStatus {
        status: StatusCode,
        retry_after: Option<Duration>,
        failure_kind: Option<ProviderFailureKind>,
    },
    #[error("provider response exceeded the configured byte limit")]
    ResponseTooLarge,
    #[error("provider response did not contain usable text")]
    InvalidResponse,
}

impl ProviderError {
    pub fn transport_kind(&self) -> Option<ProviderTransportKind> {
        match self {
            Self::Transport { kind } => Some(*kind),
            Self::InvalidConfig(_)
            | Self::MissingCredential(_)
            | Self::PromptTooLarge
            | Self::HttpStatus { .. }
            | Self::ResponseTooLarge
            | Self::InvalidResponse => None,
        }
    }

    pub fn http_status(&self) -> Option<StatusCode> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            Self::InvalidConfig(_)
            | Self::MissingCredential(_)
            | Self::PromptTooLarge
            | Self::Transport { .. }
            | Self::ResponseTooLarge
            | Self::InvalidResponse => None,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::HttpStatus { retry_after, .. } => *retry_after,
            Self::InvalidConfig(_)
            | Self::MissingCredential(_)
            | Self::PromptTooLarge
            | Self::Transport { .. }
            | Self::ResponseTooLarge
            | Self::InvalidResponse => None,
        }
    }

    pub fn failure_kind(&self) -> Option<ProviderFailureKind> {
        match self {
            Self::HttpStatus { failure_kind, .. } => *failure_kind,
            Self::InvalidConfig(_)
            | Self::MissingCredential(_)
            | Self::PromptTooLarge
            | Self::Transport { .. }
            | Self::ResponseTooLarge
            | Self::InvalidResponse => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn transport_for_test(kind: ProviderTransportKind) -> Self {
        Self::Transport { kind }
    }

    #[cfg(test)]
    pub(crate) fn http_status_for_test(
        status: StatusCode,
        retry_after: Option<Duration>,
        failure_kind: Option<ProviderFailureKind>,
    ) -> Self {
        Self::HttpStatus {
            status,
            retry_after,
            failure_kind,
        }
    }
}

fn classify_transport(error: &reqwest::Error) -> ProviderTransportKind {
    if error.is_timeout() {
        ProviderTransportKind::Timeout
    } else if error.is_connect() {
        ProviderTransportKind::Connect
    } else {
        ProviderTransportKind::Request
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after_at(headers, SystemTime::now())
}

fn parse_retry_after_at(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    deadline.duration_since(now).ok()
}

fn parse_provider_failure(
    protocol: ProviderProtocol,
    bytes: &[u8],
) -> Option<ProviderFailureKind> {
    let body = serde_json::from_slice::<Value>(bytes).ok()?;
    let code = match protocol {
        ProviderProtocol::OpenAiResponses | ProviderProtocol::OpenAiCompatibleChat => body
            .pointer("/error/code")
            .or_else(|| body.pointer("/error/type")),
        ProviderProtocol::AnthropicMessages => body.pointer("/error/type"),
        ProviderProtocol::GeminiGenerateContent => body
            .pointer("/error/status")
            .or_else(|| body.pointer("/error/code")),
    }
    .and_then(Value::as_str)?
    .trim()
    .to_ascii_lowercase()
    .replace(['-', ' '], "_");

    if code.contains("rate_limit") || code.contains("resource_exhausted") {
        Some(ProviderFailureKind::RateLimited)
    } else if code.contains("overload") {
        Some(ProviderFailureKind::Overloaded)
    } else if code.contains("temporarily_unavailable")
        || code.contains("service_unavailable")
        || code == "unavailable"
    {
        Some(ProviderFailureKind::TemporarilyUnavailable)
    } else if code.contains("server_error") || code.contains("internal") {
        Some(ProviderFailureKind::ServerError)
    } else {
        None
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn retry_after_supports_seconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(
            parse_retry_after_at(&headers, SystemTime::UNIX_EPOCH),
            Some(Duration::from_secs(7))
        );

        let deadline = SystemTime::UNIX_EPOCH + Duration::from_secs(120);
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&httpdate::fmt_http_date(deadline)).unwrap(),
        );
        assert_eq!(
            parse_retry_after_at(
                &headers,
                SystemTime::UNIX_EPOCH + Duration::from_secs(30)
            ),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn provider_failure_parser_returns_only_stable_categories() {
        assert_eq!(
            parse_provider_failure(
                ProviderProtocol::AnthropicMessages,
                br#"{"error":{"type":"overloaded_error","message":"secret body"}}"#,
            ),
            Some(ProviderFailureKind::Overloaded)
        );
        assert_eq!(
            parse_provider_failure(
                ProviderProtocol::GeminiGenerateContent,
                br#"{"error":{"status":"RESOURCE_EXHAUSTED"}}"#,
            ),
            Some(ProviderFailureKind::RateLimited)
        );
        assert_eq!(
            parse_provider_failure(
                ProviderProtocol::OpenAiResponses,
                br#"{"error":{"code":"invalid_api_key"}}"#,
            ),
            None
        );
    }
}
