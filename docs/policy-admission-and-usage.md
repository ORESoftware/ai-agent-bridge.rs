# Policy admission and cumulative provider usage

Managed provider execution is fail-closed behind one durable admission per
workflow. The bridge evaluates the versioned policy engine server-side; a runner
cannot upload a previously approved decision or replace an existing admission.

## Admission API

```text
GET  /workflows/{id}/admission
POST /workflows/{id}/admission
POST /workflows/{id}/admission/usage
POST /workflows/{id}/admission/complete
POST /workflows/{id}/admission/cancel
```

The create request includes the raw `PolicyRequest`, runner identity, optional
human approver, and optional override reason. The bridge verifies that:

- `requested_mode` exactly matches the immutable workflow plan;
- repository-write intent matches the workflow's required Fiducia lease;
- required capabilities exactly match the plan;
- provider candidates exactly match workflow assignments;
- policy evaluation is allowed;
- policy-selected providers exactly match the assignments;
- a human approver is present whenever policy requires one.

The admission snapshot stores policy version, mode, selected provider keys, all
hard ceilings, approval and Fiducia requirements, reasons, requester, optional
approver/override, status, usage totals, and terminal reason under the workflow's
persisted `workflow.admission.v1` context key.

Repeated create requests return the existing record and never change its budget or
selection.

## Hard usage accounting

The runner reports cumulative deltas for:

- input and output tokens;
- provider cost in micro-USD;
- retry count;
- provider-call count;
- elapsed wall-clock milliseconds;
- peak concurrency.

The bridge computes the attempted total with checked arithmetic before committing
it. It also derives a provider-call ceiling from providers × rounds ×
(retries + 1). If any attempted increment exceeds a ceiling, the current accepted
totals remain unchanged, the rejected delta is recorded, and the admission becomes
terminally `exhausted`. Later usage or completion is rejected.

A runner identity (`updated_by`) and the provider whose usage is being attributed
(`provider_agent_key`) are distinct. Scoped authentication binds the request to the
runner identity; the admission store separately requires the provider to be in the
policy-selected set.

## Provider-runner sequence

Before a vendor call, the runner:

1. loads or creates the workflow admission;
2. skips all work unless the admission is active and the provider is selected;
3. acquires the distributed assignment claim and any required file lease;
4. clamps `max_output_tokens` to the current remaining output-token budget;
5. conservatively reserves one token per prompt/system byte plus framing overhead,
   the full clamped output-token allowance, fixed call reserve, token-rate cost,
   provider-call count, and phase concurrency in one atomic admission update.

Concurrent providers therefore cannot all spend the same remaining output or cost
budget. A failed call keeps its conservative reservation; budgets are never
refunded after an external request has started.

After the vendor attempt, the runner:

1. extracts common OpenAI, Anthropic, Gemini, Kimi, or Qwen token fields;
2. uses provider-reported micro-USD cost when present, otherwise computes cost from
   `AI_PROVIDER_PRICING_JSON`;
3. reports only actual input/output/cost above the conservative reservation plus
   elapsed time before accepting the output;
4. discards output and releases both leases if accounting is rejected or cannot be
   proven;
5. validates the assignment claim again before submission;
6. submits the result and uses the authoritative updated workflow response;
7. completes the admission only when that response reports workflow completion;
8. cancels the admission on heartbeat loss, stale claim, reservation failure, or
   post-execution submission failure.

The runner now consumes the workflow response returned by the submission endpoint;
it no longer attempts to deserialize a nonexistent top-level `submission` field.

## Provider pricing

`AI_PROVIDER_PRICING_JSON` is required for every configured provider:

```json
{
  "codex": {
    "input_micro_usd_per_million": 1000,
    "output_micro_usd_per_million": 4000,
    "fixed_call_reserve_micro_usd": 5000,
    "max_context_tokens": 200000
  }
}
```

`fixed_call_reserve_micro_usd` covers non-token charges and any vendor-specific
minimum or safety reserve. The legacy key `estimated_call_cost_micro_usd` is
accepted as an alias. Token-rate cost for the conservatively reserved input and
maximum output is added before the call. If a provider reports a cost above that
reservation, the overage is checked after the response and the output is discarded
when it exhausts the budget.

Rates and reserves are configuration, not provider API credentials, but should
remain environment-only because they may encode commercial terms. Missing provider
pricing fails runner startup. Missing token usage after a provider response causes
the output to be discarded because cost and token ceilings cannot be proven.

## Scoped credentials

The runner credential may use:

- `workflow:read` to list and inspect workflows/admissions;
- `workflow:admit` to create the insert-only admission;
- `workflow:usage` to report usage and terminal transitions;
- existing registration, submission, and lease scopes required by the runner.

`requested_by` and `updated_by` must equal the scoped credential's `agent_key`.
Provider usage remains separately attributed through `provider_agent_key`.

## Safety invariants

- No managed vendor call starts without an active admission and an atomic pre-call
  token, cost, provider-call, and concurrency reservation.
- No provider output is submitted before actual token, cost, and elapsed usage is
  accepted.
- A one-unit overage terminally exhausts the admission; it is not a warning.
- Admission exhaustion, cancellation, lease loss, stale claim, unpriced usage, and
  submission failure all prevent repository writes and result acceptance.
- Admission records contain no provider credentials, bearer tokens, URLs, prompts,
  or raw provider error bodies.
