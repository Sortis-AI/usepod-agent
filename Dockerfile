# syntax=docker/dockerfile:1
#
# Use Pod provider-agent — multi-stage container build.
#
# Build context: repo root.
#   docker build -t usepod/provider-agent:dev .

# --- Build stage ---
FROM rust:1.95-slim-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Manifests first for dependency caching.
COPY Cargo.toml Cargo.lock* ./

RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'pub fn _stub() {}' > src/lib.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release

# Real sources.
COPY src   src
COPY tests tests
COPY agent.example.toml agent.example.toml

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    touch src/main.rs src/lib.rs \
    && cargo build --release \
    && cp target/release/usepod-agent /usr/local/bin/usepod-agent \
    && strip /usr/local/bin/usepod-agent

# --- Runtime stage ---
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system usepod \
    && useradd --system --gid usepod --home-dir /var/lib/usepod-agent --shell /usr/sbin/nologin usepod \
    && install -d -o usepod -g usepod -m 0750 /var/lib/usepod-agent

COPY --from=builder /usr/local/bin/usepod-agent /usr/local/bin/usepod-agent

USER usepod
WORKDIR /var/lib/usepod-agent
VOLUME ["/var/lib/usepod-agent"]
EXPOSE 9090

ENTRYPOINT ["/usr/local/bin/usepod-agent"]
CMD ["run"]
