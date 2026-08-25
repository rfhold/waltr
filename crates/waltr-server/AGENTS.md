# Boundaries

Waltr server is a coordination layer so multiple clients can stay on the same page about state.

# Index

| Path | Info |
| --- | --- |
| [src/persistence/](src/persistence/) | SQLite/PostgreSQL selection, Waltr migrations, and MCP store composition. |
| [src/access_auth/](src/access_auth/) | Waltr access OIDC config, discovery, principals, browser sessions, bearer validation, and SQL domain store. |
| [src/mcp_oauth.rs](src/mcp_oauth.rs) | Hosted MCP OAuth configuration, Authentik-session bridge, consent adapter, keyring loading, and lifecycle composition. |
| [src/persistence/outbound_oauth.rs](src/persistence/outbound_oauth.rs) | Durable outbound MCP callback, credential, and refresh-claim state. |
| [src/routine/](src/routine/) | Server-owned event adapters, host boundary, Routine engine lifecycle, invocation execution, and expressions. |
| [../../docs/architecture/routines.md](../../docs/architecture/routines.md) | Canonical Routine lifecycle, persistence, replay, restart, and client boundary. |
| [../../docs/architecture/persistence.md](../../docs/architecture/persistence.md) | Canonical database ownership, migration, and startup contract. |
| [../../docs/architecture/access-authentication.md](../../docs/architecture/access-authentication.md) | Canonical access-auth contract. |
| [../../docs/deployment.md](../../docs/deployment.md) | Environment and deployment contract. |

# Dependents

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Client depends on the server via the proto contract. |

# Contracts

Server code should coordinate shared state without taking ownership of client-side UI state or reducer behavior.

Keep Waltr access authentication, hosted MCP authorization, and provider/upstream-MCP credential OAuth separate. Authentik authenticates resource owners; the generic MCP server owns downstream grants/tokens, and Waltr's outbound flow is target-scoped.

`build_app()` owns environment loading and is async/fallible because persistence migrations and OIDC discovery precede binding. Explicit builders receive initialized `Persistence` and, where enabled, `AccessAuthenticator`.

Waltr and MCP use independent pools and standard embedded SQLx migrators. SQLite requires separate ownership files; PostgreSQL migration histories live under the `waltr` and `mcp` schemas. Waltr owns outbound OAuth attempts/credentials; MCP owns hosted clients, grants, tokens, authorization leases, and encrypted signing keys.

Manual Routine admission captures immutable source state under the session operation gate, then releases that gate before provider work. `spec.sourceSession` is manual-only; event-triggered Routines cannot request source capture. Request IDs provide correlation only; do not add retry or deduplication semantics implicitly. Durable admission and lifecycle state belong to Waltr SQL.

Every source-backed `sessionTemplate` atomically creates a durable helper with completed rollback-aware source history followed by its task prompt. Inference-only templates start with only their task prompt. Provider requests derive only from durable helper state, and ordinary session retention, delete, and wipe rules apply.

The fork compact helper stores `<fork-direction>\n<exact direction>\n</fork-direction>` after copied history. Its replacement system prompt treats the entire user message as untrusted emphasis text. The ordinary destination stores only compaction plus the original direction, not source history.

`SessionCreateRequest` atomically persists a Routine destination and its exact validated first prompt. Successful creation is the prompt-durability boundary. After cancellation or start failure, retain the destination and its ID.

# Hints

Use the server-modification skill when modifying this crate.
