# Multi-model workflow coordination

The bridge coordinates model adapters without embedding vendor credentials or
vendor-specific HTTP clients in the conversation-bus process. Codex, Claude,
Gemini, Kimi, Qwen, and future adapters register as ordinary agents, advertise
capabilities in `agent.meta.capabilities`, subscribe over SSE or TCP, and submit
results through the workflow API.

This keeps three boundaries explicit:

1. **Conversation and workflow state** live in the bridge's existing channels,
   versioned context, messages, and optional PostgreSQL mirror.
2. **Model execution** lives in independently deployed adapters. Direct provider
   calls, credentials, budget controls, and vendor-specific retries belong there
   (tracked separately in Linear DEN-173).
3. **Edit ownership** stays authoritative in Fiducia through the existing fenced
   `/file-leases/acquire` and `/file-leases/release` API. A workflow plan describes
   the exact repository paths to lease; it does not create a second lock system.

## Modes

| Mode | Execution contract |
|---|---|
| `single` | Exactly one worker receives the task. |
| `sequential` | Workers submit in phase order. The next worker is rejected until the previous phase has submitted. |
| `competitive` | Two or more workers submit independently in phase 0. One worker's failure does not block the others. |
| `consensus` | Two or more phase-0 workers submit proposals, then a distinct phase-1 reviewer synthesizes or selects the result. |

`compete` is accepted as a wire alias for `competitive`.

## Register adapters

```sh
curl -s localhost:8142/agents/register \
  -H "Authorization: Bearer $API_AUTH_BEARER" \
  -H 'content-type: application/json' \
  -d '{
    "agent_key":"gemini-rust-1",
    "display_name":"Gemini Rust worker",
    "kind":"gemini",
    "meta":{"capabilities":["rust","github","review"]}
  }'
```

Use a distinct `agent_key` per independently schedulable adapter instance. The
kind is a provider family, not a secret-bearing provider configuration.

## Create a workflow

```sh
curl -s localhost:8142/workflows \
  -H "Authorization: Bearer $API_AUTH_BEARER" \
  -H 'content-type: application/json' \
  -d '{
    "title":"Harden lease renewal semantics",
    "prompt":"Audit the renewal path, propose a patch, and include tests.",
    "created_by":"coordinator",
    "mode":"consensus",
    "agent_kinds":["codex","claude","gemini","kimi","qwen"],
    "required_capabilities":["rust"],
    "worker_count":3,
    "repository":"fiducia-cloud/fiducia-node.rs",
    "paths":["src/leases.rs","tests/lease_renewal.rs"],
    "require_file_leases":true,
    "lease_ttl_ms":30000,
    "meta":{"linear_issue":"DEN-171"}
  }'
```

The response contains the immutable `workflow.plan.v1`, derived status, and the
channel stream/message paths. Selection is deterministic by registered
`agent_key` unless `agent_keys` is supplied, in which case request order is
preserved.

The bridge caps a workflow at 30 assignments, leaving room in the 32-seat channel
for a separate coordinator and observer. Human agents are excluded from automatic
selection but may be named explicitly.

## Adapter loop

Each adapter performs this loop:

1. Register with a stable `agent_key`, provider `kind`, and capability list.
2. Poll `GET /workflows` or receive an out-of-band wake-up from its local runner.
3. Read `GET /workflows/{id}` and confirm the adapter is assigned and currently
   eligible according to `status`.
4. Subscribe to `/channels/{channel}/stream?agent_key={agent_key}` for live peer
   messages and lifecycle events.
5. When `workflow.plan.file_lease.required` is true, atomically acquire every exact
   path from `POST /file-leases/acquire`. Carry the returned fencing token through
   guarded work and fail closed if the lease cannot be acquired.
6. Execute the assigned model/tool work. Sequential workers may use prior channel
   submissions as bounded context; competitive workers should produce independent
   proposals; the consensus reviewer waits for every worker.
7. Submit through `POST /workflows/{id}/submissions`.
8. Release the entire union lease through `POST /file-leases/release` using the
   exact `agent_key` and fencing token.

The bridge publishes every accepted submission as an ordinary channel message
with `kind=workflow_submission`, so SSE and TCP subscribers see the result live.
Each assignment has one insert-only submission key. The orchestration module
serializes workflow inserts inside the single authoritative bridge process;
simultaneous or later duplicate submissions receive `400 bad_request` instead of
overwriting an accepted proposal or review.

## Submission

```sh
curl -s localhost:8142/workflows/WORKFLOW_ID/submissions \
  -H "Authorization: Bearer $API_AUTH_BEARER" \
  -H 'content-type: application/json' \
  -d '{
    "agent_key":"gemini-rust-1",
    "content":"Proposed patch and test rationale...",
    "meta":{"branch":"agent/gemini/lease-renewal","tests":["cargo test lease"]}
  }'
```

For `sequential`, only the first pending worker can submit. For `consensus`, the
reviewer receives `awaiting_review` only after every worker has submitted.
`single` and `competitive` complete when all worker assignments have submissions.

## Failure and durability semantics

- A provider adapter crashing does not crash the bridge. An assignment remains
  pending until a submission is accepted and can be retried by restarting the
  adapter with the same `agent_key`.
- A competitive worker failing does not erase successful peer submissions.
- Workflow plans and submissions use existing channel context. They are in-memory
  by default and restored with the existing optional PostgreSQL feature. The
  orchestration routes treat `workflow.*` keys as insert-only, but the generic
  context endpoint does not yet enforce a reserved namespace; deployments should
  grant the bridge bearer only to trusted adapters until state-level namespace
  enforcement is added.
- File leases are not inferred from workflow status. Fiducia remains the source of
  truth, and adapters must treat an expired or stale fencing token as loss of
  ownership.
- The bridge's external-control-plane renewal route is not implemented yet
  (Linear DEN-203). Until the authoritative Fiducia heartbeat contract lands,
  write assignments must finish and release within their acquired TTL; they must
  not continue writing after expiry or try to bridge an ownership gap by
  reacquiring after expiry.
- Workflow channels are not private competition sandboxes. Agents sharing the API
  bearer can read channel history; independent-solution behavior is a coordinator
  policy unless adapters are isolated behind separate authorization domains.

## Deliberate non-goals in this bridge layer

- storing provider API keys;
- calling OpenAI, Anthropic, Google, Moonshot/Kimi, or Alibaba/Qwen directly;
- pricing/budget accounting;
- merging Git branches automatically;
- replacing GitHub or Linear as the durable async system of record;
- replacing Fiducia's lock/lease authority.
