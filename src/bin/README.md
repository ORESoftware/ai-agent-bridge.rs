# Bridge binaries

Small operator-facing binaries that share the `ai_agent_bridge` library.

- `fiducia-ai-agent-bridge-preflight.rs` — credential-safe LAN preflight for
  the HTTP health/readiness endpoints and the TCP JSONL listener. It reads a
  bearer only from the environment and emits a redacted JSON report; it never
  accepts credentials on the command line.

Run it with `cargo run --bin fiducia-ai-agent-bridge-preflight -- --help`.
The bridge daemon itself remains the crate's default binary (`src/main.rs`).
