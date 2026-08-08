# `com.ores.ai-agent-bridge` connection contract

`com.ores.ai-agent-bridge` is the stable ORES **logical service identifier** for
the AI-agent conversation bridge. It is not a DNS name, a public hostname, a
Kubernetes object name, or a replacement for the current Rust package/release
identity.

The reviewed runtime currently maps that identifier to:

| Scope | HTTP REST/SSE | TCP JSONL |
|---|---|---|
| Same machine | `http://127.0.0.1:8142` | `127.0.0.1:8143` |
| Kubernetes `default` namespace | `http://dd-ai-agent-bridge.default.svc.cluster.local:8142` | `dd-ai-agent-bridge.default.svc.cluster.local:8143` |

No public ingress is required or implied. Keep the bridge cluster-local unless a
separate reviewed exposure contract authorizes otherwise.

## Credential-safe client

The repository provides the explicit binary target `ores-ai-agent-bridge`.
Bearer credentials are accepted only through the environment and never through
a command-line flag.

```sh
export ORES_AI_AGENT_BRIDGE_BASE_URL=http://127.0.0.1:8142
export ORES_AI_AGENT_BRIDGE_TCP_PORT=8143
export ORES_AI_AGENT_BRIDGE_BEARER="$(your-reviewed-secret-reader)"

cargo run --locked --bin ores-ai-agent-bridge -- probe
cargo run --locked --bin ores-ai-agent-bridge -- smoke
```

`probe` verifies, in order:

1. name resolution and a usable route;
2. the TCP JSONL listener;
3. HTTP liveness and readiness;
4. bearer-authenticated access to `/agents`; and
5. the root wire identity (`ai-agent-bridge`, REST/SSE, and TCP JSONL).

`smoke` performs the same checks, then registers the stable default agent
`chatgpt-bridge-smoke`, resolves the stable connectivity topic, joins its
canonical channel, posts a unique marker, and reads the same message back by its
per-channel sequence number. The stable agent/topic avoid consuming a new agent
and channel on every run.

The JSON report omits bearer values, request and response bodies, agent prompts,
and topic text. Exit status is `0` for success, `1` for a connection, auth,
identity, or smoke failure, and `2` for command-line parsing errors.

## Kubernetes use

From a workstation with reviewed cluster access, forward both bridge transports:

```sh
kubectl -n default port-forward service/dd-ai-agent-bridge \
  8142:8142 8143:8143
```

In another terminal, obtain `ORES_AI_AGENT_BRIDGE_BEARER` through the approved
secret-delivery path and run `probe` before `smoke`. Do not print the Secret,
copy it into source, or pass it as a CLI argument.

An in-cluster caller uses the Service DNS name directly:

```sh
export ORES_AI_AGENT_BRIDGE_BASE_URL=http://dd-ai-agent-bridge.default.svc.cluster.local:8142
export ORES_AI_AGENT_BRIDGE_TCP_PORT=8143
cargo run --locked --bin ores-ai-agent-bridge -- probe
```

## Compatibility

The client falls back to the existing `FIDUCIA_BRIDGE_BASE_URL`,
`FIDUCIA_BRIDGE_TCP_PORT`, `FIDUCIA_BRIDGE_PREFLIGHT_BEARER`, and
`API_AUTH_BEARER` variables so current operators can migrate without weakening
the environment-only credential rule.

The logical identifier does not resolve the separate canonical-repository and
release-ownership decision tracked by DEN-601. It gives clients one stable name
while that governance work continues, and it fails closed if an endpoint does
not present the reviewed bridge wire contract.
