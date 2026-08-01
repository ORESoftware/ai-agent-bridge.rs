# alex-main-agent routing-manifest drift contract

Tracking: `DEN-1320`

## Why this contract exists

The Slack runtime registry and the project repositories answer different questions:

- `config/alex-main-agent.channels.json` is the runtime authority for stable Slack and Linear identifiers, repository allowlists, authorized principals, agent modes, write policy, and bounded concurrency, runtime, token, spend, and retry policy.
- `.github/alex-main-agent.json` in each project repository records the project-local routing identity, canonical review issues, callback/idempotency expectations, organization boundary, and explicit temporary-target exceptions.

Neither layer may silently replace the other. A project pull request can move to a different head, close without promotion, or change a routing field after the central registry was reviewed. The lock file binds both layers to one auditable state.

## Files

- `config/alex-main-agent.manifests.lock.json`: exact repository pull-request heads plus canonical SHA-256 digests of the thirteen project manifests.
- `scripts/audit_alex_main_agent_manifests.py`: dependency-free fail-closed validator and read-only GitHub verifier.
- `tests/test_audit_alex_main_agent_manifests.py`: positive and mutation tests.
- `.github/workflows/alex-main-agent-manifest-drift.yml`: offline validation on every change and remote read-only verification on pull requests, pushes, schedules, and manual runs.

## What is locked

Each entry records:

- repository, pull request, expected state, base ref, head ref, and exact head SHA;
- canonical manifest SHA-256 rather than a formatting-sensitive byte hash;
- workspace, app, channel, Linear project, routing issue, and delivery issue identity;
- GitHub organization and repository identity;
- the central Linear project UUID and runtime repository target;
- explicit Daedalus typo-channel rejection;
- explicit MemeBank legacy-target and Voxletra temporary-target markers.

The central registry is bound by a canonical SHA-256 over strict duplicate-key-free JSON. Whitespace-only changes do not require lock churn; any semantic policy or routing change does.

## Fail-closed behavior

The audit rejects:

- duplicate JSON keys, unknown fields, oversized files, malformed identifiers, or duplicate channel/repository/issue identities;
- a central binding missing from the lock or a locked project missing from the central registry;
- moved pull-request heads, changed base/head refs, repository escape, unexpected closure, or unrecorded merge promotion;
- changed project manifests, removed callbacks or idempotency, organization escape, or secret-redaction weakening;
- mapping the misspelled `#daadalus-fab` channel;
- removal of the explicit temporary/legacy target markers and their canonical bootstrap issues.

The remote audit reads only pull-request metadata and one manifest file at each immutable commit. It never reads Slack messages, prompts, Linear bodies, repository source trees, or credentials. Its report contains only stable routing identifiers and digests.

## Commands

Offline validation:

```sh
python3 -m py_compile scripts/audit_alex_main_agent_manifests.py tests/test_audit_alex_main_agent_manifests.py
python3 -m unittest -v tests/test_audit_alex_main_agent_manifests.py
python3 scripts/audit_alex_main_agent_manifests.py \
  --registry config/alex-main-agent.channels.json \
  --lock config/alex-main-agent.manifests.lock.json \
  --report artifacts/alex-main-agent-manifest-audit.json
```

Read-only remote verification with the deliberately provisioned cross-organization credential:

```sh
GITHUB_TOKEN="${ALEX_MAIN_AGENT_MANIFEST_AUDIT_TOKEN:-}" \
python3 scripts/audit_alex_main_agent_manifests.py \
  --registry config/alex-main-agent.channels.json \
  --lock config/alex-main-agent.manifests.lock.json \
  --report artifacts/alex-main-agent-manifest-audit.json \
  --remote
```

The workflow does not reuse its repository-scoped `GITHUB_TOKEN`: several locked repositories are outside the current installation boundary and correctly return `404`. Configure the `ALEX_MAIN_AGENT_MANIFEST_AUDIT_TOKEN` Actions secret with read-only metadata, contents, and pull-request access across all thirteen repositories. Until that credential exists, CI records the remote audit as `blocked` while still enforcing the full offline schema, digest, mutation, and central-registry checks. The credential is never printed or included in the report.

## Promotion procedure

When a project routing pull request changes:

1. Review the project manifest and the central runtime policy together.
2. Update the central registry first when stable identifiers, repository targets, principals, modes, write policy, or budgets change.
3. Record the reviewed project pull-request head and canonical manifest digest in the lock.
4. Run offline tests and the remote audit.
5. Preserve the same entry after merge by changing `expected_state` to `merged`; do not delete provenance merely because the project manifest reached its default branch.
6. Update temporary-target entries only after the canonical repository exists and has its own reviewed routing manifest.

A green offline drift audit is configuration evidence only. It does not prove exact remote head freshness, Slack deployment, provider execution, Linear lifecycle projection, GitHub write authorization, or successful end-to-end work delivery. Production activation remains blocked until the remote audit is verified with the cross-organization read credential.
