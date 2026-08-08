# Slack signed-ingress security contract

Tracking: `DEN-1041`, `DEN-1298`, `DEN-1321`, `DEN-1325`

This document defines the security boundary for the `fiducia-slack-command` service that receives the reviewed `alex-main-agent` slash commands and modal submissions. Slack is a thin authenticated intake surface. It is not the durable run queue, provider executor, GitHub writer, Linear lifecycle engine, or policy authority.

## Public endpoints

The public deployment exposes only these Slack-signed request routes:

```text
POST /slack/commands/ores-claude
POST /slack/commands/ores-chatgpt
POST /slack/interactions
```

The six reviewed command names map onto the two provider endpoints:

```text
/ores-claude   /x-claude   /my-claude
/ores-chatgpt  /x-chatgpt  /my-chatgpt
```

The payload command must agree with the endpoint provider. A valid HMAC for a Claude payload sent to the ChatGPT endpoint is rejected before channel policy, history access, modal creation, run journaling, bridge dispatch, or coordinator dispatch.

## Request-validation order

The service fails closed in this order:

1. enforce the Axum request-body ceiling;
2. verify the exact `v0` Slack HMAC over the unmodified request body;
3. require a parseable Slack timestamp no more than 300 seconds in the past or future;
4. percent-decode form keys and reject duplicate normalized keys;
5. enforce endpoint-to-command provider agreement;
6. enforce the configured Slack app ID and workspace/team ID on non-loopback deployments;
7. parse and bound identifiers, prompt text, modal fields, and context selection;
8. resolve immutable workspace/channel/user or user-group policy;
9. claim the deterministic run ID in the durable local journal;
10. perform only the writes authorized by the resolved capability and write policy.

A rejected request must not read Slack history, open a modal, post a status message, create a run-journal claim, create a bridge workflow, create a coordinator job, or write to Linear or GitHub.

## URL and transport policy

Remote bridge and coordinator URLs must use HTTPS. Plain HTTP is accepted only for exact loopback hosts used by local tests and sidecars:

- `localhost`;
- any address in IPv4 `127.0.0.0/8`;
- IPv6 `::1`, including its bracketed URL representation.

Embedded URL credentials, query strings, and fragments are rejected. Hostname lookalikes such as `localhost.attacker.example` and `127.0.0.1.attacker.example` are not loopback addresses.

The Slack API base URL defaults to `https://slack.com/api/`. Its test override remains loopback-only so a deployment cannot redirect bearer-authenticated Slack requests to an arbitrary remote host.

## Required Slack bot scopes

The reviewed manifest grants exactly these bot scopes:

| Scope | Runtime purpose |
|---|---|
| `commands` | Receive the reviewed slash commands. |
| `chat:write` | Post bounded run acknowledgements and status messages. |
| `channels:history` | Read approved public-channel context. |
| `groups:history` | Read approved private-channel context. |
| `usergroups:read` | Resolve `allowed_user_group_ids` through `usergroups.list`. |

The initial pilot authorizes one immutable user ID and does not need a user-group lookup for that user. The runtime nevertheless supports group-based authorization and calls `usergroups.list` whenever a binding requires it, so `usergroups:read` must be granted before such a binding is activated. Any scope or command change requires review, manifest reconciliation, and app reinstallation in the target workspace.

No Slack token, signing secret, app configuration token, provider credential, GitHub credential, Linear credential, bridge bearer, or coordinator bearer belongs in the manifest, repository, test fixture, Actions log, artifact, or Linear document.

## Fast Rust coverage

The repository-local suite locks the following boundaries:

- exact HMAC verification, body tamper detection, missing headers, malformed timestamps, wrong signature versions, and forged signatures;
- acceptance at both `-300` and `+300` seconds and rejection at `-301` and `+301` seconds;
- canonical 64-character signature decoding;
- loopback URL classification for IPv4, the full `127/8` range, `localhost`, and bracketed IPv6;
- remote plaintext, hostname lookalike, embedded-credential, query, and fragment rejection;
- duplicate decoded form keys, malformed escapes, and invalid UTF-8 rejection;
- deterministic run IDs, distinct Slack trigger IDs, command aliases, prompt limits, identifier limits, and canonical Linear issue identifiers;
- exact manifest scope set, duplicate-scope rejection, secret-literal exclusion, and the runtime-to-manifest `usergroups.list` dependency.

These tests run in the normal pinned CI lane through formatting, Clippy with warnings denied, and `cargo test --all-targets --locked`.

## Chromium security lane

`.github/workflows/slack-ingress-browser-security.yml` builds and starts the production `fiducia-slack-command` binary with synthetic, loopback-only test configuration. Headless Chromium then sends real browser requests to the production routes and verifies:

- readiness reports dry-run mode and installed-app identity enforcement;
- missing and forged signatures return `401`;
- missing and stale timestamps return `401`;
- a correctly signed request from a different app returns `403`;
- endpoint/payload provider confusion returns `400`;
- duplicate keys that collide after percent decoding return `400`;
- hostile HTML is not reflected as executable markup;
- browser traffic to non-loopback hosts is blocked by the test harness.

