use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request as AxumRequest, State};
use axum::http::{Method as HttpMethod, StatusCode as HttpStatusCode};
use axum::response::Response;
use axum::Router;
use futures::stream;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use super::*;

const TEST_SECRET: &str = "unit-test-provider-secret";

#[derive(Clone)]
struct MockState {
    status: HttpStatusCode,
    headers: Arc<Vec<(String, String)>>,
    body: Arc<MockBody>,
    delay: Duration,
    seen: Arc<Mutex<Vec<CapturedRequest>>>,
}

#[derive(Clone)]
enum MockBody {
    Json(Value),
    Text(String),
    Chunks(Vec<Vec<u8>>),
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: HttpMethod,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Value,
}

impl MockState {
    fn json(body: Value) -> Self {
        Self {
            status: HttpStatusCode::OK,
            headers: Arc::new(Vec::new()),
            body: Arc::new(MockBody::Json(body)),
            delay: Duration::ZERO,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_status(mut self, status: HttpStatusCode) -> Self {
        self.status = status;
        self
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        Arc::make_mut(&mut self.headers).push((name.to_string(), value.to_string()));
        self
    }

    fn with_body(mut self, body: MockBody) -> Self {
        self.body = Arc::new(body);
        self
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

async fn mock_provider(State(state): State<MockState>, request: AxumRequest) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    state.seen.lock().unwrap().push(CapturedRequest {
        method: parts.method,
        path: parts.uri.path().to_string(),
        query: parts.uri.query().map(str::to_string),
        headers,
        body,
    });

    if !state.delay.is_zero() {
        tokio::time::sleep(state.delay).await;
    }

    let mut builder = Response::builder().status(state.status);
    for (name, value) in state.headers.iter() {
        builder = builder.header(name.as_str(), value.as_str());
    }
    match state.body.as_ref() {
        MockBody::Json(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
        MockBody::Text(body) => builder.body(Body::from(body.clone())).unwrap(),
        MockBody::Chunks(chunks) => {
            let chunks = chunks
                .clone()
                .into_iter()
                .map(|chunk| Ok::<Bytes, Infallible>(Bytes::from(chunk)));
            builder
                .body(Body::from_stream(stream::iter(chunks)))
                .unwrap()
        }
    }
}

async fn spawn(state: MockState) -> (String, MockState) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(mock_provider)
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/"), state)
}

fn config(
    name: &str,
    protocol: ProviderProtocol,
    base_url: String,
    model: &str,
    max_response_bytes: usize,
    timeout_secs: u64,
) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        protocol,
        base_url,
        model: model.to_string(),
        api_key_env: "UNIT_TEST_PROVIDER_KEY".into(),
        allowed_hosts: vec!["127.0.0.1".into()],
        timeout_secs,
        connect_timeout_secs: 1,
        max_response_bytes,
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        prompt: "Solve the issue".into(),
        max_output_tokens: 512,
        system: Some("Be precise".into()),
    }
}

#[derive(Clone)]
struct ContractCase {
    name: &'static str,
    protocol: ProviderProtocol,
    prefix: &'static str,
    model: &'static str,
    expected_path: &'static str,
    request_id_header: &'static str,
    request_id: &'static str,
    expected_text: &'static str,
    response: Value,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[tokio::test]
async fn executes_all_provider_wire_contracts_and_normalizes_usage() {
    let cases = [
        ContractCase {
            name: "codex",
            protocol: ProviderProtocol::OpenAiResponses,
            prefix: "v1/",
            model: "gpt-5-codex",
            expected_path: "/v1/responses",
            request_id_header: "x-request-id",
            request_id: "req-openai",
            expected_text: "openai result",
            response: json!({
                "output_text":"openai result",
                "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}
            }),
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
        },
        ContractCase {
            name: "claude",
            protocol: ProviderProtocol::AnthropicMessages,
            prefix: "v1/",
            model: "claude-sonnet-test",
            expected_path: "/v1/messages",
            request_id_header: "request-id",
            request_id: "req-anthropic",
            expected_text: "claude result",
            response: json!({
                "content":[{"type":"text","text":"claude result"}],
                "usage":{"input_tokens":11,"output_tokens":6}
            }),
            input_tokens: 11,
            output_tokens: 6,
            total_tokens: 17,
        },
        ContractCase {
            name: "gemini",
            protocol: ProviderProtocol::GeminiGenerateContent,
            prefix: "v1beta/",
            model: "gemini-2.5-pro",
            expected_path: "/v1beta/models/gemini-2.5-pro:generateContent",
            request_id_header: "x-goog-request-id",
            request_id: "req-gemini",
            expected_text: "gemini result",
            response: json!({
                "candidates":[{"content":{"parts":[{"text":"gemini result"}]}}],
                "usageMetadata":{
                    "promptTokenCount":12,
                    "candidatesTokenCount":7,
                    "totalTokenCount":19
                }
            }),
            input_tokens: 12,
            output_tokens: 7,
            total_tokens: 19,
        },
        ContractCase {
            name: "kimi",
            protocol: ProviderProtocol::OpenAiCompatibleChat,
            prefix: "v1/",
            model: "moonshot-v1-test",
            expected_path: "/v1/chat/completions",
            request_id_header: "x-request-id",
            request_id: "req-kimi",
            expected_text: "kimi result",
            response: json!({
                "choices":[{"message":{"content":"kimi result"}}],
                "usage":{"prompt_tokens":13,"completion_tokens":8,"total_tokens":21}
            }),
            input_tokens: 13,
            output_tokens: 8,
            total_tokens: 21,
        },
        ContractCase {
            name: "qwen",
            protocol: ProviderProtocol::OpenAiCompatibleChat,
            prefix: "compatible/v1/",
            model: "qwen-plus-test",
            expected_path: "/compatible/v1/chat/completions",
            request_id_header: "x-request-id",
            request_id: "req-qwen",
            expected_text: "qwen result",
            response: json!({
                "choices":[{"message":{"content":[{"type":"text","text":"qwen result"}]}}],
                "usage":{"input_tokens":14,"output_tokens":9,"total_tokens":23}
            }),
            input_tokens: 14,
            output_tokens: 9,
            total_tokens: 23,
        },
    ];

    for case in cases {
        let state = MockState::json(case.response.clone())
            .with_header(case.request_id_header, case.request_id);
        let (base, state) = spawn(state).await;
        let client = ProviderClient::with_api_key(
            config(
                case.name,
                case.protocol,
                format!("{base}{}", case.prefix),
                case.model,
                1024 * 1024,
                5,
            ),
            TEST_SECRET,
        )
        .unwrap();
        let response = client.execute(&request()).await.unwrap();
        assert_eq!(response.provider, case.name);
        assert_eq!(response.model, case.model);
        assert_eq!(response.text, case.expected_text);
        assert_eq!(response.request_id.as_deref(), Some(case.request_id));
        assert_eq!(response.usage["input_tokens"], case.input_tokens);
        assert_eq!(response.usage["output_tokens"], case.output_tokens);
        assert_eq!(response.usage["total_tokens"], case.total_tokens);
        assert!(!response.usage["raw"].is_null());

        let seen = state.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let captured = &seen[0];
        assert_eq!(captured.method, HttpMethod::POST);
        assert_eq!(captured.path, case.expected_path);
        assert_eq!(captured.query, None);
        assert!(captured
            .headers
            .get("content-type")
            .is_some_and(|value| value.starts_with("application/json")));
        match case.protocol {
            ProviderProtocol::OpenAiResponses => {
                assert_eq!(
                    captured.headers.get("authorization").map(String::as_str),
                    Some("Bearer unit-test-provider-secret")
                );
                assert_eq!(captured.body["input"], "Solve the issue");
                assert_eq!(captured.body["instructions"], "Be precise");
                assert_eq!(captured.body["max_output_tokens"], 512);
                assert_eq!(captured.body["store"], false);
            }
            ProviderProtocol::AnthropicMessages => {
                assert_eq!(
                    captured.headers.get("x-api-key").map(String::as_str),
                    Some(TEST_SECRET)
                );
                assert_eq!(
                    captured
                        .headers
                        .get("anthropic-version")
                        .map(String::as_str),
                    Some("2023-06-01")
                );
                assert_eq!(captured.body["messages"][0]["content"], "Solve the issue");
                assert_eq!(captured.body["system"], "Be precise");
                assert_eq!(captured.body["max_tokens"], 512);
            }
            ProviderProtocol::GeminiGenerateContent => {
                assert_eq!(
                    captured.headers.get("x-goog-api-key").map(String::as_str),
                    Some(TEST_SECRET)
                );
                assert_eq!(
                    captured.body["contents"][0]["parts"][0]["text"],
                    "Solve the issue"
                );
                assert_eq!(
                    captured.body["systemInstruction"]["parts"][0]["text"],
                    "Be precise"
                );
                assert_eq!(captured.body["generationConfig"]["maxOutputTokens"], 512);
            }
            ProviderProtocol::OpenAiCompatibleChat => {
                assert_eq!(
                    captured.headers.get("authorization").map(String::as_str),
                    Some("Bearer unit-test-provider-secret")
                );
                assert_eq!(captured.body["messages"][0]["role"], "system");
                assert_eq!(captured.body["messages"][1]["role"], "user");
                assert_eq!(captured.body["max_tokens"], 512);
            }
        }
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_errors_are_redacted() {
    let state = MockState::json(json!({
        "secret":TEST_SECRET,
        "prompt":"provider-controlled error body"
    }))
    .with_status(HttpStatusCode::TEMPORARY_REDIRECT)
    .with_header("location", "/redirect-target");
    let (base, state) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "codex",
            ProviderProtocol::OpenAiResponses,
            format!("{base}v1/"),
            "gpt-test",
            1024,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    let error = client.execute(&request()).await.unwrap_err();
    assert!(matches!(
        error,
        ProviderError::HttpStatus(StatusCode::TEMPORARY_REDIRECT)
    ));
    let rendered = error.to_string();
    assert!(!rendered.contains(TEST_SECRET));
    assert!(!rendered.contains("provider-controlled"));
    assert_eq!(state.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn non_success_responses_do_not_leak_bodies_or_credentials() {
    let state = MockState::json(json!({
        "error":"billing denied",
        "credential":TEST_SECRET,
        "html":"<script>provider-controlled</script>"
    }))
    .with_status(HttpStatusCode::TOO_MANY_REQUESTS);
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "claude",
            ProviderProtocol::AnthropicMessages,
            format!("{base}v1/"),
            "claude-test",
            1024,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    let error = client.execute(&request()).await.unwrap_err();
    assert!(matches!(
        error,
        ProviderError::HttpStatus(StatusCode::TOO_MANY_REQUESTS)
    ));
    let rendered = error.to_string();
    assert!(!rendered.contains(TEST_SECRET));
    assert!(!rendered.contains("billing denied"));
    assert!(!rendered.contains("script"));
}

#[tokio::test]
async fn response_limit_applies_to_content_length_and_streamed_bodies() {
    let state = MockState::json(json!({"output_text":"x".repeat(256)}));
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "codex",
            ProviderProtocol::OpenAiResponses,
            format!("{base}v1/"),
            "gpt-test",
            64,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    assert!(matches!(
        client.execute(&request()).await,
        Err(ProviderError::ResponseTooLarge)
    ));

    let state = MockState::json(Value::Null)
        .with_body(MockBody::Chunks(vec![vec![b'x'; 40], vec![b'y'; 40]]));
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "codex",
            ProviderProtocol::OpenAiResponses,
            format!("{base}v1/"),
            "gpt-test",
            64,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    assert!(matches!(
        client.execute(&request()).await,
        Err(ProviderError::ResponseTooLarge)
    ));
}

#[tokio::test]
async fn timeout_and_invalid_response_fail_closed() {
    let state =
        MockState::json(json!({"output_text":"too late"})).with_delay(Duration::from_millis(1_200));
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "codex",
            ProviderProtocol::OpenAiResponses,
            format!("{base}v1/"),
            "gpt-test",
            1024,
            1,
        ),
        TEST_SECRET,
    )
    .unwrap();
    assert!(matches!(
        client.execute(&request()).await,
        Err(ProviderError::Transport)
    ));

    let state = MockState::json(Value::Null).with_body(MockBody::Text("not-json".to_string()));
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "gemini",
            ProviderProtocol::GeminiGenerateContent,
            format!("{base}v1beta/"),
            "gemini-test",
            1024,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    assert!(matches!(
        client.execute(&request()).await,
        Err(ProviderError::InvalidResponse)
    ));

    let state = MockState::json(json!({"usage":{"input_tokens":1}}));
    let (base, _) = spawn(state).await;
    let client = ProviderClient::with_api_key(
        config(
            "codex",
            ProviderProtocol::OpenAiResponses,
            format!("{base}v1/"),
            "gpt-test",
            1024,
            5,
        ),
        TEST_SECRET,
    )
    .unwrap();
    assert!(matches!(
        client.execute(&request()).await,
        Err(ProviderError::InvalidResponse)
    ));
}
