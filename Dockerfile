# syntax=docker/dockerfile:1.7

ARG NODE_VERSION=24.18.0
ARG RUST_VERSION=1.96.0
ARG REVISION=unknown

FROM node:${NODE_VERSION}-bookworm-slim AS node

FROM rust:${RUST_VERSION}-slim-bookworm AS build-base

ARG PNPM_VERSION=11.5.0
ARG WASM_BINDGEN_VERSION=0.2.122
ARG TARGETARCH

ENV PNPM_HOME=/pnpm
ENV PATH=${PNPM_HOME}:/usr/local/bin:${PATH}
WORKDIR /workspace

COPY --from=node /usr/local /usr/local

RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential ca-certificates git pkg-config && \
    rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@${PNPM_VERSION}
RUN rustup target add wasm32-unknown-unknown
RUN --mount=type=cache,id=waltr-${TARGETARCH}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=waltr-${TARGETARCH}-cargo-git,target=/usr/local/cargo/git \
    cargo install wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION} --locked

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY crates/waltr-web/package.json crates/waltr-web/package.json
RUN --mount=type=cache,id=waltr-${TARGETARCH}-pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store --config.dangerouslyAllowAllBuilds=true

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates

FROM build-base AS server-build

ARG REVISION

RUN --mount=type=cache,id=waltr-${TARGETARCH}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=waltr-${TARGETARCH}-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=waltr-${TARGETARCH}-cargo-target,target=/workspace/target \
    --mount=type=cache,id=waltr-${TARGETARCH}-pnpm-store,target=/pnpm/store \
    touch crates/waltr-server/build.rs && \
    WALTR_BUILD_REVISION="${REVISION}" cargo build -p waltr-server --features web --locked --release && \
    cp /workspace/target/release/waltr-server /usr/local/bin/waltr-server

FROM build-base AS test-build

RUN --mount=type=cache,id=waltr-${TARGETARCH}-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=waltr-${TARGETARCH}-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=waltr-${TARGETARCH}-cargo-target,target=/workspace/target \
    cargo build -p waltr-agent --bin waltr-agent-test-mcp-server --locked --release && \
    cp /workspace/target/release/waltr-agent-test-mcp-server /usr/local/bin/waltr-agent-test-mcp-server

FROM debian:bookworm-slim AS runtime-base

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    install -d -o 65532 -g 65532 /data

ENV WALTR_SERVER_ADDR=0.0.0.0:14333
ENV WALTR_AUTH_PATH=/data/auth.json
ENV WALTR_SESSION_FILESYSTEM_ROOT=/data/sessions
ENV WALTR_SESSION_STORE=filesystem
ENV HOME=/data

WORKDIR /data

FROM runtime-base AS runtime

ARG REVISION

LABEL org.opencontainers.image.source="https://git.holdenitdown.net/rfhold/waltr" \
      org.opencontainers.image.revision="${REVISION}"

COPY --from=server-build /usr/local/bin/waltr-server /usr/local/bin/waltr-server

EXPOSE 14333
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/waltr-server"]

FROM runtime-base AS test-runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends bash coreutils curl findutils procps && \
    rm -rf /var/lib/apt/lists/*

COPY --from=server-build /usr/local/bin/waltr-server /usr/local/bin/waltr-server
COPY --from=test-build /usr/local/bin/waltr-agent-test-mcp-server /usr/local/bin/waltr-agent-test-mcp-server

CMD ["/usr/local/bin/waltr-server"]

FROM grafana/k6:1.3.0-with-browser AS k6-browser

USER root
RUN apk add --no-cache ca-certificates nss-tools
COPY --chmod=755 tests/web/k6/support/k6-entrypoint.sh /usr/local/bin/waltr-k6-entrypoint
USER 12345

ENTRYPOINT ["/usr/local/bin/waltr-k6-entrypoint"]

FROM alpine:3.22 AS test-ca

RUN apk add --no-cache openssl
COPY --chmod=755 tests/web/k6/support/generate-test-ca.sh /usr/local/bin/generate-test-ca

ENTRYPOINT ["/usr/local/bin/generate-test-ca"]
