# Canonical public repository

The canonical open-source home of the generic AI-agent bridge is now:

- [`agent-pontifex/ai-agent-bridge.rs`](https://github.com/agent-pontifex/ai-agent-bridge.rs)

The reusable protocol contracts and typed Rust clients live in:

- [`agent-pontifex/agent-sdk.rs`](https://github.com/agent-pontifex/agent-sdk.rs)

This `ORESoftware/ai-agent-bridge.rs` repository remains available for historical
provenance and ORESoftware-specific integration work. New generic bridge
features, public API changes, releases, and community issues belong in Agent
Pontifex. The repositories are independent GitHub repositories; changes are not
assumed to mirror automatically in either direction.

## Product-specific downstream implementation

Fiducia Cloud maintains its own downstream bridge implementation and advertises
product behavior through `fiducia.*` protocol extensions. Fiducia authority,
tenancy, persistence, review policy, and fencing behavior must not become hidden
requirements of the public Agent Pontifex protocol.

## Contribution routing

- Generic bridge bugs and features: `agent-pontifex/ai-agent-bridge.rs`
- Protocol, compatibility, and SDK changes: `agent-pontifex/agent-sdk.rs`
- ORESoftware-only integration and historical maintenance: this repository
- Fiducia-specific behavior: the corresponding private `fiducia-cloud` repository

Before changing a public wire contract, update the SDK fixtures and the
independent conformance lane in
[`fiducia-cloud-test/control-plane-e2e`](https://github.com/fiducia-cloud-test/control-plane-e2e).
The initial bridge-and-coordinator release gate is reviewed in
[`control-plane-e2e#3`](https://github.com/fiducia-cloud-test/control-plane-e2e/pull/3).
