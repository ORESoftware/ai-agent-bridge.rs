# Policy selection v2

Policy version `2026-07-31.v2` makes model count, execution order, provider outage behavior, and review boundaries explicit before provider work starts.

## Dry-run contract

`POST /workflow-policy/explain` is side-effect free. It returns:

- policy version;
- allow/queue/deny/human-approval disposition;
- selected workflow mode;
- coordination protocol and execution target;
- ranked selected and excluded providers;
- hard token, time, retry, concurrency, provider-count, and cost ceilings;
- estimated calls, cost, wall-clock time, and minimum selected context capacity;
- the default and effective degradation behavior;
- a structured degradation decision when a fallback, queue, or reduction occurred;
- deterministic human-readable reasons.

The endpoint never creates a workflow, admission, provider request, assignment claim, or Fiducia lease.

## Execution shapes

| Coordination protocol | Workflow mode | Executor | Meaning |
|---|---|---|---|
| `direct` | `single` | standard workflow | One model. Parallel candidates and reviewer consensus are intentionally not selected. |
| `sequential_handoff` | `sequential` | standard workflow | Two or more providers execute one at a time. The runner passes only bounded accepted prior context. |
| `independent_candidates` | `competitive` | standard workflow | Workers receive no peer submission before producing their own candidate. |
| `reviewer_consensus` | `consensus` | standard workflow | Independent worker candidates are followed by one reviewer synthesis phase. |
| `blind_candidates_with_reviewer_reveal` | `consensus` | blind competition | Candidate contents remain hidden until the designated reviewer reveals the immutable set. |
| `adversarial_review_required` | `consensus` | adversarial review | The separate DEN-84 author/reviewer/conflict-resolver protocol is required before merge or production. |

Standard workflow admission rejects a policy decision whose executor is `blind_competition` or `adversarial_review`; callers must use the specialized route. This prevents a blind or adversarial decision from being silently executed as an ordinary consensus workflow.

## Provider signals

Provider ranking is deterministic and bounded. The order is:

1. availability (`available` before `degraded`);
2. historical quality, descending;
3. health, descending;
4. recent error rate, ascending;
5. p95 latency, ascending;
6. estimated cost, ascending;
7. agent key, ascending.

Quality, health, and error-rate inputs are basis points in `0..=10000`. Availability is one of `available`, `degraded`, `outage`, or `disabled`. The policy request contains no prompt, repository path, user identifier, or metric label derived from those values.

A provider is excluded when it is disabled, in outage, explicitly unavailable, untrusted for restricted data, or missing a required capability. Exclusion responses contain only the stable agent key and an enum reason.

## Required providers and reviewers

`required_agent_keys` and `required_reviewer_agent_key` refer to candidates that must remain in the selected set. A required provider that is unavailable or ineligible cannot be silently replaced. The effective degradation behavior is raised to at least `queue_until_required_providers_are_available`; restricted or critical work remains `fail_closed`.

A required reviewer is placed last in the selected consensus set and receives the reviewer role. The policy elevates an otherwise weaker requested mode to consensus when a reviewer is required.

## Degradation semantics

Degradation is executed exactly as reported:

- `fail_closed`: deny; no provider work may start;
- `queue_until_required_providers_are_available`: queue; no provider work may start;
- `fallback_to_single_with_human_approval`: select one provider only when at least one is eligible and require an explicit approver;
- `reduce_provider_count`: select the strongest feasible lower mode and identify the from/to mode and protocol.

A caller may request a stricter behavior, but cannot weaken the policy default. Specialized executors and explicit required providers cannot degrade to an ordinary weaker workflow.

Cost reduction removes only optional providers, never a required provider or reviewer. If the remaining required set still exceeds the hard cost ceiling, the decision is denied.

## Compatibility with admission

The standard runner constructs a policy request from the immutable workflow plan. Every planned agent is marked required and the plan reviewer, when present, is marked as the required reviewer. This prevents admission from silently dropping an existing assignment.

Admission remains insert-only. It accepts only `standard_workflow` decisions whose mode and selected agent set exactly match the immutable plan and whose required approval is supplied. Blind competition and adversarial review continue through their specialized governance APIs.
