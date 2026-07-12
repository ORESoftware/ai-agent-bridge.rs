# tests

Integration tests that exercise the bridge end-to-end over its real transports,
against an in-memory instance bound to OS-assigned localhost ports (no database
required).

- `common/` — shared harness that boots a bridge for the tests to talk to.
- `http_api.rs` — HTTP tests: REST round-trips, semantic (embedding) routing,
  the 32-member cap, SSE streaming, and shared context.
- `tcp_protocol.rs` — TCP/JSONL tests: request/response round-trips, a
  multi-agent streaming group chat, and the 32-member cap bounce over a socket.
- `hardening.rs` — security and resource-limit tests: auth gating, oversized
  payloads, connection/frame caps, and other abuse-resistance checks.
