# Waltr Server Persistence Foundation Plan

## Status

Implemented by `b79820a` after the generic MCP store work. This is the second historical plan in the persistence and authentication sequence. Use [Server Persistence](../architecture/persistence.md) and [Deployment](../deployment.md) for the current contract.

## Objective

Give `waltr-server` a durable database foundation for OIDC sessions and future server-owned data while composing the MCP session stores supplied by the generic `mcp` crate.

The server owns Waltr and MCP database-target selection, startup policy, Waltr migrations, and dependency injection. It does not reimplement MCP session persistence or turn the generic MCP crate into a Waltr database layer.

## Preimplementation Baseline

Before this plan was implemented, Waltr had several persistence mechanisms with different ownership:

| Data | Owner before implementation | Storage before implementation |
| --- | --- | --- |
| Server configuration | `waltr-server` | TOML files. |
| Agent sessions | `waltr-agent` through `waltr-server` | Memory or filesystem by default; other agent backends exist. |
| Provider and upstream MCP credentials | `waltr-agent` credential store | Global JSON file. |
| Waltr MCP Streamable HTTP sessions | `mcp` router | Process-local memory. |
| User login sessions | Not implemented | None. |

These stores represent different contracts. Introducing a server database must not silently merge their schemas, keys, lifecycle, or configuration.

## Scope

- Add server database configuration and backend selection.
- Support SQLite and PostgreSQL.
- Open database resources during server startup.
- Run forward-only Waltr-owned raw SQL migrations during startup.
- Keep Waltr and MCP pools, SQLite files, and migration histories separate.
- Construct the matching MCP session store and inject it into the Waltr MCP router.
- Make readiness depend on successful database initialization and migration.
- Provide a server-owned persistence handle from which domain stores can be constructed.
- Establish test helpers for temporary SQLite and container-backed PostgreSQL databases.

This plan creates infrastructure and composition. OIDC tables and behavior are added by [OIDC Server Gate](oidc-server-gate.md).

## Boundaries

`waltr-server` owns:

- Deployment-facing database configuration.
- Waltr connection and pool lifecycle.
- Waltr schema migrations.
- Startup failure and readiness policy.
- Construction of server-owned domain stores.
- Selection and injection of an MCP-provided session store.

`mcp` owns its Streamable HTTP tables, migration history, store implementations, and protocol semantics.

`waltr-agent` continues to own agent session storage. The existing `WALTR_SESSION_STORE` and `WALTR_SESSION_FILESYSTEM_ROOT` settings must not be repurposed for the server database.

## Configuration

`WALTR_DATABASE_URL` selects Waltr-owned persistence. Optional `WALTR_MCP_DATABASE_URL` overrides the derived MCP target.

| URL form | Backend |
| --- | --- |
| `sqlite:...` | SQLite database. |
| `postgres://...` or `postgresql://...` | PostgreSQL database. |

The implemented no-configuration policy is:

- Use durable sibling `waltr.sqlite3` and `mcp.sqlite3` files under Waltr's XDG/HOME application data directory for normal server startup.
- Derive sibling `mcp.sqlite3` from any file-backed SQLite `WALTR_DATABASE_URL` when the MCP override is absent.
- Reuse the same logical PostgreSQL database through a separate MCP pool when the override is absent.
- Allow explicit independent in-memory SQLite URLs for tests and ephemeral development.
- Reject Waltr and MCP targets that resolve to the same file-backed SQLite path.
- Do not silently fall back to process memory after a configured database fails to open.

Path expansion and parent-directory creation belong in server configuration code. Logs must identify the selected backend without printing credentials.

## Persistence Handle

Add a small server-owned persistence module that represents an initialized backend. It should expose only what server domain stores need and should not begin as a generic CRUD abstraction.

The initial handle is responsible for:

- Owning the Waltr pool and the separately initialized MCP pool.
- Running Waltr migrations.
- Constructing future server domain stores, beginning with OIDC stores.
- Coordinating shutdown if the selected driver requires it.

Domain behavior should be expressed by domain-specific traits such as an OIDC login/session store. SQL should remain behind those stores rather than spreading through request handlers.

## MCP Composition

After server persistence initializes, startup will construct the matching store from the `mcp` crate:

| Server backend | MCP store |
| --- | --- |
| SQLite | MCP SQLite Streamable HTTP session store. |
| PostgreSQL | MCP PostgreSQL Streamable HTTP session store. |

The server will pass that store through `StreamableHttpOptions` when building `WaltrMcpServer`'s router.

The server does not duplicate MCP SQL queries or migrations. Waltr and MCP always use separate pools. File-backed SQLite uses separate ownership files. PostgreSQL uses the same logical database by default, while an explicit `WALTR_MCP_DATABASE_URL` may select another target. Both owners retain separate schemas, migration histories, and domain APIs.

## Schema Ownership

PostgreSQL will use separate schemas:

| Schema | Owner |
| --- | --- |
| `mcp` | Generic MCP Streamable HTTP persistence. |
| `waltr` | Waltr server-owned data. |

SQLite uses separate files because SQLx's standard history table has one fixed name per database:

