use std::{
    collections::BTreeSet,
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use ai_agent_bridge::slack_project_bindings::{
    AgentMode, RegistryError, RequestedCapability, ResolveRequest, SlackProjectRegistry,
    WritePolicy,
};
use anyhow::{ensure, Context};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

const DEFAULT_ADDRESS: &str = "127.0.0.1:8160";
const DEFAULT_REGISTRY_PATH: &str = "config/alex-main-agent.channels.json";
const MAX_REGISTRY_BYTES: u64 = 1_048_576;
const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct ProbeState {
    registry: Arc<SlackProjectRegistry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveInput {
    workspace_id: String,
    channel_id: String,
    user_id: String,
    #[serde(default)]
    user_group_ids: Vec<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    linear_issue_identifier: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResolveSuccess {
    status: &'static str,
    workspace_id: String,
    channel_id: String,
    linear_team_key: String,
    linear_project_id: String,
    repository: String,
    agent_mode: &'static str,
    write_policy: &'static str,
    linear_issue_identifier: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResolveFailure {
    status: &'static str,
    code: &'static str,
    message: &'static str,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    bindings: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry_path = env::var_os("ALEX_MAIN_AGENT_REGISTRY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REGISTRY_PATH));
    let registry_bytes = read_registry(&registry_path)?;
    let registry = SlackProjectRegistry::from_json(&registry_bytes)
        .context("alex-main-agent registry failed validation")?;

    let address = env::var("ALEX_MAIN_AGENT_PROBE_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDRESS.to_string())
        .parse::<SocketAddr>()
        .context("ALEX_MAIN_AGENT_PROBE_ADDR must be a socket address")?;
    ensure!(
        address.ip().is_loopback(),
        "the registry browser probe may bind only to a loopback address"
    );

    let state = ProbeState {
        registry: Arc::new(registry),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/healthz", get(healthz))
        .route("/api/resolve", post(resolve))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);

    let listener = TcpListener::bind(address)
        .await
        .context("failed to bind registry browser probe")?;
    println!("alex-main-agent registry probe listening on http://{address}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("registry browser probe failed")?;
    Ok(())
}

fn read_registry(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect registry at {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "registry path must not be a symbolic link"
    );
    ensure!(metadata.is_file(), "registry path must be a file");

    let file = fs::File::open(path)
        .with_context(|| format!("failed to open registry at {}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_REGISTRY_BYTES) as usize);
    file.take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read registry from {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_REGISTRY_BYTES,
        "registry file exceeds the maximum size"
    );
    Ok(bytes)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|value| loopback_host(value))
    else {
        return hardened_response(
            (
                StatusCode::MISDIRECTED_REQUEST,
                "the registry probe accepts only strict loopback Host authorities",
            )
                .into_response(),
        );
    };

    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin_is_allowed = origin
            .to_str()
            .ok()
            .is_some_and(|value| origin_matches_host(value, host));
        if !origin_is_allowed {
            return hardened_response(
                (
                    StatusCode::FORBIDDEN,
                    "the registry probe accepts only same-origin browser requests",
                )
                    .into_response(),
            );
        }
    }

    if let Some(fetch_site) = request.headers().get("sec-fetch-site") {
        let fetch_site_is_allowed = fetch_site.to_str().ok().is_some_and(fetch_site_is_allowed);
        if !fetch_site_is_allowed {
            return hardened_response(
                (
                    StatusCode::FORBIDDEN,
                    "the registry probe rejects cross-site browser requests",
                )
                    .into_response(),
            );
        }
    }

    hardened_response(next.run(request).await)
}

fn hardened_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; img-src 'none'; object-src 'none'; script-src 'self'; style-src 'self'",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-embedder-policy"),
        HeaderValue::from_static("require-corp"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "accelerometer=(), camera=(), geolocation=(), gyroscope=(), microphone=(), payment=(), usb=()",
        ),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn valid_port(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u16>().is_ok()
}

fn loopback_host(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return false;
    }

    if let Some(rest) = value.strip_prefix('[') {
        let Some((address, suffix)) = rest.split_once(']') else {
            return false;
        };
        if address != "::1" {
            return false;
        }
        return if suffix.is_empty() {
            true
        } else if let Some(port) = suffix.strip_prefix(':') {
            valid_port(port)
        } else {
            false
        };
    }

    if value.contains('[') || value.contains(']') || value.matches(':').count() > 1 {
        return false;
    }

    let (hostname, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(hostname, port)| (hostname, Some(port)));
    if hostname != "127.0.0.1" && !hostname.eq_ignore_ascii_case("localhost") {
        return false;
    }

    port.is_none_or(valid_port)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Ok(uri) = origin.trim().parse::<Uri>() else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.query().is_some()
        || (!uri.path().is_empty() && uri.path() != "/")
    {
        return false;
    }

    let Some(authority) = uri.authority() else {
        return false;
    };
    loopback_host(authority.as_str()) && authority.as_str().eq_ignore_ascii_case(host.trim())
}

fn fetch_site_is_allowed(value: &str) -> bool {
    matches!(value.trim(), "same-origin" | "same-site" | "none")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

async fn healthz(State(state): State<ProbeState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        bindings: state.registry.binding_count(),
    })
}

async fn resolve(State(state): State<ProbeState>, Json(input): Json<ResolveInput>) -> Response {
    let request = ResolveRequest {
        workspace_id: input.workspace_id,
        channel_id: input.channel_id,
        user_id: input.user_id,
        user_group_ids: input.user_group_ids.into_iter().collect::<BTreeSet<_>>(),
        requested_repository: input.repository.filter(|value| !value.trim().is_empty()),
        requested_agent_mode: None,
        requested_capability: RequestedCapability::RepositoryWrite,
        linear_issue_identifier: input
            .linear_issue_identifier
            .filter(|value| !value.trim().is_empty()),
    };

    match state.registry.resolve(&request) {
        Ok(context) => Json(ResolveSuccess {
            status: "resolved",
            workspace_id: context.workspace_id,
            channel_id: context.channel_id,
            linear_team_key: context.linear_team_key,
            linear_project_id: context.linear_project_id,
            repository: context.repository,
            agent_mode: agent_mode_name(context.agent_mode),
            write_policy: write_policy_name(context.write_policy),
            linear_issue_identifier: context.issue.map(|issue| issue.identifier),
        })
        .into_response(),
        Err(error) => {
            let (status, code, message) = registry_error_response(&error);
            (
                status,
                Json(ResolveFailure {
                    status: "rejected",
                    code,
                    message,
                }),
            )
                .into_response()
        }
    }
}

fn registry_error_response(error: &RegistryError) -> (StatusCode, &'static str, &'static str) {
    match error {
        RegistryError::UnmappedChannel => (
            StatusCode::NOT_FOUND,
            "unmapped_channel",
            "The Slack channel is not mapped to a project.",
        ),
        RegistryError::UnauthorizedPrincipal => (
            StatusCode::FORBIDDEN,
            "unauthorized_principal",
            "The Slack principal is not authorized for this project.",
        ),
        RegistryError::RepositoryNotAllowed => (
            StatusCode::FORBIDDEN,
            "repository_not_allowed",
            "The requested repository is outside the project allowlist.",
        ),
        RegistryError::AgentModeNotAllowed => (
            StatusCode::FORBIDDEN,
            "agent_mode_not_allowed",
            "The requested agent mode is not allowed for this project.",
        ),
        RegistryError::WriteNotAllowed => (
            StatusCode::FORBIDDEN,
            "write_not_allowed",
            "The requested write capability is not allowed for this project.",
        ),
        RegistryError::IssueTeamMismatch => (
            StatusCode::BAD_REQUEST,
            "issue_team_mismatch",
            "The Linear issue belongs to a different team.",
        ),
        RegistryError::InvalidIssueIdentifier => (
            StatusCode::BAD_REQUEST,
            "invalid_issue_identifier",
            "The Linear issue identifier is invalid.",
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "The request did not satisfy the registry contract.",
        ),
    }
}

fn agent_mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Claude => "claude",
        AgentMode::Chatgpt => "chatgpt",
        AgentMode::BothParallel => "both_parallel",
        AgentMode::BothSequential => "both_sequential",
        AgentMode::Review => "review",
    }
}

fn write_policy_name(policy: WritePolicy) -> &'static str {
    match policy {
        WritePolicy::ReadOnly => "read_only",
        WritePolicy::LinearOnly => "linear_only",
        WritePolicy::DraftPullRequest => "draft_pull_request",
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>alex-main-agent registry probe</title>
  <link rel="stylesheet" href="/app.css">
  <script src="/app.js" defer></script>
</head>
<body>
  <main>
    <h1>alex-main-agent registry probe</h1>
    <p>This loopback-only diagnostic resolves Slack project routes through the production Rust registry policy.</p>
    <form id="resolve-form">
      <label>Workspace ID <input id="workspace-id" name="workspace_id" value="T01B3C83PMK" required></label>
      <label>Channel ID <input id="channel-id" name="channel_id" value="C0BMF6JDSHX" required></label>
      <label>User ID <input id="user-id" name="user_id" value="U01AZNU2LJ2" required></label>
      <label>Repository override <input id="repository" name="repository" value=""></label>
      <label>Linear issue <input id="linear-issue" name="linear_issue_identifier" value="DEN-1280"></label>
      <button id="resolve-button" type="submit">Resolve route</button>
    </form>
    <pre id="result" data-status="idle" aria-live="polite">Submit a route to inspect the policy decision.</pre>
  </main>
</body>
</html>
"#;

const APP_JS: &str = r#"const form = document.querySelector('#resolve-form');
const result = document.querySelector('#result');

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  result.dataset.status = 'loading';
  result.textContent = 'Resolving…';

  const values = Object.fromEntries(new FormData(form).entries());
  for (const key of ['repository', 'linear_issue_identifier']) {
    if (!values[key] || !values[key].trim()) delete values[key];
  }

  try {
    const response = await fetch('/api/resolve', {
      method: 'POST',
      headers: {'content-type': 'application/json'},
      body: JSON.stringify(values),
    });
    const payload = await response.json();
    result.dataset.status = response.ok ? 'success' : 'error';
    result.textContent = JSON.stringify(payload, null, 2);
  } catch (_error) {
    result.dataset.status = 'error';
    result.textContent = JSON.stringify({
      status: 'rejected',
      code: 'network_error',
      message: 'The registry probe could not be reached.',
    }, null, 2);
  }
});
"#;

