// Socket Mode ingress.
//
// Slack's Request URL transport needs a publicly reachable HTTPS endpoint. This
// is the alternative: the process opens an *outbound* WebSocket to Slack and
// Slack pushes commands down it, so no ingress, DNS record, TLS certificate or
// inbound firewall rule is required.
//
// Security note, stated plainly because it is a real trade: Socket Mode frames
// arrive over a connection authenticated once by an app-level `xapp-` token.
// There is no per-request `X-Slack-Signature` to verify, so the HMAC check the
// Request URL path applies does not exist here. Connection-level auth replaces
// per-request auth. That is why this transport is opt-in and off by default.
//
// Everything downstream of ingress is shared: the payload is re-encoded into the
// exact form encoding the HTTP path already validates, then handed to the same
// `validate_slash_envelope` and `SlashCommand::parse`. Socket Mode therefore
// cannot accept a command the Request URL path would reject.

const SOCKET_MODE_OPEN_METHOD: &str = "apps.connections.open";
const SOCKET_MODE_MAX_FRAME_BYTES: usize = 262_144;
const SOCKET_MODE_RECONNECT_MIN: Duration = Duration::from_secs(1);
const SOCKET_MODE_RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Longest silence tolerated on an established socket before it is treated as
/// dead. Slack pings roughly every five seconds.
const SOCKET_MODE_READ_IDLE: Duration = Duration::from_secs(45);
/// How long a connection must survive before its predecessor's failures are
/// forgotten. Resetting on the handshake alone lets a connection that Slack
/// accepts and immediately closes reset the backoff every second, forever.
const SOCKET_MODE_HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// `/readyz` fails closed once the socket has been silent this long, even if
/// the receive task has not yet broken out of its read. Slack pings every ~5s
/// and the read idle window is 45s, so 60s is a dead connection rather than a
/// slow one.
const SOCKET_MODE_READY_STALE: Duration = Duration::from_secs(60);
/// Jitter applied to reconnect sleep so replicas do not resynchronise on the
/// same 1/2/4/8/16/30 ladder.
const SOCKET_MODE_RECONNECT_JITTER: f64 = 0.25;

/// One Socket Mode frame. Slack sends `hello` and `disconnect` control frames
/// alongside the event envelopes, and only envelopes carry an `envelope_id`.
#[derive(Debug, Deserialize)]
struct SocketModeEnvelope {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct SocketModeOpen {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Why `drive_socket` returned, so the supervisor can skip backoff on a Slack
/// recycle (the `disconnect` frame is sent *in advance* so the replacement can
/// be opened before this connection is dropped).
#[derive(Debug)]
enum SocketDriveEnd {
    Recycle { next_url: Option<String> },
    Dead,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn socket_last_frame_age_seconds(last_frame_at: u64, now: u64) -> u64 {
    if last_frame_at == 0 {
        0
    } else {
        now.saturating_sub(last_frame_at)
    }
}

/// Independent of HTTP health: Socket Mode is ready only while a connection is
/// marked live *and* a frame (or the handshake) has been seen recently enough
/// that inbound Slack events can still be received.
fn socket_mode_ready(
    socket_mode: bool,
    connected: bool,
    last_frame_at: u64,
    now: u64,
    stale_after: Duration,
) -> bool {
    if !socket_mode {
        return true;
    }
    if !connected || last_frame_at == 0 {
        return false;
    }
    now.saturating_sub(last_frame_at) <= stale_after.as_secs()
}

impl App {
    fn mark_socket_frame(&self, now: u64) {
        self.socket_connected.store(true, Ordering::SeqCst);
        self.last_frame_at.store(now, Ordering::SeqCst);
    }

