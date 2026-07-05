# Contributing

Contributions should preserve clear layer boundaries, minimum-assumption documentation, and a working codebase.

# Running

Run the server:

`cargo run -p waltr-server`

Run web without starting the server:

`pnpm --filter @rfhold/waltr-web dev`

Run TUI without starting the server:

`cargo run -p waltr-tui`

# Building

Build a Rust crate:

`cargo build -p <crate>`

Build the MCP crate:

`cargo build -p mcp`

Build the server:

`cargo build -p waltr-server`

Build the TUI:

`cargo build -p waltr-tui`

Build the web client:

`pnpm --filter @rfhold/waltr-web build`

# Protocol Buffers

Proto contracts live under `proto/`, Buf configuration lives at the repository root, and generated Rust is committed under `crates/waltr/src/grpc/`.

Run `buf lint` and `buf generate` after changing proto files.

# Testing

See [docs/testing.md](docs/testing.md) for test structure, commands, layering expectations, live-test setup, and container-backed test notes.

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run all MCP tests:

`cargo test -p mcp --tests`

# Commits

Use Conventional Commits.

Examples:

- `feat(agent): add shell permission flow`
- `fix(client): preserve reducer state on reconnect`
- `docs(architecture): describe config watch flow`
- `test(web): cover provider form transition`

Suggested scopes:

- `agent`
- `mcp`
- `client`
- `server`
- `tui`
- `web`
- `docs`
- `tests`
- `proto`
- `config`
- `status`
- `release`
