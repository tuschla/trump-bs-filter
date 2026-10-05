FROM rust:1.95-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM node:22-bookworm-slim
# Pinned to the CLI version the pipeline flags (--no-session-persistence,
# --setting-sources) were verified against; bump deliberately.
ARG CLAUDE_CODE_VERSION=2.1.280
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates python3 tini \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} \
    && npm cache clean --force
# HOME=/tmp: the CLI needs a writable home, and the container may run under any
# uid (TrueNAS apps default to 568). Nothing in it needs to survive a restart.
ENV HOME=/tmp \
    DISABLE_AUTOUPDATER=1
WORKDIR /app
COPY --from=builder /build/target/release/non-violent-trump /usr/local/bin/
COPY scripts/check-ingest-health.py /usr/local/bin/check-ingest-health
# tini as PID 1: forwards SIGTERM (the daemon ignored it as PID 1 and got
# SIGKILLed after the stop timeout) and reaps orphaned claude/node children.
ENTRYPOINT ["tini", "--", "non-violent-trump", "--config", "/app/config.toml"]
CMD ["daemon"]