    fn mark_socket_disconnected(&self) {
        self.socket_connected.store(false, Ordering::SeqCst);
    }
}

/// Re-encodes a Socket Mode JSON payload into `application/x-www-form-urlencoded`.
///
/// The HTTP ingress validates form bytes, so converting here keeps exactly one
/// validated parse path rather than a second, subtly different one. Only string
/// scalars are carried: Slack sends slash-command fields as strings, and
/// silently stringifying anything else would let a nested object smuggle a value
/// past `id_field`'s character checks.
fn socket_payload_to_form(payload: &Value) -> Result<Vec<u8>> {
    let object = payload.as_object().ok_or(Error::Request)?;
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    let mut carried = 0usize;
    for (key, value) in object {
        let Some(value) = value.as_str() else {
            continue;
        };
        serializer.append_pair(key, value);
        carried += 1;
    }
    if carried == 0 {
        return Err(Error::Request);
    }
    let encoded = serializer.finish();
    if encoded.len() > SOCKET_MODE_MAX_FRAME_BYTES {
        return Err(Error::Request);
    }
    Ok(encoded.into_bytes())
}

/// The acknowledgement Slack expects back on the socket within three seconds.
fn socket_ack(envelope_id: &str, body: Option<Value>) -> Value {
    match body {
        Some(payload) => json!({ "envelope_id": envelope_id, "payload": payload }),
        None => json!({ "envelope_id": envelope_id }),
    }
}

/// Which provider a Socket Mode slash command names.
///
/// The HTTP path derives this from the route it arrived on; a socket frame has
/// no path, so it comes from the reviewed `command` field instead. Unknown
/// commands are refused rather than defaulted.
fn socket_expected_provider(payload: &Value) -> Result<Provider> {
    let command = payload
        .get("command")
        .and_then(Value::as_str)
        .ok_or(Error::Request)?;
    Provider::from_command(command).ok_or(Error::Request)
}

/// Socket Mode authenticates the whole connection with an app-level token, so a
/// bot token pasted here would fail only at connect time, long after startup.
/// Reject the shape at configuration instead.
fn validate_app_token(socket_mode: bool, token: Option<&str>) -> Result<()> {
    if let Some(token) = token {
        if !token.starts_with("xapp-") || token.len() < 16 {
            return Err(Error::Config(
                "SLACK_APP_TOKEN must be an app-level token beginning with 'xapp-'".into(),
            ));
        }
    }
    if socket_mode && token.is_none() {
        return Err(Error::Config(
            "SLACK_APP_TOKEN is required when SLACK_SOCKET_MODE=true".into(),
        ));
    }
    Ok(())
}

fn reconnect_base_secs(attempt: u32) -> u64 {
    let seconds = SOCKET_MODE_RECONNECT_MIN
        .as_secs()
        .saturating_mul(1u64 << attempt.min(5));
    seconds.min(SOCKET_MODE_RECONNECT_MAX.as_secs())
}

/// `jitter` is a signed fraction in `[-SOCKET_MODE_RECONNECT_JITTER, +...]`.
fn reconnect_delay_jittered(attempt: u32, jitter: f64) -> Duration {
    let jitter = jitter.clamp(-SOCKET_MODE_RECONNECT_JITTER, SOCKET_MODE_RECONNECT_JITTER);
    let seconds = (reconnect_base_secs(attempt) as f64 * (1.0 + jitter)).round() as u64;
    Duration::from_secs(
        seconds
            .max(SOCKET_MODE_RECONNECT_MIN.as_secs())
            .min(SOCKET_MODE_RECONNECT_MAX.as_secs()),
    )
}

fn reconnect_jitter_ratio() -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    unix_now().hash(&mut hasher);
    let unit = (hasher.finish() % 10_000) as f64 / 10_000.0;
    unit * (SOCKET_MODE_RECONNECT_JITTER * 2.0) - SOCKET_MODE_RECONNECT_JITTER
}

fn reconnect_delay(attempt: u32) -> Duration {
    reconnect_delay_jittered(attempt, reconnect_jitter_ratio())
}

impl App {
    /// Asks Slack for a single-use WebSocket URL.
    async fn open_socket_connection(&self) -> Result<String> {
        let token = self.config.app_token.as_deref().ok_or(Error::Config(
            "SLACK_APP_TOKEN is required for Socket Mode".into(),
        ))?;
        let response = self
            .client
            .post(self.config.slack_url(SOCKET_MODE_OPEN_METHOD)?)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| {
                warn!(
                    error = %log_safe(&error.to_string()),
                    "Slack Socket Mode open request failed"
                );
                Error::Slack
            })?;
        let status = response.status();
        let body = read_bounded(response, MAX_SLACK_RESPONSE_BYTES)
            .await
            .ok_or_else(|| {
                warn!(
                    status = status.as_u16(),
                    "Slack Socket Mode open response exceeded the read bound"
                );
                Error::Slack
            })?;
        let opened = serde_json::from_slice::<SocketModeOpen>(&body).unwrap_or(SocketModeOpen {
            ok: false,
            url: None,
            error: None,
        });
        if !opened.ok {
            warn!(
                status = status.as_u16(),
                slack_error = %opened
                    .error
                    .as_deref()
                    .map(log_safe)
                    .unwrap_or_else(|| "unknown".into()),
                "Slack refused a Socket Mode connection"
            );
            return Err(Error::Slack);
        }
        let url = opened.url.ok_or_else(|| {
            warn!(
                status = status.as_u16(),
                slack_error = "missing_url",
                "Slack refused a Socket Mode connection"
            );
            Error::Slack
        })?;
        if !url.starts_with("wss://") {
            warn!(
                status = status.as_u16(),
                slack_error = "invalid_url",
                "Slack refused a Socket Mode connection"
            );
            return Err(Error::Slack);
        }
        Ok(url)
    }

