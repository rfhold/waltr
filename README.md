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

Run only the web development stack:

`pnpm dev:web`

# Configuration

Waltr stores workspace configuration in `.waltr/waltr.toml`. The configuration includes provider settings and MCP server definitions. When config changes, connected clients update from the shared workspace state instead of each client keeping its own separate copy.

Useful environment variables:

| Variable | Purpose |
| --- | --- |
| `WALTR_SERVER_ADDR` | Address used by the local server. |
| `WALTR_SERVER_ENDPOINT` | Server endpoint used by the TUI. |
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