The lane has `contents: read`, uses no live external credentials, and runs one supervised service process with a private state directory and bounded timeout. It is complementary to the cross-repository canary in `ORESoftware/k8s-cluster`, which exercises the bridge, coordinator, PostgreSQL, Slack API double, and browser together.

## Event-callback ingress (`fiducia-slack-bridge`)

The slash-command service above is not the only Slack intake. The
`fiducia-slack-bridge` binary receives Events API callbacks on:

```text
POST /slack/events
```

It enforces the same signed-request contract — exact `v0` HMAC over the
unmodified body, a ±300 second replay window, canonical 64-character signature
decoding, and fail-closed rejection before any downstream read or write. On top
of the shared contract it applies:

- **Installed-application identity.** `SLACK_EXPECTED_APP_ID` pins the reviewed
  install. A signed `event_callback` whose `api_app_id` is absent or belongs to
  another application is rejected before channel policy, journaling, bridge
  dispatch, or any Slack post. The variable is optional for a loopback bind
  (local tests and sidecars) and **required** for a non-loopback bind, matching
  `SLACK_EXPECTED_APP_ID`/`SLACK_EXPECTED_TEAM_ID` on the command service.
- **Workspace, channel, and thread allowlists**, plus bot/self-authored event
  suppression so the adapter cannot loop on its own replies.
- **Deterministic single delivery.** Slack retries the same `event_id`; the
  durable journal claims it once so a retry cannot fan out a second workflow.

`/readyz` reports `dry_run` and `installed_app_identity_enforced` so a deployment
gate can assert the boundary before activation.

### Admission ordering

Duplicate detection runs **before** the concurrency reservation:

```text
signature -> workspace/channel policy -> installed-app identity
  -> duplicate check (read-only)      <- answers 200 duplicate at any load
  -> capacity reservation             <- answers 503 only for genuinely new work
  -> authoritative claim              <- admits exactly once
```

Slack retries on `503`. If a retry of an already-claimed delivery were answered
`capacity_exceeded`, the response would invite another delivery of work that is
already claimed — retry amplification exactly when the service is saturated. The
duplicate check therefore commits nothing and reserves nothing, so idempotency
holds at any ceiling, including one. The authoritative claim still happens under
the permit, so a genuinely new delivery racing that read is admitted exactly
once, and a shed delivery leaves no journal claim for work that never ran.

### Read-only status lookups

```text
<prefix> status <event-id>
```

Resolves a prior delivery from the durable journal and returns its state,
workflow ID, posted agents, and last update. It claims nothing, reserves no
capacity, and starts no workflow, so it stays answerable while the service is at
its ceiling. An unknown delivery reports `state: "unknown"` rather than failing;
a target that is not a valid event identifier is rejected rather than looked up.
`status` is matched only as the first token, so prose that merely mentions the
word is still routed as work.

### Provider selection

```text
<prefix> --model <claude|chatgpt|both> <task>
```

Optional and defaulting to `both`, so an unqualified command behaves exactly as
it did before the flag existed. `chatgpt` also accepts `openai` and `gpt`. An
unrecognized provider is refused rather than silently defaulted, and a flag with
no task is not a request.

The flag is recognized **only at the front of the command**, so it cannot be
smuggled in from untrusted channel text quoted later in a prompt.

Selection drives every downstream count — requested agent keys, expected
submissions, and startup-failure replies — rather than each site assuming a
pair. Two consequences worth stating:

- a single-provider request is dispatched as a `single` workflow, not a
  `competitive` one, because competitive mode compares submissions against each
  other and that is meaningless with one participant;
- the workflow creator is drawn from the selected agents, so a ChatGPT-only
  request is not attributed to a Claude agent that is not running.

### Cancellation

```text
<prefix> cancel <event-id>
```

Records cancellation intent in the durable journal. The worker observes it at
the next stage boundary and stops cooperatively — nothing is aborted mid-write,
so the journal is never left describing a partially applied step. Like `status`,
it reserves no capacity, so a run can be stopped precisely when the service is
saturated and stopping it is most useful.

Intent is journaled rather than held in memory, so it survives a restart: a
worker resuming after a crash still sees that the run was canceled instead of
quietly finishing it. `Canceled` is a terminal state distinct from `Completed`,
so a stopped run is never mistaken for one that delivered its submissions.

A cancel against an already-terminal run reports `already_terminal` with the
state; an unknown run reports `unknown`. Neither invents a journal entry.
Cancellation stops further work — it does not retract submissions already posted
to the thread.

### Operational metrics

```text
GET /metrics
```

Renders Prometheus text with one counter, `slack_bridge_requests_total`, labelled
by terminal outcome. Every outcome is declared up front so an outcome that has
not occurred yet still scrapes as `0` — a series that only appears after its
first occurrence cannot be alerted on, because its absence is indistinguishable
from the absence of scraping.

