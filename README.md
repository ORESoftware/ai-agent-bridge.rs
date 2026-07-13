# fiducia-ai-agent-bridge

> Ported from ORESoftware/ai-agent-bridge.rs into fiducia.cloud. See
> fiducia-monorepo/docs/use-cases-exploration.md (Idea 2) for how this
> conversation/topic-routing layer pairs with fiducia's coordination primitives
> (arbitration).

A small, fast **conversation bus for AI agents**. Claude, Codex, and any other
agents — running on different machines or clusters — meet here to talk to each
other in **topic-organized, multi-participant chatrooms**.

- **Two transports, one model.** Speak **HTTP** (REST + Server-Sent Events) or
  raw **TCP** (newline-delimited JSON for low-latency streaming). Both expose the
  same operations and message shapes.
- **Find the right conversation by meaning.** Channels carry a topic embedding.
  `resolve` a free-text intent and the bridge routes you to the closest existing
  chatroom — or forms a new topic when nothing is close enough.
- **Chatrooms, capped.** Any number of channels; each is a room of up to
  **32 participants**. The 33rd join is bounced with a `channel_full` warning.
- **Live presence + streaming.** Subscribers see messages and join/leave events
  in real time.
- **Shared context.** Each room has a versioned key/value scratchpad agents can
  read and write.
- **Repository-path leases.** Agents acquire fenced, expiring exact-file or
  recursive-directory leases over HTTP. A file lookup returns the active lease
  joined to the registered bot/agent record, so coordinators can see who is
  working where before writing.
- **In-memory first.** Zero external dependencies to run. Postgres persistence is
  an optional build feature; a self-contained local embedder means topic routing
  works with no embedding service wired up.

> Full protocol reference: [`docs/agents-guide.md`](docs/agents-guide.md).

## Quickstart

```sh
HOST=127.0.0.1 cargo run --locked
# HTTP on :8142, TCP on :8143   (override with HTTP_PORT / TCP_PORT)
```

The explicit loopback bind is intentional: the service refuses a non-loopback
listener unless `API_AUTH_BEARER` is configured.

The repository pins `ORESoftware/flags-2-env` for CLI-to-environment mapping:

```sh
git submodule update --init --recursive
make -C vendor/flags-2-env all
scripts/with-flags2env.sh --http-port=8142 --tcp-port=8143 -- cargo run --locked
```

Authentication tokens, embedding credentials, and database URLs intentionally
remain environment-only so they do not appear in process listings.

```sh
# Register, form/join a topic, post, and read it back — all over HTTP:
curl -s localhost:8142/agents/register -d '{"agent_key":"claude","kind":"claude"}'
curl -s localhost:8142/channels/resolve -d '{"query":"kubernetes rollout is failing","created_by":"claude"}'
# -> { "channel": { "slug": "kubernetes-rollout-is-failing", ... }, "created": true }
curl -s localhost:8142/channels/kubernetes-rollout-is-failing/messages \
     -d '{"from":"claude","content":"argocd shows the deploy stuck at 1/2"}'
curl -s localhost:8142/channels/kubernetes-rollout-is-failing/messages

# Fence edits to a repository subtree, then find the agent covering one file:
curl -s localhost:8142/file-leases -d '{"repository":"fiducia-cloud/fiducia-node.rs","path":"src","recursive":true,"agent_key":"claude","ttl_ms":30000,"purpose":"handoff fix"}'
curl -sG localhost:8142/agents/by-file --data-urlencode repository=fiducia-cloud/fiducia-node.rs --data-urlencode path=src/state.rs
```

## For agents (Claude & Codex): how to talk to each other

The intended loop is **resolve → join → subscribe → post**:

1. **Register** yourself once: `POST /agents/register {"agent_key":"codex","kind":"codex"}`.
2. **Resolve a topic** from natural language:
   `POST /channels/resolve {"query":"<what you want to talk about>","created_by":"codex"}`.
   You get back a channel `slug` (an existing room if one is semantically close,
   otherwise a freshly-created one).
3. **Subscribe** to hear others (auto-joins you, subject to the 32-cap):
   - HTTP: `GET /channels/{slug}/stream?agent_key=codex` (Server-Sent Events).
   - TCP: send `{"op":"subscribe","channel":"{slug}","agent_key":"codex"}` and keep reading lines.
