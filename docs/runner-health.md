# Provider-runner health and readiness

The standalone `fiducia-ai-agent-runner` serves a dedicated, non-sensitive HTTP
health surface. It is independent of the bridge process and must receive its own
Kubernetes probes.

```text
GET :8144/healthz
GET :8144/readyz
```

`/healthz` reports process liveness. `/readyz` returns `200` only when all of the
following are true:

1. every provider named by `AI_PROVIDER_CONFIG_JSON` appears in the bridge agent
   registry;
2. when distributed claims are enabled, the unique `runner/{instance_id}` claim
   owner also appears in the registry;
3. the authenticated bridge `/workflows` probe succeeded within the configured
   staleness window;
4. shutdown/drain has not begun.

A transient bridge failure does not immediately fail readiness. The last successful
poll remains valid only until `AI_AGENT_RUNNER_READY_MAX_STALENESS_MS` expires.
The bounded monitor refuses redirects, requires HTTPS and a bearer token for a
non-loopback bridge, caps response bodies at 1 MiB, and never emits upstream error
bodies.

Health responses contain only:

* `ok` and a small status label;
* whether required registrations are present;
* whether the last workflow poll is fresh;
* whether shutdown is in progress;
* the age of the last successful workflow poll.

They never include provider names, models, prompts, assignments, workflow data,
bridge URLs, bearer tokens, provider credentials, upstream response bodies, or
configuration JSON.

## Configuration

| Environment variable | Default | Meaning |
|---|---:|---|
| `AI_AGENT_RUNNER_HEALTH_HOST` | `0.0.0.0` | Dedicated health listener address |
| `AI_AGENT_RUNNER_HEALTH_PORT` | `8144` | Liveness/readiness port |
| `AI_AGENT_RUNNER_READY_MAX_STALENESS_MS` | `30000` | Maximum age of the last successful workflows probe |
| `AI_AGENT_RUNNER_HEALTH_PROBE_INTERVAL_MS` | `5000` | Bridge probe interval, bounded to 250–60000 ms and never above the staleness limit |
| `AI_AGENT_RUNNER_HEALTH_PROBE_TIMEOUT_SECS` | `5` | Total/connect timeout for each bounded probe |

The monitor uses the same `AI_AGENT_RUNNER_BRIDGE_URL` and
`AI_AGENT_RUNNER_BRIDGE_BEARER`/`API_AUTH_BEARER` boundary as the runner. A scoped
credential must have both `agent:read` and `workflow:read`.

## Kubernetes probe contract

```yaml
ports:
  - name: runner-health
    containerPort: 8144
livenessProbe:
  httpGet:
    path: /healthz
    port: runner-health
  periodSeconds: 10
  timeoutSeconds: 2
  failureThreshold: 3
readinessProbe:
  httpGet:
    path: /readyz
    port: runner-health
  periodSeconds: 5
  timeoutSeconds: 2
  failureThreshold: 2
```

Do not point runner probes at bridge port 8142. A healthy bridge does not prove that
the provider runner has registered, can poll workflows, or is safe to receive work.
Likewise, a runner readiness failure must not restart the bridge.

## Rollout rules

* Start with one runner replica.
* Keep replicas at one until renewable file leases and distributed assignment
  claims merge and their race/crash/handoff tests pass.
* Readiness must be false before provider registration, after probe staleness, and
  while the runner drains on shutdown.
* Health endpoints are not a substitute for assignment fencing, provider-call
  cancellation, submission-boundary validation, or audit records.
