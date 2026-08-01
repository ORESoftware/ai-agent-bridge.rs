# tests/common

Shared test harness reused by the integration test files (`http_api.rs`,
`tcp_protocol.rs`, `hardening.rs`).

- `mod.rs` — spins up an in-memory bridge with a deterministic local embedder,
  binds the HTTP and TCP listeners to OS-assigned ports on localhost, and hands
  back their addresses so each test can drive the running server over the wire.
  Marked `#![allow(dead_code)]` because each test file uses only a subset of the
  helpers.