| File | Owner and contents |
| --- | --- |
| `waltr.sqlite3` | Standard `_sqlx_migrations`, `waltr_` tables, and OIDC state. |
| `mcp.sqlite3` | Standard `_sqlx_migrations` and `mcp_` Streamable HTTP session/event tables. |

PostgreSQL migration connections use the owning search path, producing `waltr._sqlx_migrations` and `mcp._sqlx_migrations`. Both owners may start at migration version 1 without colliding.

## Migrations

Waltr migration files will be committed at:

- `crates/waltr-server/migrations/sqlite/`
- `crates/waltr-server/migrations/postgresql/`

Migration rules:

- Files contain raw SQL.
- Migrations are forward-only.
- Both owners embed migrations with standard `sqlx::migrate!()` migrators.
- SQLx owns `_sqlx_migrations`, applied-version tracking, and checksum validation.
- Applied files are never edited.
- Pending migrations run before protected routes and gRPC services are made available.
- Migration failure fails startup rather than serving against a partial schema.
- Migration coordination and history behavior follow SQLx's standard migrator behavior; Waltr adds no migration-history layer.
- SQLite foreign key enforcement is enabled on every connection.

The final migration set establishes Waltr ownership and adds the OIDC login-attempt and browser-session tables described by the OIDC plan.

## Startup Flow

Server construction in `crates/waltr-server/src/app.rs` separates database initialization so asynchronous startup completes before the router is returned.

The implemented startup order is:

1. Parse and validate `WALTR_DATABASE_URL` and optional `WALTR_MCP_DATABASE_URL`, deriving defaults where required.
2. Open the Waltr pool and run Waltr's standard embedded migrator.
3. Open the independent MCP pool and construct the MCP session store, which runs MCP's standard embedded migrator.
4. Build server domain services with the initialized persistence handle.
5. Build the Waltr MCP router with its persistent store.
6. Expose health and application routes.

The process does not bind or report readiness before both migrators succeed. Tests that construct the app without external resources receive two independent in-memory SQLite databases or temporary ownership files.

## Implementation Steps

1. Add database configuration parsing with redacted diagnostics.
2. Add the server persistence module and backend initialization.
3. Add Waltr migration directories and isolated history handling.
4. Refactor app construction to receive initialized persistence.
5. Select and inject the MCP SQL store for the configured backend.
6. Add test fixtures for migrated SQLite and PostgreSQL databases.
7. Document deployment configuration and startup failure behavior.

## Tests

Follow the server layering in [testing.md](../testing.md).

Configuration tests must cover:

- SQLite and PostgreSQL URL recognition.
- The chosen default database location.
- Explicit in-memory test configuration.
- Invalid URLs and unsupported schemes.
- Error messages and logs not exposing passwords.

Migration tests must cover:

- A new database reaching the latest version.
- A second startup applying no migrations.
- Checksum mismatch failing startup.
- A failed migration not being marked applied.
- MCP and Waltr migration versions not colliding across their ownership files or schemas.
- SQLite file collision rejection and PostgreSQL owner search paths.

Server integration tests must cover:

- Readiness after successful initialization.
- Startup failure for unavailable or unmigratable storage.
- MCP lifecycle and queued events surviving app reconstruction.
- Existing server configuration and agent session behavior remaining unchanged.

SQLite coverage runs normally. PostgreSQL behavior uses ignored container-backed targets selected with `-- --ignored`; see [Testing](../testing.md).

## Acceptance Criteria

- `WALTR_DATABASE_URL` selects Waltr persistence and optional `WALTR_MCP_DATABASE_URL` overrides MCP persistence.
- Default SQLite uses separate sibling ownership files and default PostgreSQL uses separate pools against one logical database.
- Waltr migrations run automatically before the server becomes ready.
- MCP migrations run through the MCP-provided implementation.
- Waltr and MCP tables and migration histories are isolated.
- Restarting `waltr-server` preserves MCP Streamable HTTP sessions.
- Configured database failures are fail-closed and visible during startup.
- Existing agent session storage configuration retains its current meaning.
- Future Waltr domain stores can receive the initialized persistence handle without changing router construction again.

## Phase-Local Non-Goals

These items were outside this persistence-foundation plan, not necessarily outside the completed four-plan implementation or future Waltr work.

- Moving TOML configuration into SQL.
- Replacing `waltr-agent::SessionStore` or changing agent session defaults.
- Moving provider credentials into SQL.
- Defining a users table or multi-user ownership model.
- Implementing OIDC routes, middleware, or sessions.
- Extracting a workspace-wide generic database crate before a second concrete consumer warrants it.

## Dependencies

This plan depends on [MCP Streamable HTTP Session Persistence](mcp-session-persistence.md). It enables [OIDC Server Gate](oidc-server-gate.md).

## References

- [`crates/waltr-server/src/app.rs`](../../crates/waltr-server/src/app.rs)
- [`crates/waltr-server/src/session/storage.rs`](../../crates/waltr-server/src/session/storage.rs)
- [`crates/waltr-server/AGENTS.md`](../../crates/waltr-server/AGENTS.md)
- [`crates/mcp/AGENTS.md`](../../crates/mcp/AGENTS.md)
- [Testing](../testing.md)