    /// Turns one Socket Mode frame into the same response the HTTP route would
    /// have produced, as an ack payload.
    ///
    /// Every branch that can fail says so. This used to return `None` on four
    /// separate error paths, which `drive_socket` turned into a bare ack --
    /// Slack then treats the envelope as handled and never redelivers, so the
    /// command was consumed and produced literal silence.
    async fn handle_socket_envelope(
        self: &Arc<Self>,
        envelope: &SocketModeEnvelope,
    ) -> Option<Value> {
        match envelope.kind.as_str() {
            "slash_commands" => {
                let Ok(expected) = socket_expected_provider(&envelope.payload) else {
                    warn!("Socket Mode frame named a command outside the reviewed namespace");
                    return Some(socket_text("That command is not one this app serves."));
                };
                let Ok(body) = socket_payload_to_form(&envelope.payload) else {
                    warn!("Socket Mode slash command payload could not be re-encoded");
                    return Some(socket_text("Invalid slash command payload."));
                };
                if validate_slash_envelope(&self.config, &body, expected).is_err() {
                    return Some(socket_text(
                        "This request did not originate from the installed Slack app and workspace.",
                    ));
                }
                let Ok(command) = SlashCommand::parse(&body) else {
                    warn!("Socket Mode slash command failed to parse after validation");
                    return Some(socket_text("Invalid slash command payload."));
                };
                match tokio::time::timeout(SLACK_ACK_DEADLINE, handle_command(self.clone(), command))
                    .await
                {
                    Ok(response) => response_ack_payload(response).await,
                    Err(_) => {
                        warn!("Socket Mode command missed the acknowledgement deadline");
                        Some(socket_text(
                            "The command could not be acknowledged safely before Slack's deadline.",
                        ))
                    }
                }
            }
            // Modal submissions. This arm used to do nothing at all and ack
            // empty, so with Socket Mode enabled -- the only transport running
            // in production -- the entire guided surface was dead, and it
            // failed in the shape that looks most like success: the modal
            // opened, accepted five fields, and closed cleanly with no run.
            "interactive" => {
                let value = envelope.payload.clone();
                let payload = match interaction_payload(&self.config, value) {
                    Ok(payload) => payload,
                    Err(error) => {
                        warn!(reason = %error, "Socket Mode interaction rejected at the envelope");
                        return response_ack_payload(interaction_envelope_error(&error)).await;
                    }
                };
                response_ack_payload(handle_interaction(self.clone(), payload).await).await
            }
            _ => None,
        }
    }
}

/// An ephemeral reply carried back over the socket rather than over HTTP.
fn socket_text(text: &str) -> Value {
    json!({ "response_type": "ephemeral", "text": text })
}

/// Extracts an axum response body so the socket ack shows a member the same text
/// the HTTP transport would have returned.
async fn response_ack_payload(response: Response) -> Option<Value> {
    let bytes = axum::body::to_bytes(response.into_body(), MAX_SLACK_RESPONSE_BYTES)
        .await
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let value = serde_json::from_slice::<Value>(&bytes).ok()?;
    if value.as_object().is_some_and(|object| object.is_empty()) {
        return None;
    }
    Some(value)
}

/// Runs the Socket Mode receive loop until the process stops, reconnecting with
/// bounded backoff. Slack recycles a connection roughly every few minutes and
/// sends a `disconnect` frame first, so reconnecting is normal operation rather
/// than an error path.
async fn run_socket_mode(app: Arc<App>) {
    let mut attempt = 0u32;
    let mut pending_url: Option<String> = None;
    loop {
        let url = match pending_url.take() {
            Some(url) => url,
            None => match app.open_socket_connection().await {
                Ok(url) => url,
                Err(_) => {
                    emit_metric("socket_refused");
                    let delay = reconnect_delay(attempt);
                    attempt = attempt.saturating_add(1);
                    sleep(delay).await;
                    continue;
                }
            },
        };
        match tokio_tungstenite::connect_async(&url).await {
            Ok((stream, _)) => {
                info!("Slack Socket Mode connection established");
                let opened = tokio::time::Instant::now();
                let end = drive_socket(&app, stream).await;
                let uptime = opened.elapsed();
                // Reset only once the connection has proven itself. A
                // handshake Slack accepts and closes immediately -- what
                // happens when the app is not enabled for Socket Mode --
                // would otherwise reset the backoff on every iteration and
                // hammer the rate-limited apps.connections.open at 1/s.
                if uptime >= SOCKET_MODE_HEALTHY_AFTER {
                    attempt = 0;
                }
                warn!(
                    uptime_secs = uptime.as_secs(),
                    "Slack Socket Mode connection closed; reconnecting"
                );
                match end {
                    SocketDriveEnd::Recycle { next_url } => {
                        emit_metric("socket_recycle");
                        pending_url = next_url;
                        continue;
                    }
                    SocketDriveEnd::Dead => emit_metric("socket_reconnect"),
                }
            }
            Err(error) => {
                emit_metric("socket_handshake_failed");
                warn!(
                    error = %log_safe(&error.to_string()),
                    "Slack Socket Mode handshake failed"
                );
            }
        }
        let delay = reconnect_delay(attempt);
        attempt = attempt.saturating_add(1);
        sleep(delay).await;
    }
}

async fn drive_socket<S>(app: &Arc<App>, stream: S) -> SocketDriveEnd
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    app.mark_socket_frame(unix_now());
    let mut stream = stream;
    let end = loop {
        // A blackholed TCP connection -- NAT or idle timeout with no FIN, which
        // is routine for long-lived WebSockets -- leaves `next()` pending
        // forever. The loop never returns, so nothing ever reconnects and the
        // service is silently dead. Slack pings roughly every five seconds, so
        // a long silence means the connection is gone, not quiet.
        let message = match tokio::time::timeout(SOCKET_MODE_READ_IDLE, stream.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(_) => break SocketDriveEnd::Dead,
            Err(_) => {
                emit_metric("socket_idle_timeout");
                warn!(
                    idle_secs = SOCKET_MODE_READ_IDLE.as_secs(),
                    "no Slack Socket Mode frame within the idle window; treating the connection as dead"
                );
                break SocketDriveEnd::Dead;
            }
        };
        app.mark_socket_frame(unix_now());
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Ping(payload) => {
                let _ = stream.send(Message::Pong(payload)).await;
                continue;
            }
            Message::Close(_) => break SocketDriveEnd::Dead,
            _ => continue,
        };
        if text.len() > SOCKET_MODE_MAX_FRAME_BYTES {
            warn!("dropping an oversized Slack Socket Mode frame");
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<SocketModeEnvelope>(&text) else {
            continue;
        };
        if envelope.kind == "disconnect" {
            // Slack sends this in advance so the replacement can be opened
            // before the current connection is dropped. Skip the reconnect
            // sleep either way: a recycle is not a failure.
            let next_url = app.open_socket_connection().await.ok();
            break SocketDriveEnd::Recycle { next_url };
        }
        let Some(envelope_id) = envelope.envelope_id.clone() else {
            continue;
        };

        let body = app.handle_socket_envelope(&envelope).await;
        let ack = socket_ack(&envelope_id, body);
        if stream
            .send(Message::Text(ack.to_string().into()))
            .await
            .is_err()
        {
            break SocketDriveEnd::Dead;
        }
    };
    app.mark_socket_disconnected();
    end
}

