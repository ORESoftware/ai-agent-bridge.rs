# Provider workflow runner

`fiducia-ai-agent-runner` is the execution process paired with the conversation
bridge. The bridge owns workflow state and live communication; the runner owns
provider credentials and outbound model calls.

## Execution loop

1. Parse `AI_PROVIDER_CONFIG_JSON` and resolve each configured credential from
   its named environment variable.
2. Register every provider as a bridge agent using the provider config `name` as
   its stable `agent_key`.
3. Poll `GET /workflows` and select pending assignments for those agent keys.
4. For file-scoped assignments, acquire the exact Fiducia path union before any
   provider call.
5. Execute the configured provider protocol.
6. Submit either the successful result or a redacted provider failure to
   `/workflows/{id}/submissions`.
7. Release the complete Fiducia union lease using the returned fencing token.

The runner does not execute model-requested tools and does not edit a repository
by itself. Its output is a workflow submission for a separate Git/GitHub worker
or human reviewer. This keeps untrusted model text outside the runner process's
command-execution boundary.

## Required environment

| Variable | Purpose |
|---|---|
| `AI_PROVIDER_CONFIG_JSON` | Provider protocol configuration; required. |
| Provider-specific key variables | Named by each config's `api_key_env`. |

## Runner environment

| Variable | Default | Purpose |
|---|---:|---|
| `AI_AGENT_RUNNER_BRIDGE_URL` | `http://127.0.0.1:8142/` | Bridge REST base URL. Remote URLs require HTTPS. |
| `AI_AGENT_RUNNER_BRIDGE_BEARER` | falls back to `API_AUTH_BEARER` | Bridge credential; required for remote bridges. |
| `AI_PROVIDER_CAPABILITIES_JSON` | `{}` | Object mapping provider agent keys to capability arrays. |
| `AI_AGENT_RUNNER_POLL_INTERVAL_MS` | `5000` | Workflow polling cadence, minimum 250 ms. |
| `AI_AGENT_RUNNER_MAX_CONCURRENCY` | `4` | Maximum simultaneous provider calls, capped at 64. |
| `AI_AGENT_RUNNER_MAX_OUTPUT_TOKENS` | `4096` | Requested output-token limit, capped at 65,536. |
| `AI_AGENT_RUNNER_LEASE_SAFETY_MARGIN_MS` | `15000` | Required time remaining beyond provider timeout. |
| `AI_AGENT_RUNNER_BRIDGE_TIMEOUT_SECS` | `30` | Bridge request timeout. |
| `AI_AGENT_RUNNER_MAX_BRIDGE_RESPONSE_BYTES` | `8388608` | Bounded bridge response size, capped at 32 MiB. |

Example capabilities:

```sh
export AI_PROVIDER_CAPABILITIES_JSON='{
  "openai-codex": ["rust", "github", "review"],
  "claude": ["rust", "architecture", "review"],
  "gemini": ["research", "review"],
  "kimi": ["long-context"],
  "qwen": ["rust", "review"]
}'
```

## Start

```sh
cargo run --locked --bin fiducia-ai-agent-runner
```

The process registers providers before its first poll and fails startup when a
configured provider key is missing. Provider configuration therefore describes
only workers that are actually available.

## Scheduling and duplicate execution

The workflow service rejects duplicate submissions. This runner also maintains a
local in-flight set and a bounded semaphore, so one process does not execute the
same assignment twice.

This is not a distributed assignment claim. Deploy one active runner replica
until a Fiducia-backed assignment claim primitive is added. File-scoped work is
still protected across processes by exact-path Fiducia leases, but non-file
provider calls could be duplicated by multiple runner replicas.

## Lease safety before DEN-203

External lease renewal is not yet implemented. For a required file lease, the
runner verifies:

```text
lease TTL >= provider timeout + AI_AGENT_RUNNER_LEASE_SAFETY_MARGIN_MS
```

Unsafe assignments remain pending and are not sent to the provider. A successful
or failed provider call is submitted before release. Release failures are logged
without leaking credentials, and the runner waits for authoritative TTL expiry.

This rule prevents the runner from knowingly continuing work after its fencing
window. DEN-203 remains responsible for renewable heartbeat support.

## Context rules

- `competitive` workers do not receive peer outputs, preserving independent
  proposals within the limits of the shared channel authorization model.
- `sequential` workers receive earlier accepted submissions in ordinal order.
- `consensus` workers remain independent; the reviewer receives every accepted
  worker submission.
- Prior context is bounded to 512 KiB.
- Provider errors are submitted with `status=failed` and a redacted error string,
  preserving both successes and failures in competitive workflows.

## Security boundaries

- Provider and bridge redirects are disabled.
- Remote bridge and provider connections require HTTPS.
- Credentials remain environment-only and are never included in submission meta.
- Bridge and provider response bodies are bounded before JSON parsing.
- Provider HTTP error bodies are not logged.
- The runner cannot execute shell commands or model-requested tools.
