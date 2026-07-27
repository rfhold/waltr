# Audience

Waltr server tests should act like a frontend is using the server through the client-facing contract.

# Boundaries

Server tests should validate shared state coordination, updates, watches, and status flows without asserting UI rendering behavior.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared gRPC and server setup for integration tests. |
| [config/](config/) | Config read, watch, validation, provider, and stale ETag flows. |
| [status/](status/) | Status watch and health flows. |
| [system/](system/) | Process boot and broader server system flows. |
| [access_auth.rs](access_auth.rs) | Public/protected routing, browser code flow, cookie/bearer gRPC, MCP authorization, restart, and security regressions. |
| [mcp_oauth_server.rs](mcp_oauth_server.rs) | Hosted issuer metadata/JWKS, browser resumption and consent, local token lifecycle, direct-upstream rejection, and restart. |

# Contracts

Keep access-auth tests at the server boundary. Unit tests own parser/verifier/store details; integration tests own externally visible HTTP, gRPC, MCP, persistence, and fail-closed startup behavior. Use the `web` feature for embedded-web navigation coverage.

# Hints

Use the test-modification skill when modifying tests.
