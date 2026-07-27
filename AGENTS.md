# Index

| Path | Info |
| --- | --- |
| [crates/waltr-server/](crates/waltr-server/) | Server coordination layer. |
| [crates/waltr-agent/](crates/waltr-agent/) | Isolated agent library. |
| [crates/mcp/](crates/mcp/) | Generic Model Context Protocol primitives, client, server, transports, and spec tests. |
| [crates/waltr-client/](crates/waltr-client/) | Shared client state machine. |
| [crates/waltr-tui/](crates/waltr-tui/) | Terminal UI client. |
| [crates/waltr-web/](crates/waltr-web/) | Web UI client. |
| [docs/](docs/) | Repository documentation. |
| [docs/architecture/access-authentication.md](docs/architecture/access-authentication.md) | Canonical Waltr access-auth boundary and flows. |
| [docs/architecture/persistence.md](docs/architecture/persistence.md) | Canonical Waltr/MCP database ownership and durable OAuth state. |
| [docs/deployment.md](docs/deployment.md) | Database, Compose, OIDC, proxy, and native-client deployment. |
| [docs/runbooks/mcp-oauth-key-rotation-recovery.md](docs/runbooks/mcp-oauth-key-rotation-recovery.md) | Hosted MCP OAuth wrapping/signing-key rotation and recovery. |
| [docs/plans/](docs/plans/) | Active implementation plans and completed records; use canonical docs for current behavior. |
| [docs/mcp/](docs/mcp/) | Generic MCP protocol documentation. |

# Hints

Use the documenting skill when updating docs or AGENTS.md files.
Use the mcp-modification skill when modifying generic MCP protocol/client/server behavior.
Use the test-modification skill when modifying tests.
Use the protobuf-generation skill when changing proto files, Buf config, or generated gRPC Rust.
Use [CONTRIBUTING.md](CONTRIBUTING.md) for run, build, and test commands and commit guidance.