/// Shared by both socket test modules, so it lives at file scope rather than
/// inside either one.
#[cfg(test)]
fn socket_test_config() -> Config {
    Config {
        host: "127.0.0.1".parse().unwrap(),
        port: 8151,
        signing_secret: "test-signing-secret".into(),
        bot_token: "test-bot-token".into(),
        registry_path: PathBuf::from("/tmp/registry.json"),
        state_dir: PathBuf::from("/tmp/slack-command-state"),
        bridge_url: "http://127.0.0.1:8142/".into(),
        bridge_bearer: None,
        coordinator_url: "http://127.0.0.1:8160/".into(),
        coordinator_bearer: None,
        slack_api_base_url: "http://127.0.0.1:8170/api/".into(),
        claude_agent: "claude-fable-5".into(),
        chatgpt_agent: "gpt-5.6-sol".into(),
        linear_run_project_id: DEFAULT_LINEAR_RUN_PROJECT.into(),
        context_messages: 5,
        socket_mode: true,
        app_token: Some("xapp-1-test-token".into()),
        dry_run: true,
        max_concurrent_runs: 1,
        allow_unpinned_identity: true,
    }
}

#[cfg(test)]
mod socket_mode_tests {
    use super::*;

    #[test]
    fn payload_is_reencoded_into_the_form_the_http_path_validates() {
        let payload = json!({
            "command": "/x-ores-claude",
            "team_id": "T1",
            "channel_id": "C1",
            "user_id": "U1",
            "api_app_id": "A0BMBAMM5NJ",
            "text": "harden the cron canary",
            "trigger_id": "abc.def"
        });
        let form = socket_payload_to_form(&payload).expect("form");
        let parsed = parse_form(&form).expect("parses with the shared decoder");

        assert_eq!(parsed.get("command").map(String::as_str), Some("/x-ores-claude"));
        assert_eq!(parsed.get("team_id").map(String::as_str), Some("T1"));
        // Spaces survive the round trip through the hand-rolled decoder.
        assert_eq!(
            parsed.get("text").map(String::as_str),
            Some("harden the cron canary")
        );
    }

    #[test]
    fn reencoding_escapes_separators_rather_than_splitting_a_field() {
        // A task containing & or = must not become extra form fields.
        let payload = json!({
            "command": "/x-ores-claude",
            "text": "fix a=b & c=d",
            "team_id": "T1"
        });
        let form = socket_payload_to_form(&payload).expect("form");
        let parsed = parse_form(&form).expect("parse");

        assert_eq!(parsed.len(), 3, "no field may be split by its own content");
        assert_eq!(parsed.get("text").map(String::as_str), Some("fix a=b & c=d"));
    }

