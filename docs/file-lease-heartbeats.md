# Renewable Fiducia file-lease heartbeats

Long-running repository-scoped assignments must retain one continuous fenced
lease from acquisition through the last permitted write. Reacquiring after expiry
is not renewal: another worker may have received a newer fencing token during the
gap.

## Canonical renewal contract

The bridge proxies the authoritative control-plane endpoint:

```text
POST /v1/file-leases/renew
```

through either bridge route:

```text
POST /file-leases/renew
POST /file-leases/{legacy_lease_id}/renew
```

The legacy path identifier is retained only for wire compatibility. External
renewal is authorized by the complete canonical repository/path union and current
fencing token in the body:

```json
{
  "repository": "owner/repository",
  "paths": ["src/lib.rs", "src/main.rs"],
  "agent_key": "codex-rust",
  "fencing_token": 42,
  "ttl_ms": 30000
}
```

Every path in the originally acquired atomic union must be present. The bridge
does not fabricate local state, replace the fencing token, partially renew a path
subset, or fall back to in-memory leases while a control plane is configured.

## Runner behavior

For a required file lease, the provider runner:

1. acquires the exact path union;
2. starts the provider request and a renewal timer together;
3. renews before `AI_AGENT_RUNNER_LEASE_SAFETY_MARGIN_MS` would be crossed;
4. records successful renewal count in submission metadata;
5. cancels and discards the provider operation immediately on timeout, transport
   failure, HTTP conflict, wrong owner, stale token, changed path union, or a
   different returned fencing token;
6. submits only a redacted failure record after lease loss;
7. attempts fenced release, while accepting that a stale release may be rejected.

No repository write may start or continue after renewal failure or lease expiry.
Downstream tools that apply generated patches must validate the same fencing token
at their write boundary; a model response carrying an old token is not authority.

## HTTP hardening

The control-plane client:

- uses the configured total timeout and a bounded connect timeout;
- disables redirects;
- sends the internal secret only in `x-internal-auth`;
- caps every response at 1 MiB, including chunked responses;
- returns generic transport/read errors without URLs, headers, response fragments,
  secrets, or provider-controlled HTML;
- preserves authoritative 4xx/5xx statuses and JSON bodies.

## Operational guidance

Choose a TTL larger than the safety margin plus normal scheduling jitter. A
30-second TTL with a 15-second margin renews every 15 seconds. The runner refuses
to start leased work when the configured margin leaves less than 250 ms for the
first heartbeat.

Alert on `lease_heartbeat_failed=true`, repeated release conflicts, zero successful
renewals for work lasting longer than one renewal interval, and control-plane
timeouts. Scaling the runner above one replica additionally requires distributed
assignment claims; file-path leases alone do not prevent duplicate non-file model
calls.
