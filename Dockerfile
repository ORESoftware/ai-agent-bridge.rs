# Multi-stage build: compile the release binary (with the postgres feature) on
# the Rust image, then ship it on a minimal non-root distroless runtime.
FROM rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS build
WORKDIR /workspace
COPY fiducia-ai-agent-bridge.rs/ fiducia-ai-agent-bridge.rs/
RUN cargo build --release --locked --features postgres --manifest-path fiducia-ai-agent-bridge.rs/Cargo.toml
RUN mkdir -p /workspace/runtime-state

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:ce0d66bc0f64aae46e6a03add867b07f42cc7b8799c949c2e898057b7f75a151
COPY --from=build --chown=65532:65532 /workspace/fiducia-ai-agent-bridge.rs/target/release/fiducia-ai-agent-bridge /app
COPY --from=build --chown=65532:65532 /workspace/runtime-state /var/lib/fiducia-ai-agent-bridge
ENV AI_AGENT_BRIDGE_DIR=/var/lib/fiducia-ai-agent-bridge/claude-inbox
EXPOSE 8142 8143
USER 65532:65532
ENTRYPOINT ["/app"]