    #[test]
    fn non_string_fields_are_dropped_rather_than_stringified() {
        // Slack sends scalars as strings. Stringifying a nested object would let
        // it past the identifier character checks downstream.
        let payload = json!({
            "command": "/x-ores-claude",
            "team_id": "T1",
            "enterprise": Value::Null,
            "is_enterprise_install": false,
            "nested": {"team_id": "T-EVIL"}
        });
        let form = socket_payload_to_form(&payload).expect("form");
        let parsed = parse_form(&form).expect("parse");

        assert_eq!(parsed.len(), 2);
        assert!(!parsed.contains_key("nested"));
        assert!(!parsed.contains_key("is_enterprise_install"));
    }

    #[test]
    fn an_empty_or_non_object_payload_is_refused() {
        assert!(socket_payload_to_form(&json!({})).is_err());
        assert!(socket_payload_to_form(&json!([1, 2])).is_err());
        assert!(socket_payload_to_form(&json!("nope")).is_err());
        assert!(socket_payload_to_form(&json!({"only": {"nested": 1}})).is_err());
    }

    #[test]
    fn the_provider_comes_from_the_reviewed_command_field() {
        assert_eq!(
            socket_expected_provider(&json!({"command": "/x-ores-claude"})).unwrap(),
            Provider::Claude
        );
        // The socket transport resolves the same namespace the HTTP routes do.
        assert_eq!(
            socket_expected_provider(&json!({"command": "/x-ores-chatgpt"})).unwrap(),
            Provider::Chatgpt
        );
        assert!(socket_expected_provider(&json!({"command": "/unknown"})).is_err());
        assert!(socket_expected_provider(&json!({})).is_err());
    }

    /// Both transports must accept and refuse exactly the same commands.
    ///
    /// The asymmetry that makes this worth pinning: on HTTP the provider comes
    /// from the route path and is then cross-checked against the signed
    /// `command` field, so a stale mapping still routes. A socket frame has no
    /// path, so `Provider::from_command` is the sole authority — a stale
    /// mapping drops the frame with no ack, which looks like a dead connection
    /// rather than a routing bug.
    #[test]
    fn both_transports_resolve_the_same_namespace() {
        for (command, expected) in [
            ("/x-ores-claude", Provider::Claude),
            ("/x-ores-chatgpt", Provider::Chatgpt),
        ] {
            let over_socket = socket_expected_provider(&json!({ "command": command }))
                .expect("the socket transport must accept a reviewed command");
            let over_http = Provider::from_command(command)
                .expect("the HTTP transport must accept a reviewed command");
            assert_eq!(over_socket, expected);
            assert_eq!(over_socket, over_http, "{command} must mean one provider");
        }

        for command in [
            "/ores-claude",
            "/ores-chatgpt",
            "/x-claude",
            "/x-chatgpt",
            "/my-claude",
            "/my-chatgpt",
            "/x-ores-gemini",
        ] {
            assert!(
                socket_expected_provider(&json!({ "command": command })).is_err(),
                "{command} must be refused over the socket"
            );
            assert!(
                Provider::from_command(command).is_none(),
                "{command} must be refused over HTTP"
            );
        }
    }

    /// A socket frame carries no signature, so app/team pinning is a larger
    /// share of what stands between the handler and an unintended payload.
    /// `socket_payload_to_form` must carry those fields through to the shared
    /// envelope validation rather than dropping them on the floor.
    #[test]
    fn identity_fields_survive_the_socket_to_form_conversion() {
        let payload = json!({
            "command": "/x-ores-claude",
            "api_app_id": "A0BMBAMM5NJ",
            "team_id": "T01B3C83PMK",
            "channel_id": "C1",
            "user_id": "U1",
            "text": "fix DEN-1041",
            "trigger_id": "t1",
        });
        let body = socket_payload_to_form(&payload).expect("a real payload must convert");
        let form = parse_form(&body).expect("the converted body must parse");
        for (field, value) in [
            ("api_app_id", "A0BMBAMM5NJ"),
            ("team_id", "T01B3C83PMK"),
            ("command", "/x-ores-claude"),
            ("trigger_id", "t1"),
        ] {
            assert_eq!(
                form.get(field).map(String::as_str),
                Some(value),
                "{field} must reach the shared envelope validation"
            );
        }
    }

