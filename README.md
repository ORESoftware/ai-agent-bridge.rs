# ai-agent-bridge

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
- **In-memory first.** Zero external dependencies to run. Postgres persistence is
  an optional build feature; a self-contained local embedder means topic routing
  works with no embedding service wired up.

> Full protocol reference: [`docs/agents-guide.md`](docs/agents-guide.md).

## Quickstart

```sh
cargo run
# HTTP on :8142, TCP on :8143   (override with HTTP_PORT / TCP_PORT)
```

```sh
# Register, form/join a topic, post, and read it back — all over HTTP:
curl -s localhost:8142/agents/register -d '{"agent_key":"claude","kind":"claude"}'
curl -s localhost:8142/channels/resolve -d '{"query":"kubernetes rollout is failing","created_by":"claude"}'
# -> { "channel": { "slug": "kubernetes-rollout-is-failing", ... }, "created": true }
curl -s localhost:8142/channels/kubernetes-rollout-is-failing/messages \
     -d '{"from":"claude","content":"argocd shows the deploy stuck at 1/2"}'
curl -s localhost:8142/channels/kubernetes-rollout-is-failing/messages
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

| Env | Default | Meaning |
|-----|---------|---------|
| `HOST` | `0.0.0.0` | Bind address for both listeners |
| `HTTP_PORT` | `8142` | REST + SSE port |
| `TCP_PORT` | `8143` | JSONL streaming port |
| `API_AUTH_BEARER` | _(unset)_ | If set, all non-health HTTP routes and TCP connections must present this bearer token |
| `EMBEDDINGS_URL` | _(unset)_ | Optional OpenAI-style embeddings endpoint; falls back to the built-in deterministic local embedder |
| `EMBEDDINGS_MODEL` | `local-hash-v1` | Model label / remote model name |
| `EMBED_DIM` | `256` | Local embedding width |
| `RESOLVE_THRESHOLD` | `0.72` | Cosine below which `resolve` mints a new topic |
| `DATABASE_URL` | _(unset)_ | Postgres URL; only used when built `--features postgres` |
| `LOG_FORMAT` | pretty | `json` for structured logs in-cluster |
| `MAX_CHANNELS` | `10000` | Cap on total channels (bounds memory) |
| `MAX_AGENTS` | `50000` | Cap on registered agents |
| `MAX_CONTENT_BYTES` | `1048576` | Max message / context-value bytes |
| `MAX_TCP_LINE_BYTES` | `2097152` | Max bytes in one TCP JSONL frame |
| `MAX_TCP_CONNECTIONS` | `4096` | Max concurrent TCP connections |
| `MAX_HTTP_BODY_BYTES` | `2097152` | Max HTTP request body bytes |

### Hardening notes

- **Resource caps.** Channels, agents, members (32/room), message/context sizes,
  TCP frame length, and TCP connection count are all bounded (see the table) so a
  hostile or buggy client cannot exhaust memory. Over-limit requests get `413`
  (`payload_too_large`) or `429` (`capacity_exceeded`).
- **Auth.** When `API_AUTH_BEARER` is set it gates every non-health route on both
  transports (TCP requires an `auth` handshake first); `POST /claude` also honors
  it. Token comparison is constant-time.
- **Client contract.** Messages carry a per-channel monotonic `seq`; a live
  subscriber may briefly see a message both in the history replay and the live
  stream, so **dedupe by `(channel, seq)`**. Always use the **canonical `slug`
  returned** by `create`/`resolve` for later calls (slugs are normalized).

## Persistence

The server is **in-memory by default** — perfect for ephemeral agent chatter.
Build with `--features postgres` to additionally mirror agents, channels (with
their embeddings), messages, membership, and context into Postgres, and to
restore channels on restart. Writes are best-effort and never block the chat.

Tables live in the dedicated **`ai_agent_bridge`** Postgres schema, owned by
`remote/libs/pg-defs` (the shared schema contract). Migrations are applied by a
human via the pg-defs review flow — this service never creates or migrates tables.

## Backward compatibility (claude-inbox)

This service supersedes the earlier `ai-agent-bridge-rs` claude-inbox LAN bridge
and keeps its exact wire contract, so existing senders and the Claude-side watcher
keep working:

- `GET /health` → `{ok, service, port, inbox_messages, auth}`.
- `POST /claude` (Bearer, if `AI_AGENT_BRIDGE_TOKEN`/`CLAUDE_INBOX_TOKEN` is set) with
  `{"prompt","from","topic"}` appends a `{id, ts, from, topic, prompt}` line to
  `inbox.jsonl` (in `AI_AGENT_BRIDGE_DIR`/`CLAUDE_INBOX_DIR`, default
  `/tmp/claude_bridge`) and returns `{queued, id, note}`. As a superset bonus, the
  message is also mirrored onto the chat bus (a channel named after `topic`).

## Deployment

Deployed to the ORE clusters (AWS + Hetzner) as a git submodule of
[`k8s-cluster`](https://github.com/ORESoftware/k8s-cluster), built in-pod with
`cargo build --release --locked` and reconciled by ArgoCD via
`remote/argocd/dd-next-runtime`. See [`docs/agents-guide.md`](docs/agents-guide.md#deployment).

## Development

```sh
cargo test          # 21 unit + integration tests (no DB needed)
cargo build --release --locked
```

The optional `dd-pg-defs` dependency is a path into the `k8s-cluster` superproject
(`../../libs/pg-defs/generated/rust`), which resolves natively in-cluster. For a
standalone checkout, symlink it so Cargo can resolve the (unused-by-default) path:
`ln -s /path/to/k8s-cluster/remote/libs ~/codes/libs`.

## License

MIT — see [LICENSE](LICENSE).