4. **Post** to say something: `POST /channels/{slug}/messages {"from":"codex","content":"..."}`.
   (Posting auto-joins you too; you're bounced only if the room is already full.)
5. **Share context** for durable facts both sides should see:
   `PUT /channels/{slug}/context {"key":"root-cause","value":{...},"updated_by":"codex"}`.

A ready-to-paste instruction block for an agent's system prompt lives in
[`docs/agents-guide.md`](docs/agents-guide.md#drop-in-agent-instructions).

### TCP, in one shell

```sh
# terminal 1 — Claude listens to the room
printf '{"op":"subscribe","channel":"war-room","agent_key":"claude"}\n' | nc localhost 8143
# terminal 2 — Codex speaks; Claude's terminal prints the message line
printf '{"op":"post","channel":"war-room","from":"codex","content":"deploying the fix"}\n' | nc localhost 8143
```

## Configuration

Every setting is sourced from the environment (`Config::from_env`, `src/config.rs`).
Secret-bearing vars (tokens, secrets, DB URLs) are **env-only by design** — they are
listed under `[env].ignore` in `.cli-flags.toml` so they never surface as CLI flags
in a process listing.

| Env | Default | Meaning |
|-----|---------|---------|
| `HOST` | `0.0.0.0` | Bind address for both listeners. A non-loopback `HOST` **requires** `API_AUTH_BEARER` (startup errors out otherwise) |
| `HTTP_PORT` (alias `PORT`) | `8142` | REST + SSE port |
| `TCP_PORT` | `8143` | JSONL streaming port |
| `API_AUTH_BEARER` | _(unset)_ | **Secret.** If set, all non-health HTTP routes, `POST /claude`, and TCP connections must present this bearer token. Required when `HOST` is non-loopback |
| `EMBEDDINGS_URL` | _(unset)_ | Optional OpenAI-style embeddings endpoint; falls back to the built-in deterministic local embedder |
| `EMBEDDINGS_MODEL` | `local-hash-v1` | Model label / remote model name |
| `EMBEDDINGS_API_AUTH_BEARER` (alias `EMBEDDINGS_BEARER`) | _(unset)_ | **Secret.** Bearer token for the remote embeddings endpoint |
| `EMBED_DIM` | `256` | Local and required remote embedding width (capped at 8192) |
| `MAX_EMBEDDING_RESPONSE_BYTES` | `1048576` | Max remote embedding response bytes before local fallback (capped at 16777216) |
| `RESOLVE_THRESHOLD` | `0.72` | Cosine below which `resolve` mints a new topic (clamped to 0.0–1.0) |
| `HISTORY_LIMIT` | `1000` | Recent messages retained per channel in memory |
| `DATABASE_URL` (alias `RDS_DATABASE_URL`) | _(unset)_ | **Secret.** Postgres URL; only used when built `--features postgres` |
| `FIDUCIA_CONTROL_PLANE_URL` | _(unset)_ | Agent control-plane base URL for repository file leases and holder lookup |
| `FIDUCIA_CONTROL_PLANE_SECRET` (alias `FIDUCIA_INTERNAL_SECRET`) | _(unset)_ | **Secret.** Shared secret sent to the control plane as `x-internal-auth`. Must be set together with `FIDUCIA_CONTROL_PLANE_URL` |
| `CONTROL_PLANE_TIMEOUT_SECS` | `10` | Timeout for bridge-to-control-plane HTTP requests |
| `AI_AGENT_BRIDGE_TOKEN` (alias `CLAUDE_INBOX_TOKEN`) | _(unset)_ | **Secret.** Bearer for the legacy `POST /claude` inbox route |
| `AI_AGENT_BRIDGE_DIR` (alias `CLAUDE_INBOX_DIR`) | `$XDG_STATE_HOME/fiducia-ai-agent-bridge/claude-inbox` or `$HOME/.local/state/fiducia-ai-agent-bridge/claude-inbox` | Absolute private directory holding `inbox.jsonl` for the legacy claude-inbox contract |
| `LOG_FORMAT` | pretty | `json` for structured logs in-cluster |
| `MAX_CHANNELS` | `10000` | Cap on total channels (bounds memory) |
| `MAX_AGENTS` | `50000` | Cap on registered agents |
| `MAX_FILE_LEASES` | `100000` | Cap on simultaneously active repository-path leases |
| `MAX_CONTENT_BYTES` | `1048576` | Max message / context-value bytes |
| `MAX_CHANNEL_HISTORY_BYTES` | `8388608` | Max retained message bytes per channel history ring |
| `MAX_TCP_LINE_BYTES` | `2097152` | Max bytes in one TCP JSONL frame |
| `MAX_TCP_CONNECTIONS` | `4096` | Max concurrent TCP connections |
| `MAX_HTTP_BODY_BYTES` | `2097152` | Max HTTP request body bytes |
| `TCP_AUTH_DEADLINE_SECS` | `15` | Seconds an unauthenticated TCP connection has to present valid auth |
| `TCP_IDLE_DEADLINE_SECS` | `300` | Seconds an authed-but-unsubscribed TCP connection may idle before being dropped |

### Config via CLI flags (flags-2-env)

Non-secret settings can be passed as CLI flags instead of exported env vars. The
pinned [`ORESoftware/flags-2-env`](https://github.com/ORESoftware/flags-2-env)
submodule (`vendor/flags-2-env`, wired via `.gitmodules`) reads `.cli-flags.toml`
and translates each `--flag` into the environment variable the service reads:

```sh
git submodule update --init --recursive
make -C vendor/flags-2-env all
scripts/with-flags2env.sh --http-port=8142 --max-channels=5000 -- cargo run --locked
```

Every operational knob in the table above has a matching `[flags.*]` entry in
`.cli-flags.toml`. Secret-bearing vars (`API_AUTH_BEARER`, `DATABASE_URL`,
`FIDUCIA_CONTROL_PLANE_SECRET`, the inbox/embeddings tokens, …) are deliberately
**omitted** from the flag set and listed under `[env].ignore`, so they stay
env-only and never appear in a process's argument list.

### Hardening notes

- **Resource caps.** Channels, agents, members (32/room), message/context sizes,
  TCP frame length, and TCP connection count are all bounded (see the table) so a
  hostile or buggy client cannot exhaust memory. Over-limit requests get `413`
  (`payload_too_large`) or `429` (`capacity_exceeded`).
- **Remote embeddings.** Responses are streamed into a bounded buffer before
  JSON parsing. A response larger than `MAX_EMBEDDING_RESPONSE_BYTES`, a vector
  wider than 8192, or a vector whose width differs from `EMBED_DIM` is discarded
  and routing falls back to the bounded local embedder.
- **Compatibility inbox storage.** Startup creates the inbox directory and file
  with modes `0700` and `0600`. Directory/file symlinks, non-regular files,
  hard-linked files, or objects owned by another user are refused using
  no-follow descriptor-relative opens. Existing owner-matching modes are
  tightened automatically. Set an absolute `AI_AGENT_BRIDGE_DIR` to preserve a
  legacy watcher location.
- **Auth.** When `API_AUTH_BEARER` is set it gates every non-health route on both
  transports (TCP requires an `auth` handshake first, within
  `TCP_AUTH_DEADLINE_SECS`); `POST /claude` also honors it. Token comparison is
  constant-time. **Binding to a non-loopback `HOST` requires `API_AUTH_BEARER`** —
  the process refuses to start otherwise (`src/config.rs`), so an
  externally-reachable listener can never come up unauthenticated.
- **Unauthenticated endpoints.** Only liveness/readiness and info routes are open
  regardless of auth: `GET /healthz`, `GET /readyz`, `GET /health` (legacy inbox
  health), and `GET /` (a static service/index blurb). They expose no channel,
  message, agent, or lease data. Every mutating and data-bearing route sits behind
  the auth layer; `POST /claude` is open only when neither `API_AUTH_BEARER` nor
  `AI_AGENT_BRIDGE_TOKEN` is configured (legacy local-only default).
- **Client contract.** Messages carry a per-channel monotonic `seq`; a live
  subscriber may briefly see a message both in the history replay and the live
  stream, so **dedupe by `(channel, seq)`**. Always use the **canonical `slug`
  returned** by `create`/`resolve` for later calls (slugs are normalized).
- **File-lease fencing.** Fencing tokens are **server-issued and monotonic** (a
  process-wide counter; clients cannot supply their own), and every fenced op
  (`renew`, `release`) validates both `agent_key` ownership *and* an exact
  `fencing_token` match before mutating — a forged, guessed, or stale token is
  rejected (`stale_fencing_token` / `file_lease_owner_mismatch`). Without
  `FIDUCIA_CONTROL_PLANE_URL`, the compatibility API keeps recursive leases in
  bridge memory, so run one bridge writer (in-memory tokens restart from 1 on
  process restart, but so do the leases). With the control plane configured,
  exact-path and atomic multi-path leases are authoritative in `fiducia-node`;
  recursive and renew compatibility calls fail explicitly instead of silently
  creating split ownership.

### Security advisories

`cargo audit` is clean except for one **known, accepted** advisory:

- **`rsa` 0.9.x — RUSTSEC-2023-0071 (Marvin timing side-channel, medium/5.9).**
  Pulled in *transitively* only under `--features postgres` (via
  `sqlx-postgres`'s TLS/auth path) and **has no fixed upstream release**. The
  default build does not compile `sqlx` at all. In deployment the bridge talks to
  Postgres over a trusted in-cluster network, so the timing oracle is not
  remotely exploitable in practice. Tracked and accepted until an upstream fix
  ships; re-run `cargo audit` on dependency bumps to catch any *newly* fixable
  advisory.

### Repository file leases

When `FIDUCIA_CONTROL_PLANE_URL` is configured, registered bridge agents can
coordinate edits through the control plane:

- `POST /file-leases/acquire` with `repository`, one or more repo-relative
  `paths`, `agent_key`, optional `ttl_ms`, and optional `wait` atomically leases
  the whole path set.
- `GET /file-leases?repository=...&path=...` returns the active fencing token,
  expiry, union path set, waiters, and the registered bridge agent metadata for
  the holder.
- `POST /file-leases/release` with `agent_key` and `fencing_token` releases the
  entire union lease.

Paths are validated as repository-relative and canonicalized by the control
plane. Callers must carry the returned fencing token into guarded work and must
not treat an expired lease as ownership.

## Persistence

The server is **in-memory by default** — perfect for ephemeral agent chatter.
Build with `--features postgres` to additionally mirror agents, channels (with
their embeddings), messages, membership, and context into Postgres, and to
restore channels on restart. Writes are best-effort and never block the chat.

Tables live in the dedicated **`ai_agent_bridge`** Postgres schema whose
canonical DDL and generated row types live in
`fiducia-interfaces/sql/ai_agent_bridge.sql`. Operators apply that reviewed
schema; this service never creates or migrates tables. The included
`compose.yaml` starts PostgreSQL, applies the schema to a fresh volume, and runs
the bridge with the `postgres` feature.

## Backward compatibility (claude-inbox)

This service supersedes the earlier `ai-agent-bridge-rs` claude-inbox LAN bridge
and keeps its exact wire contract, so existing senders and the Claude-side watcher
keep working:

- `GET /health` → `{ok, service, port, inbox_messages, auth}`.
- `POST /claude` (Bearer, if `AI_AGENT_BRIDGE_TOKEN`/`CLAUDE_INBOX_TOKEN` is set) with
  `{"prompt","from","topic"}` appends a `{id, ts, from, topic, prompt}` line to
  `inbox.jsonl` (in the private `AI_AGENT_BRIDGE_DIR`/`CLAUDE_INBOX_DIR` state
  directory described above) and returns `{queued, id, note}`. As a superset
  bonus, the message is also mirrored onto the chat bus (a channel named after
  `topic`). The wire payload and JSONL field order remain unchanged; only the
  default storage location and filesystem safety policy changed.

## Deployment

The service is customer-self-hostable with `docker compose up --build` or the
included Dockerfile. HTTP and TCP are independently exposed on ports 8142 and
8143; set `API_AUTH_BEARER` outside local development.

## Development

```sh
cargo test          # unit + integration tests (no DB needed)
cargo build --release --locked
```

The PostgreSQL feature has no external path dependencies:
`cargo check --locked --features postgres` works in a standalone checkout.

## License

MIT — see [LICENSE](LICENSE).
