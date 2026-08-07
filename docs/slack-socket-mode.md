# Slack Socket Mode

The Slack command service normally receives signed HTTPS requests. Socket Mode is
an opt-in alternative for environments that cannot expose public ingress: the
service opens an outbound WebSocket to Slack and receives slash-command payloads
over that authenticated connection.

Set `SLACK_SOCKET_MODE=true` and provide an app-level `SLACK_APP_TOKEN` beginning
with `xapp-`. The existing bot token, signing secret, project registry, and service
configuration remain required. The HTTP listener remains available for health and
readiness checks; signed Request URL routes continue to work if they are used.

Socket Mode uses connection-level authentication rather than the per-request HMAC
used by Request URLs. Payloads still pass through the same installed-app,
workspace, command, and project-policy validation before work is accepted. The
client acknowledges envelopes within Slack's deadline and reconnects with bounded
backoff when Slack rotates or closes a connection.
