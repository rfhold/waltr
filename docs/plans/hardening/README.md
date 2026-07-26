# Hardening Execution Plans

Working coordination documents for resolving all findings from the pre-commit review of the persistence, access-authentication, and integration-test work. All workstreams execute directly on `main`. Nothing here is committed until every workstream completes and the full verification order passes.

Ground rules for every workstream:

- Migrations are not deployed anywhere; edit existing migration files in place rather than adding forward migrations.
- `compose.yml` is local development and test infrastructure only. It is statically configured through profiles and is not a configurable deployment interface.
- Every browser RPC must flow through the shared reducer path; no direct transports in WASM bindings.
- Keep layer boundaries: generic `mcp` crate has no `waltr-*` dependencies; server owns validation/persistence; client owns shared server-data state; web/TUI own presentation only.
- Update each plan's Status line when starting and finishing it.

| Order | Status | Plan | Covers |
| --- | --- | --- | --- |
| 1 | Complete | [Test Harness](01-test-harness.md) | Static Compose profiles, TLS fixture, runner isolation, exit-code and teardown correctness. |
| 2 | Complete | [Persistence And OIDC Hardening](02-persistence-oidc.md) | SQLite target identity, redirect-free OIDC back channel, discovery/PKCE/metadata, insecure mode, token algorithms, advisory cleanup, trusted proxies. |
| 3 | Complete | [Browser Session Binding](03-session-binding.md) | Bind persisted browser sessions to configured issuer and client ID. |
| 4 | Complete | [Stream-Lifetime Authentication](04-stream-auth.md) | Principal leases, revocation registry, guarded gRPC bodies, cross-process revalidation. |
| 5 | Complete | [Client Transport And Lifecycle](05-client-lifecycle.md) | Native TLS, credential revisions, access generations, cancellation-safe effects, denied-state parking, logout reprobe. |
| 6 | Complete | [Web Shared Reducer Path](06-web-reducer-path.md) | Catalog commands/state/snapshot, removal of all direct WASM RPCs, web form migration. |
| 7 | Complete | [TUI Access Ownership](07-tui-access.md) | Route restoration, explicit-logout clearing, native metadata fail-closed, atomic credential clears. |
| 8 | Complete | [MCP Session Store Contract](08-mcp-session-store.md) | Owner-bound bounded durable sessions, acknowledgment-based resumable delivery, initialization atomicity, rewritten migrations. |
| 9 | Complete | [MCP Transport Refactor](09-mcp-transports.md) | Default-deny Origin, long-lived GET delivery, incremental SSE client, graceful DELETE shutdown, protocol version persistence. |
| 10 | Complete | [Waltr Integration And Docs](10-waltr-integration-docs.md) | MCP principal derivation, layered auth coverage, real logout browser test, documentation corrections. |

## Dependency Order

1 unblocks trustworthy verification for everything else. 2-4 are server-side and sequential (4 builds on 3's store changes). 5 precedes 6 and 7 (both consume client changes). 8 precedes 9 (transports consume the store contract), and 9 precedes 10. Workstreams 5-7 and 8-9 can proceed as parallel tracks after 4.

## Verification Order

Run after all workstreams complete, in this order:

1. `node --check scripts/test-web-integration.mjs`
2. `node --test scripts/compose-config.test.mjs scripts/test-web-integration.test.mjs`
3. `docker compose --profile dev config --quiet`
4. `docker compose --profile test config --quiet`
5. `docker compose --profile test-auth config --quiet`
6. `cargo test -p waltr-server --lib`
7. `cargo test -p waltr-server --tests`
8. `cargo test -p waltr-client --lib`
9. `cargo test -p waltr-client --tests`
10. `cargo test -p waltr-tui --lib`
11. `cargo test -p waltr-tui --tests`
12. `cargo test -p mcp --test spec`
13. `cargo test -p mcp --tests`
14. `cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store`
15. `cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored`
16. `pnpm --filter @rfhold/waltr-web wasm:build`
17. `pnpm --filter @rfhold/waltr-web test`
18. `pnpm --filter @rfhold/waltr-web build`
19. `pnpm test:web:integration:no-auth` and `pnpm test:web:integration:auth` focused runs
20. `pnpm test:web:integration` (dual profile)
21. Hostile-environment persistence-isolation run (exported `WALTR_DATABASE_URL`, `WALTR_MCP_DATABASE_URL`, `WALTR_AUTH_MODE`, `WALTR_OIDC_ISSUER`, `COMPOSE_PROFILES` must not affect behavior)
22. Controlled `SIGINT` run: exit 130, no containers/networks/volumes/temp roots remain
23. `cargo fmt --all -- --check`
24. `cargo clippy -p mcp -p waltr-server -p waltr-client -p waltr-tui --all-targets -- -D warnings`
25. `git diff --check`

## Final Acceptance

- Integration tests cannot touch ambient persistence and cannot report success after failure.
- OIDC back-channel requests never follow redirects; discovery and metadata fail closed.
- Reconfigured issuer/client invalidates persisted browser sessions.
- No protected gRPC stream survives logout, revocation, or credential expiry.
- Native HTTPS works; stale results and stale 401s cannot corrupt newer credential state.
- Commands cannot drop in-flight effects; denied access does not busy-loop.
- No browser RPC bypasses the shared reducer.
- TUI transient expiry preserves ownership; explicit logout clears protected local state.
- MCP sessions are owner-bound, bounded, resumable without pre-delivery deletion, and cleaned up gracefully; idle clients cause no write churn; present Origins are denied by default.
