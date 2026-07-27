# Blind multi-model competition

Blind competition is an explicit API separate from collaborative `competitive`
workflows. It is intended for independently generated candidate solutions that
must not influence one another before judging.

## Contract

1. A coordinator creates `POST /blind-workflows` with an immutable prompt, at
   least two distinct registered workers, and one distinct registered reviewer.
2. Every worker reads the same plan and submits exactly once to
   `POST /blind-workflows/{id}/submissions` using its scoped adapter identity.
3. Candidate records are append-only context values under an internal
   `workflow.blind.*` namespace. A duplicate ordinal is rejected rather than
   overwritten.
4. Before reveal:
   - a worker can read only its own candidate;
   - another worker, the reviewer, and an operator receive no candidate content;
   - status and hidden counts still show whether the competition is progressing;
   - the shared channel receives only redacted lifecycle receipts.
5. The designated reviewer calls `POST /blind-workflows/{id}/reveal` only after
   every worker has submitted. No operator override is accepted by this endpoint.
6. After reveal, authorized workflow readers receive the complete immutable,
   clearly attributed candidate set.

## Scoped authorization

The outer workflow-security boundary applies the existing scopes:

- coordinator: `workflow:create` and usually `workflow:read`;
- worker: `workflow:submit` and usually `workflow:read`;
- reviewer: `workflow:read`.

The reveal handler additionally requires the authenticated adapter identity to
match the plan's designated reviewer. Possession of the global operator bearer is
not enough to reveal candidates.

## Example

```sh
curl -s localhost:8142/blind-workflows \
  -H 'authorization: Bearer <coordinator credential>' \
  -H 'content-type: application/json' \
  -d '{
    "title":"Compare fenced lease implementations",
    "prompt":"Produce an independent Rust implementation and test plan.",
    "created_by":"coordinator",
    "worker_agent_keys":["codex-rust","claude-rust","gemini-rust"],
    "reviewer_agent_key":"qwen-reviewer"
  }'
```

## Security boundaries and remaining work

- Candidate content is kept out of the shared message channel before and after
  reveal; authorized readers obtain it from the workflow view.
- The current isolation boundary applies to HTTP adapters using scoped identity.
  TCP JSONL still uses the global bearer and must not be used for blind candidate
  retrieval or submission until DEN-281 adds per-connection capabilities.
- This increment provides reveal, attribution, failure preservation through
  append-only records, and deterministic ordering. Reviewer scoring/synthesis,
  early termination policy, artifact ACLs, and provider usage accounting remain
  separate orchestration work.
