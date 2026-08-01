async fn read_bounded(response: HttpResponse, limit: usize) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return None;
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if output.len() + chunk.len() > limit {
            return None;
        }
        output.extend_from_slice(&chunk);
    }
    Some(output)
}

pub async fn run() -> anyhow::Result<()> {
    let _telemetry = fiducia_telemetry::init("fiducia-slack-command");
    let config = Config::from_env().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let address = std::net::SocketAddr::new(config.host, config.port);
    let app = Arc::new(App::new(config).map_err(|error| anyhow::anyhow!(error.to_string()))?);
    let listener = TcpListener::bind(address).await?;
    info!(%address, dry_run = app.config.dry_run, "starting ORESoftware Slack commands");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/slack/commands/ores-claude", post(command))
        .route("/slack/commands/ores-chatgpt", post(command))
        .route("/slack/interactions", post(interaction))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(app)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn ready(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "dry_run": app.config.dry_run,
        "default_context_messages": app.config.context_messages
    }))
}

async fn command(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    if !verify_signature(&app.config, &headers, &body, Utc::now().timestamp()) {
        return ephemeral(StatusCode::UNAUTHORIZED, "Request authentication failed.");
    }
    let command = match SlashCommand::parse(&body) {
        Ok(command) => command,
        Err(_) => return ephemeral(StatusCode::BAD_REQUEST, "Invalid slash command payload."),
    };
    if command.text.trim().is_empty() {
        return match app.command_binding(&command).await {
            Ok(binding) => match app.open_modal(&command, &binding).await {
                Ok(()) => json_response(StatusCode::OK, json!({})),
                Err(Error::Policy) => ephemeral(
                    StatusCode::FORBIDDEN,
                    "This channel or user is not authorized.",
                ),
                Err(_) => ephemeral(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The agent menu could not be opened safely.",
                ),
            },
            Err(Error::Policy) => ephemeral(
                StatusCode::FORBIDDEN,
                "This channel or user is not authorized.",
            ),
            Err(_) => ephemeral(
                StatusCode::SERVICE_UNAVAILABLE,
                "The agent menu could not be opened safely.",
            ),
        };
    }
    let request = match RunRequest::direct(&command, app.config.context_messages) {
        Ok(request) => request,
        Err(_) => {
            return ephemeral(
                StatusCode::BAD_REQUEST,
                "Provide a bounded task after the command.",
            )
        }
    };
    match app.resolve(&request).await {
        Ok(_) => accept(app, request),
        Err(Error::Policy) => ephemeral(
            StatusCode::FORBIDDEN,
            "This channel, user, repository, or write scope is not authorized.",
        ),
        Err(_) => ephemeral(
            StatusCode::SERVICE_UNAVAILABLE,
            "The task could not be authorized safely.",
        ),
    }
}

async fn interaction(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    if !verify_signature(&app.config, &headers, &body, Utc::now().timestamp()) {
        return json_response(StatusCode::UNAUTHORIZED, json!({}));
    }
    let request = parse_form(&body)
        .ok()
        .and_then(|form| form.get("payload").cloned())
        .and_then(|payload| serde_json::from_str::<InteractionPayload>(&payload).ok())
        .and_then(|payload| RunRequest::interaction(payload).ok());
    let Some(request) = request else {
        return json_response(StatusCode::BAD_REQUEST, json!({}));
    };
    let authorized = app.resolve(&request).await;
    let accepted = match authorized {
        Ok(_) => accept(app, request),
        Err(_) => ephemeral(StatusCode::FORBIDDEN, "The submitted scope is not authorized."),
    };
    if accepted.status() == StatusCode::OK {
        json_response(StatusCode::OK, json!({}))
    } else {
        json_response(
            StatusCode::OK,
            json!({"response_action": "errors", "errors": {"task": "The run could not be accepted safely."}}),
        )
    }
}

fn accept(app: Arc<App>, request: RunRequest) -> Response {
    let permit = match app.capacity.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ephemeral(
                StatusCode::SERVICE_UNAVAILABLE,
                "The agent queue is at capacity.",
            )
        }
    };
    match app.claim(&request) {
        Ok(false) => ephemeral(
            StatusCode::OK,
            &format!("Run `{}` was already accepted.", request.run_id),
        ),
        Err(_) => ephemeral(
            StatusCode::SERVICE_UNAVAILABLE,
            "The durable run journal is unavailable.",
        ),
        Ok(true) => {
            let run_id = request.run_id.clone();
            let provider = request.provider.label();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = dispatch(&app, &request).await {
                    warn!(run_id = %request.run_id, error = %error, "Slack agent dispatch failed");
                }
            });
            ephemeral(
                StatusCode::OK,
                &format!("Accepted {provider} run `{run_id}`. IDs and progress will be posted in-channel."),
            )
        }
    }
}

async fn dispatch(app: &App, request: &RunRequest) -> Result<()> {
    let resolved = app.resolve(request).await?;
    let context = app
        .context(&request.channel_id, request.context_messages)
        .await?;
    if app.config.dry_run {
        let response = app
            .client
            .post(app.config.slack_url("chat.postMessage")?)
            .bearer_auth(&app.config.bot_token)
            .json(&json!({
                "channel": request.channel_id,
                "text": format!(
                    ":test_tube: *Dry-run {} task*\nRun: `{}`\nRepository: `{}`\nLinear project: `{}`\nContext: {} latest non-bot messages\nNo coordinator, bridge, Linear, or GitHub write was performed.",
                    request.provider.label(), request.run_id, resolved.repository,
                    resolved.linear_project_id, context.len()
                )
            }))
            .send()
            .await
            .map_err(|_| Error::Slack)?;
        slack_ok(response).await?;
        return Ok(());
    }
    let workflow_id = app.create_workflow(request, &resolved, &context).await?;
    let job_id = app
        .create_job(request, &resolved, &context, &workflow_id)
        .await?;
    app.post_status(request, &resolved, context.len(), &workflow_id, &job_id)
        .await?;
    Ok(())
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn ephemeral(status: StatusCode, text: &str) -> Response {
    json_response(status, json!({"response_type": "ephemeral", "text": text}))
}

fn truncate(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_string();
    }
    let mut boundary = maximum_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_commands() {
        let command = SlashCommand::parse(
            b"command=%2Fores-claude&team_id=T1&channel_id=C1&user_id=U1&text=fix+DEN-1041&trigger_id=1.2",
        )
        .expect("valid command");
        assert_eq!(command.provider(), Provider::Claude);
        assert_eq!(command.text, "fix DEN-1041");
    }

    #[test]
    fn run_ids_are_deterministic() {
        assert_eq!(run_id("same"), run_id("same"));
        assert_ne!(run_id("same"), run_id("different"));
        assert!(run_id("same").starts_with("ores-"));
    }

    #[test]
    fn finds_linear_issue() {
        assert_eq!(
            find_issue("implement DEN-1041 now"),
            Some("DEN-1041".into())
        );
        assert_eq!(find_issue("no issue"), None);
    }

    #[test]
    fn slack_api_override_is_loopback_only() {
        assert_eq!(
            slack_api_base_url("https://slack.com/api").unwrap(),
            "https://slack.com/api/"
        );
        assert_eq!(
            slack_api_base_url("http://127.0.0.1:9999/api").unwrap(),
            "http://127.0.0.1:9999/api/"
        );
        assert!(slack_api_base_url("https://attacker.example/api").is_err());
        assert!(slack_api_base_url("http://slack.com/api").is_err());
    }
}
