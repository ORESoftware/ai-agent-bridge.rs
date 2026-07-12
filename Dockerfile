FROM rust:1.85-bookworm AS build
WORKDIR /workspace
COPY fiducia-ai-agent-bridge.rs/ fiducia-ai-agent-bridge.rs/
RUN cargo build --release --locked --features postgres --manifest-path fiducia-ai-agent-bridge.rs/Cargo.toml

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /workspace/fiducia-ai-agent-bridge.rs/target/release/fiducia-ai-agent-bridge /app
EXPOSE 8142 8143
ENTRYPOINT ["/app"]
