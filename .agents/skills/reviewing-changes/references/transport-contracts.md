# Transport Contracts

## Invariant

Transport layers preserve proto, gRPC, gRPC-web, WASM, status-code, ETag, and message semantics without changing domain meaning.

## Why It Matters

Waltr relies on shared contracts across Rust clients, web WASM, server RPCs, generated code, and tests.

## Source Files To Inspect

- `proto/`
- `buf.yaml`
- `buf.gen.yaml`
- `crates/waltr/src/grpc/`
- `crates/waltr-server/src/`
- `crates/waltr-client/src/transport.rs`
- `crates/waltr-web/src/`

## Common Drift Signs

- Proto changes are not regenerated or server/client code assumes old fields.
- ETag conflicts stop mapping to `failed_precondition` or equivalent client conflict state.
- Optional fields are interpreted differently across server, client, and web.
- gRPC-web/WASM adapters collapse distinct error statuses.

## Suggested Tests

Run proto lint/generation checks after proto changes, server contract tests for status codes, client integration tests for transport behavior, and web integration tests for WASM/gRPC-web paths.

## Finding Format

`[transport-contracts] <path>:<line> changes <contract field/status/message> semantics across <layers>.`
