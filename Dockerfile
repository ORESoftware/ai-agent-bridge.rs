# Multi-stage build: compile the release binary (with the postgres feature) on
# the Rust image, then ship it on a minimal non-root distroless runtime.
FROM rust:1.85-bookworm AS build
WORKDIR /workspace
COPY fiducia-ai-agent-bridge.rs/ fiducia-ai-agent-bridge.rs/
RUN cargo build --release --locked --features postgres --manifest-path fiducia-ai-agent-bridge.rs/Cargo.toml
RUN mkdir -p /workspace/runtime-state

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build --chown=65532:65532 /workspace/fiducia-ai-agent-bridge.rs/target/release/fiducia-ai-agent-bridge /app
COPY --from=build --chown=65532:65532 /workspace/runtime-state /var/lib/fiducia-ai-agent-bridge
ENV AI_AGENT_BRIDGE_DIR=/var/lib/fiducia-ai-agent-bridge/claude-inbox
EXPOSE 8142 8143
ENTRYPOINT ["/app"]
