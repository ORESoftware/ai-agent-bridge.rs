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
    configured_slack_identity(&app.config)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let listener = TcpListener::bind(address).await?;
    info!(%address, dry_run = app.config.dry_run, "starting ORESoftware Slack commands");
    // Socket Mode is additive: the HTTP listener still serves health and
    // readiness for the orchestrator, and the Request URL routes stay mounted so
    // a deployment can move between transports without a code change.
    if app.config.socket_mode {
        tokio::spawn(run_socket_mode(app.clone()));
    }

    axum::serve(listener, router(app)).await?;
    Ok(())
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/slack/commands/x-ores-claude", post(command))
        .route("/slack/commands/x-ores-chatgpt", post(command))
        .route("/slack/interactions", post(interaction))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(app)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn ready(State(app): State<Arc<App>>) -> Response {
    match configured_slack_identity(&app.config) {
        Ok(identity) => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "dry_run": app.config.dry_run,
                "default_context_messages": app.config.context_messages,
                "installed_app_identity_enforced": identity.is_some()
            }),
        ),
        Err(_) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "ok": false,
                "error": "installed_slack_app_identity_not_configured"
            }),
        ),
    }
}

async fn command(
    State(app): State<Arc<App>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !verify_signature(&app.config, &headers, &body, Utc::now().timestamp()) {
        return reject(StatusCode::UNAUTHORIZED);
    }
    let expected_provider = match uri.path() {
        "/slack/commands/x-ores-claude" => Provider::Claude,
        "/slack/commands/x-ores-chatgpt" => Provider::Chatgpt,
        _ => return reject(StatusCode::NOT_FOUND),
    };
    match validate_slash_envelope(&app.config, &body, expected_provider) {
        Ok(()) => {}
        Err(error) => return slash_envelope_error(&error),
    }
    let command = match SlashCommand::parse(&body) {
        Ok(command) => command,
        Err(_) => return ephemeral("Invalid slash command payload."),
    };
    match tokio::time::timeout(SLACK_ACK_DEADLINE, handle_command(app, command)).await {
        Ok(response) => response,
        Err(_) => ephemeral("The command could not be acknowledged safely before Slack's deadline."),
    }
}

async fn handle_command(app: Arc<App>, command: SlashCommand) -> Response {
    if command.text.trim().is_empty() {
        return match app.command_binding(&command).await {
            Ok(binding) => match app.open_modal(&command, &binding).await {
                Ok(()) => json_response(StatusCode::OK, json!({})),
                Err(Error::Policy) => reject(StatusCode::FORBIDDEN),
                Err(_) => ephemeral("The agent menu could not be opened safely."),
            },
            Err(Error::Policy) => reject(StatusCode::FORBIDDEN),
            Err(_) => ephemeral("The agent menu could not be opened safely."),
        };
    }
    let request = match RunRequest::direct(&command, app.config.context_messages) {
        Ok(request) => request,
        Err(_) => return ephemeral("Provide a bounded task after the command."),
    };
    // Bind before matching. Temporaries in a match scrutinee live until the end
    // of the match, so the future returned by `resolve` would still borrow
    // `*app` inside the arms -- and an arm moves `app` into `accept`.
    let authorized = app.resolve(&request).await;
    match authorized {
        Ok(_) => ephemeral(&accept(app, request).await.message()),
        Err(Error::Policy) => reject(StatusCode::FORBIDDEN),
        Err(_) => ephemeral("The task could not be authorized safely."),
    }
}

async fn interaction(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    if !verify_signature(&app.config, &headers, &body, Utc::now().timestamp()) {
        return reject(StatusCode::UNAUTHORIZED);
    }
    let payload = match parse_interaction_envelope(&app.config, &body) {
        Ok(payload) => payload,
        Err(error) => return interaction_envelope_error(&error),
    };
    handle_interaction(app, payload).await
}

