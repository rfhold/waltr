# Contributing

Contributions should preserve clear layer boundaries, minimum-assumption documentation, and a working codebase.

# Running

Run the server:

`cargo run -p waltr-server`

Run web without starting the server:

`pnpm --filter @rfhold/waltr-web dev`

Run the static local Compose development profile:

`docker compose --profile dev up`

Repository Compose profiles are local development/test infrastructure. The Dockerfile's `runtime` target supports production containers; repository infrastructure and workload deployment are defined only in `infra/pulumi/`.

Run TUI without starting the server:

`cargo run -p waltr-tui`

Install and run the bundled TUI with an embedded server and web assets:

`pnpm install:bundled`

The server uses separate durable `waltr.sqlite3` and `mcp.sqlite3` files under `XDG_DATA_HOME` or `$HOME/.local/share` when database URLs are unset. `WALTR_DATABASE_URL` selects Waltr persistence; optional `WALTR_MCP_DATABASE_URL` overrides the derived MCP target. Access auth defaults to disabled. OIDC mode uses Authentik for browser/native authentication and requires a local MCP OAuth wrapping-key ring; Waltr itself issues the only bearer accepted by `/mcp`. See [docs/deployment.md](docs/deployment.md).

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

Run feature-gated MCP SQL store tests:

`cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store`

`cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored`

Run hosted MCP OAuth and SQL-store tests:

`cargo test -p mcp --features oauth-authorization-server --test oauth_authorization_server --test oauth_store`

`cargo test -p mcp --features oauth-authorization-server-sqlite --test oauth_sqlite_store`

`cargo test -p mcp --features oauth-authorization-server-postgres --test oauth_postgres_store -- --ignored`

Run server access-auth tests:

`cargo test -p waltr-server --test access_auth`

`cargo test -p waltr-server --test mcp_oauth_server`

Run deterministic agent MCP integration tests:

`cargo test -p waltr-agent --features shell --test agent mcp`

Compile the exact shared-client integration targets:

`cargo test -p waltr-client --tests --no-run`

Run the authenticated browser integration phase:

`pnpm test:web:integration:auth`

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
