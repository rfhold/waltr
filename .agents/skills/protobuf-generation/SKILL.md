---
name: protobuf-generation
description: Helps an agent modify Waltr Protocol Buffer contracts and Buf generation when changing proto files, gRPC services, generated Rust, or cross-crate RPC compatibility.
---

# Protobuf Generation

Treat proto files as cross-crate RPC contracts. Source definitions live under `proto/`, Buf owns linting and generation configuration at the repository root, and generated Rust is committed under `crates/waltr/src/grpc/`.

## When To Use

Use this skill when changing `.proto` files, `buf.yaml`, `buf.gen.yaml`, generated gRPC Rust, service package layout, or RPC compatibility between server, client, web, and TUI crates.

Do not use this skill for service implementation changes that do not alter proto contracts or generated code; use the server/client skills for those layers.

## Layout

| Path | Purpose |
| --- | --- |
| `proto/` | Source `.proto` files grouped by package and version. |
| `buf.yaml` | Buf module, lint, and breaking-change configuration. |
| `buf.gen.yaml` | Rust generation configuration for local Prost and Tonic plugins. |
| `crates/waltr/src/grpc/` | Committed generated Rust used by server, client, web, and TUI crates. |

## Buf Configuration

`buf.yaml` declares a v2 Buf module rooted at `proto/`. Linting uses Buf's `STANDARD` rule set. Breaking-change checks use `FILE`, so changes are evaluated at the file level.

`buf.gen.yaml` runs local Rust plugins:

| Plugin | Output | Notes |
| --- | --- | --- |
| `protoc-gen-prost` | `crates/waltr/src/grpc` | Generates message and enum types. Adds serde derives for `.waltr.config.v1` and `.waltr.status.v1`. |
| `protoc-gen-tonic` | `crates/waltr/src/grpc` | Generates service and client code with `no_transport=true`; transport setup stays in client/server crates. |

Generation is explicit, not wired into Cargo builds. Regenerate after changing protos and commit the updated generated files with the proto change.

## Workflow

1. Inspect the current proto package and generated module paths before editing.
2. Change source `.proto` files under `proto/` and keep package paths versioned, such as `waltr.config.v1`.
3. Run `buf lint` after proto or Buf config changes.
4. Run `buf breaking --against <base>` when checking compatibility against a base branch or commit.
5. Run `buf generate` after source proto changes.
6. If a new package or version is added, update `crates/waltr/src/grpc/mod.rs` so crates can include the generated modules.
7. Build at least the affected Rust crate, commonly `cargo build -p waltr-server` for server-facing RPC changes.
8. Run relevant server/client tests for changed service behavior.

## Compatibility Expectations

Preserve compatibility unless a breaking change is intentional and coordinated.

- Do not reuse field numbers for different meanings.
- Do not rename or repurpose enum values in a way that changes existing semantics.
- Reserve removed field numbers and names when deleting fields from a shipped message.
- Prefer adding optional fields or new messages over changing existing field meaning.
- Introduce a new package version when compatibility cannot be preserved.

## Commands

Run Buf lint:

`buf lint`

Run Buf generation:

`buf generate`

Run compatibility checks against a base branch or commit:

`buf breaking --against <base>`

Build the server after server-facing RPC changes:

`cargo build -p waltr-server`

## References

| Path | Info |
| --- | --- |
| [../../../proto/](../../../proto/) | Source proto contracts. |
| [../../../buf.yaml](../../../buf.yaml) | Buf module, lint, and breaking-change configuration. |
| [../../../buf.gen.yaml](../../../buf.gen.yaml) | Prost and Tonic generation configuration. |
| [../../../crates/waltr/src/grpc/](../../../crates/waltr/src/grpc/) | Committed generated Rust. |
| [../../../crates/waltr/src/grpc/mod.rs](../../../crates/waltr/src/grpc/mod.rs) | Generated module include tree. |
| [../../../crates/waltr-server/src/app.rs](../../../crates/waltr-server/src/app.rs) | Server route registration for generated services. |
| [../../../crates/waltr-client/src/transport.rs](../../../crates/waltr-client/src/transport.rs) | Shared client transport over generated service clients. |
| [../../../docs/architecture/config.md](../../../docs/architecture/config.md) | Config RPC architecture and watch behavior. |
| [../../../docs/architecture/clients.md](../../../docs/architecture/clients.md) | Client transport boundaries for native gRPC and gRPC-web. |
