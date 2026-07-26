# syntax=docker/dockerfile:1.7

ARG NODE_VERSION=24.18.0
ARG RUST_VERSION=1.96.0
ARG REVISION=unknown

FROM node:${NODE_VERSION}-bookworm-slim AS node

FROM rust:${RUST_VERSION}-slim-bookworm AS build

ARG PNPM_VERSION=11.5.0
ARG WASM_BINDGEN_VERSION=0.2.122
ARG REVISION

ENV PNPM_HOME=/pnpm
ENV PATH=${PNPM_HOME}:/usr/local/bin:${PATH}
WORKDIR /workspace

COPY --from=node /usr/local /usr/local

RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential ca-certificates git pkg-config && \
    rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@${PNPM_VERSION}
RUN rustup target add wasm32-unknown-unknown
RUN --mount=type=cache,id=waltr-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=waltr-cargo-git,target=/usr/local/cargo/git \
    cargo install wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION} --locked

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY crates/waltr-web/package.json crates/waltr-web/package.json
RUN --mount=type=cache,id=waltr-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store --config.dangerouslyAllowAllBuilds=true

COPY . .
RUN --mount=type=cache,id=waltr-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=waltr-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=waltr-cargo-target,target=/workspace/target \
    --mount=type=cache,id=waltr-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store --config.dangerouslyAllowAllBuilds=true && \
    pnpm --filter @rfhold/waltr-web build && \
    WALTR_BUILD_REVISION="${REVISION}" cargo build -p waltr-server --features web --locked --release && \
    cargo build -p waltr-agent --bin waltr-agent-test-mcp-server --locked --release && \
    cp /workspace/target/release/waltr-server /usr/local/bin/waltr-server && \
    cp /workspace/target/release/waltr-agent-test-mcp-server /usr/local/bin/waltr-agent-test-mcp-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

ENV WALTR_SERVER_ADDR=0.0.0.0:14333
ENV WALTR_AUTH_PATH=/data/auth.json
ENV WALTR_SESSION_FILESYSTEM_ROOT=/data/sessions
ENV WALTR_SESSION_STORE=filesystem

WORKDIR /data
COPY --from=build /usr/local/bin/waltr-server /usr/local/bin/waltr-server
COPY --from=build /usr/local/bin/waltr-agent-test-mcp-server /usr/local/bin/waltr-agent-test-mcp-server

EXPOSE 14333
CMD ["waltr-server"]
