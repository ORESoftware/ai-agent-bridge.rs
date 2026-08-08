use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use futures::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde_json::{json, Value};

use ai_agent_bridge::service_identity::{
    AGENT_KEY_ENV, BASE_URL_ENV, BEARER_ENV, DEFAULT_LOCAL_HTTP_BASE_URL, DEFAULT_TCP_PORT,
    HTTP_TRANSPORT, LOGICAL_SERVICE_ID, TCP_PORT_ENV, TCP_TRANSPORT, TOPIC_ENV, WIRE_SERVICE_NAME,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_AGENT_KEY: &str = "chatgpt-bridge-smoke";
const DEFAULT_TOPIC: &str = "com.ores.ai-agent-bridge connectivity smoke";
const HELP: &str = r#"ORES AI-agent bridge client for com.ores.ai-agent-bridge

Usage:
  ores-ai-agent-bridge [probe|smoke] [--base-url URL] [--tcp-port PORT]
                       [--timeout-seconds SECONDS]

`probe` verifies route, TCP, health, readiness, bearer auth, and wire identity.
`smoke` additionally registers a stable agent, resolves and joins the stable
connectivity topic, posts a unique marker, and reads it back by sequence.

Bearer credentials are read only from ORES_AI_AGENT_BRIDGE_BEARER, with
FIDUCIA_BRIDGE_PREFLIGHT_BEARER and API_AUTH_BEARER as compatibility fallbacks.
The base URL and TCP port use ORES_AI_AGENT_BRIDGE_BASE_URL and
ORES_AI_AGENT_BRIDGE_TCP_PORT. Smoke identity/topic overrides use
ORES_AI_AGENT_BRIDGE_AGENT_KEY and ORES_AI_AGENT_BRIDGE_TOPIC.
"#;

#[derive(Clone, Copy)]
enum Mode {
    Probe,
    Smoke,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Smoke => "smoke",
        }
    }
}

struct Args {
    mode: Mode,
    base_url: String,
    tcp_port: u16,
    timeout: Duration,
}

fn env_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn parse_port(value: &str, name: &str) -> Result<u16> {
    value.parse().map_err(|_| anyhow!("{name} is invalid"))
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn parse_args() -> Result<Option<Args>> {
    let mut mode = Mode::Probe;
    let mut mode_seen = false;
    let mut base_url = env_value(&[BASE_URL_ENV, "FIDUCIA_BRIDGE_BASE_URL"])
        .unwrap_or_else(|| DEFAUL_LOCAL_HTTP_BASE_URL.to_string());
    let mut tcp_port = env_value(&[TCP_PORT_ENV, "FIDUCIA_BRIDGE_TCP_PORT"])
        .map(|value| parse_port(&value, TCP_PORT_ENV))
        .transpose()?
        .unwrap_or(DEFAULT_TCP_PORT);
    let mut timeout_seconds = 5_u64;
    let mut arguments = env::args().skip(1);

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "probe" | "smoke" => {
                ensure!(!mode_seen, "only one mode may be selected");
                mode = if argument == "probe" {
                    Mode::Probe
                } else {
                    Mode::Smoke
                };
                mode_seen = true;
            }
            "--base-url" => base_url = next_value(&mut arguments, "--base-url")?,
            "--tcp-port" => {
                let value = next_value(&mut arguments, "--tcp-port")?;
                tcp_port = parse_port(&value, "--tcp-port")?;
            }
            "--timeout-seconds" => {
                timeout_seconds = next_value(&mut arguments, "--timeout-seconds")?
                    .parse()
                    .map_err(|_| anyhow!("--timeout-seconds is invalid"))?;
                ensure!(
                    (1..=60).contains(&timeout_seconds),
                    "--timeout-seconds must be between 1 and 60"
                );
            }
            value if value.starts_with('-') => bail!("unsupported argument; run with --help"),
            _ => bail!("unexpected positional argument; run with --help"),
        }
    }

    Ok(Some(Args {
        mode,
        base_url,
        tcp_port,
        timeout: Duration::from_secs(timeout_seconds),
    }))
}

