# syntax=docker/dockerfile:1.7

# One reviewed source tree builds both runtime binaries. The final targets copy
# only the selected executable into a non-root distroless image.
FROM rust:1.97.1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa AS builder

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
    && install -D -m 0755 target/release/fiducia-ai-agent-runner /out/fiducia-ai-agent-runner \
    && mkdir -p /out/runtime-state/claude-inbox

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e AS bridge

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="Fiducia live AI-agent conversation and orchestration bridge"

COPY --from=builder /out/fiducia-ai-agent-bridge /usr/local/bin/fiducia-ai-agent-bridge
COPY --from=builder --chown=nonroot:nonroot /out/runtime-state/ /var/lib/bridge/

ENV HOST=0.0.0.0 \
    HTTP_PORT=8142 \
    TCP_PORT=8143 \
    AI_AGENT_BRIDGE_DIR=/var/lib/bridge/claude-inbox

USER nonroot:nonroot
EXPOSE 8142 8143
ENTRYPOINT ["/usr/local/bin/fiducia-ai-agent-bridge"]

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e AS runner

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="Fiducia multi-provider AI-agent runner"

COPY --from=builder /out/fiducia-ai-agent-runner /usr/local/bin/fiducia-ai-agent-runner

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/fiducia-ai-agent-runner"]
