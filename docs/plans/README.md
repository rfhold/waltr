# Implementation Plans

Plans preserve objectives, sequencing, acceptance criteria, and implementation decisions for substantial cross-cutting work. Active plans describe target behavior and are not the canonical description of current behavior. Completed plans remain as implementation records, including future-tense language from their planning phase.

Use [Access Authentication](../architecture/access-authentication.md), [Server Persistence](../architecture/persistence.md), [Clients](../architecture/clients.md), [MCP Protocol](../mcp/protocol.md), and [Deployment](../deployment.md) for current contracts. The plans remain ordered by their implementation dependency sequence.

## Active Plans

| Status | Plan | Covers |
| --- | --- | --- |
| In Progress | [MCP OAuth Authorization Server](mcp-oauth-authorization-server/README.md) | Hosted server, outbound migration, deployment definitions, aggregate gates, and browser OAuth are complete; live operational and load exercises remain. |

## Completed Foundation

The persistence and access-authentication foundation was implemented through these plans. Their persistence sections reflect the final standard SQLx migrators, separate SQLite ownership files, and PostgreSQL owner schemas rather than superseded shared-pool or custom-history designs.

| Order | Status | Plan | Covers |
| --- | --- | --- | --- |
| 1 | Complete | [MCP Streamable HTTP Session Persistence](mcp-session-persistence.md) | Generic MCP store trait, in-memory parity, SQLx-managed SQLite and PostgreSQL migrations, and durable SSE state. |
| 2 | Complete | [Waltr Server Persistence Foundation](waltr-server-persistence.md) | Separate target/pool configuration, SQLite ownership files, PostgreSQL owner schemas, startup migration, readiness, and MCP store injection. |
| 3 | Complete | [OIDC Server Gate](oidc-server-gate.md) | OIDC browser sessions, bearer validation, protected routes, gRPC, and the original direct-OIDC MCP access gate. |
| 4 | Complete | [OIDC Client And UI Flows](oidc-client-ui-flows.md) | Shared login state, web login, native TUI PKCE, transport credentials, reconnection, and the original external-issuer MCP flow. |

The completed [Hardening](hardening/README.md) workstreams resolved review findings across the test harness, access auth, client lifecycle, and MCP durability.
