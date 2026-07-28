# Scoped TCP identities and state-layer context namespaces

The TCP JSONL transport uses the same secret-backed `WORKFLOW_ADAPTER_AUTH_JSON`
document as scoped HTTP. A connection authenticates once and retains one principal
for its lifetime:

- `operator`: the global `API_AUTH_BEARER`, with administrative compatibility;
- `adapter`: one credential-bound `agent_key`, `token_id`, and explicit scopes;
- `open`: compatibility mode only when neither global nor scoped credentials exist;
- unauthenticated: may use `ping` and `auth` only.

A successfully authenticated connection cannot switch to a different credential or
principal. Re-authenticating with the same principal is harmless; attempting to
rotate an established socket to another token returns `principal_switch_denied`.
Credential rotation remains available by opening a new connection with either
overlapping credential.

## TCP operation scopes

| Operation | Required adapter scope | Bound identity |
|---|---|---|
| `register` | `agent:register` | `agent_key` |
| `list_channels`, `search`, `members`, `history`, `subscribe` | `channel:read` | optional subscription `agent_key` |
| `create_channel`, `resolve` | `channel:create` | `created_by` |
| `join`, `leave` | `channel:join` | `agent_key` |
| `post` | `channel:post` | `from` |
| `get_context` | `context:read` | none |
| `set_context` | `context:write` | `updated_by` |

A missing scope or identity mismatch rejects only that frame; the valid connection
remains usable for its other capabilities. Authentication responses expose the
principal kind and adapter key only. They never echo tokens, token IDs, or the full
credential document.

## Safe-by-default context APIs

Generic `AppState` context methods are now external-safe:

- `set_context` rejects `workflow.*` and `internal.*`;
- `get_context` filters all reserved entries;
- `get_context_key` rejects a reserved key.

Internal orchestration and blind-competition code uses explicit crate-internal
methods:

- `set_context_internal`;
- `get_context_internal`;
- `get_context_key_internal`.

This is a state-layer boundary, not merely a transport filter. Generic HTTP, TCP,
and direct state callers cannot overwrite or retrieve workflow plans, submissions,
blind candidates, reveal records, or other internal coordination state. HTTP still
retains its outer reserved-namespace check as defense in depth.

## HTTP scope completion

The same credential document now recognizes `channel:create`, `context:read`, and
`context:write`. Scoped HTTP access covers channel creation/resolution, channel
listing/search/read/stream, and context read/write in addition to the previously
merged workflow, agent, message, join, and lease routes.

## Rotation and deployment

Two enabled credentials may share an `agent_key` when their token IDs and token
material differ. Existing sockets keep their original principal. New sockets may
use either token during the overlap window. Revoke the old credential only after
old connections have drained or been terminated.

Keep `WORKFLOW_ADAPTER_AUTH_JSON`, `API_AUTH_BEARER`, and all token material in the
approved secret manager. Do not place them in flags, process arguments, URLs,
Linear, GitHub comments, logs, traces, or protocol error frames.