`rejected_app_identity` is counted separately from `rejected_policy`: a non-zero
rate means a foreign application is posting correctly signed events to this
endpoint, which is a security signal rather than a malformed-payload signal, and
is invisible when folded into a generic rejection counter.

The label set is a closed list of internal outcome names. No Slack workspace,
channel, user, application identifier, prompt text, or channel content appears
in a scrape, and the Chromium lane asserts that. The endpoint is unauthenticated
and intended for a private scrape path; it must not be exposed publicly
alongside the signed Slack route.

### Deployment exposure contract

`fiducia-slack-bridge` is a **third workload**, distinct from the two that are
easy to confuse it with. All three build from this one source tree:

| Binary | Port | `/metrics` |
|---|---|---|
| `fiducia-ai-agent-bridge` | `8142` | pre-existing, rendered from `crate::metrics` |
| `fiducia-slack-command` | — | none |
| `fiducia-slack-bridge` | `8150` | the outcome counters described above |

A NetworkPolicy that scopes the bridge's metrics port therefore does **not**
cover this service. Any manifest for the Events API ingress needs its own:

- `/slack/events` is the only route that may be reachable externally;
- `/metrics`, `/healthz`, and `/readyz` belong on a private path, with
  `/metrics` scoped to the metrics scraper;
- the service binds `127.0.0.1` by default, so serving in a pod requires
  `SLACK_BRIDGE_HOST=0.0.0.0` — which is exactly the moment `/metrics` stops
  being protected by loopback and the policy above starts carrying the weight;
- `SLACK_EXPECTED_APP_ID` is **required** for any non-loopback bind and the
  service refuses to start without it, so a pod-networked deployment cannot run
  without the installed-application identity check.

When pinning images by digest, read the `image-digest-<target>` evidence
artifact from the `container images` run for the exact source SHA. This
repository merges several times a day and every merge that touches `src/**`
rebuilds all four images, so a digest copied by hand goes stale quickly.

### Request-URL handshake

Slack's Events API request-URL handshake posts only `token`, `challenge`, and
`type`. It is sent while the Request URL is being configured, before the endpoint
is bound to a workspace, so no `team_id` is available to match. The handshake is
therefore accepted on a valid signature alone and echoes only the challenge
value. When Slack does supply a workspace it is still held to
`SLACK_ALLOWED_TEAM_IDS`.

### Chromium security lane

`.github/workflows/slack-bridge-events-browser-security.yml` builds and starts
the production `fiducia-slack-bridge` binary with synthetic, loopback-only
dry-run configuration, then drives real Chromium requests against
`/slack/events` to verify:

- readiness reports dry-run mode and installed-app identity enforcement;
- missing and forged signatures return `401`;
- missing, stale, and far-future timestamps return `401`;
- the request-URL handshake completes and echoes only the challenge;
- a handshake naming an unapproved workspace is ignored, not echoed;
- a correctly signed event from another `api_app_id`, or with none, returns `400`;
- unapproved workspaces and channels are ignored rather than accepted;
- bot-authored events are ignored so the adapter cannot loop;
- hostile channel text is never reflected as executable markup;
- a retried delivery is claimed exactly once;
- malformed JSON carrying a valid signature returns `400`;
- a retry is still recognized as a duplicate with the concurrency ceiling at one;
- a status lookup resolves a known delivery while the service is saturated,
  reports `unknown` for an unseen one, and refuses an unusable identifier;
- a cancel reports `already_terminal` for a finished run and `unknown` for an
  unseen one, invents no journal entry, and refuses an unusable identifier;
- each `--model` value is accepted, an unknown provider and a flag with no task
  are refused, and the flag is not honoured mid-prompt;
- `/metrics` renders a zero series for every declared outcome and leaks no Slack
  workspace, channel, application identifier, or prompt text.

## Production activation checklist

Before turning off `SLACK_COMMAND_DRY_RUN`:

1. reconcile and validate the complete remote app manifest;
2. reinstall the app after the `usergroups:read` grant and confirm all six commands appear;
3. set `SLACK_EXPECTED_APP_ID` and `SLACK_EXPECTED_TEAM_ID` to the installed immutable IDs;
4. source all secrets from the protected deployment secret path, never environment files committed to Git;
5. keep bridge and coordinator URLs on loopback or HTTPS and require bearer credentials for remote services;
6. verify the focused Rust and Chromium lanes on the exact release commit;
7. verify the current-main cross-repository browser canary;
8. retain dry-run mode until the channel registry, repository allowlist, Linear routing, cancellation, status projection, and incident rollback are reviewed;
9. rotate any credential exposed in chat, issue text, logs, or other untrusted history before activation.

## Incident response

If signature verification, identity matching, authorization, journaling, or downstream dispatch behaves unexpectedly:

- disable the public route or set the deployment to dry-run;
- rotate the affected Slack or service credential;
- preserve metadata-only request IDs, run IDs, commit SHAs, and check URLs;
- do not copy raw tokens, signing secrets, private prompts, or channel history into GitHub, Slack, Linear, or retained CI artifacts;
- reconcile the durable Linear issue and GitHub PR before resuming writes.
