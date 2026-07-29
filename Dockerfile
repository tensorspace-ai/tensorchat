# TensorChat — one binary, its frontend, and nothing else.
#
# The web client and the server build in parallel stages, then land in a slim
# runtime image. rusqlite bundles SQLite from source, so the Rust stage needs a
# C toolchain; the runtime stage does not.

# ---- web client -------------------------------------------------------------
FROM node:26-bookworm-slim AS web

WORKDIR /web
# Copy the manifests first so `npm ci` is cached until dependencies change.
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build

# ---- server -----------------------------------------------------------------
FROM rust:1-bookworm AS server

WORKDIR /src

# Prime the dependency cache with the manifests and stub sources, so editing
# application code does not rebuild the whole dependency graph.
COPY Cargo.toml Cargo.lock ./
COPY crates/tc-core/Cargo.toml crates/tc-core/
COPY crates/tc-store/Cargo.toml crates/tc-store/
COPY crates/tc-server/Cargo.toml crates/tc-server/
RUN mkdir -p crates/tc-core/src crates/tc-store/src crates/tc-server/src \
    && echo "" > crates/tc-core/src/lib.rs \
    && echo "" > crates/tc-store/src/lib.rs \
    && echo "" > crates/tc-server/src/lib.rs \
    && echo "fn main() {}" > crates/tc-server/src/main.rs \
    && cargo build --release -p tc-server \
    && rm -rf crates/*/src

COPY crates/ crates/
# Cargo keys rebuilds on mtime; the stub artifacts must not shadow the real ones.
RUN touch crates/*/src/lib.rs crates/tc-server/src/main.rs \
    && cargo build --release -p tc-server

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# curl is here only for HEALTHCHECK — the server has no outbound HTTP calls.
# Unprivileged by default. The volume mount point is owned by this user so a
# fresh `docker run -v` can create the database without a chown dance.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /app tensorchat \
    && mkdir -p /data \
    && chown tensorchat:tensorchat /data

COPY --from=server /src/target/release/tc-server /usr/local/bin/tc-server
COPY --from=web /web/dist /app/web

USER tensorchat
WORKDIR /app

# Listen on all interfaces: inside a container, loopback would be unreachable.
# Publish the port deliberately, and terminate TLS in front of it.
ENV TC_BIND=0.0.0.0:8080 \
    TC_DB=/data/tensorchat.db \
    TC_BLOBS=/data/blobs \
    TC_WEB=/app/web \
    RUST_LOG=tc_server=info

VOLUME ["/data"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail --silent --output /dev/null http://127.0.0.1:8080/healthz

ENTRYPOINT ["/usr/local/bin/tc-server"]
