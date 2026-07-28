# Distributed assignment claims

The provider runner's process-local `in_flight` set prevents duplicate work only
inside one process. Multiple pods require a shared authoritative claim for each
`(workflow_id, assignment_ordinal)` before any provider request starts.

## Claim namespace

Assignment claims reuse Fiducia's exact-union lease protocol with a dedicated
canonical namespace:

```text
repository: fiducia-cloud/ai-agent-assignment-claims
path:       workflows/{workflow_id}/assignments/{assignment_ordinal}
owner:      runner/{AI_AGENT_RUNNER_INSTANCE_ID}
```

The namespace and path are coordination identifiers, not source files. File-path
leases for repository writes remain separate and may be held simultaneously.

## Runner startup contract

```text
AI_AGENT_RUNNER_DISTRIBUTED_CLAIMS=true
AI_AGENT_RUNNER_REPLICA_COUNT=<declared deployment replicas>
AI_AGENT_RUNNER_INSTANCE_ID=<unique pod/instance identity>
AI_AGENT_RUNNER_ASSIGNMENT_CLAIM_REPOSITORY=fiducia-cloud/ai-agent-assignment-claims
AI_AGENT_RUNNER_ASSIGNMENT_CLAIM_TTL_MS=60000
```

`HOSTNAME` may supply the instance identity when it is non-empty and path-safe.
Startup fails when the declared replica count exceeds one but distributed claims
are disabled, or when the claim TTL cannot preserve the configured renewal safety
margin. Keep the Kubernetes replica count at one until the bridge is configured to
require claims and all claim tests pass.

The runner registers `runner/{instance}` as a bridge agent, then:

1. acquires the assignment claim before any repository lease or provider call;
2. leaves work pending when another replica owns the claim;
3. acquires any exact repository-path lease required by the workflow;
4. renews the assignment claim and repository lease together while the provider
   request is in flight;
5. discards provider output after loss of either lease;
6. performs one final assignment-claim renewal immediately before submission;
7. includes the claim repository, exact path, owner, instance, assignment ordinal,
   TTL, and fencing token in submission metadata;
8. releases the repository lease and assignment claim after submission, accepting
   TTL expiry as the recovery path after a stale release.

## Submission-boundary fencing

Set the bridge-side policy:

```text
AI_AGENT_BRIDGE_REQUIRE_ASSIGNMENT_CLAIMS=true
AI_AGENT_BRIDGE_ASSIGNMENT_CLAIM_REPOSITORY=fiducia-cloud/ai-agent-assignment-claims
AI_AGENT_BRIDGE_ASSIGNMENT_CLAIM_MAX_TTL_MS=60000
```

For every `POST /workflows/{id}/submissions`, the bridge derives the expected
assignment ordinal from the immutable workflow plan. It rejects missing,
malformed, wrong-repository, wrong-path, wrong-owner, wrong-ordinal, zero-token,
or over-TTL claims before touching workflow state.

The bridge then calls authoritative `POST /v1/file-leases/renew` with the exact
claim union and presented token. Only a successful renewal returning the same
fencing token reaches the append-only workflow submission handler. This creates a
fresh TTL window around the state transition: a stale runner cannot submit after a
successor receives a newer token.

## Credentials

The runner currently uses its bridge credential to register its instance identity
and operate claims. In scoped-auth deployments, provision a credential whose
`agent_key` exactly matches `runner/{instance}` with `agent:register` and
`lease:operate`, or use the tightly controlled operator credential until separate
claim and provider clients are deployed. Never put credentials in claim metadata,
workflow records, flags, logs, URLs, or command lines.

## Failure and recovery

* Claim conflict: another runner owns the assignment; leave it pending and retry
  after ordinary polling/backoff.
* Provider crash: the claim and any file lease expire; a successor acquires newer
  fencing tokens and restarts the assignment.
* Renewal failure: cancel provider work and discard output.
* Submission conflict: treat the local result as stale and do not retry with the
  same token.
* Release failure: log a bounded warning and wait for TTL expiry.

Process-local `in_flight` remains an optimization. Fiducia is the sole authority
for cross-process ownership.
