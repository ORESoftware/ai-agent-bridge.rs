# Prometheus metrics contract

`GET /metrics` exposes Prometheus text format from the bridge HTTP listener. It is public like `/healthz` and `/readyz`; production access must be restricted by Kubernetes NetworkPolicy and the central Prometheus scrape configuration.

## Cardinality and privacy rules

Metric labels are limited to fixed enums such as status class, result, reason, dependency, persistence mode, resource, and capacity kind.

The endpoint never labels metrics with:

- prompts or model output;
- message bodies or metadata;
- channel names;
- agent keys;
- repository or file paths;
- provider/model names;
- tokens, credentials, peer addresses, request IDs, workflow IDs, or user identifiers.

## Main metric groups

- process/build: `ai_agent_bridge_build_info`, process start, uptime, and resident memory when `/proc` is available;
- HTTP: in-flight requests, status-class counters, overload rejection, and bounded duration histogram;
- TCP: active/admitted/rejected connections and bounded frame outcomes;
- chat bus: registered agents, channels, membership, retained and cumulative messages, context keys, broadcast backlog, message delivery outcomes, history eviction, and send duration;
- compatibility: inbox line count and SSE connection count;
- leases: active local leases plus bounded conflict, not-found, owner-mismatch, and stale-fencing counters;
- persistence: selected mode, readiness, in-flight queue depth, and shed best-effort writes;
- control plane: configured gauge, in-flight requests, bounded result counters, last success timestamp, and request duration;
- capacities: current and configured limits for agents, channels, leases, SSE, and TCP.

## Operational interpretation

Prometheus should alert when:

- `up{job="dd-ai-agent-bridge"} == 0` for a sustained window;
- current capacity exceeds 80% of the configured limit;
- HTTP capacity rejections increase;
- persistence shed writes increase;
- stale fencing, owner mismatch, or lease conflicts increase unexpectedly;
- the control plane is configured but has no recent success while error counters increase.

The provider runner is a separate process and must expose or forward its own readiness and provider-call metrics. Bridge metrics do not invent provider health from absent runner data.
