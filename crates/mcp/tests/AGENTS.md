# Audience

Tests here serve generic MCP protocol consumers and ecosystem compatibility, not Waltr agent/session behavior.

# Boundaries

| Path | Info |
| --- | --- |
| [spec/](spec/) | Revision-scoped MCP protocol conformance tests. |
| [ecosystem/](ecosystem/) | Interoperability tests against other MCP implementations. |
| [oauth_authorization_server.rs](oauth_authorization_server.rs) | Hosted OAuth authorization, registration, CIMD, and HTTP security behavior. |
| [oauth_sqlite_store.rs](oauth_sqlite_store.rs) | SQLite OAuth store parity, reopen, migrations, and secret-at-rest checks. |
| [oauth_postgres_store.rs](oauth_postgres_store.rs) | Ignored container-backed PostgreSQL OAuth parity and concurrency. |
| [oauth_store_support/](oauth_store_support/) | Shared memory, SQLite, and PostgreSQL OAuth atomic-state contract. |

# Contracts

- Keep spec tests grouped under the single supported protocol revision, currently `spec/v2026_07_28/`.
- Keep shared spec helpers minimal and protocol-focused.
- Use ecosystem tests for compatibility with crates such as `rmcp`; do not use ecosystem dependencies in runtime code.
- Cover list-change subscriptions and preserve explicit assertions that resource URI subscriptions, sampling, elicitation, and task runtime extensions are unsupported.

# Hints

Run focused spec coverage with `cargo test -p mcp --test spec`.
Run all MCP integration tests with `cargo test -p mcp --tests`.
Run OAuth SQL stores with `oauth-authorization-server-sqlite` or `oauth-authorization-server-postgres`. PostgreSQL targets require Docker and `-- --ignored`.
