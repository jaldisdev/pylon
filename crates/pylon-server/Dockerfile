# syntax=docker/dockerfile:1

# Built with two contexts, the default one being this repository:
#
#   docker buildx build -f crates/pylon-server/Dockerfile \
#     --build-context pylon-ui=../pylon-ui -t pylon-server .

FROM node:24-trixie AS ui
WORKDIR /ui
COPY --from=pylon-ui package.json package-lock.json ./
RUN npm ci
COPY --from=pylon-ui index.html vite.config.ts tsconfig.json tsconfig.app.json tsconfig.node.json ./
COPY --from=pylon-ui src ./src
RUN npm run build

# Tracks the latest 1.x: the workspace declares a rust-version floor, not a
# ceiling.
FROM rust:1-trixie AS builder
# aws-lc-sys, which rustls pulls in through reqwest, drives cmake and perl.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake perl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The checkout's own static/ carries nothing but .gitkeep, so this COPY is
# what puts a UI in the binary — it is embedded at compile time.
COPY --from=ui /ui/dist ./crates/pylon-server/static
RUN cargo build --release --locked -p pylon-server

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r pylon \
    && useradd -r -g pylon pylon

COPY --from=builder /src/target/release/pylon-server /usr/local/bin/pylon-server

# No config is baked in: pylon-server walks up from the working directory, so
# mounting a pylon.toml at /etc/pylon/pylon.toml is enough.
WORKDIR /etc/pylon

USER pylon
EXPOSE 5656

# [webserver].host defaults to localhost, unreachable from outside the
# container. Overriding the command drops this, so set [webserver].host too.
CMD ["--host", "0.0.0.0"]

# A worker-only container (--no-http) serves nothing; disable this there.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD curl -fsS http://localhost:5656/health || exit 1

ENTRYPOINT ["/usr/local/bin/pylon-server"]
