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

fn reconnect_delay(attempt: u32) -> Duration {
    let seconds = SOCKET_MODE_RECONNECT_MIN
        .as_secs()
        .saturating_mul(1u64 << attempt.min(5));
    Duration::from_secs(seconds.min(SOCKET_MODE_RECONNECT_MAX.as_secs()))
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
            .map_err(|_| Error::Slack)?;
        let body = read_bounded(response, MAX_SLACK_RESPONSE_BYTES)
            .await
            .ok_or(Error::Slack)?;
        let opened = serde_json::from_slice::<SocketModeOpen>(&body).map_err(|_| Error::Slack)?;
        if !opened.ok {
            return Err(Error::Slack);
        }
        let url = opened.url.ok_or(Error::Slack)?;
        if !url.starts_with("wss://") {
            return Err(Error::Slack);
        }
        Ok(url)
    }

    /// Turns one Socket Mode frame into the same response the HTTP route would
    /// have produced, as an ack payload.
    async fn handle_socket_envelope(
        self: &Arc<Self>,
        envelope: &SocketModeEnvelope,
    ) -> Option<Value> {
        match envelope.kind.as_str() {
            "slash_commands" => {
                let expected = socket_expected_provider(&envelope.payload).ok()?;
                let body = socket_payload_to_form(&envelope.payload).ok()?;
                if validate_slash_envelope(&self.config, &body, expected).is_err() {
                    return Some(json!({
                        "response_type": "ephemeral",
                        "text": "This request did not originate from the installed Slack app and workspace."
                    }));
                }
                let command = SlashCommand::parse(&body).ok()?;
                let response =
                    tokio::time::timeout(SLACK_ACK_DEADLINE, handle_command(self.clone(), command))
                        .await
                        .ok()?;
                response_ack_payload(response).await
            }
            "interactive" => {
                // Interactions ack with an empty body; the surface updates the
                // view or posts to the channel out of band, exactly as on HTTP.
                let _ = self;
                None
            }
            _ => None,
        }
    }
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
    loop {
        match app.open_socket_connection().await {
            Ok(url) => match tokio_tungstenite::connect_async(&url).await {
                Ok((stream, _)) => {
                    attempt = 0;
                    info!("Slack Socket Mode connection established");
                    drive_socket(&app, stream).await;
                    warn!("Slack Socket Mode connection closed; reconnecting");
                }
                Err(_) => warn!("Slack Socket Mode handshake failed"),
            },
            Err(_) => warn!("Slack refused a Socket Mode connection"),
        }
        let delay = reconnect_delay(attempt);
        attempt = attempt.saturating_add(1);
        sleep(delay).await;
    }
}

async fn drive_socket<S>(app: &Arc<App>, stream: S)
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

    let mut stream = stream;
    while let Some(Ok(message)) = stream.next().await {
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Ping(payload) => {
                let _ = stream.send(Message::Pong(payload)).await;
                continue;
            }
            Message::Close(_) => break,
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
            break;
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
            break;
        }
    }
}

#[cfg(test)]
mod socket_mode_tests {
    use super::*;

    #[test]
    fn payload_is_reencoded_into_the_form_the_http_path_validates() {
        let payload = json!({
            "command": "/ores-claude",
            "team_id": "T1",
            "channel_id": "C1",
            "user_id": "U1",
            "api_app_id": "A0BMBAMM5NJ",
            "text": "harden the cron canary",
            "trigger_id": "abc.def"
        });
        let form = socket_payload_to_form(&payload).expect("form");
        let parsed = parse_form(&form).expect("parses with the shared decoder");

        assert_eq!(parsed.get("command").map(String::as_str), Some("/ores-claude"));
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
            "command": "/ores-claude",
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
            "command": "/ores-claude",
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
            socket_expected_provider(&json!({"command": "/ores-claude"})).unwrap(),
            Provider::Claude
        );
        // Aliases resolve exactly as they do on the HTTP routes.
        assert_eq!(
            socket_expected_provider(&json!({"command": "/my-chatgpt"})).unwrap(),
            Provider::Chatgpt
        );
        assert!(socket_expected_provider(&json!({"command": "/unknown"})).is_err());
        assert!(socket_expected_provider(&json!({})).is_err());
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
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(3), Duration::from_secs(8));
        // Never grows without limit, however long Slack stays unreachable.
        for attempt in 0..64 {
            assert!(reconnect_delay(attempt) <= SOCKET_MODE_RECONNECT_MAX);
        }
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
            r#"{"type":"slash_commands","envelope_id":"e1","payload":{"command":"/ores-claude"}}"#,
        )
        .expect("envelope");
        assert_eq!(envelope.envelope_id.as_deref(), Some("e1"));
        assert_eq!(envelope.payload["command"], "/ores-claude");
    }
}
