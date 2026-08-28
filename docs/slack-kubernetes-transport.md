# Slack ingress Kubernetes service transport

The Slack command ingress calls two internal services:

- the AI agent bridge for workflow creation;
- the AI agent coordinator for durable job dispatch.

Non-loopback service URLs require HTTPS by default. A Kubernetes deployment may
use plain HTTP only when the destination host is an exact service FQDN ending in
`.svc.cluster.local` and that host appears in `SLACK_INTERNAL_HTTP_HOSTS`.
Wildcards, public DNS names, IP addresses, short service names, URL schemes,
ports, paths, malformed DNS labels, and empty entries are rejected.

The allowlist is transport policy only. It does not replace authentication:
`SLACK_BRIDGE_BEARER` and `SLACK_COORDINATOR_BEARER` remain required for every
non-loopback bridge and coordinator URL. Redirects remain disabled by the
outbound clients, and Kubernetes NetworkPolicy must constrain the pod to the
expected service ports.

Example:

```sh
export SLACK_BRIDGE_URL='http://dd-ai-agent-bridge.default.svc.cluster.local:8142/'
export SLACK_COORDINATOR_URL='http://ai-agent-coordinator.ai-agent-coordinator.svc.cluster.local:8080/'
export SLACK_INTERNAL_HTTP_HOSTS='dd-ai-agent-bridge.default.svc.cluster.local,ai-agent-coordinator.ai-agent-coordinator.svc.cluster.local'
```

The production Slack adapter should remain in `SLACK_COMMAND_DRY_RUN=true`
until the bridge, coordinator, ExternalSecrets, signed ingress, project registry,
provider runner, budgets, and rollback gates have all been proven on the exact
deployed image digests.
