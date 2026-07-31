# Slack channel to Linear project bindings

This document defines the first bounded implementation slice of DEN-1042: a strict, auditable registry that resolves an authenticated Slack workspace/channel request to one Linear project, repository allowlist, agent-mode policy, write policy, and budget.

The registry is deliberately independent of channel names, project names, Slack history, provider credentials, and the private implementation details of the Linear Slack app. It uses immutable Slack and Linear identifiers only.

This slice does **not** expose `/agent`, process app mentions or shortcuts, enqueue a run, write to Linear, create a branch, or deploy a Slack app. The existing authenticated DEN-766 Slack adapter continues to own request signature verification and dual-model thread delivery. A later DEN-1042 slice must connect accepted Slack commands to this resolver before any provider or tool invocation.

## Registry schema

```json
{
  "schema_version": 1,
  "bindings": [
    {
      "workspace_id": "T012345",
      "channel_id": "C012345",
      "linear_team_id": "eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc",
      "linear_team_key": "DEN",
      "linear_project_id": "3abf3f94-6ce2-489d-a810-344f010aa068",
      "default_repository": "ORESoftware/ai-agent-bridge.rs",
      "repository_allowlist": [
        "ORESoftware/ai-agent-bridge.rs",
        "ORESoftware/ai-agent-coordinator.rs"
      ],
      "default_agent_mode": "both-parallel",
      "allowed_agent_modes": [
        "claude",
        "chatgpt",
        "both-parallel",
        "review"
      ],
      "allowed_user_ids": ["U012345"],
      "allowed_user_group_ids": ["S012345"],
      "write_policy": "draft_pull_request",
      "budget_policy": {
        "max_concurrent_runs": 2,
        "max_runtime_secs": 900,
        "max_tokens": 100000,
        "max_spend_cents": 1000,
        "max_retries": 2
      },
      "updated_by": "UADMIN",
      "updated_at": "2026-07-31T06:00:00Z"
    }
  ]
}
```

Unknown fields fail parsing. Schema versions other than `1` fail closed. A workspace/channel pair can appear only once, so a request can never resolve to two competing default projects.

## Validation

Every binding must satisfy all of the following:

- workspace, channel, Linear team/project, updater, user, and user-group values are stable identifier-shaped values without whitespace or control characters;
- `linear_team_key` is an uppercase Linear key such as `DEN`;
- `updated_at` is RFC 3339;
- repositories use canonical `owner/name` form rather than URLs or `.git` suffixes;
- the default repository appears in the repository allowlist;
- at least one user or user group is authorized;
- at least one agent mode is allowed and the default mode appears in that set;
- all concurrency, runtime, token, spend, and retry ceilings are nonzero where required and within hard implementation limits.

Repository names are normalized case-insensitively for comparison. The resolver never expands an allowlist from a display name or untrusted prompt text.

## Resolution contract

An already authenticated Slack request supplies:

- workspace ID;
- channel ID;
- invoking user ID;
- verified user-group IDs;
- optional requested repository;
- optional requested agent mode;
- requested capability: read-only, Linear write, or repository write;
- optional Linear issue identifier such as `DEN-1042`.

The resolver then:

1. finds exactly one workspace/channel binding;
2. verifies the user directly or through an explicitly allowed user group;
3. selects the default repository or validates the requested repository against the allowlist;
4. selects the default agent mode or validates the requested mode;
5. proves the requested capability is permitted by the binding's write policy;
6. parses an optional Linear issue identifier and verifies that its team key matches the bound Linear team;
7. returns the stable Linear IDs, repository, mode, write policy, and immutable budget ceiling.

Unmapped channels, unauthorized principals, project-team mismatches, unlisted repositories, unlisted modes, and prohibited write capabilities return bounded errors without choosing a fallback project.

## Write policies

- `read_only` permits analysis and lookup only.
- `linear_only` additionally permits separately authenticated and policy-controlled Linear issue/comment work.
- `draft_pull_request` additionally permits feature-branch and draft-PR workflows. It never grants direct protected-default-branch writes or automatic merge permission.

These values are maximum capabilities. Downstream tool policy, repository branch protection, and human approval may further restrict a run.

## Budget policy

The registry records per-project maximums for:

- concurrent runs;
- wall-clock runtime;
- model tokens;
- model spend in cents;
- retries.

Downstream orchestration must take the minimum of this binding, the user/channel/workspace policy, and the provider/task policy. No caller may widen a binding budget through Slack text.

## Security and operational ownership

- The registry must be stored in a reviewed configuration source with protected writes and an audit trail.
- Slack and provider credentials remain in protected secret storage and are never present in the registry.
- User-group membership must come from an authenticated Slack lookup or installation cache, not request-supplied text.
- Binding changes should require a feature branch, review, tests, and an explicit updater/timestamp change.
- The first live configuration should contain only the `#oresoftware` pilot and should default to read-only until DEN-1041 credentials and operational canaries are complete.

## Remaining DEN-1042 work

1. Define the `/agent`, app-mention, and message-shortcut parser and prompt-bounding contract.
2. Load a reviewed binding registry into `fiducia-slack-bridge` and call this resolver after DEN-766 request authentication.
3. Add prompt acknowledgement, durable queueing, run IDs, status, cancellation, and retries.
4. Integrate policy-controlled Linear lookup/comment/issue creation and feature-branch/draft-PR execution.
5. Add Kubernetes/External Secret/network-policy configuration and operator runbooks.
6. Execute the read-only `#oresoftware` pilot before binding other active project channels.