fn base_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| anyhow!("bridge base URL is invalid"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "bridge base URL must use http or https"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "bridge credentials must come from the bearer environment variable"
    );
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "bridge base URL must be an origin without path, query, or fragment"
    );
    Ok(url)
}

fn endpoint(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn channel_endpoint(base: &Url, slug: &str, action: &str) -> Result<Url> {
    let mut url = base.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow!("bridge base URL cannot contain path segments"))?;
    segments.clear().push("channels").push(slug).push(action);
    drop(segments);
    Ok(url)
}

fn authorized(request: RequestBuilder, bearer: &HeaderValue) -> RequestBuilder {
    request.header(AUTHORIZATION, bearer.clone())
}

async fn bounded_body(response: Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("bridge response exceeded the client limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow!("bridge response transport failed"))?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "bridge response exceeded the client limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn json_request(request: RequestBuilder) -> Result<Value> {
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("bridge request transport failed"))?;
    ensure!(
        response.status().is_success(),
        "bridge endpoint returned HTTP {}",
        response.status().as_u16()
    );
    serde_json::from_slice(&bounded_body(response).await?)
        .map_err(|_| anyhow!("bridge endpoint returned invalid JSON"))
}

fn require_identity(value: &Value) -> Result<()> {
    ensure!(
        value.get("service").and_then(Value::as_str) == Some(WIRE_SERVICE_NAME),
        "bridge wire service identity did not match"
    );
    ensure!(
        value.pointer("/transports/http").and_then(Value::as_str) == Some(HTTP_TRANSPORT),
        "bridge HTTQ transport identity did not match"
    );
    ensure!(
        value.pointer("/transports/tcp").and_then(Value::as_str) == Some(TCP_TRANSPORT),
        "bridge TCP transport identity did not match"
    );
    Ok(())
}

async fn smoke(client: &Client, base: &Url, bearer: &HeaderValue) -> Result<Value> {
    let agent_key = env_value(&[AGENT_KEY_ENV]).unwrap_or_else(|| DEFAULT_AGENT_KEY.to_string());
    let topic = env_value(&[TOPIC_ENV]).unwrap_or_else(|| DEFAULT_TOPIC.to_string());
    ensure!(
        agent_key.len() <= 128 && !agent_key.chars().any(char::is_control),
        "smoke agent key is invalid"
    );
    ensure!(
        topic.len() <= 1024 && !topic.chars().any(char::is_control),
        "smoke topic is invalid"
    );

    let registration = json_request(authorized(
        client
            .post(endpoint(base, "/agents/register"))
            .json(&json!({
                "agent_key": agent_key,
                "display_name": "ORES bridge smoke client",
                "kind": "other",
                "meta": {"service_id": LOGICAL_SERVICE_ID, "purpose": "connectivity-smoke"}
            })),
        bearer,
    ))
    .await
    .context("bridge smoke registration failed")?;
    ensure!(
        registration
            .pointer("/agent/agent_key")
            .and_then(Value::as_str)
            == Some(agent_key.as_str()),
        "bridge smoke registration contract failed"
    );

    let resolved = json_request(authorized(
        client
            .post(endpoint(base, "/channels/resolve"))
            .json(&json!({
                "query": topic,
                "created_by": agent_key,
                "threshold": 0.999
            })),
        bearer,
    ))
    .await
    .context("bridge smoke topic resolution failed")?;
    let slug = resolved
        .pointer("/channel/slug")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("bridge smoke resolution omitted the channel slug"))?;
    ensure!(
        resolved.pointer("/channel/topic").and_then(Value::as_str) == Some(topic.as_str()),
        "bridge smoke topic resolved to a different channel"
    );

    let joined = json_request(authorized(
        client
            .post(channel_endpoint(base, slug, "join")?)
            .json(&json!({"agent_key": agent_key, "role": "member"})),
        bearer,
    ))
    .await
    .context("bridge smoke channel join failed")?;
    ensure!(
        joined.pointer("/member/agent_key").and_then(Value::as_str) == Some(agent_key.as_string()),
        "bridge smoke channel join contract failed"
    );

    let marker = format!(
        "{LOGICAL_SERVICE_ID} smoke {}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let messages = channel_endpoint(base, slug, "messages")?;
    let posted = json_request(authorized(
        client.post(messages.clone()).json(&json!({
            "from": agent_key,
            "content": marker,
            "meta": {"service_id": LOGICAL_SERVICE_ID, "smoke": true}
        })),
        bearer,
    ))
    .await
    .context("bridge smoke post failed")?;
    let sequence = posted
        .pointer("/message/seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("bridge smoke post omitted the message sequence"))?;
    ensure!(
        posted.pointer("/message/content").and_then(Value::as_str) == Some(marker.as_str()),
        "bridge smoke post contract failed"
    );

    let mut history_url = messages;
    history_url
        .query_pairs_mut()
        .append_pair("since", &sequence.saturating_sub(1).to_string());
    let history = json_request(authorized(client.get(history_url), bearer))
        .await
        .context("bridge smoke read-back failed")?;
    let read_back = history
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("seq").and_then(Value::as_u64) == Some(sequence)
                    && item.get("from").and_then(Value::as_str) == Some(agent_key.as_str())
                    && item.get("content").and_then(Value::as_str) == Some(marker.as_str())
            })
        });
    ensure!(
        read_back,
        "bridge smoke message was not readable by sequence"
    );

    Ok(json!({
        "registered": true,
        "resolved": true,
        "joined": true,
        "posted": true,
        "read_back": true,
        "sequence": sequence
    }))
}

