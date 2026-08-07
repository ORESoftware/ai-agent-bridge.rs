# syntax=docker/dockerfile:1.7

# One reviewed source tree builds all runtime binaries. Final targets copy only
# the selected executable and explicitly audited non-secret runtime data into a
# non-root distroless image.
#
# Runtime images intentionally use the credential-free, in-memory feature set.
# The optional Postgres adapter is private shared-schema material and is tested
# in the centralized exact-schema certification lane; it is not required by the
# currently deployed bridge or provider runner.
FROM rust:1.97.1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa AS builder

WORKDIR /workspace
COPY . .

# Cache dependency downloads and target artifacts outside committed image layers.
# All binaries are compiled from the same source tree and Cargo.lock.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/workspace/target,sharing=locked \
    cargo build --release --locked \
      --bin fiducia-ai-agent-bridge \
      --bin fiducia-ai-agent-runner \
      --bin fiducia-slack-bridge \
      --bin fiducia-slack-command \
    && install -D -m 0755 target/release/fiducia-ai-agent-bridge /out/fiducia-ai-agent-bridge \
    && install -D -m 0755 target/release/fiducia-ai-agent-runner /out/fiducia-ai-agent-runner \
    && install -D -m 0755 target/release/fiducia-slack-bridge /out/fiducia-slack-bridge \
    && install -D -m 0755 target/release/fiducia-slack-command /out/fiducia-slack-command \
    && mkdir -p /out/runtime-state/claude-inbox \
    && mkdir -p /out/slack-state \
    && mkdir -p /out/slack-command-state/runs

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

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e AS slack

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="Authenticated Slack ingress for bounded dual-model workflows"

COPY --from=builder /out/fiducia-slack-bridge /usr/local/bin/fiducia-slack-bridge
COPY --from=builder --chown=nonroot:nonroot /out/slack-state/ /var/lib/slack-bridge/

ENV SLACK_BRIDGE_HOST=0.0.0.0 \
    SLACK_BRIDGE_PORT=8150 \
    SLACK_IDEMPOTENCY_PATH=/var/lib/slack-bridge/events.jsonl \
    SLACK_BRIDGE_DRY_RUN=true

USER nonroot:nonroot
EXPOSE 8150
ENTRYPOINT ["/usr/local/bin/fiducia-slack-bridge"]

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e AS slack-command

LABEL org.opencontainers.image.source="https://github.com/ORESoftware/ai-agent-bridge.rs" \
      org.opencontainers.image.description="ORESoftware Claude and ChatGPT Slack slash-command ingress"

COPY --from=builder /out/fiducia-slack-command /usr/local/bin/fiducia-slack-command
COPY --from=builder --chown=nonroot:nonroot /out/slack-command-state/ /var/lib/slack-command/
COPY --from=builder --chown=nonroot:nonroot /workspace/config/alex-main-agent.channels.json /etc/alex-main-agent/alex-main-agent.channels.json

ENV SLACK_COMMAND_HOST=0.0.0.0 \
    SLACK_COMMAND_PORT=8151 \
    SLACK_COMMAND_STATE_DIR=/var/lib/slack-command/runs \
    SLACK_PROJECT_REGISTRY_PATH=/etc/alex-main-agent/alex-main-agent.channels.json \
    SLACK_CONTEXT_MESSAGE_COUNT=5 \
    SLACK_COMMAND_DRY_RUN=true

USER nonroot:nonroot
EXPOSE 8151
ENTRYPOINT ["/usr/local/bin/fiducia-slack-command"]