/// A `view_submission` reply is always HTTP 200 -- Slack reads `response_action`
/// from the body, and a non-200 leaves the modal open with a generic failure.
fn interaction_envelope_error(error: &Error) -> Response {
    let text = match error {
        Error::Config(_) => "The installed Slack app identity is not configured safely.",
        Error::Policy => "This request did not originate from the installed Slack app and workspace.",
        _ => "The submitted form could not be read.",
    };
    view_error("task", text)
}

fn view_error(block: &str, text: &str) -> Response {
    json_response(
        StatusCode::OK,
        json!({"response_action": "errors", "errors": {block: text}}),
    )
}

/// The whole modal-submission path, shared by both transports.
///
/// This used to live inline in the HTTP handler, which is why Socket Mode --
/// the transport actually running in production -- silently dropped every
/// submission: its `"interactive"` arm had nothing to call.
async fn handle_interaction(app: Arc<App>, payload: InteractionPayload) -> Response {
    let request = match RunRequest::interaction(payload) {
        Ok(request) => request,
        Err(_) => return view_error("task", "The submitted form could not be read."),
    };
    // The deadline must cover the run claim too, not just the policy resolve:
    // the claim writes and fsyncs a journal entry, and an unbounded tail here
    // is what actually blows Slack's 3s acknowledgement window.
    let outcome = tokio::time::timeout(SLACK_ACK_DEADLINE, async {
        let authorized = app.resolve(&request).await;
        match authorized {
            Ok(_) => Ok(accept(app, request).await),
            Err(error) => Err(error),
        }
    })
    .await;

    match outcome {
        Err(_) => view_error(
            "task",
            "Authorization did not finish before Slack's acknowledgement deadline.",
        ),
        Ok(Err(Error::Policy)) => view_error(
            "write_scope",
            "This channel, user, repository, or write scope is not authorized.",
        ),
        Ok(Err(_)) => view_error("task", "The task could not be authorized safely."),
        // Slack gives a successful submission no signal beyond the modal
        // closing, which is indistinguishable from the submission being
        // dropped. The in-channel dispatch post is currently the only
        // confirmation; a chat.postEphemeral to the submitter is still owed.
        Ok(Ok(accepted)) if accepted.is_started() => {
            json_response(StatusCode::OK, json!({}))
        }
        Ok(Ok(accepted)) => view_error(accepted.modal_block(), &accepted.message()),
    }
}

/// What `accept` decided. Deliberately not a `Response`.
///
/// Slack renders a slash command body only on HTTP 200, so the outcome cannot
/// be carried in the status code. It also has to be rendered two different ways
/// -- as an ephemeral message for a slash command, and as a `view_submission`
/// reply for a modal -- so the decision and its presentation are separated.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Accepted {
    Started {
        run_id: String,
        provider: &'static str,
        dry_run: bool,
    },
    Duplicate {
        run_id: String,
    },
    AtCapacity,
    JournalUnavailable,
}

impl Accepted {
    fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    /// The text a person sees. Every one of these is delivered with HTTP 200,
    /// because Slack discards the body of anything else.
    fn message(&self) -> String {
        match self {
            Self::Started {
                run_id,
                provider,
                dry_run: false,
            } => format!(
                "Accepted {provider} run `{run_id}`. IDs and progress will be posted in-channel."
            ),
            // The dry-run marker belongs in the acknowledgement the user
            // actually reads. It used to appear only in a separate in-channel
            // post, so a failure to post left them holding an "Accepted" that
            // read as real work.
            Self::Started {
                run_id,
                provider,
                dry_run: true,
            } => format!(
                "Accepted {provider} run `{run_id}` — dry run, no external writes will be performed."
            ),
            Self::Duplicate { run_id } => {
                format!("Run `{run_id}` was already accepted.")
            }
            Self::AtCapacity => "The agent queue is at capacity. Try again shortly.".into(),
            Self::JournalUnavailable => {
                "The durable run journal is unavailable, so the run was not started.".into()
            }
        }
    }