async fn run(args: &Args) -> Result<Value> {
    let base = base_url(&args.base_url)?;
    let bearer = env_value(&[
        BEARER_ENV,
        "FIDUCIA_BRIDGE_PREFLIGHT_BEARER",
        "API_AUTH_BEARER",
    ]);
    let preflight = ai_agent_bridge::preflight::run(
        base.as_str(),
        args.tcp_port,
        bearer.as_deref(),
        args.timeout,
    )
    .await?;
    if !preflight.ok {
        return Ok(json!({
            "ok": false,
            "service_id": LOGICAL_SERVICE_ID,
            "mode": args.mode.name(),
            "diagnosis": preflight.diagnosis,
            "preflight": preflight
        }));
    }

    let client = Client::builder()
        .timeout(args.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let identity = json_request(client.get(endpoint(&base, "/"))).await?;
    require_identity(&identity)?;
    let smoke = match args.mode {
        Mode::Probe => None,
        Mode::Smoke => {
            let bearer = bearer.ok_or_else(|| anyhow!("bridge bearer is required"))?;
            let bearer = HeaderValue::from_str(&format!("Bearer {bearer}"))
                .map_err(|_| anyhow!("bridge bearer is invalid"))?;
            Some(smoke(&client, &base, &bearer).await?)
        }
    };

    Ok(json!({
        "ok": true,
        "service_id": LOGICAL_SERVICE_ID,
        "endpoint": base.origin().ascii_serialization(),
        "tcp_port": args.tcp_port,
        "mode": args.mode.name(),
        "diagnosis": "ready",
        "identity": {
            "wire_service": WIRE_SERVICE_NAME,
            "http_transport": HTTP_TRANSPORT,
            "tcp_transport": TCP_TRANSPORT
        },
        "preflight": preflight,
        "smoke": smoke
    }))
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{HELP}");
            return;
        }
        Err(error) => {
            eprintln!("ORES bridge client configuration error: {error}");
            std::process::exit(2);
        }
    };

    let report = match run(&args).await {
        Ok(report) => report,
        Err(error) => json!({
            "ok": false,
            "service_id": LOGICAL_SERVICE_ID,
            "mode": args.mode.name(),
            "diagnosis": "connection_or_contract_failure",
            "message": format!("{error:#}")
        }),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("bridge client report serializes")
    );
    if report.get("ok").and_then(Value::as_bool) != Some(true) {
        std::process::exit(1);
    }
}
