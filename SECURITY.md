# Security policy

## Reporting a vulnerability

Report privately through GitHub's **[private vulnerability reporting](https://github.com/ORESoftware/ai-agent-bridge.rs/security/advisories/new)** on this repository. That channel is preferred because it keeps the report, the fix, and the advisory in one place and never exposes an unpatched issue publicly.

Do **not** open a public issue or pull request for a suspected vulnerability, and do not describe it in a Slack channel or Linear issue that mirrors into one.

Please include the affected component (HTTP API, TCP transport, Slack adapter, workflow orchestration, file leases, or persistence), the commit or image digest you tested, and a minimal reproduction. A proof of concept against your own instance is welcome; do not test against infrastructure you do not own.

### Never include a live secret in a report

This service handles Slack signing secrets and bot tokens, bridge bearer tokens, provider credentials, and Linear API keys. If a report would otherwise contain one:

1. rotate it first;
2. reference it by name and location only (for example "the `SLACK_SIGNING_SECRET` used by the staging adapter");
3. never paste the value into an advisory, issue, pull request, commit, log, or telemetry payload.

If you believe a live credential has already been exposed, treat rotation as the first step and the report as the second.

## Supported versions

This crate is pre-1.0 (`0.1.0`) and carries no release tags. Only the current `main` branch and the container digests built from it are supported. There are no backports; fixes land on `main` and are consumed by advancing the pinned `@sha256:` digest in the deploying GitOps repository.

Establishing a canonical release identity is tracked in DEN-601 and is not yet resolved — until it is, "the version you are running" means the image digest, not the crate version.

## Scope

In scope:

- the HTTP API and TCP transport, including scoped bearer authentication and per-route exposure;
- the Slack adapter — request signature verification, replay/freshness checks, workspace/channel allowlists, and the durable idempotency journal;
- the Slack project binding registry, which decides which Linear project and GitHub repository a channel may reach;
- workflow orchestration, including blind/competitive submission privacy and bounded concurrency;
- repository file leases and fencing;
- secret handling, telemetry redaction, and the optional PostgreSQL store.

Out of scope:

- vulnerabilities in a deployment's own Slack workspace configuration, Linear workspace, or GitHub organization settings;
- findings that require an already-compromised host or an operator deliberately disabling a documented control;
- model output quality, prompt-response content, or provider-side behaviour. This process contains no provider SDKs and no model credentials by design;
- denial of service achieved purely by exceeding documented, configurable ceilings.

## Security posture

The controls a report is measured against are documented, not implied:

| Area | Reference |
|---|---|
| Slack ingress, signatures, replay, journal | [`docs/slack-bridge.md`](docs/slack-bridge.md) |
| Slash-command dispatch, channel context, fan-out | [`docs/slack-slash-commands.md`](docs/slack-slash-commands.md) |
| Channel → project → repository routing | [`docs/slack-project-bindings.md`](docs/slack-project-bindings.md) |
| Workflow adapter authentication | [`docs/workflow-adapter-auth.md`](docs/workflow-adapter-auth.md) |
| TCP scoped authentication | [`docs/tcp-scoped-auth.md`](docs/tcp-scoped-auth.md) |
| Blind competition privacy | [`docs/blind-competition.md`](docs/blind-competition.md) |
| Lease authority and fencing | [`docs/authoritative-lease-descriptors.md`](docs/authoritative-lease-descriptors.md) |

Two properties are worth stating explicitly, because a report that breaks either is high severity:

- **No provider credentials live in the Slack adapter process.** It brokers work to the bridge workflow API and holds no model keys.
- **Ingress is fail-closed.** Unknown workspaces, channels, principals, repositories, and agent modes are refused rather than defaulted.

## Automated checks

Every pull request and push to `main` runs, and must pass:

- `scripts/audit-provider-secrets.py` — provider credential hygiene;
- `cargo clippy --all-targets --locked --features postgres -- -D warnings`;
- `cargo test --all-targets --locked`;
- `cargo audit`;
- `flags2env audit .cli-flags.toml` — CLI/config surface audit;
- `actionlint` on the workflow definitions.

Dependabot raises weekly Cargo, GitHub Actions, and Docker updates.

`cargo audit` carries one documented exception, recorded in `.cargo/audit.toml` and explained under "Security advisories" in the [README](README.md): `rsa` 0.9.x (RUSTSEC-2023-0071) remains only as unreachable `sqlx-mysql` metadata in `Cargo.lock`, since the optional store enables PostgreSQL only. Re-run `cargo audit` on dependency bumps so a newly reachable advisory still fails the gate.

## Disclosure

We aim to acknowledge a report within a few working days and to agree a disclosure timeline with the reporter. Fixes are published as a GitHub Security Advisory on this repository together with the commit that resolves them, and credit is given unless the reporter asks otherwise.
