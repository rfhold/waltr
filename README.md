# Waltr

Waltr is an AI workspace for running agent-assisted work from either a terminal UI or a browser UI.

It is designed around shared workspace state, so configuration and status stay consistent while different clients interact with the same project.

# What It Does

| Capability | What It Means |
| --- | --- |
| Terminal workflow | Use the TUI for keyboard-driven project work. |
| Browser workflow | Use the web UI for visual configuration and browser-based interaction. |
| Shared configuration | Configure providers and MCP servers once and have clients stay in sync. |
| Agent tools | Give agents controlled access to shell commands, filesystem operations, MCP tools, and project instructions. |
| Provider support | Run against supported model providers directly or through a shared gateway. |
| Safety boundaries | Keep permissions, tool access, and workspace behavior explicit and testable. |

# Everyday Use

Run the full local development loop:

`pnpm dev`

This starts the local server, the web UI, and the TUI together. Web and server logs are written under `.dev/logs/`.

Run only the server:

`cargo run -p waltr-server`

Run only the TUI:

`cargo run -p waltr-tui`

This plain build connects to `WALTR_SERVER_ENDPOINT` and does not bundle a server. Install the bundled binary, which starts its embedded server and web assets by default, with `pnpm install:bundled`; pass `--no-server` to connect that binary to an external server.

Run only the web development stack:

`pnpm dev:web`

# Configuration

Waltr stores workspace configuration in `.waltr/waltr.toml`. The configuration includes provider settings and MCP server definitions. When config changes, connected clients update from the shared workspace state instead of each client keeping its own separate copy.

Useful environment variables:

| Variable | Purpose |
| --- | --- |
| `WALTR_SERVER_ADDR` | Address used by the local server. |
| `WALTR_SERVER_ENDPOINT` | Server endpoint used by the TUI. |
| `WALTR_DATABASE_URL` | SQLite or PostgreSQL URL for Waltr-owned persistence. Unset uses durable `waltr.sqlite3` under the XDG/HOME data directory. |
| `WALTR_MCP_DATABASE_URL` | Optional MCP persistence override. Otherwise SQLite derives sibling `mcp.sqlite3`, while PostgreSQL uses the same logical database through a separate pool. |
| `WALTR_AUTH_MODE` | Waltr access mode: `disabled` by default or `oidc`. |
| `WALTR_PUBLIC_URL`, `WALTR_OIDC_*` | Public resource URL and independent browser OIDC settings. |
| `WALTR_API_OIDC_*`, `WALTR_API_REQUIRED_SCOPES` | Native Waltr API bearer issuer, audience, and scopes. |
| `WALTR_MCP_OAUTH_*`, `WALTR_MCP_REQUIRED_SCOPES` | Waltr-hosted MCP OAuth issuer, signing-key wrapping ring, token lifetimes, registration policy, and required scopes. |
| `WALTR_OIDC_NATIVE_CLIENT_ID` | Public-client ID used by TUI login. |
| `WALTR_OIDC_NATIVE_SCOPES`, `WALTR_OIDC_NATIVE_AUDIENCE`, `WALTR_OIDC_NATIVE_LOOPBACK_POLICY` | Optional TUI authorization request and loopback settings. |
| `WALTR_SESSION_STORE` | Agent session backend: filesystem, memory, or S3. |
| `WALTR_OTEL_EXPORT` | Enables OpenTelemetry export when set to `1` or `true`. |
| `WALTR_PYROSCOPE_URL` | Enables server profiling. |

# Build And Test

Common build commands:

| Command | Purpose |
| --- | --- |
| `cargo build -p waltr-server` | Build the server. |
| `cargo build -p waltr-tui` | Build the terminal UI. |
| `pnpm --filter @rfhold/waltr-web build` | Build the web UI. |

Common test command:

`pnpm --filter @rfhold/waltr-web test`

Run the local development container profile:

`docker compose --profile dev up`

`compose.yml` is fixed local development/test infrastructure. The Dockerfile's nonroot `runtime` target supports production containers; Pulumi under `infra/pulumi/` owns the repository deployment. See [deployment configuration](docs/deployment.md) for OIDC, storage, containers, and release behavior.

Run browser-level server-backed web tests against the Compose image:

`pnpm test:web:integration`

This runs the browser suite against the static `test` profile and then against the HTTPS `test-auth` fixture. Use `pnpm test:web:integration:no-auth` or `pnpm test:web:integration:auth` to isolate one phase.

For the full test matrix, see [docs/testing.md](docs/testing.md).

# How It Is Organized

| Area | Purpose |
| --- | --- |
| `crates/waltr-agent/` | Agent behavior, tools, permissions, sessions, skills, MCP, and provider integration. |
| `crates/waltr-server/` | Local coordination server for shared state, status, and config. |
| `crates/waltr-client/` | Shared client behavior used by the terminal and browser clients. |
| `crates/waltr-tui/` | Terminal user interface. |
| `crates/waltr-web/` | Browser user interface. |
| `docs/` | Architecture, testing, agent, and contributor documentation. |

# Documentation

| Document | Covers |
| --- | --- |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contributor commands and commit conventions. |
| [docs/testing.md](docs/testing.md) | Test structure, commands, and expectations. |
| [docs/architecture/README.md](docs/architecture/README.md) | Technical architecture details. |
| [docs/agent/README.md](docs/agent/README.md) | Agent feature docs. |
| [docs/architecture/config.md](docs/architecture/config.md) | Config persistence and sync behavior. |
| [docs/architecture/clients.md](docs/architecture/clients.md) | Shared client, web, and TUI boundaries. |
| [docs/architecture/access-authentication.md](docs/architecture/access-authentication.md) | Waltr access authentication and browser, native, and MCP flows. |
| [docs/deployment.md](docs/deployment.md) | SQLite/PostgreSQL, Compose persistence, OIDC, hosted MCP OAuth, reverse proxy, and native-client deployment. |
