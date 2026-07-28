# Workflow execution policy

`POST /workflow-policy/explain` evaluates a task before provider execution and
returns a deterministic, versioned decision. The endpoint is side-effect free: it
does not create a workflow, acquire a lease, or call a model provider.

The decision records:

- `single`, `sequential`, `competitive`, or `consensus` mode;
- selected provider/model identities and worker/reviewer roles;
- hard provider, round, wall-clock, token, retry, concurrency, and cost ceilings;
- whether human approval and a Fiducia repository-path lease are mandatory;
- the degradation rule and human-readable policy reasons;
- the exact policy version used.

## Safety rules

- Restricted data and critical work fail closed unless at least three eligible,
  explicitly trusted providers are available for reviewer-backed consensus.
- Security, secrets, authentication, authorization, and cryptography capabilities
  raise the effective risk to at least `high`.
- Repository writes raise the effective risk to at least `medium`, require a
  Fiducia lease, and may require human approval.
- A requested mode may increase review rigor but cannot reduce the policy minimum.
- Caller-supplied budgets are ceilings, never requests to exceed the policy
  profile. The engine also applies absolute service caps.
- Provider selection is deterministic: health, latency, estimated cost, and
  stable `agent_key` break ties in that order.

## Example

```sh
curl -s localhost:8142/workflow-policy/explain \
  -H 'content-type: application/json' \
  -d '{
    "task_risk":"high",
    "data_sensitivity":"confidential",
    "required_capabilities":["rust","security"],
    "requires_repository_write":true,
    "requested_budget":{"max_cost_micro_usd":25000000},
    "providers":[
      {"agent_key":"codex","kind":"codex","model":"codex","capabilities":["rust","security"]},
      {"agent_key":"claude","kind":"claude","model":"claude","capabilities":["rust","security"]},
      {"agent_key":"gemini","kind":"gemini","model":"gemini","capabilities":["rust","security"]}
    ]
  }'
```

The next integration step is to require an allowed policy decision when creating a
managed workflow and to persist the policy version, reasons, estimates, overrides,
and final usage in the workflow audit record. Provider adapters must enforce the
returned ceilings locally as well; a bridge decision is not permission to exceed a
provider or organization-level quota.
