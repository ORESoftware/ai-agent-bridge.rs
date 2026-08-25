# Slack Socket Mode

The Slack command service normally receives signed HTTPS requests. Socket Mode is
an opt-in alternative for environments that cannot expose public ingress: the
service opens an outbound WebSocket to Slack and receives slash-command payloads
over that authenticated connection.

Set `SLACK_SOCKET_MODE=true` and provide an app-level `SLACK_APP_TOKEN` beginning
with `xapp-`. The existing bot token, signing secret, project registry, and service
configuration remain required. The HTTP listener remains available for health and
readiness checks; signed Request URL routes continue to work if they are used.

## The security trade, stated plainly

Socket Mode uses connection-level authentication rather than the per-request HMAC
used by Request URLs. Frames arrive over a WebSocket authenticated once by the
app-level token, so there is **no `X-Slack-Signature` to verify** — the signature
check the Request URL path applies does not exist on this transport. That is why
it is opt-in and off by default, and why the reviewed production posture remains
the signed Request URL.

## One validated parse path

Slack delivers slash-command payloads as JSON over the socket, but as form
encoding over HTTP. Rather than add a second parser, the socket payload is
re-encoded into the exact `application/x-www-form-urlencoded` bytes the HTTP path
already validates and handed to the same `validate_slash_envelope` and
`SlashCommand::parse`. **Socket Mode therefore cannot accept a command the
Request URL path would reject** — installed-app identity, workspace, command name,
and project policy are all enforced identically.

Only string scalars are carried across. Slack sends command fields as strings, and
silently stringifying a nested object would let it smuggle a value past the
identifier character checks downstream.

## Operation

The client acknowledges envelopes within Slack's three-second deadline and
reconnects with bounded exponential backoff, capped at 30 seconds. Slack recycles
a connection every few minutes and sends a `disconnect` frame first, so
reconnecting is normal operation rather than an error path.

`SLACK_APP_TOKEN` is shape-checked at startup — a bot token pasted into that slot
would otherwise fail only at connect time, long after the process reported ready.

## Activation

This transport removes the ingress, DNS, TLS and NetworkPolicy prerequisites, but
not the Slack-side ones: the app manifest still needs `socket_mode_enabled: true`,
an app-level token minted with `connections:write`, and a reinstall. Every other
activation gate in [`slack-ores-commands.md`](./slack-ores-commands.md) — registry
review, dry-run canary, budget ceilings — applies unchanged.