    /// The pinning decision itself is transport-independent: it runs on the
    /// form bytes, which is exactly what the socket path hands it.
    #[test]
    fn a_pinned_deployment_refuses_a_foreign_workspace_over_the_socket() {
        let pinned = configured_slack_identity_from_values(
            &Config {
                allow_unpinned_identity: false,
                ..socket_test_config()
            },
            Some("A0BMBAMM5NJ".into()),
            Some("T01B3C83PMK".into()),
        )
        .expect("a paired identity is valid")
        .expect("a paired identity is present");

        let ours = socket_payload_to_form(&json!({
            "command": "/x-ores-claude",
            "api_app_id": "A0BMBAMM5NJ",
            "team_id": "T01B3C83PMK",
        }))
        .unwrap();
        let theirs = socket_payload_to_form(&json!({
            "command": "/x-ores-claude",
            "api_app_id": "A0BMBAMM5NJ",
            "team_id": "T09999999",
        }))
        .unwrap();

        let team_of = |body: &[u8]| {
            parse_form(body)
                .ok()
                .and_then(|form| form.get("team_id").cloned())
        };
        assert_eq!(team_of(&ours).as_deref(), Some(pinned.1.as_str()));
        assert_ne!(team_of(&theirs).as_deref(), Some(pinned.1.as_str()));
    }

    #[test]
    fn acks_carry_the_envelope_id_and_omit_an_empty_body() {
        let bare = socket_ack("env-1", None);
        assert_eq!(bare, json!({"envelope_id": "env-1"}));
        assert!(bare.get("payload").is_none());

        let with_body = socket_ack("env-2", Some(json!({"text": "denied"})));
        assert_eq!(with_body["envelope_id"], "env-2");
        assert_eq!(with_body["payload"]["text"], "denied");
    }

    #[test]
    fn app_token_shape_is_validated_at_startup() {
        // Off by default and with no token: nothing to check.
        assert!(validate_app_token(false, None).is_ok());
        assert!(validate_app_token(true, Some("xapp-1-A0BMBAMM5NJ-123-abc")).is_ok());

        // Enabling the transport without a token cannot silently no-op.
        assert!(validate_app_token(true, None).is_err());

        // A bot token here would otherwise fail only at connect time.
        assert!(validate_app_token(true, Some("xoxb-not-an-app-token")).is_err());
        assert!(validate_app_token(false, Some("xoxb-not-an-app-token")).is_err());
        assert!(validate_app_token(true, Some("xapp-")).is_err());
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(reconnect_delay_jittered(0, 0.0), Duration::from_secs(1));
        assert_eq!(reconnect_delay_jittered(3, 0.0), Duration::from_secs(8));
        // Never grows without limit, however long Slack stays unreachable.
        for attempt in 0..64 {
            assert!(reconnect_delay_jittered(attempt, 0.0) <= SOCKET_MODE_RECONNECT_MAX);
            assert!(reconnect_delay_jittered(attempt, 0.25) <= SOCKET_MODE_RECONNECT_MAX);
            assert!(reconnect_delay_jittered(attempt, -0.25) >= SOCKET_MODE_RECONNECT_MIN);
        }
    }

    #[test]
    fn reconnect_backoff_applies_bounded_jitter() {
        assert_eq!(reconnect_delay_jittered(3, 0.25), Duration::from_secs(10));
        assert_eq!(reconnect_delay_jittered(3, -0.25), Duration::from_secs(6));
        // Live jitter stays inside the same cap as the unjittered ladder.
        for attempt in 0..16 {
            let delay = reconnect_delay(attempt);
            assert!(delay >= SOCKET_MODE_RECONNECT_MIN);
            assert!(delay <= SOCKET_MODE_RECONNECT_MAX);
        }
    }

    #[test]
    fn a_socket_mode_refusal_carries_the_slack_error_code() {
        let opened = serde_json::from_str::<SocketModeOpen>(r#"{"ok":false,"error":"invalid_auth"}"#)
            .expect("refusal");
        assert!(!opened.ok);
        assert_eq!(opened.error.as_deref(), Some("invalid_auth"));
    }

    #[test]
    fn control_frames_carry_no_envelope_id() {
        let hello = serde_json::from_str::<SocketModeEnvelope>(
            r#"{"type":"hello","num_connections":1}"#,
        )
        .expect("hello");
        assert_eq!(hello.kind, "hello");
        assert!(hello.envelope_id.is_none());

        let envelope = serde_json::from_str::<SocketModeEnvelope>(
            r#"{"type":"slash_commands","envelope_id":"e1","payload":{"command":"/x-ores-claude"}}"#,
        )
        .expect("envelope");
        assert_eq!(envelope.envelope_id.as_deref(), Some("e1"));
        assert_eq!(envelope.payload["command"], "/x-ores-claude");
    }
}

#[cfg(test)]
mod socket_interaction_tests {
    use super::*;

    fn submission(team: &str) -> Value {
        json!({
            "type": "view_submission",
            "api_app_id": "A0BMBAMM5NJ",
            "team": { "id": team },
            "user": { "id": "U1" },
            "view": {
                "id": "V1",
                "callback_id": CALLBACK_ID,
                "private_metadata": "{}",
                "state": { "values": {} }
            }
        })
    }