    /// Which modal input to attach a failure to. Attaching everything to the
    /// task field puts the red note under the textarea while the offending
    /// control sits further down, unmarked.
    fn modal_block(&self) -> &'static str {
        // Every current outcome is about the run as a whole rather than about
        // one input, so they all attach to the task field. Scope-specific
        // denials should attach to `write_scope` or `repository` once the
        // registry's reason survives that far -- see DEN-3898.
        "task"
    }
}

async fn accept(app: Arc<App>, request: RunRequest) -> Accepted {
    // Answer a duplicate truthfully *before* reserving capacity. Replying
    // "queue is at capacity" to an already-claimed delivery invites another
    // delivery of work that is already running — retry amplification exactly
    // when the service is saturated.
    match journal({
        let app = app.clone();
        let run_id = request.run_id.clone();
        move || Ok(app.claimed(&run_id))
    })
    .await
    {
        Ok(true) => {
            return Accepted::Duplicate {
                run_id: request.run_id,
            }
        }
        Ok(false) => {}
        Err(()) => return Accepted::JournalUnavailable,
    }

    let permit = match app.capacity.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return Accepted::AtCapacity,
    };

    let claimed = journal({
        let app = app.clone();
        let request = request.clone();
        move || app.claim(&request)
    })
    .await;

    match claimed {
        Ok(false) => Accepted::Duplicate {
            run_id: request.run_id,
        },
        Err(()) => Accepted::JournalUnavailable,
        Ok(true) => {
            let run_id = request.run_id.clone();
            let provider = request.provider.label();
            let dry_run = app.config.dry_run;
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = dispatch(&app, &request).await {
                    warn!(
                        run_id = %request.run_id,
                        team_id = %request.team_id,
                        channel_id = %request.channel_id,
                        user_id = %request.user_id,
                        error = %error,
                        "Slack agent dispatch failed",
                    );
                }
            });
            Accepted::Started {
                run_id,
                provider,
                dry_run,
            }
        }
    }
}

/// Run a blocking run-journal operation off the async runtime. `claim` opens a
/// file and fsyncs; `tokio::time::timeout` cannot preempt a blocking syscall,
/// so doing this inline would let a slow disk blow Slack's 3s deadline and
/// stall unrelated requests sharing the worker thread.
async fn journal<F>(operation: F) -> std::result::Result<bool, ()>
where
    F: FnOnce() -> Result<bool> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) | Err(_) => Err(()),
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

/// A reply meant for a person.
///
/// Always HTTP 200. Slack renders a slash command's response body *only* on
/// 200 -- "Any other flavor of response will result in a user-facing error" --
/// so a carefully worded 403 reaches the user as `http_status_code_403` and the
/// text is discarded. Every diagnostic here was invisible for exactly that
/// reason. The outcome travels in the body.
fn ephemeral(text: &str) -> Response {
    json_response(StatusCode::OK, json!({"response_type": "ephemeral", "text": text}))
}

/// A refusal aimed at Slack rather than at a person. No body is rendered, so
/// nothing is lost by using a real status code -- and an unauthenticated caller
/// should not be told why it failed.
fn reject(status: StatusCode) -> Response {
    json_response(status, json!({}))
}

/// Reject an invalid slash-command envelope before any user-facing policy
/// work begins. These responses are security boundaries, not Slack messages:
/// returning HTTP 200 would make a wrong app, provider-confused route, or
/// ambiguous form look accepted to callers and observability.
fn slash_envelope_error(error: &Error) -> Response {
    let status = match error {
        Error::Config(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Policy => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    reject(status)
}

/// Render a remote-controlled string safe for a structured log line: no
/// newlines or control characters that could forge a log record, and bounded
/// so a misbehaving downstream cannot flood the log pipeline.
fn log_safe(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | ' ')
            {
                character
            } else {
                '?'
            }
        })
        .collect::<String>();
    truncate(&cleaned, 64)
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
            b"command=%2Fx-ores-claude&team_id=T1&channel_id=C1&user_id=U1&text=fix+DEN-1041&trigger_id=1.2",
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

