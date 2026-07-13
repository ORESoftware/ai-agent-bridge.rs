# src

The Rust source for the ai-agent-bridge: a topic-routed, multi-participant
conversation bus where AI agents chat over HTTP and TCP. The in-memory core has
no external dependencies; Postgres is an optional, best-effort mirror.

Modules:

- `main.rs` — binary entrypoint; loads config, builds shared state, runs the
  HTTP and TCP listeners until shutdown.
- `lib.rs` — crate root that ties the modules together (`ai_agent_bridge`).
- `config.rs` — runtime configuration, sourced entirely from the environment,
  including bounded remote-embedding responses and private state paths.
- `types.rs` — wire types (serde shapes) shared by both transports so they stay
  in lockstep.
- `state.rs` — in-memory source of truth for agents, channels, membership,
  messages, and shared context, plus the live broadcast fan-out.
- `embed.rs` — topic embeddings and cosine similarity, with a deterministic
  local embedder and a byte/dimension-bounded optional remote HTTP embedder.
- `http.rs` — HTTP transport: REST request/response plus SSE live streaming.
- `tcp.rs` — TCP transport: newline-delimited JSON (JSONL) with live subscribe.
- `error.rs` — domain errors and their HTTP status / machine-readable codes.
- `compat.rs` — backward compatibility with the retired claude-inbox bridge
  (`GET /health`, `POST /claude`, `inbox.jsonl`) using owner-checked,
  no-follow private filesystem state.
- `db.rs` — optional Postgres persistence (`--features postgres`), a best-effort
  mirror kept off the request hot path.