    /// A Socket Mode frame carries the interaction as JSON directly, not
    /// form-wrapped under `payload=`. The shared decoder must take it as-is;
    /// re-encoding it to a form just to parse it back would be a second,
    /// subtly different path.
    #[test]
    fn a_socket_shaped_payload_decodes_without_a_form_wrapper() {
        let config = Config {
            allow_unpinned_identity: true,
            ..socket_test_config()
        };
        let payload = interaction_payload(&config, submission("T01B3C83PMK"))
            .expect("a socket interaction payload must decode directly");
        assert_eq!(payload.kind, "view_submission");
        assert_eq!(payload.view.callback_id, CALLBACK_ID);
        assert_eq!(payload.team.id, "T01B3C83PMK");
    }

    /// Identity pinning applies to interactions on the socket exactly as it
    /// does on HTTP. This matters more here: a socket frame carries no
    /// signature, so the pinned pair is a larger share of what stands between
    /// the handler and an unintended payload.
    #[test]
    fn a_foreign_workspace_interaction_is_refused_on_the_socket() {
        std::env::set_var("SLACK_EXPECTED_APP_ID", "A0BMBAMM5NJ");
        std::env::set_var("SLACK_EXPECTED_TEAM_ID", "T01B3C83PMK");
        let config = Config {
            allow_unpinned_identity: false,
            ..socket_test_config()
        };
        let ours = interaction_payload(&config, submission("T01B3C83PMK"));
        let theirs = interaction_payload(&config, submission("T09999999"));
        std::env::remove_var("SLACK_EXPECTED_APP_ID");
        std::env::remove_var("SLACK_EXPECTED_TEAM_ID");

        assert!(ours.is_ok(), "the installed workspace must be accepted");
        assert!(
            matches!(theirs, Err(Error::Policy)),
            "another workspace must be refused by policy",
        );
    }

    /// Regression guard for the defect this file shipped: the `"interactive"`
    /// arm did nothing and acked empty, so with Socket Mode enabled every modal
    /// submission was dropped in the way that looks most like success.
    #[test]
    fn the_interactive_arm_is_wired_to_the_shared_handler() {
        let source = include_str!("part15.rs");
        let arm = source
            .split(r#""interactive" => {"#)
            .nth(1)
            .expect("the interactive arm must exist");
        let body = &arm[..arm.find("\n            _ => None,").unwrap_or(arm.len())];
        assert!(
            body.contains("handle_interaction"),
            "the interactive arm must dispatch through the shared handler",
        );
        assert!(
            !body.contains("let _ = self;"),
            "the interactive arm must not be a no-op again",
        );
    }
}

#[cfg(test)]
mod socket_liveness_tests {
    use super::*;
    use futures::{Sink, Stream};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio_tungstenite::tungstenite::Message;

    type WsError = tokio_tungstenite::tungstenite::Error;
    type WsSinkResult = std::result::Result<(), WsError>;

    /// A connection that never yields a frame: the blackholed-TCP case.
    struct HangSocket;

    impl Stream for HangSocket {
        type Item = std::result::Result<Message, WsError>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Sink<Message> for HangSocket {
        type Error = WsError;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: Message) -> WsSinkResult {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }
    }

    /// Yields one text frame, then hangs. Used to prove a frame updates liveness
    /// before the idle window fires.
    struct OneFrameThenHang {
        frame: Option<Message>,
    }

    impl Stream for OneFrameThenHang {
        type Item = std::result::Result<Message, WsError>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.get_mut().frame.take() {
                Some(frame) => Poll::Ready(Some(Ok(frame))),
                None => Poll::Pending,
            }
        }
    }

    impl Sink<Message> for OneFrameThenHang {
        type Error = WsError;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _: Message) -> WsSinkResult {
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<WsSinkResult> {
            Poll::Ready(Ok(()))
        }
    }

