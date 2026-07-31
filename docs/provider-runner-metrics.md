# Provider-runner Prometheus metrics

The provider runner exposes `GET /metrics` on the independent health listener configured by `AI_AGENT_RUNNER_HEALTH_HOST` and `AI_AGENT_RUNNER_HEALTH_PORT`. It remains separate from bridge metrics because provider attempts, retries, pricing, admission, claims, and shutdown drain state exist in the runner process, not the bridge process.

## Privacy and cardinality

Labels are restricted to fixed enums such as result, reason, attempt kind, token kind, lease kind, and delay source. Metrics never label with:

- provider or model names;
- agent keys;
- workflow IDs or assignment ordinals;
- repository or file paths;
- prompts, provider output, or message bodies;
- credentials, request IDs, peer addresses, or user identifiers.

## Metric groups

- process/build and uptime;
- registration, poll freshness, readiness, shutdown, required identity count, and readiness staleness;
- configured provider count, distributed-claim mode, active assignments, and concurrency limit;
- workflow poll success/error and duration;
- assignment started/submitted/submission-failed/discarded outcomes;
- provider attempt started/success/failure/aborted outcomes and duration;
- retries by bounded reason plus cumulative delay by Retry-After or exponential-jitter source;
- admission admitted/rejected/reservation-rejected/completed/cancelled/error events;
- conservative initial/retry token and micro-USD reservations;
- provider-reported token and cost usage accepted by admission accounting;
- assignment-claim and file-lease acquire/unavailable/renewal-lost/release/stale-before-submission events;
- workflow submission acceptance/rejection;
- draining state and latest shutdown drain duration.

## Accounting semantics

An external provider attempt increments `started` immediately before the guarded provider request. The attempt receives exactly one terminal result: success, failure, or aborted. A retry is counted when its bounded plan is accepted, before sleep and before a separately admitted retry reservation. Initial and retry reservations are reported separately so their sum reconciles with admission usage even when actual provider usage is lower than the conservative output allowance.

Actual token and cost metrics are recorded only after normalized provider usage is accepted by the admission endpoint. Missing or unprovable usage therefore produces no actual-usage increment and the output remains discarded.

## Zero-replica behavior

The runner Deployment is intentionally absent or scaled to zero before activation. Prometheus must not page on an intentionally absent runner target. Runner target-down alerts should be added only with the separate zero-replica Deployment/Service contract and should be gated by an activation signal or desired replica count. Bridge target alerts do not imply runner health.
