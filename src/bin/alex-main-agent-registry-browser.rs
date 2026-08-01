use std::{
    collections::BTreeSet,
    env, fs,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use ai_agent_bridge::slack_project_bindings::{
    AgentMode, RegistryError, RequestedCapability, ResolveRequest, SlackProjectRegistry,
    WritePolicy,
};
use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

const DEFAULT_ADDRESS: &str = "127.0.0.1:8160";
const DEFAULT_REGISTRY_PATH: &str = "config/alex-main-agent.channels.json";
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
    message: String,
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
    let registry_bytes = fs::read(&registry_path)
        .with_context(|| format!("failed to read registry from {}", registry_path.display()))?;
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; img-src 'none'; object-src 'none'; script-src 'self'; style-src 'self'",
        ),
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
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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

async fn resolve(
    State(state): State<ProbeState>,
    Json(input): Json<ResolveInput>,
) -> Response {
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
            let (status, code) = registry_error_response(&error);
            (
                status,
                Json(ResolveFailure {
                    status: "rejected",
                    code,
                    message: error.to_string(),
                }),
            )
                .into_response()
        }
    }
}

fn registry_error_response(error: &RegistryError) -> (StatusCode, &'static str) {
    match error {
        RegistryError::UnmappedChannel => (StatusCode::NOT_FOUND, "unmapped_channel"),
        RegistryError::UnauthorizedPrincipal => {
            (StatusCode::FORBIDDEN, "unauthorized_principal")
        }
        RegistryError::RepositoryNotAllowed => {
            (StatusCode::FORBIDDEN, "repository_not_allowed")
        }
        RegistryError::AgentModeNotAllowed => {
            (StatusCode::FORBIDDEN, "agent_mode_not_allowed")
        }
        RegistryError::WriteNotAllowed => (StatusCode::FORBIDDEN, "write_not_allowed"),
        RegistryError::IssueTeamMismatch => (StatusCode::BAD_REQUEST, "issue_team_mismatch"),
        RegistryError::InvalidIssueIdentifier => {
            (StatusCode::BAD_REQUEST, "invalid_issue_identifier")
        }
        _ => (StatusCode::BAD_REQUEST, "invalid_request"),
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