    fn socket_test_app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!(
            "fiducia-socket-liveness-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let registry = dir.join("registry.json");
        fs::write(
            &registry,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "bindings": []
            }))
            .unwrap(),
        )
        .unwrap();
        let mut config = socket_test_config();
        config.registry_path = registry;
        config.state_dir = dir.join("state");
        Arc::new(App::new(config).expect("socket test app"))
    }

    /// Fake clock: readiness is a pure function of (now, last_frame, connected).
    #[test]
    fn socket_mode_readiness_uses_a_fake_clock() {
        let stale = SOCKET_MODE_READY_STALE;
        assert!(
            socket_mode_ready(false, false, 0, 1_700_000_000, stale),
            "HTTP-only deployments must not be gated on the socket"
        );
        assert!(
            !socket_mode_ready(true, false, 1_700_000_000, 1_700_000_000, stale),
            "a disconnected socket cannot be ready"
        );
        assert!(
            !socket_mode_ready(true, true, 0, 1_700_000_000, stale),
            "a socket that has never received a frame cannot be ready"
        );
        assert!(
            socket_mode_ready(true, true, 1_700_000_000, 1_700_000_045, stale),
            "a live socket with a recent frame is ready"
        );
        assert!(
            !socket_mode_ready(true, true, 1_700_000_000, 1_700_000_061, stale),
            "a still-marked-connected socket that has gone silent is not ready"
        );
    }

    #[test]
    fn command_metrics_declare_every_outcome_as_a_zero_series() {
        let app = socket_test_app();
        let body = render_command_metrics(&app, 1_700_000_060);
        for outcome in COMMAND_METRIC_OUTCOMES {
            assert!(
                body.contains(&format!(
                    "slack_command_requests_total{{outcome=\"{outcome}\"}}"
                )),
                "{outcome} must appear before its first occurrence"
            );
        }
        assert!(body.contains("slack_command_socket_connected 0"));
        assert!(body.contains("slack_command_socket_last_frame_age_seconds 0"));
        app.mark_socket_frame(1_700_000_000);
        let live = render_command_metrics(&app, 1_700_000_012);
        assert!(live.contains("slack_command_socket_connected 1"));
        assert!(live.contains("slack_command_socket_last_frame_age_seconds 12"));
    }

    #[tokio::test]
    async fn readyz_fails_closed_when_socket_mode_cannot_receive() {
        let app = socket_test_app();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = app.clone();
        tokio::spawn(async move {
            axum::serve(listener, router(serving)).await.unwrap();
        });
        let base = format!("http://{address}");

        let health = reqwest::get(format!("{base}/healthz")).await.unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::OK);

        let dead = reqwest::get(format!("{base}/readyz")).await.unwrap();
        assert_eq!(dead.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let dead_body = dead.json::<Value>().await.unwrap();
        assert_eq!(dead_body["error"], "slack_socket_mode_unavailable");

        app.mark_socket_frame(unix_now());
        let live = reqwest::get(format!("{base}/readyz")).await.unwrap();
        assert_eq!(live.status(), reqwest::StatusCode::OK);

        app.last_frame_at
            .store(unix_now().saturating_sub(61), Ordering::SeqCst);
        let stale = reqwest::get(format!("{base}/readyz")).await.unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        let metrics = reqwest::get(format!("{base}/metrics")).await.unwrap();
        assert_eq!(metrics.status(), reqwest::StatusCode::OK);
        let metrics_body = metrics.text().await.unwrap();
        assert!(metrics_body.contains("slack_command_socket_connected"));
        assert!(metrics_body.contains("slack_command_socket_last_frame_age_seconds"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_mock_connection_is_treated_as_dead() {
        let app = socket_test_app();
        app.mark_socket_frame(unix_now());
        assert!(app.socket_connected.load(Ordering::SeqCst));

        let drive = drive_socket(&app, HangSocket);
        tokio::pin!(drive);
        tokio::select! {
            _ = &mut drive => panic!("the idle window must not fire before 45s"),
            _ = tokio::time::sleep(Duration::from_secs(44)) => {}
        }
        let end = drive.await;
        assert!(matches!(end, SocketDriveEnd::Dead));
        assert!(
            !app.socket_connected.load(Ordering::SeqCst),
            "a dead socket must not keep readiness green"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hello_frame_keeps_the_socket_alive_until_the_idle_window() {
        let app = socket_test_app();
        let socket = OneFrameThenHang {
            frame: Some(Message::text(r#"{"type":"hello","num_connections":1}"#)),
        };
        let drive = drive_socket(&app, socket);
        tokio::pin!(drive);
        tokio::select! {
            _ = &mut drive => panic!("a recent hello must not be treated as death"),
            _ = tokio::time::sleep(Duration::from_secs(44)) => {}
        }
        assert!(
            app.socket_connected.load(Ordering::SeqCst),
            "a hello frame is inbound traffic and must mark the socket live"
        );
        let end = drive.await;
        assert!(matches!(end, SocketDriveEnd::Dead));
        assert!(!app.socket_connected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_disconnect_frame_recycles_without_waiting_out_the_idle_window() {
        let app = socket_test_app();
        let socket = OneFrameThenHang {
            frame: Some(Message::text(r#"{"type":"disconnect","reason":"renew"}"#)),
        };
        let end = tokio::time::timeout(Duration::from_secs(5), drive_socket(&app, socket))
            .await
            .expect("a disconnect must not wait for the idle window");
        assert!(
            matches!(end, SocketDriveEnd::Recycle { .. }),
            "Slack recycle frames skip backoff rather than counting as death: {end:?}"
        );
        assert!(!app.socket_connected.load(Ordering::SeqCst));
    }
}
