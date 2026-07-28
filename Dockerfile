# syntax=docker/dockerfile:1.7

# One reviewed source tree builds both runtime binaries. The final targets copy
# only the selected executable into a non-root distroless image.
FROM rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS builder

WORKDIR /workspace
COPY . .

# Cache dependency downloads and target artifacts outside committed image layers.
# Both binaries are compiled from the same source tree and Cargo.lock.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/workspace/target,sharing=locked \
    cargo build --release --locked --features postgres \
      --bin fiducia-ai-agent-bridge \
      --bin fiducia-ai-agent-runner \
    && install -D -m 0755 target/release/fiducia-ai-agent-bridge /out/fiducia-ai-agent-bridge \
    && install -D -m 0755 target/release/fiducia-ai-agent-runner /out/fiducia-ai-agent-runner

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:66aa873a4a14fb164aa01296058efd8253744606d72715e45acface073359faa AS bridge

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="Fiducia live AI-agent conversation and orchestration bridge"

COPY --from=builder /out/fiducia-ai-agent-bridge /usr/local/bin/fiducia-ai-agent-bridge
COPY --chown=nonroot:nonroot runtime-state/ /var/lib/bridge/

ENV HOST=0.0.0.0 \
    HTTP_PORT=8142 \
    TCP_PORT=8143 \
    AI_AGENT_BRIDGE_DIR=/var/lib/bridge/claude-inbox

USER nonroot:nonroot
EXPOSE 8142 8143
ENTRYPOINT ["/usr/local/bin/fiducia-ai-agent-bridge"]

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:66aa873a4a14fb164aa01296058efd8253744606d72715e45acface073359faa AS runner

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="Fiducia multi-provider AI-agent runner"

COPY --from=builder /out/fiducia-ai-agent-runner /usr/local/bin/fiducia-ai-agent-runner

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/fiducia-ai-agent-runner"]