const APP_CSS: &str = r#"html { color-scheme: light dark; font-family: system-ui, sans-serif; }
body { margin: 0; }
main { margin: 0 auto; max-width: 48rem; padding: 2rem; }
form { display: grid; gap: 1rem; }
label { display: grid; gap: 0.25rem; font-weight: 600; }
input, button { font: inherit; padding: 0.65rem; }
button { cursor: pointer; }
pre { border: 1px solid currentColor; min-height: 8rem; overflow: auto; padding: 1rem; }
pre[data-status="success"] { outline: 0.2rem solid CanvasText; }
pre[data-status="error"] { outline: 0.2rem dashed CanvasText; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_require_strict_authorities_and_numeric_ports() {
        for host in [
            "localhost",
            "LOCALHOST:8160",
            "127.0.0.1",
            "127.0.0.1:8160",
            "[::1]",
            "[::1]:8160",
        ] {
            assert!(loopback_host(host), "expected {host:?} to be accepted");
        }

        for host in [
            "",
            "localhost:",
            "localhost:not-a-port",
            "localhost:65536",
            "127.0.0.1:70000",
            "[::1]:bogus",
            "[::1]:65536",
            "[::1]extra",
            "::1",
            "localhost.attacker.example",
            "attacker@localhost",
            "localhost/path",
            "local host",
            "localhost:\n8160",
        ] {
            assert!(!loopback_host(host), "expected {host:?} to be rejected");
        }
    }

    #[test]
    fn browser_origin_must_match_the_exact_loopback_authority() {
        assert!(origin_matches_host(
            "http://127.0.0.1:8160",
            "127.0.0.1:8160"
        ));
        assert!(origin_matches_host(
            "https://LOCALHOST:8160/",
            "localhost:8160"
        ));

        for origin in [
            "null",
            "https://attacker.example",
            "http://127.0.0.1:9999",
            "http://attacker@localhost:8160",
            "http://localhost:8160/path",
            "http://localhost:8160/?query=1",
        ] {
            assert!(
                !origin_matches_host(origin, "localhost:8160"),
                "expected {origin:?} to be rejected"
            );
        }
    }

    #[test]
    fn fetch_metadata_rejects_cross_site_contexts() {
        for value in ["same-origin", "same-site", "none"] {
            assert!(fetch_site_is_allowed(value));
        }
        for value in ["cross-site", "", "unknown"] {
            assert!(!fetch_site_is_allowed(value));
        }
    }

    #[cfg(unix)]
    #[test]
    fn registry_reader_rejects_symbolic_links() {
        use std::{
            os::unix::fs::symlink,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be after the Unix epoch")
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "alex-main-agent-registry-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temporary directory should be created");

        let target = directory.join("registry.json");
        let link = directory.join("registry-link.json");
        fs::write(&target, b"{}").expect("temporary registry should be written");
        symlink(&target, &link).expect("temporary symlink should be created");

        let error = read_registry(&link).expect_err("symbolic-link registry must fail closed");
        assert!(format!("{error:#}").contains("symbolic link"));

        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}