#[cfg(test)]
mod log_safety_tests {
    use super::*;

    #[test]
    fn a_downstream_cannot_forge_log_records_or_flood_the_pipeline() {
        let forged = "not_authed\n2026-08-22 ERROR fabricated line";
        let rendered = log_safe(forged);
        assert!(!rendered.contains('\n'));
        assert!(rendered.starts_with("not_authed"));

        let flood = "a".repeat(4_096);
        assert!(log_safe(&flood).len() <= 64);
    }

    #[test]
    fn ordinary_slack_error_codes_survive_unchanged() {
        for code in ["not_authed", "channel_not_found", "ratelimited", "invalid_auth"] {
            assert_eq!(log_safe(code), code);
        }
    }
}

#[cfg(test)]
mod reply_contract_tests {
    use super::*;

    /// Slack renders a slash command's response body only on HTTP 200 --
    /// "Any other flavor of response will result in a user-facing error". A
    /// reply carrying text on any other status is text no one will ever read.
    #[test]
    fn every_reply_meant_for_a_person_is_two_hundred() {
        for text in [
            "This channel or user is not authorized.",
            "The agent queue is at capacity. Try again shortly.",
            "Provide a bounded task after the command.",
            "Invalid slash command payload.",
        ] {
            let response = ephemeral(text);
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{text:?} would be discarded by Slack on a non-200",
            );
        }
    }

    /// The converse: a refusal aimed at Slack carries no body, so a real status
    /// code costs nothing and an unauthenticated caller learns nothing.
    #[test]
    fn refusals_aimed_at_slack_keep_their_status_and_say_nothing() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::NOT_FOUND] {
            assert_eq!(reject(status).status(), status);
        }
    }

    #[test]
    fn slash_envelope_failures_are_never_rendered_as_success() {
        assert_eq!(
            slash_envelope_error(&Error::Policy).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            slash_envelope_error(&Error::Request).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            slash_envelope_error(&Error::Config("identity pin missing".into())).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn the_dry_run_marker_is_in_the_acknowledgement_itself() {
        let live = Accepted::Started {
            run_id: "ores-abc".into(),
            provider: "Claude",
            dry_run: false,
        };
        let dry = Accepted::Started {
            run_id: "ores-abc".into(),
            provider: "Claude",
            dry_run: true,
        };
        assert!(!live.message().contains("dry run"));
        // The only disclosure used to be a separate in-channel post. If that
        // post failed, the user was left holding an "Accepted" that read as
        // real work.
        assert!(dry.message().contains("dry run"));
        assert!(dry.message().contains("no external writes"));
        assert!(live.is_started() && dry.is_started());
    }

    #[test]
    fn a_duplicate_is_reported_as_a_duplicate_not_as_capacity() {
        let duplicate = Accepted::Duplicate {
            run_id: "ores-abc".into(),
        };
        assert!(duplicate.message().contains("already accepted"));
        assert!(!duplicate.message().contains("capacity"));
        assert!(!duplicate.is_started());

        assert!(Accepted::AtCapacity.message().contains("capacity"));
        assert!(!Accepted::AtCapacity.is_started());
        assert!(Accepted::JournalUnavailable
            .message()
            .contains("not started"));
        assert!(!Accepted::JournalUnavailable.is_started());
    }

    #[test]
    fn a_view_submission_error_is_two_hundred_and_names_a_block() {
        let response = view_error("write_scope", "nope");
        // Slack reads response_action from the body; a non-200 leaves the modal
        // open with a generic failure instead.
        assert_eq!(response.status(), StatusCode::OK);
    }
}
