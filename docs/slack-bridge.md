# Authenticated Slack dual-model bridge

The `fiducia-slack-bridge` binary is a thin Slack Events/API adapter over the
existing `fiducia-ai-agent-bridge` workflow API. It does not contain provider
SDKs or model credentials.

A valid command is routed to exactly two registered bridge identities:

- `claude-fable-5` — labeled **Claude Fable** in Slack;
- `gpt-5.6-sol` — labeled **ChatGPT 5.6 Sol** in Slack.

The adapter creates one `competitive` workflow with `worker_count=2`, polls the
authoritative workflow record, and posts one deterministic, labeled reply for
each configured identity to the originating `channel` and `thread_ts`.

## Security boundary

The Events endpoint is `POST /slack/events`.

Before JSON parsing or event handling, the adapter:

1. validates `X-Slack-Request-Timestamp`;
2. rejects requests more than five minutes old in either direction;
3. verifies `X-Slack-Signature` as HMAC-SHA256 over
   `v0:<timestamp>:<raw-body>`;
4. handles Slack URL verification independently;
5. rejects or ignores unauthorized workspaces, channels, and optional threads;
6. ignores bot events, message subtypes, and the configured bot user;
7. requires an explicit command prefix;
8. durably claims the Slack `event_id` before starting any workflow.

The journal stores only event IDs, workflow IDs, state, and per-model reply
markers. It never stores prompt text, model output, Slack tokens, bridge bearer
tokens, or signing secrets. On Unix, the parent directory is forced to `0700`,
the journal to `0600`, symlinks and hard links are rejected, and appends use
`O_NOFOLLOW`.

Structured trace events contain Slack correlation IDs and outcome categories,
but never prompt bodies or model responses. The adapter refuses redirects and
caps every remote response body.

## Command contract

Default syntax:

```text
!ask-both <prompt>
```

For an app mention, the leading mention is ignored:

```text
@Fiducia !ask-both explain the safest rollout order
```

A root message starts a thread by using its own `ts`; a reply preserves the
existing `thread_ts`.

## Required environment

Secrets are environment-only and must come from a protected secret manager:

| Variable | Required | Purpose |
|---|---:|---|
| `SLACK_SIGNING_SECRET` | yes | Slack request signature verification |
| `SLACK_BOT_TOKEN` | when active | `chat.postMessage`; not required in dry-run |
| `SLACK_BRIDGE_BEARER` | for non-loopback bridge URL | Bearer for the existing bridge API |

Fail-closed allowlists are required:

| Variable | Meaning |
|---|---|
| `SLACK_ALLOWED_TEAM_IDS` | Comma-separated Slack workspace/team IDs |
| `SLACK_ALLOWED_CHANNEL_IDS` | Comma-separated channel IDs |
| `SLACK_ALLOWED_THREAD_TS` | Optional comma-separated thread timestamps |
| `SLACK_BOT_USER_ID` | Optional bot user ID for self-loop prevention |

Routing and operation:

| Variable | Default |
|---|---|
| `SLACK_COMMAND_PREFIX` | `!ask-both` |
| `SLACK_CLAUDE_AGENT_KEY` | `claude-fable-5` |
| `SLACK_OPENAI_AGENT_KEY` | `gpt-5.6-sol` |
| `SLACK_BRIDGE_URL` | `http://127.0.0.1:8142/` |
| `SLACK_BRIDGE_DRY_RUN` | `true` |
| `SLACK_BRIDGE_HOST` | `127.0.0.1` |
| `SLACK_BRIDGE_PORT` | `8150` |
| `SLACK_MAX_REQUEST_AGE_SECS` | `300`, capped at 300 |
| `SLACK_WORKFLOW_TIMEOUT_SECS` | `120`, capped at 900 |
| `SLACK_POLL_INTERVAL_MS` | `1000` |
| `SLACK_MAX_BODY_BYTES` | `262144`, capped at 1 MiB |
| `SLACK_MAX_CONCURRENT_WORKFLOWS` | `8`, capped at 128 |
| `SLACK_IDEMPOTENCY_PATH` | XDG/HOME state directory |

The two agent keys must be distinct. A non-loopback bridge URL must use HTTPS
and must have `SLACK_BRIDGE_BEARER`. URLs containing userinfo, query strings, or
fragments are rejected.

## Dry-run activation

Dry-run is the default. In dry-run mode, authenticated and allowlisted commands
are validated, claimed, traced, and marked complete without calling the bridge
or Slack APIs.

A production activation sequence is:

1. register and verify both model identities in the bridge;
2. configure DEN-287 policy ceilings for provider calls, tokens, elapsed time,
   retries, concurrency, and spend;
3. mount a persistent journal path;
4. provision the signing secret, bot token, and bridge bearer through External
   Secrets or the approved secret manager;
5. set exact team/channel/thread allowlists;
6. perform URL verification;
7. send one bounded test command with `SLACK_BRIDGE_DRY_RUN=true`;
8. restart the adapter, replay that exact signed `event_id`, and verify the
   durable journal prevents a second workflow or Slack reply;
9. change only `SLACK_BRIDGE_DRY_RUN=false` through the reviewed GitOps PR.

No adapter path merges pull requests or writes directly to protected branches.

## Partial failure

A successful model response is posted as soon as it appears. If the other model
does not submit before the bounded deadline, the successful reply is preserved
and the missing model receives a labeled warning. If workflow creation fails,
the adapter attempts one bounded labeled failure reply for each configured
model.

Slack API delivery is attempted at most three times, honoring a bounded
`Retry-After`. A reply marker is persisted only after Slack returns `ok=true`.

## Health and telemetry

- `GET /healthz` — process liveness;
- `GET /readyz` — readiness plus the non-sensitive dry-run state.

Outcome trace events cover accepted, ignored, rejected-signature,
rejected-policy, duplicate, capacity, success, partial, failed, and journal
failure paths. Collector-side metric extraction can turn the
`fiducia.slack_bridge.metrics` events into counters.

## Container target

The repository Dockerfile publishes a third non-root distroless target:

```text
target: slack
image:  ghcr.io/oresoftware/fiducia-slack-bridge
```

Deploy only an immutable `@sha256:` digest. The live cluster remains disabled
until secrets, allowlists, NetworkPolicy, rollback, and incident procedures are
reviewed in a separate `k8s-cluster` PR.
