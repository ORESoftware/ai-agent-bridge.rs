# Scoped HTTP authentication for workflow adapters

`WORKFLOW_ADAPTER_AUTH_JSON` optionally defines least-privilege HTTP credentials
for managed model adapters. Each credential binds one or more overlapping rotation
tokens to one `agent_key` and a bounded scope set. The global `API_AUTH_BEARER`
remains the operator/admin credential.

```json
{
  "credentials": [
    {
      "token_id": "codex-2026-07-v2",
      "token": "secret-from-the-runtime-secret-store",
      "agent_key": "codex-rust-1",
      "scopes": [
        "agent:register",
        "workflow:read",
        "workflow:submit",
        "channel:post",
        "channel:read",
        "lease:operate"
      ]
    }
  ]
}
```

The JSON belongs in a secret environment variable. Never place it in a ConfigMap,
command line, Git repository, log message, URL, workflow record, or model prompt.
Multiple enabled credentials may target the same `agent_key`; add the replacement
credential, roll the deployment, switch the adapter, then remove the old token in
a later roll for zero-service-downtime rotation.

## Enforced HTTP rules

- Scoped tokens can only call routes represented by their explicit scopes.
- Identity-bearing request fields (`agent_key`, `created_by`, or `from`) must equal
  the authenticated adapter identity.
- After verification, the outer boundary rewrites the request to the internal
  global bearer so the existing inner fail-closed auth remains intact.
- Generic HTTP context writes cannot use `workflow.*` or `internal.*`, including
  when the caller holds the operator bearer. Workflow-owned code writes those
  records directly inside the process.
- Credential tokens are compared in constant time and are never included in
  response bodies or logs.

## Compatibility and remaining work

When no scoped credential document is configured, existing global-bearer behavior
continues, but reserved HTTP context namespaces are still blocked. This increment
covers HTTP adapters. The TCP JSONL transport continues to use the global bearer;
per-connection TCP capabilities and state-layer namespace enforcement remain
required before DEN-281 can be completed.
