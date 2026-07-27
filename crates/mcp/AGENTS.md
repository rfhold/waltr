# Index

| Path | Info |
| --- | --- |
| [src/](src/) | Generic MCP protocol types, typed client/server APIs, transports, and tracing helpers. |
| [tests/](tests/) | Protocol spec conformance and ecosystem interoperability tests. |
| [src/oauth/server/](src/oauth/server/) | Generic hosted authorization server, registration, stores, signing keys, tokens, revocation, and leases. |
| [migrations/](migrations/) | MCP-owned SQLite/PostgreSQL Streamable HTTP and hosted OAuth migrations. |

# Boundaries

This crate is a generic Model Context Protocol crate. It owns protocol primitives, JSON-RPC behavior, typed client/server APIs, transports, trace propagation helpers, and protocol-level tests.

Do not add Waltr-specific session behavior, provider behavior, permissions, timeline events, config policy, server APIs, or UI behavior here.

# Contracts

- The package name is `mcp`.
- The crate must not depend on `waltr-agent`, `waltr-server`, `waltr-client`, `waltr-tui`, or `waltr-web`.
- `rmcp` is allowed only as a dev-dependency for ecosystem interoperability tests.
- Trace propagation remains opt-in; generic client options default to disabled.
- Unsupported MCP areas should stay explicit in code and tests instead of silently passing.
- `StreamableHttpSessionStore` and `OAuthAuthorizationStore` are public protocol persistence contracts. SQL stores stay optional behind their SQLite/PostgreSQL features, use standard embedded SQLx migrators, own their database/schema and `_sqlx_migrations` history, and contain no Waltr, Authentik, cookie, or deployment policy.
- Hosted OAuth supports public clients only: preregistration, hardened CIMD, optional DCR, Authorization Code with PKCE S256, ES256 access tokens, rotating refresh families, revocation, and bounded authorization leases.
- MCP clients requiring transport cleanup must use explicit async shutdown; dropping a handle is not the cleanup contract.

# Hints

Use the mcp-modification skill when changing protocol types, client/server APIs, transports, trace propagation, spec conformance, or ecosystem interoperability.
Use the test-modification skill when changing MCP tests.
Update [../../docs/mcp/](../../docs/mcp/) when public MCP behavior changes.
